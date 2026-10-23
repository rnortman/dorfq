use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use super::*;

const BASE: &str = "https://api.example.test";
const REPO: &str = "https://api.example.test/repos/acme/widgets";
const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn sha(s: &str) -> Sha {
    Sha::parse(s).unwrap()
}

/// A canned response.
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

/// What the transport saw: method, url, body.
type Seen = Rc<RefCell<Vec<(Method, String, Option<Value>)>>>;

/// A client whose transport answers `"<METHOD> <url>"` from `routes`; an
/// unrouted request fails the test.
fn client(routes: Vec<(String, Canned)>) -> (RestClient, Seen) {
    let routes: BTreeMap<String, Canned> = routes.into_iter().collect();
    let seen: Seen = Rc::default();
    let log = Rc::clone(&seen);
    let transport: Transport = Box::new(move |request: &HttpRequest<'_>| {
        let key = format!("{} {}", request.method, request.url);
        log.borrow_mut().push((
            request.method,
            request.url.to_owned(),
            request.body.cloned(),
        ));
        let canned = routes
            .get(&key)
            .unwrap_or_else(|| panic!("unrouted request {key}"));
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
        RestClient::with_transport(BASE, "acme", "widgets", transport),
        seen,
    )
}

fn route(method: &str, path: &str, canned: Canned) -> (String, Canned) {
    (format!("{method} {REPO}{path}"), canned)
}

fn is_transient(e: &ApiError) -> bool {
    matches!(e, ApiError::Transient(_))
}

fn pr_json(number: u64, head: &str, labels: &[&str], mergeable: Value) -> Value {
    json!({
        "number": number,
        "title": format!("change {number}"),
        "state": "open",
        "head": { "sha": head, "ref": "topic" },
        "base": { "sha": A, "ref": "main" },
        "labels": labels.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>(),
        "mergeable": mergeable,
    })
}

#[test]
fn get_pr_decodes_null_mergeable_as_unknown() {
    let (mut api, _) = client(vec![route(
        "GET",
        "/pulls/12",
        ok(pr_json(12, B, &["queue", "bug"], Value::Null)),
    )]);
    let pr = api.get_pr(PrNumber(12)).unwrap();
    assert_eq!(
        pr,
        PrDetail {
            open: true,
            base: "main".to_owned(),
            head: sha(B),
            labels: vec!["queue".to_owned(), "bug".to_owned()],
            mergeable: None,
        }
    );
}

#[test]
fn get_pr_decodes_mergeable_and_closed() {
    let mut closed = pr_json(3, C, &[], json!(false));
    closed["state"] = json!("closed");
    let (mut api, _) = client(vec![
        route("GET", "/pulls/2", ok(pr_json(2, B, &[], json!(true)))),
        route("GET", "/pulls/3", ok(closed)),
    ]);
    let two = api.get_pr(PrNumber(2)).unwrap();
    assert_eq!((two.open, two.mergeable), (true, Some(true)));
    let three = api.get_pr(PrNumber(3)).unwrap();
    assert_eq!((three.open, three.mergeable), (false, Some(false)));
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

    let mut merged = pr_json(4, B, &[], Value::Null);
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
            ok(json!({ "status": status, "ahead_by": 1, "behind_by": 0 })),
        )]);
        match (api.is_ancestor(&sha(A), &sha(B)), expected) {
            (Ok(got), Some(want)) => assert_eq!(got, want, "{status}"),
            (Err(e), None) => assert!(!is_transient(&e), "{status}"),
            (got, want) => panic!("{status}: got {got:?}, want {want:?}"),
        }
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
    let page2 = format!("{REPO}/pulls?state=open&base=main&per_page=100&page=2");
    let mut first = ok(json!([pr_json(1, A, &["queue"], Value::Null)]));
    first.headers.push((
        "link",
        format!("<{page2}>; rel=\"next\", <{page2}>; rel=\"last\""),
    ));
    let mut second = ok(json!([pr_json(2, B, &[], Value::Null)]));
    second.headers.push((
        "link",
        format!("<{REPO}/pulls?page=1>; rel=\"prev\", <{REPO}/pulls?page=1>; rel=\"first\""),
    ));
    let (mut api, seen) = client(vec![
        route("GET", "/pulls?state=open&base=main&per_page=100", first),
        (format!("GET {page2}"), second),
    ]);
    let prs = api.list_open_prs("main").unwrap();
    assert_eq!(
        prs,
        vec![
            PrSummary {
                number: PrNumber(1),
                head: sha(A),
                labels: vec!["queue".to_owned()],
                title: "change 1".to_owned(),
            },
            PrSummary {
                number: PrNumber(2),
                head: sha(B),
                labels: vec![],
                title: "change 2".to_owned(),
            },
        ]
    );
    assert_eq!(seen.borrow().len(), 2);
}

#[test]
fn a_failed_later_page_fails_the_list() {
    let page2 = format!("{REPO}/pulls?state=open&base=main&per_page=100&page=2");
    let mut first = ok(json!([pr_json(1, A, &["queue"], Value::Null)]));
    first
        .headers
        .push(("link", format!("<{page2}>; rel=\"next\"")));
    let (mut api, _) = client(vec![
        route("GET", "/pulls?state=open&base=main&per_page=100", first),
        (
            format!("GET {page2}"),
            status(502, json!({ "message": "Bad Gateway" })),
        ),
    ]);
    let error = api.list_open_prs("main").unwrap_err();
    assert!(is_transient(&error), "{error}");
    assert!(error.to_string().contains("HTTP 502"), "{error}");
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
        let mut api = RestClient::with_transport(BASE, "acme", "widgets", transport);
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
}

#[test]
fn api_error_maps_to_forge_error_of_the_same_kind() {
    let transient: ForgeError = ApiError::Transient("later".into()).into();
    assert!(matches!(transient, ForgeError::Transient(_)));
    let permanent: ForgeError = ApiError::Permanent("never".into()).into();
    assert!(matches!(permanent, ForgeError::Permanent(_)));
    assert_eq!(permanent.to_string(), "permanent forge error: never");
}
