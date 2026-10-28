use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;
use std::time::UNIX_EPOCH;

use super::*;
use crate::app::tests::{PKCS1, verified};

const BASE: &str = "https://api.example.test";
const REPO: &str = "https://api.example.test/repos/acme/widgets";
const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn sha(s: &str) -> Sha {
    Sha::parse(s).unwrap()
}

/// A canned response.
#[derive(Clone)]
struct Canned {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

fn ok(body: Value) -> Canned {
    status(200, body)
}

fn status(code: u16, body: Value) -> Canned {
    Canned {
        status: code,
        headers: Vec::new(),
        body: if body.is_null() {
            String::new()
        } else {
            body.to_string()
        },
    }
}

/// What the transport saw: method, url, body, `Authorization`.
type Seen = Rc<RefCell<Vec<(Method, String, Option<Value>, String)>>>;

const TOKEN: &str = "fixed-token";

/// A client under [`TOKEN`] whose transport answers `"<METHOD> <url>"` from
/// `routes`; an unrouted request fails the test.
fn client(routes: Vec<(String, Canned)>) -> (RestClient, Seen) {
    client_with(
        Auth::Token(TOKEN.to_owned()),
        routes,
        Rc::new(Cell::new(UNIX_EPOCH)),
    )
}

/// A client under `auth` and the clock `now`. A route given more than once
/// answers with each response in turn, then repeats the last.
fn client_with(
    auth: Auth,
    routes: Vec<(String, Canned)>,
    now: Rc<Cell<SystemTime>>,
) -> (RestClient, Seen) {
    let mut queues: BTreeMap<String, VecDeque<Canned>> = BTreeMap::new();
    for (key, canned) in routes {
        queues.entry(key).or_default().push_back(canned);
    }
    let seen: Seen = Rc::default();
    let log = Rc::clone(&seen);
    let transport: Transport = Box::new(move |request: &HttpRequest<'_>| {
        let key = format!("{} {}", request.method, request.url);
        log.borrow_mut().push((
            request.method,
            request.url.to_owned(),
            request.body.cloned(),
            request.authorization.to_owned(),
        ));
        let queue = queues
            .get_mut(&key)
            .unwrap_or_else(|| panic!("unrouted request {key}"));
        let canned = if queue.len() > 1 {
            queue.pop_front().unwrap()
        } else {
            queue[0].clone()
        };
        Ok(HttpResponse {
            status: canned.status,
            headers: canned
                .headers
                .iter()
                .map(|(n, v)| ((*n).to_owned(), v.clone()))
                .collect(),
            body: canned.body.clone(),
        })
    });
    (
        RestClient::with_transport(
            BASE,
            "acme",
            "widgets",
            auth,
            transport,
            Box::new(move || now.get()),
        ),
        seen,
    )
}

fn route(method: &str, path: &str, canned: Canned) -> (String, Canned) {
    (format!("{method} {REPO}{path}"), canned)
}

fn is_transient(e: &ApiError) -> bool {
    matches!(e, ApiError::Transient(_))
}

fn pr_json(number: u64, head: &str, labels: &[&str], merge_state: &str) -> Value {
    json!({
        "number": number,
        "title": format!("change {number}"),
        "state": "open",
        "head": { "sha": head, "ref": "topic", "repo": { "full_name": "acme/widgets" } },
        "base": { "sha": A, "ref": "main" },
        "labels": labels.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>(),
        "mergeable_state": merge_state,
        "draft": false,
        "merged": false,
    })
}

#[test]
fn get_pr_decodes_every_field() {
    let mut draft = pr_json(12, B, &["queue", "bug"], "unknown");
    draft["draft"] = json!(true);
    let (mut api, _) = client(vec![route("GET", "/pulls/12", ok(draft))]);
    let pr = api.get_pr(PrNumber(12)).unwrap();
    assert_eq!(
        pr,
        PrDetail {
            open: true,
            base: "main".to_owned(),
            head: sha(B),
            labels: vec!["queue".to_owned(), "bug".to_owned()],
            merge_state: MergeState::Unknown,
            draft: true,
            merged: false,
        }
    );
}

#[test]
fn get_pr_decodes_every_mergeable_state() {
    let words = [
        ("clean", MergeState::Clean),
        ("unstable", MergeState::Unstable),
        ("has_hooks", MergeState::HasHooks),
        ("behind", MergeState::Behind),
        ("blocked", MergeState::Blocked),
        ("dirty", MergeState::Dirty),
        ("draft", MergeState::Draft),
        ("unknown", MergeState::Unknown),
    ];
    for (word, state) in words {
        let (mut api, _) = client(vec![route("GET", "/pulls/2", ok(pr_json(2, B, &[], word)))]);
        assert_eq!(
            api.get_pr(PrNumber(2)).unwrap().merge_state,
            state,
            "{word}"
        );
    }
}

#[test]
fn get_pr_requires_a_known_mergeable_state_and_draft() {
    let mut odd = pr_json(2, B, &[], "sideways");
    let mut no_state = pr_json(3, B, &[], "clean");
    no_state.as_object_mut().unwrap().remove("mergeable_state");
    let mut no_draft = pr_json(4, B, &[], "clean");
    no_draft.as_object_mut().unwrap().remove("draft");
    odd["draft"] = json!(false);
    let (mut api, _) = client(vec![
        route("GET", "/pulls/2", ok(odd)),
        route("GET", "/pulls/3", ok(no_state)),
        route("GET", "/pulls/4", ok(no_draft)),
    ]);
    for (n, needle) in [
        (2, "unknown mergeable_state \"sideways\""),
        (3, "no mergeable_state field"),
        (4, "no draft field"),
    ] {
        let error = api.get_pr(PrNumber(n)).unwrap_err();
        assert!(!is_transient(&error), "{error}");
        assert!(error.to_string().contains(needle), "{error}");
    }
}

#[test]
fn get_pr_decodes_merged_and_requires_it() {
    let mut merged = pr_json(5, B, &[], "dirty");
    merged["state"] = json!("closed");
    merged["merged"] = json!(true);
    let mut silent = pr_json(6, B, &[], "clean");
    silent.as_object_mut().unwrap().remove("merged");
    let (mut api, _) = client(vec![
        route("GET", "/pulls/5", ok(merged)),
        route("GET", "/pulls/6", ok(silent)),
    ]);
    let five = api.get_pr(PrNumber(5)).unwrap();
    assert_eq!((five.open, five.merged), (false, true));
    let error = api.get_pr(PrNumber(6)).unwrap_err();
    assert!(!is_transient(&error), "{error}");
    assert!(error.to_string().contains("no merged field"), "{error}");
}

#[test]
fn get_pr_decodes_merge_state_and_closed() {
    let mut closed = pr_json(3, C, &[], "dirty");
    closed["state"] = json!("closed");
    let (mut api, _) = client(vec![
        route("GET", "/pulls/2", ok(pr_json(2, B, &[], "clean"))),
        route("GET", "/pulls/3", ok(closed)),
    ]);
    let two = api.get_pr(PrNumber(2)).unwrap();
    assert_eq!((two.open, two.merge_state), (true, MergeState::Clean));
    let three = api.get_pr(PrNumber(3)).unwrap();
    assert_eq!((three.open, three.merge_state), (false, MergeState::Dirty));
}

#[test]
fn check_runs_and_statuses_reduce_newest_first() {
    let runs = json!({
        "total_count": 4,
        "check_runs": [
            { "name": "lint", "status": "completed", "conclusion": "failure",
              "started_at": "2026-10-23T10:00:00Z", "completed_at": "2026-10-23T10:05:00Z" },
            { "name": "check", "status": "in_progress", "conclusion": null,
              "started_at": "2026-10-23T10:20:00Z", "completed_at": null },
            { "name": "docs", "status": "completed", "conclusion": "skipped",
              "started_at": "2026-10-23T10:01:00Z", "completed_at": "2026-10-23T10:01:00Z" },
            { "name": "queued", "status": "queued", "conclusion": null,
              "started_at": null, "completed_at": null },
        ],
    });
    let statuses = json!({
        "state": "failure",
        "statuses": [
            { "context": "check", "state": "success", "updated_at": "2026-10-23T10:10:00Z" },
            { "context": "ci/old", "state": "error", "updated_at": "2026-10-23T09:00:00Z" },
            { "context": "deploy", "state": "pending", "updated_at": "2026-10-23T10:30:00Z" },
        ],
    });
    let (mut api, _) = client(vec![
        route(
            "GET",
            &format!("/commits/{A}/check-runs?per_page=100"),
            ok(runs),
        ),
        route(
            "GET",
            &format!("/commits/{A}/status?per_page=100"),
            ok(statuses),
        ),
    ]);
    let results = api.checks(&sha(A)).unwrap();
    let reduced: Vec<(&str, CheckOutcome)> = results
        .iter()
        .map(|r| (r.name.as_str(), r.outcome))
        .collect();
    assert_eq!(
        reduced,
        vec![
            ("queued", CheckOutcome::Pending),
            ("deploy", CheckOutcome::Pending),
            ("check", CheckOutcome::Pending),
            ("check", CheckOutcome::Passed),
            ("lint", CheckOutcome::Failed),
            ("docs", CheckOutcome::Passed),
            ("ci/old", CheckOutcome::Failed),
        ]
    );
}

#[test]
fn check_run_conclusions_map_as_required_checks_count_them() {
    let run = |conclusion: &str| {
        json!({ "name": conclusion, "status": "completed", "conclusion": conclusion,
                "started_at": "2026-10-23T10:00:00Z", "completed_at": "2026-10-23T10:00:00Z" })
    };
    let names = [
        "success",
        "neutral",
        "skipped",
        "failure",
        "cancelled",
        "timed_out",
        "action_required",
        "stale",
    ];
    let runs = json!({ "check_runs": names.iter().map(|n| run(n)).collect::<Vec<_>>() });
    let (mut api, _) = client(vec![
        route(
            "GET",
            &format!("/commits/{A}/check-runs?per_page=100"),
            ok(runs),
        ),
        route(
            "GET",
            &format!("/commits/{A}/status?per_page=100"),
            ok(json!({ "statuses": [] })),
        ),
    ]);
    let results = api.checks(&sha(A)).unwrap();
    let reduced: Vec<(&str, CheckOutcome)> = results
        .iter()
        .map(|r| (r.name.as_str(), r.outcome))
        .collect();
    assert_eq!(
        reduced,
        vec![
            ("success", CheckOutcome::Passed),
            ("neutral", CheckOutcome::Passed),
            ("skipped", CheckOutcome::Passed),
            ("failure", CheckOutcome::Failed),
            ("cancelled", CheckOutcome::Failed),
            ("timed_out", CheckOutcome::Failed),
            ("action_required", CheckOutcome::Failed),
            ("stale", CheckOutcome::Failed),
        ]
    );
}

#[test]
fn checks_merge_every_page_newest_first() {
    let run = |name: &str, conclusion: &str, at: &str| {
        json!({ "name": name, "status": "completed", "conclusion": conclusion,
                "started_at": at, "completed_at": at })
    };
    let page2 = format!("{REPO}/commits/{A}/check-runs?per_page=100&page=2");
    let mut first = ok(json!({ "check_runs": [
        run("check", "failure", "2026-10-23T10:00:00Z"),
        run("lint", "success", "2026-10-23T10:02:00Z"),
    ] }));
    first
        .headers
        .push(("link", format!("<{page2}>; rel=\"next\"")));
    let second = ok(json!({ "check_runs": [
        run("check", "success", "2026-10-23T10:05:00Z"),
    ] }));
    let (mut api, _) = client(vec![
        route(
            "GET",
            &format!("/commits/{A}/check-runs?per_page=100"),
            first,
        ),
        (format!("GET {page2}"), second),
        route(
            "GET",
            &format!("/commits/{A}/status?per_page=100"),
            ok(json!({ "statuses": [] })),
        ),
    ]);
    let results = api.checks(&sha(A)).unwrap();
    let reduced: Vec<(&str, CheckOutcome)> = results
        .iter()
        .map(|r| (r.name.as_str(), r.outcome))
        .collect();
    assert_eq!(
        reduced,
        vec![
            ("check", CheckOutcome::Passed),
            ("lint", CheckOutcome::Passed),
            ("check", CheckOutcome::Failed),
        ]
    );
}

#[test]
fn unexpected_check_and_pr_states_are_permanent() {
    let completed_without_conclusion = json!({ "check_runs": [
        { "name": "check", "status": "completed", "conclusion": null,
          "started_at": "2026-10-23T10:00:00Z", "completed_at": "2026-10-23T10:01:00Z" },
    ] });
    let (mut api, _) = client(vec![route(
        "GET",
        &format!("/commits/{A}/check-runs?per_page=100"),
        ok(completed_without_conclusion),
    )]);
    let error = api.checks(&sha(A)).unwrap_err();
    assert!(!is_transient(&error));
    assert!(
        error.to_string().contains("\"check\" has no conclusion"),
        "{error}"
    );

    let (mut api, _) = client(vec![
        route(
            "GET",
            &format!("/commits/{A}/check-runs?per_page=100"),
            ok(json!({ "check_runs": [] })),
        ),
        route(
            "GET",
            &format!("/commits/{A}/status?per_page=100"),
            ok(json!({ "statuses": [
                { "context": "check", "state": "expected", "updated_at": "2026-10-23T10:00:00Z" },
            ] })),
        ),
    ]);
    let error = api.checks(&sha(A)).unwrap_err();
    assert!(!is_transient(&error));
    assert!(
        error
            .to_string()
            .contains("unknown status state \"expected\""),
        "{error}"
    );

    let mut merged = pr_json(4, B, &[], "unknown");
    merged["state"] = json!("merged");
    let (mut api, _) = client(vec![route("GET", "/pulls/4", ok(merged))]);
    let error = api.get_pr(PrNumber(4)).unwrap_err();
    assert!(!is_transient(&error));
    assert!(
        error.to_string().contains("unknown state \"merged\""),
        "{error}"
    );
}

#[test]
fn compare_status_maps_to_ancestry() {
    let cases = [
        ("identical", Some(true)),
        ("ahead", Some(true)),
        ("behind", Some(false)),
        ("diverged", Some(false)),
        ("sideways", None),
    ];
    for (status, expected) in cases {
        let (mut api, _) = client(vec![route(
            "GET",
            &format!("/compare/{A}...{B}"),
            ok(json!({
                "status": status,
                "ahead_by": 1,
                "behind_by": 0,
                "merge_base_commit": { "sha": A },
            })),
        )]);
        match (api.is_ancestor(&sha(A), &sha(B)), expected) {
            (Ok(got), Some(want)) => assert_eq!(got, want, "{status}"),
            (Err(e), None) => assert!(!is_transient(&e), "{status}"),
            (got, want) => panic!("{status}: got {got:?}, want {want:?}"),
        }
    }
}

#[test]
fn compare_decodes_status_ahead_by_and_merge_base() {
    let (mut api, _) = client(vec![route(
        "GET",
        &format!("/compare/{A}...{B}"),
        ok(json!({
            "status": "diverged",
            "ahead_by": 7,
            "behind_by": 2,
            "merge_base_commit": { "sha": C, "commit": {} },
        })),
    )]);
    assert_eq!(
        api.compare(&sha(A), &sha(B)).unwrap(),
        Compare {
            status: CompareStatus::Diverged,
            ahead_by: 7,
            merge_base: sha(C),
        }
    );
    let defects = [
        (
            json!({ "status": "ahead", "merge_base_commit": { "sha": C } }),
            "ahead_by",
        ),
        (
            json!({ "status": "ahead", "ahead_by": 1 }),
            "merge_base_commit",
        ),
        (
            json!({
                "status": "ahead",
                "ahead_by": 1,
                "merge_base_commit": { "sha": "not a sha" },
            }),
            "not a sha",
        ),
    ];
    for (body, named) in defects {
        let (mut api, _) = client(vec![route("GET", &format!("/compare/{A}...{B}"), ok(body))]);
        let error = api.compare(&sha(A), &sha(B)).unwrap_err();
        assert!(!is_transient(&error), "{named}");
        assert!(error.to_string().contains(named), "{error}");
    }
}

#[test]
fn compare_not_found_is_permanent() {
    let (mut api, _) = client(vec![route(
        "GET",
        &format!("/compare/{A}...{B}"),
        status(404, json!({ "message": "Not Found" })),
    )]);
    let error = api.is_ancestor(&sha(A), &sha(B)).unwrap_err();
    assert!(!is_transient(&error));
}

#[test]
fn list_follows_link_next() {
    let page2 = format!("{REPO}/pulls?state=open&per_page=100&page=2");
    let mut first = ok(json!([pr_json(1, A, &["queue"], "unknown")]));
    first.headers.push((
        "link",
        format!("<{page2}>; rel=\"next\", <{page2}>; rel=\"last\""),
    ));
    let mut second = ok(json!([pr_json(2, B, &[], "unknown")]));
    second.headers.push((
        "link",
        format!("<{REPO}/pulls?page=1>; rel=\"prev\", <{REPO}/pulls?page=1>; rel=\"first\""),
    ));
    let (mut api, seen) = client(vec![
        route("GET", "/pulls?state=open&per_page=100", first),
        (format!("GET {page2}"), second),
    ]);
    let prs = api.list_open_prs().unwrap();
    assert_eq!(
        prs,
        vec![
            PrSummary {
                number: PrNumber(1),
                head: sha(A),
                base: "main".to_owned(),
                head_branch: Some("topic".to_owned()),
                labels: vec!["queue".to_owned()],
                title: "change 1".to_owned(),
            },
            PrSummary {
                number: PrNumber(2),
                head: sha(B),
                base: "main".to_owned(),
                head_branch: Some("topic".to_owned()),
                labels: vec![],
                title: "change 2".to_owned(),
            },
        ]
    );
    assert_eq!(seen.borrow().len(), 2);
}

#[test]
fn a_failed_later_page_fails_the_list() {
    let page2 = format!("{REPO}/pulls?state=open&per_page=100&page=2");
    let mut first = ok(json!([pr_json(1, A, &["queue"], "unknown")]));
    first
        .headers
        .push(("link", format!("<{page2}>; rel=\"next\"")));
    let (mut api, _) = client(vec![
        route("GET", "/pulls?state=open&per_page=100", first),
        (
            format!("GET {page2}"),
            status(502, json!({ "message": "Bad Gateway" })),
        ),
    ]);
    let error = api.list_open_prs().unwrap_err();
    assert!(is_transient(&error), "{error}");
    assert!(error.to_string().contains("HTTP 502"), "{error}");
}

#[test]
fn list_reads_each_base_and_head_branch_but_not_a_forks() {
    let mut stacked = pr_json(1, A, &["queue"], "unknown");
    stacked["base"]["ref"] = json!("feature/one");
    stacked["head"]["ref"] = json!("feature/two");
    let mut shouting = pr_json(2, B, &[], "unknown");
    shouting["head"]["repo"]["full_name"] = json!("ACME/Widgets");
    let mut fork = pr_json(3, C, &[], "unknown");
    fork["head"]["repo"]["full_name"] = json!("someone/widgets");
    let mut deleted_fork = pr_json(4, C, &[], "unknown");
    deleted_fork["head"]["repo"] = Value::Null;
    let (mut api, _) = client(vec![route(
        "GET",
        "/pulls?state=open&per_page=100",
        ok(json!([stacked, shouting, fork, deleted_fork])),
    )]);
    let prs = api.list_open_prs().unwrap();
    let got: Vec<(u64, &str, Option<&str>)> = prs
        .iter()
        .map(|p| (p.number.0, p.base.as_str(), p.head_branch.as_deref()))
        .collect();
    assert_eq!(
        got,
        vec![
            (1, "feature/one", Some("feature/two")),
            (2, "main", Some("topic")),
            (3, "main", None),
            (4, "main", None),
        ]
    );
}

#[test]
fn set_base_patches_the_pull_request() {
    let (mut api, seen) = client(vec![route(
        "PATCH",
        "/pulls/7",
        ok(pr_json(7, A, &[], "unknown")),
    )]);
    api.set_base(PrNumber(7), "main").unwrap();
    let seen = seen.borrow();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, Method::Patch);
    assert_eq!(seen[0].2, Some(json!({ "base": "main" })));
}

#[test]
fn set_base_maps_a_refusal() {
    let no_commits = "There are no new commits between base branch 'main' and head branch 'g'";
    let cases = [
        (
            status(
                422,
                json!({
                    "message": "Validation Failed",
                    "errors": [{
                        "message": no_commits,
                        "resource": "PullRequest",
                        "field": "base",
                        "code": "invalid",
                    }],
                }),
            ),
            false,
            Some(format!("HTTP 422: Validation Failed: {no_commits}")),
        ),
        (
            status(
                422,
                json!({ "message": "Validation Failed", "errors": [{ "code": "invalid" }] }),
            ),
            false,
            Some("HTTP 422: Validation Failed: invalid".to_owned()),
        ),
        (
            status(
                422,
                json!({
                    "message": "Validation Failed",
                    "errors": ["base is wrong", { "field": "base" }, { "code": "invalid" }],
                }),
            ),
            false,
            Some("HTTP 422: Validation Failed: base is wrong; invalid".to_owned()),
        ),
        (
            status(403, json!({ "message": "Resource not accessible" })),
            false,
            Some("HTTP 403: Resource not accessible".to_owned()),
        ),
        (status(502, Value::Null), true, None),
    ];
    for (canned, transient, text) in cases {
        let code = canned.status;
        let (mut api, _) = client(vec![route("PATCH", "/pulls/7", canned)]);
        let error = api.set_base(PrNumber(7), "main").unwrap_err();
        assert_eq!(is_transient(&error), transient, "HTTP {code}: {error}");
        let error = error.to_string();
        assert!(error.contains("PATCH /pulls/7"), "{error}");
        if let Some(text) = text {
            assert!(error.ends_with(&text), "{error}");
        }
    }
}

#[test]
fn merged_pr_heads_asks_for_trunk_merges_of_the_branch_and_keeps_merged_ones() {
    let mut merged = pr_json(5, A, &[], "unknown");
    merged["state"] = json!("closed");
    merged["merged_at"] = json!("2026-10-01T00:00:00Z");
    let mut abandoned = pr_json(6, B, &[], "unknown");
    abandoned["state"] = json!("closed");
    abandoned["merged_at"] = Value::Null;
    let mut also_merged = pr_json(8, C, &[], "unknown");
    also_merged["state"] = json!("closed");
    also_merged["merged_at"] = json!("2026-10-02T00:00:00Z");
    // The route matches the whole query, so it is what checks `base=main`.
    let (mut api, _) = client(vec![route(
        "GET",
        "/pulls?state=closed&head=acme%3Afeature%2Fone&base=main&per_page=100",
        ok(json!([merged, abandoned, also_merged])),
    )]);
    assert_eq!(
        api.merged_pr_heads("feature/one", "main").unwrap(),
        vec![sha(A), sha(C)]
    );
}

#[test]
fn next_link_parses_only_rel_next() {
    assert_eq!(
        next_link("<https://x/a?page=3>; rel=\"next\", <https://x/a?page=9>; rel=\"last\""),
        Some("https://x/a?page=3".to_owned())
    );
    assert_eq!(next_link("<https://x/a?page=1>; rel=\"prev\""), None);
    assert_eq!(next_link(""), None);
}

#[test]
fn status_codes_map_to_error_kinds() {
    let rate_limited = || {
        let mut c = status(403, json!({ "message": "API rate limit exceeded" }));
        c.headers.push(("x-ratelimit-remaining", "0".to_owned()));
        c.headers
            .push(("x-ratelimit-reset", "1792000000".to_owned()));
        c
    };
    let secondary = || {
        let mut c = status(403, json!({ "message": "secondary rate limit" }));
        c.headers.push(("retry-after", "60".to_owned()));
        c
    };
    let secondary_without_headers = || {
        status(
            403,
            json!({ "message": "You have exceeded a secondary rate limit. Please wait." }),
        )
    };
    let moved = || {
        let mut c = status(301, json!({ "message": "Moved Permanently" }));
        c.headers
            .push(("location", format!("{BASE}/repositories/42")));
        c
    };
    let cases: Vec<(Canned, bool)> = vec![
        (status(500, json!({ "message": "boom" })), true),
        (status(502, Value::Null), true),
        (status(429, json!({ "message": "slow down" })), true),
        (rate_limited(), true),
        (secondary(), true),
        (secondary_without_headers(), true),
        (moved(), false),
        (status(401, json!({ "message": "Bad credentials" })), false),
        (
            status(403, json!({ "message": "Resource not accessible" })),
            false,
        ),
        (status(404, json!({ "message": "Not Found" })), false),
        (
            status(422, json!({ "message": "Validation Failed" })),
            false,
        ),
    ];
    for (canned, transient) in cases {
        let code = canned.status;
        let (mut api, _) = client(vec![route("GET", &format!("/git/commits/{A}"), canned)]);
        let error = api.get_commit(&sha(A)).unwrap_err();
        assert_eq!(is_transient(&error), transient, "HTTP {code}: {error}");
        let text = error.to_string();
        assert!(text.contains(&format!("/git/commits/{A}")), "{text}");
        assert!(text.contains(&format!("HTTP {code}")), "{text}");
        assert!(!text.contains(INSTALLATION_TOKEN_REJECTED), "{text}");
    }
}

#[test]
fn rate_limit_message_names_the_reset_time() {
    let mut c = status(403, json!({ "message": "API rate limit exceeded" }));
    c.headers.push(("x-ratelimit-remaining", "0".to_owned()));
    c.headers
        .push(("x-ratelimit-reset", "1792000000".to_owned()));
    let (mut api, _) = client(vec![route("GET", "/labels/queue", c)]);
    let text = api.label_exists("queue").unwrap_err().to_string();
    assert!(text.contains("1792000000"), "{text}");
    assert!(text.contains("API rate limit exceeded"), "{text}");
}

#[test]
fn secondary_rate_limit_without_headers_says_wait_a_minute() {
    let c = status(
        403,
        json!({ "message": "You have exceeded a secondary rate limit." }),
    );
    let (mut api, _) = client(vec![route("GET", "/labels/queue", c)]);
    let error = api.label_exists("queue").unwrap_err();
    assert!(is_transient(&error));
    assert!(
        error.to_string().contains("retry after at least 60 s"),
        "{error}"
    );
}

#[test]
fn redirect_is_permanent_and_names_the_location() {
    let mut c = status(307, json!({ "message": "Temporary Redirect" }));
    c.headers
        .push(("location", format!("{BASE}/repositories/42/git/refs")));
    let (mut api, _) = client(vec![route("POST", "/git/refs", c)]);
    let error = api.create_ref("refs/heads/x", &sha(A)).unwrap_err();
    assert!(!is_transient(&error));
    let text = error.to_string();
    assert!(text.contains("HTTP 307"), "{text}");
    assert!(
        text.contains(&format!("redirected to {BASE}/repositories/42/git/refs")),
        "{text}"
    );
    assert!(text.contains("update owner and repo"), "{text}");
}

#[test]
fn transport_failures_keep_their_kind() {
    for transient in [true, false] {
        let transport: Transport = Box::new(move |_: &HttpRequest<'_>| {
            Err(TransportError {
                transient,
                message: "connection reset".to_owned(),
            })
        });
        let mut api = RestClient::with_transport(
            BASE,
            "acme",
            "widgets",
            Auth::Token(TOKEN.to_owned()),
            transport,
            Box::new(SystemTime::now),
        );
        let error = api.get_ref("refs/heads/main").unwrap_err();
        assert_eq!(is_transient(&error), transient);
        assert!(error.to_string().contains("connection reset"));
    }
}

#[test]
fn malformed_sha_is_permanent() {
    let (mut api, _) = client(vec![route(
        "GET",
        "/git/ref/heads/main",
        ok(json!({ "ref": "refs/heads/main", "object": { "sha": "ABC", "type": "commit" } })),
    )]);
    let error = api.get_ref("refs/heads/main").unwrap_err();
    assert!(!is_transient(&error));
    assert!(error.to_string().contains("\"ABC\""), "{error}");
}

#[test]
fn get_ref_absent_is_none() {
    let (mut api, _) = client(vec![
        route(
            "GET",
            "/git/ref/heads/dorfq/batch/7",
            status(404, json!({ "message": "Not Found" })),
        ),
        route(
            "GET",
            "/git/ref/heads/main",
            ok(json!({ "ref": "refs/heads/main", "object": { "sha": A } })),
        ),
    ]);
    assert_eq!(api.get_ref("refs/heads/dorfq/batch/7").unwrap(), None);
    assert_eq!(api.get_ref("refs/heads/main").unwrap(), Some(sha(A)));
}

#[test]
fn ref_names_must_be_full() {
    let (mut api, seen) = client(vec![]);
    assert!(!is_transient(&api.get_ref("heads/main").unwrap_err()));
    assert!(seen.borrow().is_empty());
}

#[test]
fn list_refs_uses_matching_refs() {
    let (mut api, _) = client(vec![route(
        "GET",
        "/git/matching-refs/dorfq/batches/?per_page=100",
        ok(json!([
            { "ref": "refs/dorfq/batches/1", "object": { "sha": A } },
            { "ref": "refs/dorfq/batches/2", "object": { "sha": B } },
        ])),
    )]);
    assert_eq!(
        api.list_refs("refs/dorfq/batches/").unwrap(),
        vec![
            RefEntry {
                name: "refs/dorfq/batches/1".to_owned(),
                sha: sha(A),
            },
            RefEntry {
                name: "refs/dorfq/batches/2".to_owned(),
                sha: sha(B),
            },
        ]
    );
}

#[test]
fn create_and_update_ref_send_the_documented_bodies() {
    let (mut api, seen) = client(vec![
        route("POST", "/git/refs", status(201, json!({ "ref": "x" }))),
        route("PATCH", "/git/refs/heads/main", ok(json!({ "ref": "x" }))),
    ]);
    api.create_ref("refs/dorfq/batches/3", &sha(A)).unwrap();
    assert_eq!(
        api.update_ref("refs/heads/main", &sha(B), false).unwrap(),
        RefUpdate::Updated
    );
    let seen = seen.borrow();
    assert_eq!(
        seen[0].2,
        Some(json!({ "ref": "refs/dorfq/batches/3", "sha": A }))
    );
    assert_eq!(seen[1].2, Some(json!({ "sha": B, "force": false })));
}

#[test]
fn update_ref_not_fast_forward_is_an_outcome() {
    let (mut api, _) = client(vec![route(
        "PATCH",
        "/git/refs/heads/main",
        status(422, json!({ "message": "Update is not a fast forward" })),
    )]);
    assert_eq!(
        api.update_ref("refs/heads/main", &sha(B), false).unwrap(),
        RefUpdate::NotFastForward
    );
}

#[test]
fn update_ref_other_422_is_permanent() {
    let (mut api, _) = client(vec![route(
        "PATCH",
        "/git/refs/heads/main",
        status(422, json!({ "message": "Object does not exist" })),
    )]);
    let error = api
        .update_ref("refs/heads/main", &sha(B), false)
        .unwrap_err();
    assert!(!is_transient(&error));
}

#[test]
fn delete_ref_tolerates_absence() {
    let (mut api, _) = client(vec![
        route(
            "DELETE",
            "/git/refs/heads/dorfq/build/1",
            status(204, Value::Null),
        ),
        route(
            "DELETE",
            "/git/refs/heads/dorfq/build/2",
            status(404, json!({ "message": "Not Found" })),
        ),
        route(
            "DELETE",
            "/git/refs/heads/dorfq/build/3",
            status(422, json!({ "message": "Reference does not exist" })),
        ),
        route(
            "DELETE",
            "/git/refs/heads/dorfq/build/4",
            status(422, json!({ "message": "Something else" })),
        ),
    ]);
    api.delete_ref("refs/heads/dorfq/build/1").unwrap();
    api.delete_ref("refs/heads/dorfq/build/2").unwrap();
    api.delete_ref("refs/heads/dorfq/build/3").unwrap();
    assert!(api.delete_ref("refs/heads/dorfq/build/4").is_err());
}

#[test]
fn commits_round_trip() {
    let (mut api, seen) = client(vec![
        route(
            "GET",
            &format!("/git/commits/{A}"),
            ok(json!({
                "sha": A,
                "message": "dorfq batch record v1\n",
                "tree": { "sha": B },
                "parents": [{ "sha": C }],
            })),
        ),
        route("POST", "/git/commits", status(201, json!({ "sha": A }))),
    ]);
    assert_eq!(
        api.get_commit(&sha(A)).unwrap(),
        Commit {
            message: "dorfq batch record v1\n".to_owned(),
            tree: sha(B),
            parents: vec![sha(C)],
        }
    );
    assert_eq!(
        api.create_commit("record", &sha(B), &[sha(C)]).unwrap(),
        sha(A)
    );
    assert_eq!(
        seen.borrow()[1].2,
        Some(json!({ "message": "record", "tree": B, "parents": [C] }))
    );
}

#[test]
fn merge_outcomes() {
    for (code, body, expected) in [
        (201, json!({ "sha": C }), Some(Merge::Merged(sha(C)))),
        (204, Value::Null, Some(Merge::NothingToMerge)),
        (
            409,
            json!({ "message": "Merge conflict" }),
            Some(Merge::Conflict),
        ),
        (403, json!({ "message": "refusing to allow" }), None),
        (404, json!({ "message": "Base does not exist" }), None),
    ] {
        let (mut api, seen) = client(vec![route("POST", "/merges", status(code, body))]);
        match (api.merge("dorfq/build/1", &sha(B), "Merge #2"), expected) {
            (Ok(got), Some(want)) => assert_eq!(got, want),
            (Err(e), None) => assert!(!is_transient(&e), "{code}"),
            (got, want) => panic!("HTTP {code}: got {got:?}, want {want:?}"),
        }
        assert_eq!(
            seen.borrow()[0].2,
            Some(json!({ "base": "dorfq/build/1", "head": B, "commit_message": "Merge #2" }))
        );
    }
}

#[test]
fn labels_are_encoded_and_absence_tolerated() {
    let (mut api, _) = client(vec![
        route(
            "DELETE",
            "/issues/4/labels/merge%20queue",
            status(404, json!({ "message": "Label does not exist" })),
        ),
        route(
            "GET",
            "/labels/merge%20queue",
            ok(json!({ "name": "merge queue" })),
        ),
        route(
            "GET",
            "/labels/queue",
            status(404, json!({ "message": "Not Found" })),
        ),
    ]);
    api.remove_label(PrNumber(4), "merge queue").unwrap();
    assert!(api.label_exists("merge queue").unwrap());
    assert!(!api.label_exists("queue").unwrap());
}

#[test]
fn comments_list_and_create() {
    let (mut api, seen) = client(vec![
        route(
            "GET",
            "/issues/9/comments?per_page=100",
            ok(json!([{ "body": "first" }, { "body": "second" }])),
        ),
        route(
            "POST",
            "/issues/9/comments",
            status(201, json!({ "id": 1 })),
        ),
    ]);
    assert_eq!(
        api.list_comments(PrNumber(9)).unwrap(),
        vec!["first".to_owned(), "second".to_owned()]
    );
    api.create_comment(PrNumber(9), "hello").unwrap();
    assert_eq!(seen.borrow()[1].2, Some(json!({ "body": "hello" })));
}

#[test]
fn json_helpers_address_the_repository() {
    let (mut api, seen) = client(vec![
        route("GET", "/labels", ok(json!([]))),
        route("POST", "/labels", status(201, json!({ "name": "queue" }))),
        route("PATCH", "/pulls/3", ok(json!({ "state": "closed" }))),
        route("DELETE", "/git/refs/heads/x", status(204, Value::Null)),
        route(
            "GET",
            "/missing",
            status(404, json!({ "message": "Not Found" })),
        ),
    ]);
    assert_eq!(api.get("/labels").unwrap(), json!([]));
    assert_eq!(
        api.post("/labels", &json!({ "name": "queue" })).unwrap(),
        json!({ "name": "queue" })
    );
    assert_eq!(
        api.patch("/pulls/3", &json!({ "state": "closed" }))
            .unwrap()["state"],
        "closed"
    );
    api.delete("/git/refs/heads/x").unwrap();
    assert!(!is_transient(&api.get("/missing").unwrap_err()));
    assert_eq!(seen.borrow().len(), 5);
}

#[test]
fn encode_keeps_only_unreserved() {
    assert_eq!(encode("a-b_c.d~e"), "a-b_c.d~e");
    assert_eq!(encode("merge queue/1"), "merge%20queue%2F1");
    assert_eq!(encode("é"), "%C3%A9");
    assert_eq!(encode("a&b#c=d?e+f"), "a%26b%23c%3Dd%3Fe%2Bf");
}

#[test]
fn api_error_maps_to_forge_error_of_the_same_kind() {
    let transient: ForgeError = ApiError::Transient("later".into()).into();
    assert!(matches!(transient, ForgeError::Transient(_)));
    let permanent: ForgeError = ApiError::Permanent("never".into()).into();
    assert!(matches!(permanent, ForgeError::Permanent(_)));
    assert_eq!(permanent.to_string(), "permanent forge error: never");
}

#[test]
fn token_auth_sends_the_token_on_every_request_to_the_repository() {
    let (mut api, seen) = client(vec![
        route("GET", "/labels/queue", ok(json!({ "name": "queue" }))),
        route(
            "GET",
            "/issues/9/comments?per_page=100",
            ok(json!([{ "body": "first" }])),
        ),
    ]);
    api.label_exists("queue").unwrap();
    api.list_comments(PrNumber(9)).unwrap();
    let seen = seen.borrow();
    assert_eq!(seen.len(), 2);
    for (_, url, _, authorization) in seen.iter() {
        assert!(url.starts_with(&format!("{REPO}/")), "{url}");
        assert_eq!(authorization, &format!("Bearer {TOKEN}"));
    }
}

const CLIENT_ID: &str = "Iv1.acme";
const MINT: &str = "POST https://api.example.test/app/installations/42/access_tokens";

fn app() -> Auth {
    Auth::App(AppKey::from_pem(CLIENT_ID, PKCS1).unwrap())
}

fn installation() -> (String, Canned) {
    route("GET", "/installation", ok(json!({ "id": 42 })))
}

/// An access-token response; its `expires_at` is long past, which a client
/// that read it would act on.
fn minted(token: &str) -> (String, Canned) {
    (
        MINT.to_owned(),
        status(
            201,
            json!({ "token": token, "expires_at": "2000-01-01T00:00:00Z" }),
        ),
    )
}

fn label() -> (String, Canned) {
    route("GET", "/labels/queue", ok(json!({ "name": "queue" })))
}

fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

#[test]
fn app_auth_mints_caches_and_renews_installation_tokens() {
    let t0 = 1_800_000_000;
    let clock = Rc::new(Cell::new(at(t0)));
    let (mut api, seen) = client_with(
        app(),
        vec![
            installation(),
            minted("installation-token-1"),
            minted("installation-token-2"),
            label(),
        ],
        Rc::clone(&clock),
    );
    let key = AppKey::from_pem(CLIENT_ID, PKCS1).unwrap();

    api.label_exists("queue").unwrap();
    {
        let seen = seen.borrow();
        assert_eq!(seen.len(), 3);
        assert_eq!(
            (seen[0].0, seen[0].1.as_str()),
            (Method::Get, format!("{REPO}/installation").as_str())
        );
        let jwt = seen[0].3.strip_prefix("Bearer ").unwrap();
        let (_, claims) = verified(&key, jwt);
        assert_eq!(claims["iss"], CLIENT_ID);
        assert_eq!(claims["iat"], t0 - 60);
        assert_eq!(format!("{} {}", seen[1].0, seen[1].1), MINT);
        assert_eq!(seen[1].2, Some(json!({ "repositories": ["widgets"] })));
        assert_eq!(seen[1].3, seen[0].3);
        assert_eq!(seen[2].1, format!("{REPO}/labels/queue"));
        assert_eq!(seen[2].3, "Bearer installation-token-1");
    }

    api.label_exists("queue").unwrap();
    assert_eq!(seen.borrow().len(), 4);
    assert_eq!(seen.borrow()[3].3, "Bearer installation-token-1");

    // A clock stepped back makes the token look younger, not expired.
    clock.set(at(t0 - 60 * 60));
    api.label_exists("queue").unwrap();
    assert_eq!(seen.borrow().len(), 5);
    assert_eq!(seen.borrow()[4].3, "Bearer installation-token-1");

    let renewal = 55 * 60;
    clock.set(at(t0 + renewal - 1));
    api.label_exists("queue").unwrap();
    assert_eq!(seen.borrow().len(), 6);
    assert_eq!(seen.borrow()[5].3, "Bearer installation-token-1");

    clock.set(at(t0 + renewal));
    api.label_exists("queue").unwrap();
    let seen = seen.borrow();
    assert_eq!(seen.len(), 8, "a mint, no second installation lookup");
    assert_eq!(format!("{} {}", seen[6].0, seen[6].1), MINT);
    let (_, claims) = verified(&key, seen[6].3.strip_prefix("Bearer ").unwrap());
    assert_eq!(claims["iat"], t0 + renewal - 60);
    assert_eq!(seen[7].3, "Bearer installation-token-2");
}

#[test]
fn a_missing_installation_is_permanent_and_looked_up_again() {
    let (mut api, seen) = client_with(
        app(),
        vec![route(
            "GET",
            "/installation",
            status(404, json!({ "message": "Not Found" })),
        )],
        Rc::new(Cell::new(at(1_800_000_000))),
    );
    let error = api.label_exists("queue").unwrap_err();
    assert!(!is_transient(&error), "{error}");
    let text = error.to_string();
    assert!(text.contains("finding the app's installation"), "{text}");
    assert!(
        text.contains("GET /repos/acme/widgets/installation: HTTP 404"),
        "{text}"
    );
    api.label_exists("queue").unwrap_err();
    let seen = seen.borrow();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|s| s.1.ends_with("/installation")));
}

#[test]
fn a_rejected_jwt_names_its_causes() {
    let (mut api, _) = client_with(
        app(),
        vec![route(
            "GET",
            "/installation",
            status(
                401,
                json!({ "message": "A JSON web token could not be decoded" }),
            ),
        )],
        Rc::new(Cell::new(at(1_800_000_000))),
    );
    let error = api.label_exists("queue").unwrap_err();
    assert!(!is_transient(&error), "{error}");
    let text = error.to_string();
    assert!(text.contains("HTTP 401"), "{text}");
    assert!(text.contains(JWT_REJECTED), "{text}");
    assert!(!text.contains("Bearer"), "{text}");
}

#[test]
fn an_unavailable_mint_is_transient_and_tried_again() {
    let (mut api, seen) = client_with(
        app(),
        vec![
            installation(),
            (
                MINT.to_owned(),
                status(503, json!({ "message": "Service Unavailable" })),
            ),
        ],
        Rc::new(Cell::new(at(1_800_000_000))),
    );
    let error = api.label_exists("queue").unwrap_err();
    assert!(is_transient(&error), "{error}");
    let text = error.to_string();
    assert!(text.contains("minting an installation token"), "{text}");
    assert!(text.contains("HTTP 503"), "{text}");
    api.label_exists("queue").unwrap_err();
    let seen = seen.borrow();
    let requests: Vec<String> = seen.iter().map(|s| format!("{} {}", s.0, s.1)).collect();
    assert_eq!(
        requests,
        vec![
            format!("GET {REPO}/installation"),
            MINT.to_owned(),
            MINT.to_owned()
        ]
    );
}

#[test]
fn an_access_token_response_without_a_token_is_permanent() {
    let (mut api, _) = client_with(
        app(),
        vec![
            installation(),
            (
                MINT.to_owned(),
                status(201, json!({ "expires_at": "2000-01-01T00:00:00Z" })),
            ),
        ],
        Rc::new(Cell::new(at(1_800_000_000))),
    );
    let error = api.label_exists("queue").unwrap_err();
    assert!(!is_transient(&error), "{error}");
    let text = error.to_string();
    assert!(
        text.contains("POST /app/installations/42/access_tokens"),
        "{text}"
    );
    assert!(text.contains("token"), "{text}");
}

#[test]
fn a_rejected_installation_token_is_permanent_and_not_reminted() {
    let (mut api, seen) = client_with(
        app(),
        vec![
            installation(),
            minted("installation-token-1"),
            route(
                "GET",
                "/labels/queue",
                status(401, json!({ "message": "Bad credentials" })),
            ),
        ],
        Rc::new(Cell::new(at(1_800_000_000))),
    );
    let error = api.label_exists("queue").unwrap_err();
    assert!(!is_transient(&error), "{error}");
    let text = error.to_string();
    assert!(text.contains(INSTALLATION_TOKEN_REJECTED), "{text}");
    assert!(text.contains("GET /labels/queue: HTTP 401"), "{text}");
    assert!(!text.contains("installation-token-1"), "{text}");
    assert_eq!(seen.borrow().len(), 3);
    api.label_exists("queue").unwrap_err();
    let seen = seen.borrow();
    assert_eq!(seen.len(), 4, "no mint follows a rejection");
    assert_eq!(seen[3].3, "Bearer installation-token-1");
}

const RULESETS: &str = "/rulesets?includes_parents=false&per_page=100";

fn branch_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|&n| n.to_owned()).collect()
}

#[test]
fn find_ruleset_reads_the_named_one_and_its_branches() {
    let (mut api, _) = client(vec![
        route(
            "GET",
            RULESETS,
            ok(json!([
                { "id": 4, "name": "protect main" },
                { "id": 7, "name": "dorfq/locked-heads" },
            ])),
        ),
        route(
            "GET",
            "/rulesets/7",
            ok(json!({
                "id": 7,
                "name": "dorfq/locked-heads",
                "conditions": { "ref_name": {
                    "include": ["refs/heads/a", "refs/heads/b/c"], "exclude": [] } },
            })),
        ),
    ]);
    assert_eq!(
        api.find_ruleset("dorfq/locked-heads").unwrap(),
        Some((RulesetId(7), branch_set(&["a", "b/c"])))
    );
}

#[test]
fn find_ruleset_answers_none_and_refuses_two() {
    let (mut api, _) = client(vec![route(
        "GET",
        RULESETS,
        ok(json!([{ "id": 4, "name": "other" }])),
    )]);
    assert_eq!(api.find_ruleset("dorfq/locked-heads").unwrap(), None);
    let (mut api, _) = client(vec![route(
        "GET",
        RULESETS,
        ok(json!([
            { "id": 4, "name": "dorfq/locked-heads" },
            { "id": 5, "name": "dorfq/locked-heads" },
        ])),
    )]);
    let error = api.find_ruleset("dorfq/locked-heads").unwrap_err();
    assert!(!is_transient(&error), "{error}");
    assert!(error.to_string().contains("2 rulesets"), "{error}");
}

#[test]
fn ruleset_writes_send_the_lock_and_read_their_statuses() {
    let (mut api, seen) = client(vec![
        route(
            "POST",
            "/rulesets",
            status(201, json!({ "id": 9, "name": "n" })),
        ),
        route("PUT", "/rulesets/9", ok(json!({ "id": 9 }))),
        route(
            "PUT",
            "/rulesets/9",
            status(404, json!({ "message": "Not Found" })),
        ),
        route("DELETE", "/rulesets/9", status(204, Value::Null)),
        route(
            "DELETE",
            "/rulesets/8",
            status(404, json!({ "message": "Not Found" })),
        ),
    ]);
    let locked = branch_set(&["pr-1"]);
    assert_eq!(api.create_ruleset("n", &locked).unwrap(), RulesetId(9));
    assert!(api.update_ruleset(RulesetId(9), &locked).unwrap());
    assert!(!api.update_ruleset(RulesetId(9), &locked).unwrap());
    api.delete_ruleset(RulesetId(9)).unwrap();
    api.delete_ruleset(RulesetId(8)).unwrap();
    let seen = seen.borrow();
    assert_eq!(
        seen[0].2,
        Some(json!({
            "name": "n",
            "target": "branch",
            "enforcement": "active",
            "bypass_actors": [],
            "conditions": { "ref_name": { "include": ["refs/heads/pr-1"], "exclude": [] } },
            "rules": [{ "type": "update" }],
        }))
    );
    assert!(seen[1].2.as_ref().unwrap().get("name").is_none());
}

#[test]
fn a_refused_ruleset_call_names_the_permission_unless_rate_limited() {
    let refused = status(
        403,
        json!({ "message": "Resource not accessible by integration" }),
    );
    let mut limited = status(403, json!({ "message": "secondary rate limit" }));
    limited.headers.push(("retry-after", "60".to_owned()));
    let (mut api, _) = client(vec![
        route("POST", "/rulesets", refused.clone()),
        route("POST", "/rulesets", limited),
        route("GET", RULESETS, refused),
    ]);
    let locked = branch_set(&["pr-1"]);
    let error = api.create_ruleset("n", &locked).unwrap_err();
    assert!(!is_transient(&error), "{error}");
    assert!(
        error.to_string().contains("Administration permission"),
        "{error}"
    );
    let error = api.create_ruleset("n", &locked).unwrap_err();
    assert!(is_transient(&error), "{error}");
    let error = api.find_ruleset("n").unwrap_err();
    assert!(
        error.to_string().contains("Administration permission"),
        "{error}"
    );
}

#[test]
fn find_ruleset_refuses_an_include_that_is_not_a_branch() {
    let (mut api, _) = client(vec![
        route(
            "GET",
            RULESETS,
            ok(json!([{ "id": 7, "name": "dorfq/locked-heads" }])),
        ),
        route(
            "GET",
            "/rulesets/7",
            ok(json!({
                "id": 7,
                "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
            })),
        ),
    ]);
    let error = api.find_ruleset("dorfq/locked-heads").unwrap_err();
    assert!(!is_transient(&error), "{error}");
    assert!(error.to_string().contains("is not a branch"), "{error}");
}

fn runs_path() -> String {
    format!("/actions/runs?head_sha={A}&per_page=100")
}

fn workflow_run(id: u64, branch: Option<&str>, status: &str) -> Value {
    json!({ "id": id, "head_branch": branch, "status": status, "conclusion": null })
}

#[test]
fn list_runs_reads_every_page_null_fields_and_completed_from_status() {
    let page2 = format!("{REPO}/actions/runs?head_sha={A}&per_page=100&page=2");
    let mut first = ok(json!({ "total_count": 5, "workflow_runs": [
        workflow_run(1, Some("dorfq/batch/7"), "in_progress"),
        workflow_run(2, Some("main"), "completed"),
        workflow_run(4, None, "in_progress"),
    ] }));
    first
        .headers
        .push(("link", format!("<{page2}>; rel=\"next\"")));
    let second = ok(json!({ "total_count": 5, "workflow_runs": [
        workflow_run(3, Some("dorfq/batch/7"), "queued"),
        { "id": 5, "head_branch": "dorfq/batch/7", "status": null, "conclusion": null },
    ] }));
    let (mut api, _) = client(vec![
        route("GET", &runs_path(), first),
        (format!("GET {page2}"), second),
    ]);
    let run = |id, branch: Option<&str>, completed| WorkflowRun {
        id: RunId(id),
        head_branch: branch.map(str::to_owned),
        completed,
    };
    assert_eq!(
        api.list_runs(&sha(A)).unwrap(),
        [
            run(1, Some("dorfq/batch/7"), false),
            run(2, Some("main"), true),
            run(4, None, false),
            run(3, Some("dorfq/batch/7"), false),
            run(5, Some("dorfq/batch/7"), false),
        ]
    );
}

#[test]
fn cancel_run_accepts_a_cancel_a_finished_run_and_a_gone_one() {
    let (mut api, seen) = client(vec![
        route("POST", "/actions/runs/5/cancel", status(202, json!({}))),
        route(
            "POST",
            "/actions/runs/6/cancel",
            status(
                409,
                json!({ "message": "Cannot cancel a workflow run that is completed." }),
            ),
        ),
        route(
            "POST",
            "/actions/runs/7/cancel",
            status(404, json!({ "message": "Not Found" })),
        ),
    ]);
    for id in [5, 6, 7] {
        api.cancel_run(RunId(id)).unwrap();
    }
    assert_eq!(seen.borrow().len(), 3);
}

#[test]
fn a_refused_actions_call_names_the_permission_unless_rate_limited() {
    let refused = status(
        403,
        json!({ "message": "Resource not accessible by integration" }),
    );
    let mut limited = status(403, json!({ "message": "secondary rate limit" }));
    limited.headers.push(("retry-after", "60".to_owned()));
    let (mut api, _) = client(vec![
        route("POST", "/actions/runs/5/cancel", refused.clone()),
        route("POST", "/actions/runs/5/cancel", limited.clone()),
        route("GET", &runs_path(), refused),
        route("GET", &runs_path(), limited),
    ]);
    let named = |error: &ApiError| {
        !is_transient(error) && error.to_string().contains("Actions permission: read/write")
    };
    let error = api.cancel_run(RunId(5)).unwrap_err();
    assert!(named(&error), "{error}");
    let error = api.cancel_run(RunId(5)).unwrap_err();
    assert!(is_transient(&error), "{error}");
    let error = api.list_runs(&sha(A)).unwrap_err();
    assert!(named(&error), "{error}");
    let error = api.list_runs(&sha(A)).unwrap_err();
    assert!(is_transient(&error), "{error}");
}
