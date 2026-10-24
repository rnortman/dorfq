//! The live lane: drives [`GitHubForge`] through a whole batch lifecycle on a
//! real, disposable GitHub repository and exits non-zero on the first failed
//! assertion.
//!
//! Reads `DORFQ_GITHUB_TOKEN`, `DORFQ_LIVE_REPO` (`owner/name`) and
//! optionally `DORFQ_LIVE_API_BASE` (default `https://api.github.com`).
//! Refuses, before any request, a repository whose name contains neither
//! `scratch` nor `playground`. The repository it accepts is treated as disposable: every open
//! labelled pull request is closed and every queue ref deleted, and trunk
//! (`main`) keeps the commits the run lands on it.

use std::env;
use std::fmt::Display;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dorfq_core::{
    BatchId, BatchStart, BatchState, CiStatus, ConflictWith, EjectReason, Forge, ForgeError,
    LandRefusal, Observation, PrNumber, PrStatus, Sha,
};
use dorfq_forge_github::api::{GitHubApi, RefUpdate, RestClient};
use dorfq_forge_github::messages::{Marker, last_marker};
use dorfq_forge_github::{GitHubForge, Settings, system_clock};
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
    token: String,
}

impl Target {
    fn from_env() -> Result<Target, String> {
        let required = |var: &str| match env::var(var) {
            Ok(value) if !value.is_empty() => Ok(value),
            _ => Err(format!("{var} is unset, empty or not UTF-8")),
        };
        let token = required("DORFQ_GITHUB_TOKEN")?;
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
            token,
        })
    }

    fn client(&self) -> RestClient {
        RestClient::new(
            &self.api_base,
            &self.owner,
            &self.repo,
            &self.token,
            USER_AGENT,
        )
    }

    fn forge(&self) -> Result<GitHubForge, String> {
        let settings = Settings {
            trunk: TRUNK.to_owned(),
            label: LABEL.to_owned(),
            checks: vec![CHECK.to_owned()],
        };
        GitHubForge::new(
            Box::new(self.client()),
            settings,
            Box::new(system_clock),
            Box::new(std::io::stdout()),
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

fn enqueued(observation: &Observation) -> Vec<PrNumber> {
    observation.prs.iter().map(|p| p.number).collect()
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

    /// Closes every open labelled pull request against trunk, deletes every
    /// queue ref and every branch under `stale_prefix`, and creates the label
    /// if it is missing.
    fn clean(&mut self, stale_prefix: &str) -> Result<(), String> {
        for pr in self
            .client
            .list_open_prs(TRUNK)
            .map_err(at("listing pull requests"))?
        {
            if pr.labels.iter().any(|l| l == LABEL) {
                self.close(pr.number)?;
            }
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

    /// A branch from trunk writing `content` to `path`, a pull request for
    /// it carrying the label, and a passing `check` status on its head.
    fn fixture(
        &mut self,
        trunk: &Sha,
        branch: &str,
        title: &str,
        path: &str,
        content: &str,
    ) -> Result<Fixture, String> {
        let head = self.commit_file(trunk, path, content, title)?;
        self.client
            .create_ref(&format!("refs/heads/{branch}"), &head)
            .map_err(at(&format!("creating {branch}")))?;
        let what = format!("opening a pull request for {branch}");
        let pr = self
            .client
            .post(
                "/pulls",
                &json!({ "title": title, "head": branch, "base": TRUNK }),
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
    // permission.
    let trunk = repo.trunk()?;
    let a = repo.fixture(
        &trunk,
        &format!("{prefix}-a"),
        &format!("{prefix} A"),
        "a.txt",
        &format!("{prefix} A\n"),
    )?;
    let b = repo.fixture(
        &trunk,
        &format!("{prefix}-b"),
        &format!("{prefix} B"),
        &format!(".github/workflows/{prefix}.yml"),
        &format!(
            "name: {prefix}\non: workflow_dispatch\njobs:\n  noop:\n    runs-on: ubuntu-latest\n    \
             steps:\n      - run: \"true\"\n"
        ),
    )?;
    let c = repo.fixture(
        &trunk,
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

    // 4. A batch of all three conflicts at C, attributed to A by the probe.
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
    forge.retire(BatchId(1)).map_err(at("retiring batch 1"))?;
    observe_until(&mut forge, "A and B enqueued, no batches", |o| {
        enqueued(o) == [a.number, b.number] && o.batches.is_empty()
    })?;
    repo.wait_for_marker(c.number, Marker::Ejected)?;
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
    observe_until(&mut forge, "batch 2 passed", |o| {
        batch(o, 2)
            == Some(&BatchState::Testing {
                tip: tip.clone(),
                ci: CiStatus::Passed,
            })
    })?;

    // 6. Batch 2 lands: trunk is its tip, A and B are told.
    forge.land(BatchId(2)).map_err(at("landing batch 2"))?;
    let observation = observe_until(&mut forge, "batch 2 landed, nothing enqueued", |o| {
        batch(o, 2) == Some(&BatchState::Landed { tip: tip.clone() }) && o.prs.is_empty()
    })?;
    check(observation.trunk == tip, || {
        format!("trunk is {}, expected {tip}", observation.trunk)
    })?;
    repo.wait_for_marker(a.number, Marker::Landed)?;
    repo.wait_for_marker(b.number, Marker::Landed)?;
    forge.retire(BatchId(2)).map_err(at("retiring batch 2"))?;

    // 7. Batch 3 of D passes, trunk moves under it, the landing is refused.
    let d = repo.fixture(
        &tip,
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
    forge
        .start_batch(&BatchStart::new(
            BatchId(3),
            observation.trunk.clone(),
            vec![d.number],
        ))
        .map_err(at("starting batch 3"))?;
    let observation = observe_until(&mut forge, "batch 3 testing", |o| {
        matches!(batch(o, 3), Some(BatchState::Testing { .. }))
    })?;
    let Some(BatchState::Testing { tip: tip3, .. }) = batch(&observation, 3).cloned() else {
        unreachable!("observe_until accepted batch 3 as testing");
    };
    repo.pass(&tip3)?;
    observe_until(&mut forge, "batch 3 passed", |o| {
        batch(o, 3)
            == Some(&BatchState::Testing {
                tip: tip3.clone(),
                ci: CiStatus::Passed,
            })
    })?;
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
    forge.land(BatchId(3)).map_err(at("landing batch 3"))?;
    let refused = BatchState::LandRefused {
        tip: tip3.clone(),
        reason: LandRefusal::TrunkMoved {
            trunk: pushed.clone(),
        },
    };
    observe_until(&mut forge, "batch 3 refused, trunk moved", |o| {
        batch(o, 3) == Some(&refused)
    })?;
    forge.retire(BatchId(3)).map_err(at("retiring batch 3"))?;

    // 8. Clean up the run's fixtures; trunk keeps what landed on it.
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
