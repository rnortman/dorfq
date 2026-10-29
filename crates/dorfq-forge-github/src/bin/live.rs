//! The live lane: drives [`GitHubForge`] through a whole batch lifecycle on a
//! real, disposable GitHub repository and exits non-zero on the first failed
//! assertion.
//!
//! Runs under one of two credentials: a token in `DORFQ_GITHUB_TOKEN`, or a
//! GitHub App, `DORFQ_GITHUB_APP_CLIENT_ID` with the path of its private key
//! in `DORFQ_GITHUB_APP_KEY_FILE`; exactly one form must be set. Under the
//! app it is the one place installation-token minting meets GitHub. Also
//! reads `DORFQ_LIVE_REPO` (`owner/name`) and optionally
//! `DORFQ_LIVE_API_BASE` (default `https://api.github.com`), and
//! `DORFQ_LIVE_LOCK_HEADS`: `1` runs the forge with `lock_heads` on and adds
//! the step that checks the branch lock, which needs Administration:
//! read/write; unset or empty leaves both out. `DORFQ_LIVE_CI`: `actions`
//! runs the forge with `ci = "actions"` and adds the step that checks a
//! retired batch's workflow run is cancelled, which needs Actions:
//! read/write; unset or empty leaves both out.
//! Refuses, before any request, a repository whose name contains neither
//! `scratch` nor `playground`. The repository it accepts is treated as disposable: every open
//! labelled pull request is closed and every queue ref deleted, and trunk
//! (`main`) keeps the commits the run lands on it. The lane never touches
//! trunk's protection.

use std::env;
use std::fmt::Display;
use std::fs;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dorfq_core::{
    BatchId, BatchStart, BatchState, CiStatus, ConflictWith, EjectReason, Forge, ForgeError, Hold,
    LandRefusal, NoticeReason, Observation, PrBase, PrNumber, PrSnapshot, PrStatus, Sha,
};
use dorfq_forge_github::api::{Auth, GitHubApi, RefUpdate, RestClient};
use dorfq_forge_github::app::AppKey;
use dorfq_forge_github::messages::{Marker, last_marker};
use dorfq_forge_github::refresh::Refresh;
use dorfq_forge_github::{Ci, GitHubForge, LOCK_RULESET, Settings, system_clock};
use serde_json::{Value, json};

const TRUNK: &str = "main";
const LABEL: &str = "queue";
const CHECK: &str = "check";
const USER_AGENT: &str = "dorfq-live";
/// How often a lazily computed fact is looked for before the run fails.
const ATTEMPTS: u32 = 30;
const PAUSE: Duration = Duration::from_secs(2);

fn main() -> ExitCode {
    match run() {
        Ok(()) => {
            println!("live: ok");
            ExitCode::SUCCESS
        }
        Err(problem) => {
            eprintln!("live: FAILED: {problem}");
            ExitCode::FAILURE
        }
    }
}

/// Where the run points.
struct Target {
    api_base: String,
    owner: String,
    repo: String,
    auth: Auth,
    lock_heads: bool,
    ci: Option<Ci>,
}

const TOKEN_VAR: &str = "DORFQ_GITHUB_TOKEN";
const APP_CLIENT_ID_VAR: &str = "DORFQ_GITHUB_APP_CLIENT_ID";
const APP_KEY_FILE_VAR: &str = "DORFQ_GITHUB_APP_KEY_FILE";
const LOCK_HEADS_VAR: &str = "DORFQ_LIVE_LOCK_HEADS";
const CI_VAR: &str = "DORFQ_LIVE_CI";

/// Whether the run locks batched heads, from `DORFQ_LIVE_LOCK_HEADS`'s
/// value: unset or empty is off, exactly `1` is on, anything else refused.
fn lock_heads_from_env(value: Result<String, env::VarError>) -> Result<bool, String> {
    match value {
        Err(env::VarError::NotPresent) => Ok(false),
        Ok(value) if value.is_empty() => Ok(false),
        Ok(value) if value == "1" => Ok(true),
        Ok(value) => Err(format!(
            "{LOCK_HEADS_VAR} is {value:?}; set it to `1` or leave it unset"
        )),
        Err(error) => Err(format!("{LOCK_HEADS_VAR}: {error}")),
    }
}

/// The CI system the run cancels on, from `DORFQ_LIVE_CI`'s value: unset or
/// empty is none, exactly `actions` is Actions, anything else refused.
fn ci_from_env(value: Result<String, env::VarError>) -> Result<Option<Ci>, String> {
    match value {
        Err(env::VarError::NotPresent) => Ok(None),
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) if value == "actions" => Ok(Some(Ci::Actions)),
        Ok(value) => Err(format!(
            "{CI_VAR} is {value:?}; set it to `actions` or leave it unset"
        )),
        Err(error) => Err(format!("{CI_VAR}: {error}")),
    }
}

/// Reads the one credential form the environment sets, and parses an app's
/// key so a bad file fails before any request. `set` says whether a variable
/// is present at all, empty or not; `required` reads a non-empty value.
fn auth_from_env(
    set: impl Fn(&str) -> bool,
    required: impl Fn(&str) -> Result<String, String>,
) -> Result<Auth, String> {
    let token = set(TOKEN_VAR);
    let app = set(APP_CLIENT_ID_VAR) || set(APP_KEY_FILE_VAR);
    if token == app {
        return Err(format!(
            "set exactly one credential: {TOKEN_VAR}, or {APP_CLIENT_ID_VAR} together with \
             {APP_KEY_FILE_VAR}"
        ));
    }
    if token {
        return Ok(Auth::Token(required(TOKEN_VAR)?));
    }
    let client_id = required(APP_CLIENT_ID_VAR)?;
    let path = required(APP_KEY_FILE_VAR)?;
    let pem = fs::read_to_string(&path)
        .map_err(|error| format!("{APP_KEY_FILE_VAR}: cannot read {path}: {error}"))?;
    let key = AppKey::from_pem(&client_id, &pem)
        .map_err(|message| format!("{APP_KEY_FILE_VAR}: {path} is not a usable key: {message}"))?;
    Ok(Auth::App(key))
}

impl Target {
    fn from_env() -> Result<Target, String> {
        let required = |var: &str| match env::var(var) {
            Ok(value) if !value.is_empty() => Ok(value),
            _ => Err(format!("{var} is unset, empty or not UTF-8")),
        };
        let auth = auth_from_env(|var| env::var_os(var).is_some(), required)?;
        let lock_heads = lock_heads_from_env(env::var(LOCK_HEADS_VAR))?;
        let ci = ci_from_env(env::var(CI_VAR))?;
        let full = required("DORFQ_LIVE_REPO")?;
        let (owner, repo) = match full.split_once('/') {
            Some((owner, repo)) if !owner.is_empty() && !repo.is_empty() && !repo.contains('/') => {
                (owner.to_owned(), repo.to_owned())
            }
            _ => return Err(format!("DORFQ_LIVE_REPO {full:?} is not owner/name")),
        };
        if !repo.contains("scratch") && !repo.contains("playground") {
            return Err(format!(
                "repository {full:?} has neither \"scratch\" nor \"playground\" in its name; the live lane \
                 deletes and closes things and runs only against a disposable repository"
            ));
        }
        let api_base = match env::var("DORFQ_LIVE_API_BASE") {
            Ok(base) => base,
            Err(env::VarError::NotPresent) => "https://api.github.com".to_owned(),
            Err(error) => return Err(format!("DORFQ_LIVE_API_BASE: {error}")),
        };
        Ok(Target {
            api_base,
            owner,
            repo,
            auth,
            lock_heads,
            ci,
        })
    }

    fn client(&self) -> RestClient {
        RestClient::new(
            &self.api_base,
            &self.owner,
            &self.repo,
            self.auth.clone(),
            USER_AGENT,
        )
    }

    fn forge(&self) -> Result<GitHubForge, String> {
        let settings = Settings {
            trunk: TRUNK.to_owned(),
            label: LABEL.to_owned(),
            checks: vec![CHECK.to_owned()],
            batch_checks: vec![CHECK.to_owned()],
            lock_heads: self.lock_heads,
            ci: self.ci,
            // Every entity is due a second after its read, so each
            // `observe_until` attempt, `PAUSE` apart, reads what it waits on.
            refresh: Refresh {
                listing: Duration::from_secs(1),
                pr: Duration::from_secs(1),
                batch: Duration::from_secs(1),
            },
        };
        GitHubForge::new(
            Box::new(self.client()),
            settings,
            Box::new(system_clock),
            Box::new(std::io::stdout()),
            None,
        )
        .map_err(at("constructing the forge"))
    }
}

/// Prefixes a failure with what was being done.
fn at<E: Display>(what: &str) -> impl FnOnce(E) -> String + '_ {
    move |error| format!("{what}: {error}")
}

fn check(condition: bool, problem: impl FnOnce() -> String) -> Result<(), String> {
    if condition { Ok(()) } else { Err(problem()) }
}

/// Retries `attempt` until it succeeds. `attempt` returns `Err` for a fatal
/// failure and `Ok(Err(reason))` for "not yet", which is retried after
/// [`PAUSE`] up to [`ATTEMPTS`] times.
fn poll<T>(
    what: &str,
    mut attempt: impl FnMut() -> Result<Result<T, String>, String>,
) -> Result<T, String> {
    let mut reason = String::new();
    for n in 0..ATTEMPTS {
        if n > 0 {
            thread::sleep(PAUSE);
        }
        match attempt()? {
            Ok(value) => return Ok(value),
            Err(why) => reason = why,
        }
    }
    Err(format!(
        "{what}: not reached after {ATTEMPTS} attempts: {reason}"
    ))
}

/// Observes until `done` accepts the observation; a transient forge error
/// counts as "not yet".
fn observe_until(
    forge: &mut GitHubForge,
    what: &str,
    done: impl Fn(&Observation) -> bool,
) -> Result<Observation, String> {
    poll(what, || match forge.observe() {
        Ok(observation) if done(&observation) => Ok(Ok(observation)),
        Ok(observation) => Ok(Err(format!("last observation {observation:?}"))),
        Err(error @ ForgeError::Transient(_)) => Ok(Err(error.to_string())),
        Err(error) => Err(format!("{what}: {error}")),
    })
}

fn batch(observation: &Observation, id: u64) -> Option<&BatchState> {
    observation
        .batches
        .iter()
        .find(|b| b.id == BatchId(id))
        .map(|b| &b.state)
}

/// Starts batch `id` of `members` on `trunk`, waits for it to test, passes
/// its tip and waits for the pass; returns the tip.
fn tested(
    forge: &mut GitHubForge,
    repo: &mut Repo,
    id: u64,
    trunk: &Sha,
    members: Vec<PrNumber>,
) -> Result<Sha, String> {
    forge
        .start_batch(&BatchStart::new(BatchId(id), trunk.clone(), members))
        .map_err(at(&format!("starting batch {id}")))?;
    let observation = observe_until(forge, &format!("batch {id} testing"), |o| {
        matches!(batch(o, id), Some(BatchState::Testing { .. }))
    })?;
    let Some(BatchState::Testing { tip, .. }) = batch(&observation, id).cloned() else {
        unreachable!("observe_until accepted batch {id} as testing");
    };
    repo.pass(&tip)?;
    observe_until(forge, &format!("batch {id} passed"), |o| {
        batch(o, id)
            == Some(&BatchState::Testing {
                tip: tip.clone(),
                ci: CiStatus::Passed,
            })
    })?;
    Ok(tip)
}

fn enqueued(observation: &Observation) -> Vec<PrNumber> {
    observation.prs.iter().map(|p| p.number).collect()
}

fn snapshot(observation: &Observation, pr: PrNumber) -> Option<&PrSnapshot> {
    observation.prs.iter().find(|p| p.number == pr)
}

/// Whether `observation` reports every one of `expected` ready, at its
/// head, on its base.
fn ready_on(observation: &Observation, expected: &[(&Fixture, PrBase)]) -> bool {
    expected.iter().all(|(fixture, base)| {
        snapshot(observation, fixture.number).is_some_and(|p| {
            p.status == PrStatus::Ready && p.head == fixture.head && p.base == *base
        })
    })
}

fn str_field<'a>(what: &str, value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("{what}: response has no string {key:?}: {value}"))
}

fn sha_field(what: &str, value: &Value, key: &str) -> Result<Sha, String> {
    Sha::parse(str_field(what, value, key)?).map_err(at(what))
}

/// A fixture pull request.
struct Fixture {
    number: PrNumber,
    head: Sha,
}

/// The fixture side of the run: plain REST calls, never the forge.
struct Repo {
    client: RestClient,
}

impl Repo {
    fn trunk(&mut self) -> Result<Sha, String> {
        self.client
            .get_ref(&format!("refs/heads/{TRUNK}"))
            .map_err(at("reading trunk"))?
            .ok_or_else(|| format!("trunk {TRUNK} does not exist"))
    }

    /// Closes every open labelled pull request, deletes every
    /// queue ref and every branch under `stale_prefix`, and creates the label
    /// if it is missing.
    fn clean(&mut self, stale_prefix: &str) -> Result<(), String> {
        for number in self
            .client
            .list_labelled(LABEL)
            .map_err(at("listing pull requests"))?
        {
            self.close(number)?;
        }
        for prefix in ["refs/heads/dorfq/", "refs/dorfq/", stale_prefix] {
            for entry in self.client.list_refs(prefix).map_err(at("listing refs"))? {
                self.client
                    .delete_ref(&entry.name)
                    .map_err(at(&format!("deleting {}", entry.name)))?;
            }
        }
        if !self
            .client
            .label_exists(LABEL)
            .map_err(at("looking up the label"))?
        {
            self.client
                .post("/labels", &json!({ "name": LABEL, "color": "0e8a16" }))
                .map_err(at("creating the label"))?;
        }
        Ok(())
    }

    fn close(&mut self, number: PrNumber) -> Result<(), String> {
        self.client
            .patch(
                &format!("/pulls/{}", number.0),
                &json!({ "state": "closed" }),
            )
            .map_err(at(&format!("closing #{}", number.0)))
            .map(|_| ())
    }

    /// A commit on `parent` that writes `content` to `path`.
    fn commit_file(
        &mut self,
        parent: &Sha,
        path: &str,
        content: &str,
        message: &str,
    ) -> Result<Sha, String> {
        let what = format!("writing {path}");
        let base_tree = self.client.get_commit(parent).map_err(at(&what))?.tree;
        let tree = self
            .client
            .post(
                "/git/trees",
                &json!({
                    "base_tree": base_tree.as_str(),
                    "tree": [{ "path": path, "mode": "100644", "type": "blob", "content": content }],
                }),
            )
            .map_err(at(&what))?;
        let tree = sha_field(&what, &tree, "sha")?;
        self.client
            .create_commit(message, &tree, std::slice::from_ref(parent))
            .map_err(at(&what))
    }

    /// A branch from `parent` writing `content` to `path`, a pull request
    /// for it against `base` carrying the label, and a passing `check`
    /// status on its head.
    fn fixture(
        &mut self,
        parent: &Sha,
        base: &str,
        branch: &str,
        title: &str,
        path: &str,
        content: &str,
    ) -> Result<Fixture, String> {
        let head = self.commit_file(parent, path, content, title)?;
        self.client
            .create_ref(&format!("refs/heads/{branch}"), &head)
            .map_err(at(&format!("creating {branch}")))?;
        let what = format!("opening a pull request for {branch}");
        let pr = self
            .client
            .post(
                "/pulls",
                &json!({ "title": title, "head": branch, "base": base }),
            )
            .map_err(at(&what))?;
        let number = pr["number"]
            .as_u64()
            .map(PrNumber)
            .ok_or_else(|| format!("{what}: response has no number: {pr}"))?;
        self.client
            .post(
                &format!("/issues/{}/labels", number.0),
                &json!({ "labels": [LABEL] }),
            )
            .map_err(at(&format!("labelling #{}", number.0)))?;
        self.pass(&head)?;
        Ok(Fixture { number, head })
    }

    /// Moves `fixture`'s branch to a new commit on `parent` writing
    /// `content` to `path`, passing; the fixture's new head.
    fn repush(
        &mut self,
        fixture: &mut Fixture,
        branch: &str,
        parent: &Sha,
        path: &str,
        content: &str,
    ) -> Result<(), String> {
        let head = self.commit_file(parent, path, content, &format!("{branch}: repushed"))?;
        let update = self
            .client
            .update_ref(&format!("refs/heads/{branch}"), &head, true)
            .map_err(at(&format!("pushing {branch}")))?;
        check(update == RefUpdate::Updated, || {
            format!("the forced push to {branch} was {update:?}")
        })?;
        self.pass(&head)?;
        fixture.head = head;
        Ok(())
    }

    /// Waits for GitHub to mark `pr` merged.
    fn wait_until_merged(&mut self, pr: PrNumber) -> Result<(), String> {
        let what = format!("#{} merged per GitHub", pr.0);
        poll(&what, || {
            let merged = self.client.get_pr(pr).map_err(at(&what))?.merged;
            Ok(if merged {
                Ok(())
            } else {
                Err("not merged".to_owned())
            })
        })
    }

    /// Posts a successful `check` status on `sha`.
    fn pass(&mut self, sha: &Sha) -> Result<(), String> {
        self.client
            .post(
                &format!("/statuses/{sha}"),
                &json!({ "state": "success", "context": CHECK }),
            )
            .map_err(at(&format!("posting a status on {sha}")))
            .map(|_| ())
    }

    /// Waits until the last dorfq marker on `pr` is `expected`.
    fn wait_for_marker(&mut self, pr: PrNumber, expected: Marker) -> Result<(), String> {
        let what = format!("the last marker on #{}", pr.0);
        poll(&what, || {
            let comments = self.client.list_comments(pr).map_err(at(&what))?;
            let last = last_marker(&comments);
            Ok(if last == Some(expected) {
                Ok(())
            } else {
                Err(format!("is {last:?}, expected {expected:?}"))
            })
        })
    }
}

fn run() -> Result<(), String> {
    let target = Target::from_env()?;
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(at("reading the clock"))?
        .as_secs();
    let prefix = format!("live-{seconds}");
    println!(
        "live: run {prefix} against {}/{}",
        target.owner, target.repo
    );
    let mut repo = Repo {
        client: target.client(),
    };

    // 1. A clean repository.
    repo.clean("refs/heads/live-")?;

    // 2. Fixtures: A and C change a.txt incompatibly; B adds a workflow file,
    // new on every run, so building and landing it need the Workflows
    // permission. Its job outlasts every poll, so only step 10 dispatches it
    // and only a cancel completes it.
    let trunk = repo.trunk()?;
    let a = repo.fixture(
        &trunk,
        TRUNK,
        &format!("{prefix}-a"),
        &format!("{prefix} A"),
        "a.txt",
        &format!("{prefix} A\n"),
    )?;
    let b = repo.fixture(
        &trunk,
        TRUNK,
        &format!("{prefix}-b"),
        &format!("{prefix} B"),
        &format!(".github/workflows/{prefix}.yml"),
        &format!(
            "name: {prefix}\non: workflow_dispatch\njobs:\n  noop:\n    runs-on: ubuntu-latest\n    \
             steps:\n      - run: sleep 600\n"
        ),
    )?;
    let c = repo.fixture(
        &trunk,
        TRUNK,
        &format!("{prefix}-c"),
        &format!("{prefix} C"),
        "a.txt",
        &format!("{prefix} C\n"),
    )?;
    let mut fixtures = vec![a.number, b.number, c.number];

    // 3. The forge sees all three ready: each merges into trunk and its
    // checks passed.
    let mut forge = target.forge()?;
    let observation = observe_until(&mut forge, "three pull requests ready", |o| {
        [a.number, b.number, c.number].iter().all(|&number| {
            o.prs
                .iter()
                .any(|p| p.number == number && p.status == PrStatus::Ready)
        })
    })?;
    check(observation.trunk == trunk, || {
        format!("observed trunk {} is not {trunk}", observation.trunk)
    })?;
    let heads = [
        (a.number, &a.head),
        (b.number, &b.head),
        (c.number, &c.head),
    ];
    for (number, head) in heads {
        let seen = observation.prs.iter().find(|p| p.number == number);
        check(seen.is_some_and(|p| &p.head == head), || {
            format!("#{} observed at {seen:?}, expected head {head}", number.0)
        })?;
    }

    // 4. A batch of all three conflicts at C, attributed to A by the probe;
    // C is ejected and B told it is probably broken, keeping its place.
    forge
        .start_batch(&BatchStart::new(
            BatchId(1),
            trunk.clone(),
            vec![a.number, b.number, c.number],
        ))
        .map_err(at("starting batch 1"))?;
    let expected = BatchState::Conflict {
        pr: c.number,
        with: ConflictWith::Pr(a.number),
    };
    observe_until(&mut forge, "batch 1 in conflict at C with A", |o| {
        batch(o, 1) == Some(&expected)
    })?;
    forge
        .eject(
            c.number,
            &EjectReason::Conflicts {
                with: ConflictWith::Pr(a.number),
            },
        )
        .map_err(at("ejecting C"))?;
    forge
        .notify(b.number, &NoticeReason::Broken { batch: BatchId(1) })
        .map_err(at("notifying B"))?;
    forge.retire(BatchId(1)).map_err(at("retiring batch 1"))?;
    observe_until(&mut forge, "A and B enqueued, no batches", |o| {
        enqueued(o) == [a.number, b.number] && o.batches.is_empty()
    })?;
    repo.wait_for_marker(c.number, Marker::Ejected)?;
    repo.wait_for_marker(b.number, Marker::Notice)?;
    for prefix in ["refs/heads/dorfq/", "refs/dorfq/"] {
        let left = repo.client.list_refs(prefix).map_err(at("listing refs"))?;
        check(left.is_empty(), || {
            format!("refs left under {prefix}: {left:?}")
        })?;
    }

    // 5. A batch of A and B builds and tests; a fresh forge recovers it.
    forge
        .start_batch(&BatchStart::new(
            BatchId(2),
            trunk.clone(),
            vec![a.number, b.number],
        ))
        .map_err(at("starting batch 2"))?;
    let observation = observe_until(&mut forge, "batch 2 testing, pending", |o| {
        matches!(
            batch(o, 2),
            Some(BatchState::Testing {
                ci: CiStatus::Pending,
                ..
            })
        )
    })?;
    let Some(BatchState::Testing { tip, .. }) = batch(&observation, 2).cloned() else {
        unreachable!("observe_until accepted batch 2 as testing");
    };
    let branch = repo
        .client
        .get_ref("refs/heads/dorfq/batch/2")
        .map_err(at("reading dorfq/batch/2"))?;
    check(branch.as_ref() == Some(&tip), || {
        format!("dorfq/batch/2 is at {branch:?}, expected {tip}")
    })?;
    let mut restarted = target.forge()?;
    let recovered = restarted
        .observe()
        .map_err(at("observing from a fresh forge"))?;
    check(recovered.batches == observation.batches, || {
        format!(
            "a fresh forge sees {:?}, expected {:?}",
            recovered.batches, observation.batches
        )
    })?;
    drop(restarted);
    repo.pass(&tip)?;
    let passed = observe_until(&mut forge, "batch 2 passed", |o| {
        batch(o, 2)
            == Some(&BatchState::Testing {
                tip: tip.clone(),
                ci: CiStatus::Passed,
            })
    })?;

    // 6. Batch 2 lands: trunk is its tip, A and B are told, and the trunk
    // position rises by its two members.
    forge.land(BatchId(2)).map_err(at("landing batch 2"))?;
    let observation = observe_until(&mut forge, "batch 2 landed, nothing enqueued", |o| {
        batch(o, 2) == Some(&BatchState::Landed { tip: tip.clone() }) && o.prs.is_empty()
    })?;
    check(observation.trunk == tip, || {
        format!("trunk is {}, expected {tip}", observation.trunk)
    })?;
    check(
        observation.trunk_position == passed.trunk_position + 2,
        || {
            format!(
                "the trunk position went from {} to {} at a landing of two",
                passed.trunk_position, observation.trunk_position
            )
        },
    )?;
    repo.wait_for_marker(a.number, Marker::Landed)?;
    repo.wait_for_marker(b.number, Marker::Landed)?;
    forge.retire(BatchId(2)).map_err(at("retiring batch 2"))?;

    // 7. A stack: E on trunk, F on E's branch, G on F's branch.
    let landed = repo.trunk()?;
    let (e_branch, f_branch, g_branch) = (
        format!("{prefix}-e"),
        format!("{prefix}-f"),
        format!("{prefix}-g"),
    );
    let mut e = repo.fixture(
        &landed,
        TRUNK,
        &e_branch,
        &format!("{prefix} E"),
        &format!("{prefix}-e.txt"),
        &format!("{prefix} E\n"),
    )?;
    let mut f = repo.fixture(
        &e.head,
        &e_branch,
        &f_branch,
        &format!("{prefix} F"),
        &format!("{prefix}-f.txt"),
        &format!("{prefix} F\n"),
    )?;
    let mut g = repo.fixture(
        &f.head,
        &f_branch,
        &g_branch,
        &format!("{prefix} G"),
        &format!("{prefix}-g.txt"),
        &format!("{prefix} G\n"),
    )?;
    fixtures.extend([e.number, f.number, g.number]);
    observe_until(&mut forge, "E, F and G ready, stacked", |o| {
        ready_on(
            o,
            &[
                (&e, PrBase::Trunk),
                (&f, PrBase::Pr(e.number)),
                (&g, PrBase::Pr(f.number)),
            ],
        )
    })?;

    // A push to E holds F until F and G are rebased onto it.
    let e_before = e.head.clone();
    repo.repush(
        &mut e,
        &e_branch,
        &e_before,
        &format!("{prefix}-e.txt"),
        &format!("{prefix} E, again\n"),
    )?;
    let moved = Hold::AncestorMoved { pr: e.number };
    observe_until(&mut forge, "F held, E moved", |o| {
        snapshot(o, f.number).is_some_and(
            |p| matches!(&p.status, PrStatus::Waiting(holds) if holds.contains(&moved)),
        )
    })?;
    repo.repush(
        &mut f,
        &f_branch,
        &e.head,
        &format!("{prefix}-f.txt"),
        &format!("{prefix} F, rebased\n"),
    )?;
    repo.repush(
        &mut g,
        &g_branch,
        &f.head,
        &format!("{prefix}-g.txt"),
        &format!("{prefix} G, rebased\n"),
    )?;
    let observation = observe_until(&mut forge, "E, F and G ready again", |o| {
        ready_on(
            o,
            &[
                (&e, PrBase::Trunk),
                (&f, PrBase::Pr(e.number)),
                (&g, PrBase::Pr(f.number)),
            ],
        )
    })?;

    // Batch 3 lands E alone; F is moved onto trunk on a later observation.
    let tip3 = tested(&mut forge, &mut repo, 3, &observation.trunk, vec![e.number])?;
    forge.land(BatchId(3)).map_err(at("landing batch 3"))?;
    observe_until(&mut forge, "batch 3 landed, F on trunk", |o| {
        batch(o, 3) == Some(&BatchState::Landed { tip: tip3.clone() })
            && snapshot(o, f.number).is_some_and(|p| p.base == PrBase::Trunk)
            && snapshot(o, g.number).is_some_and(|p| p.base == PrBase::Pr(f.number))
    })?;
    repo.wait_until_merged(e.number)?;
    repo.wait_for_marker(e.number, Marker::Landed)?;
    forge.retire(BatchId(3)).map_err(at("retiring batch 3"))?;

    // Batch 4 lands F and G together; G is moved onto trunk before trunk
    // moves, so GitHub marks both merged.
    let observation = observe_until(&mut forge, "F and G ready, F on trunk", |o| {
        ready_on(o, &[(&f, PrBase::Trunk), (&g, PrBase::Pr(f.number))])
    })?;
    let members = vec![f.number, g.number];
    let tip4 = tested(&mut forge, &mut repo, 4, &observation.trunk, members)?;
    forge.land(BatchId(4)).map_err(at("landing batch 4"))?;
    let base = repo
        .client
        .get_pr(g.number)
        .map_err(at(&format!("reading #{}", g.number.0)))?
        .base;
    check(base == TRUNK, || {
        format!("#{}'s base reads {base} after landing", g.number.0)
    })?;
    observe_until(&mut forge, "batch 4 landed, nothing enqueued", |o| {
        batch(o, 4) == Some(&BatchState::Landed { tip: tip4.clone() }) && o.prs.is_empty()
    })?;
    for fixture in [&e, &f, &g] {
        repo.wait_for_marker(fixture.number, Marker::Landed)?;
    }
    for fixture in [&f, &g] {
        repo.wait_until_merged(fixture.number)?;
    }
    forge.retire(BatchId(4)).map_err(at("retiring batch 4"))?;

    // 8. Batch 5 of D passes, trunk moves under it, the landing is refused.
    let trunk = repo.trunk()?;
    let d = repo.fixture(
        &trunk,
        TRUNK,
        &format!("{prefix}-d"),
        &format!("{prefix} D"),
        &format!("{prefix}-d.txt"),
        &format!("{prefix} D\n"),
    )?;
    fixtures.push(d.number);
    let observation = observe_until(&mut forge, "D enqueued, ready, no batches", |o| {
        o.batches.is_empty()
            && o.prs
                .iter()
                .any(|p| p.number == d.number && p.status == PrStatus::Ready)
    })?;
    let tip5 = tested(&mut forge, &mut repo, 5, &observation.trunk, vec![d.number])?;
    let pushed = repo.commit_file(
        &observation.trunk,
        &format!("{prefix}-trunk.txt"),
        &format!("{prefix} trunk\n"),
        &format!("{prefix}: a direct push to trunk"),
    )?;
    let update = repo
        .client
        .update_ref(&format!("refs/heads/{TRUNK}"), &pushed, false)
        .map_err(at("pushing to trunk"))?;
    check(update == RefUpdate::Updated, || {
        format!("the direct push to trunk was {update:?}")
    })?;
    forge.land(BatchId(5)).map_err(at("landing batch 5"))?;
    let refused = BatchState::LandRefused {
        tip: tip5.clone(),
        reason: LandRefusal::TrunkMoved {
            trunk: pushed.clone(),
        },
    };
    let moved = observe_until(&mut forge, "batch 5 refused, trunk moved", |o| {
        batch(o, 5) == Some(&refused)
    })?;
    // The push brought one commit, and nothing departed for it.
    check(
        moved.trunk_position == observation.trunk_position + 1,
        || {
            format!(
                "the trunk position went from {} to {} at a direct push of one commit",
                observation.trunk_position, moved.trunk_position
            )
        },
    )?;
    forge.retire(BatchId(5)).map_err(at("retiring batch 5"))?;

    // 9. With the lock on, a batched member's branch is locked: the
    // ruleset names it, a push to it is refused, and removing the label
    // unlocks it.
    if target.lock_heads {
        let trunk = repo.trunk()?;
        let h_branch = format!("{prefix}-h");
        let h = repo.fixture(
            &trunk,
            TRUNK,
            &h_branch,
            &format!("{prefix} H"),
            &format!("{prefix}-h.txt"),
            &format!("{prefix} H\n"),
        )?;
        fixtures.push(h.number);
        let observation = observe_until(&mut forge, "H ready", |o| {
            ready_on(o, &[(&h, PrBase::Trunk)])
        })?;
        forge
            .start_batch(&BatchStart::new(
                BatchId(6),
                observation.trunk.clone(),
                vec![h.number],
            ))
            .map_err(at("starting batch 6"))?;
        let locked = repo
            .client
            .find_ruleset(LOCK_RULESET)
            .map_err(at("reading the lock"))?;
        check(
            locked
                .as_ref()
                .is_some_and(|(_, branches)| branches.contains(&h_branch)),
            || format!("{LOCK_RULESET} is {locked:?}, expected it to lock {h_branch}"),
        )?;
        let blocked = repo.commit_file(
            &h.head,
            &format!("{prefix}-h.txt"),
            &format!("{prefix} H, pushed while batched\n"),
            &format!("{prefix}: a push to a batched branch"),
        )?;
        let push = repo
            .client
            .update_ref(&format!("refs/heads/{h_branch}"), &blocked, false);
        check(push.is_err(), || {
            format!("a push to the locked branch {h_branch} was {push:?}")
        })?;
        repo.client
            .remove_label(h.number, LABEL)
            .map_err(at("unlabelling H"))?;
        observe_until(&mut forge, "H departed", |o| {
            snapshot(o, h.number).is_none()
        })?;
        let locked = repo
            .client
            .find_ruleset(LOCK_RULESET)
            .map_err(at("reading the lock"))?;
        check(locked.is_none(), || {
            format!("{LOCK_RULESET} is {locked:?} after the only batched member left")
        })?;
        forge.retire(BatchId(6)).map_err(at("retiring batch 6"))?;
    }

    // 10. With `ci = "actions"`, retiring a testing batch cancels the
    // workflow run going on its branch.
    if target.ci == Some(Ci::Actions) {
        let trunk = repo.trunk()?;
        let i = repo.fixture(
            &trunk,
            TRUNK,
            &format!("{prefix}-i"),
            &format!("{prefix} I"),
            &format!("{prefix}-i.txt"),
            &format!("{prefix} I\n"),
        )?;
        fixtures.push(i.number);
        let observation = observe_until(&mut forge, "I ready", |o| {
            ready_on(o, &[(&i, PrBase::Trunk)])
        })?;
        forge
            .start_batch(&BatchStart::new(
                BatchId(7),
                observation.trunk.clone(),
                vec![i.number],
            ))
            .map_err(at("starting batch 7"))?;
        let observation = observe_until(&mut forge, "batch 7 testing, pending", |o| {
            matches!(
                batch(o, 7),
                Some(BatchState::Testing {
                    ci: CiStatus::Pending,
                    ..
                })
            )
        })?;
        let Some(BatchState::Testing { tip, .. }) = batch(&observation, 7).cloned() else {
            unreachable!("observe_until accepted batch 7 as testing");
        };
        let batch_branch = "dorfq/batch/7";
        repo.client
            .post(
                &format!("/actions/workflows/{prefix}.yml/dispatches"),
                &json!({ "ref": batch_branch }),
            )
            .map_err(at("dispatching B's workflow on batch 7"))?;
        poll("a run going on batch 7", || {
            let runs = repo
                .client
                .list_runs(&tip)
                .map_err(at("listing batch 7's runs"))?;
            Ok(
                if runs
                    .iter()
                    .any(|r| r.head_branch.as_deref() == Some(batch_branch) && !r.completed)
                {
                    Ok(())
                } else {
                    Err(format!("runs on {tip}: {runs:?}"))
                },
            )
        })?;
        forge.retire(BatchId(7)).map_err(at("retiring batch 7"))?;
        poll("every run on batch 7 completed", || {
            let runs = repo
                .client
                .list_runs(&tip)
                .map_err(at("listing batch 7's runs"))?;
            Ok(if runs.iter().all(|r| r.completed) {
                Ok(())
            } else {
                Err(format!("runs on {tip}: {runs:?}"))
            })
        })?;
    }

    // 11. Clean up the run's fixtures; trunk keeps what landed on it.
    for number in fixtures {
        let open = repo
            .client
            .get_pr(number)
            .map_err(at(&format!("reading #{}", number.0)))?
            .open;
        if open {
            repo.close(number)?;
        }
    }
    repo.clean(&format!("refs/heads/{prefix}-"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;

    /// `auth_from_env` over `vars` in place of the process environment.
    fn auth(vars: &[(&str, &str)]) -> Result<Auth, String> {
        let vars: BTreeMap<&str, &str> = vars.iter().copied().collect();
        auth_from_env(
            |var| vars.contains_key(var),
            |var| match vars.get(var) {
                Some(value) if !value.is_empty() => Ok((*value).to_owned()),
                _ => Err(format!("{var} is unset, empty or not UTF-8")),
            },
        )
    }

    fn key_file(name: &str, contents: &str) -> String {
        let path = PathBuf::from(env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"))
            .join(name);
        fs::write(&path, contents).unwrap();
        path.display().to_string()
    }

    /// The app key fixture at `name`; tests run in parallel, so each writes
    /// its own copy.
    fn app_key_file(name: &str) -> String {
        key_file(name, include_str!("../../testdata/app-key.pkcs1.pem"))
    }

    #[test]
    fn the_lock_runs_only_when_asked_for_with_1() {
        let lock = |value: &str| lock_heads_from_env(Ok(value.to_owned()));
        assert_eq!(
            lock_heads_from_env(Err(env::VarError::NotPresent)),
            Ok(false)
        );
        assert_eq!(lock(""), Ok(false));
        assert_eq!(lock("1"), Ok(true));
        for value in ["0", "false", "true", "yes"] {
            let error = lock(value).unwrap_err();
            assert_eq!(
                error,
                format!("DORFQ_LIVE_LOCK_HEADS is {value:?}; set it to `1` or leave it unset")
            );
        }
    }

    #[test]
    fn the_cancel_runs_only_when_asked_for_with_actions() {
        let ci = |value: &str| ci_from_env(Ok(value.to_owned()));
        assert_eq!(ci_from_env(Err(env::VarError::NotPresent)), Ok(None));
        assert_eq!(ci(""), Ok(None));
        assert_eq!(ci("actions"), Ok(Some(Ci::Actions)));
        for value in ["1", "Actions", "buildkite"] {
            let error = ci(value).unwrap_err();
            assert_eq!(
                error,
                format!("DORFQ_LIVE_CI is {value:?}; set it to `actions` or leave it unset")
            );
        }
    }

    #[test]
    fn a_token_alone_is_the_token_credential() {
        assert!(matches!(auth(&[(TOKEN_VAR, "t")]), Ok(Auth::Token(t)) if t == "t"));
    }

    #[test]
    fn an_app_alone_is_the_app_credential() {
        let path = app_key_file("app-alone.pem");
        let auth = auth(&[(APP_CLIENT_ID_VAR, "Iv1.test"), (APP_KEY_FILE_VAR, &path)]);
        assert!(matches!(auth, Ok(Auth::App(_))));
    }

    #[test]
    fn both_or_neither_credential_is_refused_naming_all_three() {
        let path = app_key_file("both-or-neither.pem");
        let both = [
            (TOKEN_VAR, "t"),
            (APP_CLIENT_ID_VAR, "Iv1.test"),
            (APP_KEY_FILE_VAR, path.as_str()),
        ];
        // A present but empty variable counts as set.
        let empty_token = [
            (TOKEN_VAR, ""),
            (APP_CLIENT_ID_VAR, "Iv1.test"),
            (APP_KEY_FILE_VAR, path.as_str()),
        ];
        for vars in [&both[..], &empty_token[..], &[]] {
            let Err(error) = auth(vars) else {
                panic!("accepted {vars:?}");
            };
            for var in [TOKEN_VAR, APP_CLIENT_ID_VAR, APP_KEY_FILE_VAR] {
                assert!(error.contains(var), "{error}");
            }
        }
    }

    #[test]
    fn half_an_app_credential_names_the_missing_variable() {
        let path = app_key_file("half-an-app.pem");
        let Err(error) = auth(&[(APP_KEY_FILE_VAR, &path)]) else {
            panic!("accepted a key file without a client id");
        };
        assert!(error.contains(APP_CLIENT_ID_VAR), "{error}");
        let Err(error) = auth(&[(APP_CLIENT_ID_VAR, "Iv1.test")]) else {
            panic!("accepted a client id without a key file");
        };
        assert!(error.contains(APP_KEY_FILE_VAR), "{error}");
    }

    #[test]
    fn an_unreadable_or_unusable_key_file_is_named() {
        let missing = key_file("unused.pem", "");
        fs::remove_file(&missing).unwrap();
        let not_a_key = key_file("not-a-key.pem", "just some text\n");
        for (path, why) in [(missing, "cannot read"), (not_a_key, "not PEM")] {
            let Err(error) = auth(&[(APP_CLIENT_ID_VAR, "Iv1.test"), (APP_KEY_FILE_VAR, &path)])
            else {
                panic!("accepted {path}");
            };
            assert!(error.contains(APP_KEY_FILE_VAR), "{error}");
            assert!(error.contains(&path), "{error}");
            assert!(error.contains(why), "{error}");
        }
    }
}
