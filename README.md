# dorfq

DorfQ (Dorfman + queue, after Robert Dorfman's pooled testing) is a merge
queue for large monorepos with long CI runs. It pools queued pull requests
into batched CI runs, so one passing run lands many changes. When a batch
fails, it infers the culprit from what failed instead of bisecting blind,
keeps landing the rest, and tells every author what happened to their pull
request. It lands only the exact tree that passed CI, by fast-forwarding
trunk.

## Status

The boundary between the scheduler and the forge exists, with a stub on
each side and the driver that joins them: a deliberately simple scheduler
that batches every ready pull request at once, an in-memory mock forge
driven by a scenario file, and a `dorfq run` loop that plays one against
the other. A GitHub forge exists too, polling the REST API (no webhooks
yet), so dorfq can be run against a GitHub repository; see **Running
against GitHub**. dorfq does not run on its own repository yet: batch
branches get CI only once the workflow change that covers them is on `main`.
`CHANGELOG.md` tracks what exists.

The contract carries stacks (a pull request based on another, and stacks
that land only whole), probes (a batch with no members that tests its
base's tree alone), how each pull request left the queue, the names of the
checks a batch failed, and the forge's clock. All three schedulers batch
stacked pull requests, bases before children, and land a stack that must
land whole only whole; in the simulator's world pull requests arrive as
stacks. The GitHub forge enqueues stacks and trees of any depth (see **Running
against GitHub**).

On the scheduler side there are two real policies beside the stub: `bisect`,
the conventional pipelined bisect with memory that every number is compared
to, and `bayes`, which keeps a belief about every queued pull request,
infers culprits exactly from the runs that failed and passed, and chooses
batches by expected value; its belief about a pull request starts from how
far trunk had moved since the pull request's own CI ran, and a pull
request's value grows with the hours it has waited (`w_age`). Alternatives within
`bayes`'s algorithm are flags in its configuration, each naming the status
quo and the alternative so the sweep can carry both:
`speculation = "chain"` builds one chain of batches to land with culprit
finders on trunk beside it, and `"tree"` scores, for every batch in flight,
what to run if it passes and what if it fails, each weighted by that
future's probability; `charge` says whether a run's cost is discounted by
that probability, and `landing` whether a landing regrows every other
belief by the interaction prior or transfers each pair's belief. Both are
measured with `dorfq-sim`, a seeded,
event-driven simulator that runs any scheduler against a world of pull
requests with hidden truths (pairs that break when tested together and
break whichever lands second, flaky runs; no pull request is bad on its
own) and reports runner-hours per landed pull request, time to land,
to reject and from breaking to ejection, wasted runner time and false
ejections, running each replication until its queue empties. The capacity knob is
each policy's `lanes`, the batches it runs at once: dorfq's columns in an
example share one capacity, and `github-like` runs at the concurrency
GitHub's queue is given in practice. PR CI, the CI a pull request runs on its own, runs
beside the queue, on capacity the queue does not own: at arrival, after each
push, and after the rebase a conflict forces. It fails when the pull
request is broken against trunk, and its author then pushes a fix. As the
GitHub forge does, the world lets a pull request into a batch only once its
current head has passed, so time to land includes it. The queue is billed only for the PR CI
it causes by landing part of a stack — the run of each direct child of what
landed, which under GitHub's admission is every pull request of a stack in
turn. In the busy example world, a high-velocity team's with batch and PR
CI of 45 to 120 minutes, `bayes`, the status quo, lands 322 at 1.0
runner-hours per landed pull request, ejecting 10 bad pull requests and
0.0 good ones, against 270 for `bisect` at cap 1 (no pooling at all; 24
bad and 22 good ejected, 2.6 runner-hours per landed) and 277 for
`github-like` (6 and 23, 2.8). `bayes-tree`, the tree of futures at the
same settings, lands 325 at 1.0 runner-hours per landed and ejects 9 bad
pull requests and 0.0 good ones, at a median of 5.1 hours from a pull
request breaking to its ejection against `bayes`'s 4.3; see **Simulator
reports**.

```sh
make run ARGS="run --config $PWD/crates/dorfq/examples/mock.toml --exit-when-idle"
```

plays the shipped scenario: one pull request ejected for conflicting with
trunk, three landed in one batch, and one ejected when its batch fails.

```sh
make sim ARGS="run --config $PWD/crates/dorfq-sim/examples/sim.toml"
```

runs the shipped simulator example, a week of arrivals on two lanes, and
prints one column per policy.

## Simulator reports

The report site, published at <https://rnortman.github.io/dorfq/>, shows <!-- gitleaks:allow -->
each shipped simulator example (`crates/dorfq-sim/examples/`): every policy's headline
numbers as deltas against the `github-like` column, an approximation of
GitHub's own merge queue, the full metric table, and a replay of the first
replication — batch lanes, the queue with its stacks, and trunk —
switchable between policies over the same arrivals. Each report shows the
configuration it ran with, its comments explaining every parameter, and a
playground runs the simulator in the browser, compiled to WebAssembly, on a
configuration you edit, starting from the report's own. The Pages workflow
(`.github/workflows/pages.yml`) builds and deploys it on every push to
`main`.

`dorfq-sim run --config F --json PATH` writes the document the site reads.
`make sim-report` builds every example's under `bazel-bin/site/reports/`
and the browser simulator at `bazel-bin/site/dorfq_sim_wasm.wasm`;
`make check` builds them too, so an example that fails is a red gate.
To view the site locally, open `site/index.html` and load a report through
its file picker; the playground needs the page served over HTTP.

`make sweep ARGS="--config $PWD/F"` runs a grid of simulations, optimised
and on every available thread. A sweep file is a run file with two more
top-level keys: `baseline`, the policy the others are compared against,
and `[[axis]]` tables, each with `keys` (fields such as
`world.arrivals_per_hour` or `policy.bayes.max_batch`, set together),
`values` and optional `labels`. The sweep runs every point of the axes'
cartesian product and prints one row per point and policy with every
metric's mean over the replications, then each policy's mean relative
delta against the baseline over the points where the baseline's is not 0,
with the counts of points at which it is better and worse, `(B+ W- /N)`,
over every point where both have the metric. `--json PATH` writes every replication's metrics,
`--jobs N` caps the threads. Two sweep files ship in
`crates/dorfq-sim/sweeps/`: `speculation.toml` compares the `bayes` flag
combinations with each other and with `github-like` over arrival rate, CI
length, flake rate, `max_batch`, `lanes`, `run_cost`, `w_reject` and
`w_age`, at an interaction rate of 0.001, and `bisect-cap.toml` measures `bisect`'s batch cap in the busy world.

## Running against GitHub

The forge is tested against an in-memory fake of GitHub. Its first contact
with GitHub is the live lane, so run that before pointing the queue at any
repository you care about. The steps, all done by the operator:

1. Create the intent label (`queue` in the example) in the repository.
2. Choose the queue's credential. As a GitHub App (below), the queue has an
   identity of its own: comments and merge commits appear as
   `<app name>[bot]`, and what it holds at rest is a private key from which
   it mints its own short-lived tokens. As a token, it acts as a person:
   create a fine-grained personal access token for the repository with
   Contents: read/write, Pull requests: read/write, Issues: read, Checks:
   read, Metadata: read and Workflows: read/write, export it as
   `DORFQ_GITHUB_TOKEN` where the queue runs, and set `[forge.auth]` to
   `kind = "token"`, `env = "DORFQ_GITHUB_TOKEN"`; comments and merge
   commits appear under the token's account. Under either credential,
   Workflows is needed because GitHub refuses a merge or a ref update that
   adds or changes a file under `.github/workflows/` to a credential
   without it, and the queue makes both for every queued pull request that
   touches CI; without it the first such batch stops the queue. Checks is
   what GitHub's permission reference names for listing a commit's check
   runs, which the queue does on every observation.
3. Run the live lane: create a throwaway repository whose name contains
   `scratch` or `playground`, with a `main` branch holding at least one
   commit; give the credential access to it as well, with Commit statuses:
   read/write added; then

   ```sh
   DORFQ_LIVE_REPO=<owner>/<name> make live-test
   ```

   with either `DORFQ_GITHUB_TOKEN`, or `DORFQ_GITHUB_APP_CLIENT_ID` and
   `DORFQ_GITHUB_APP_KEY_FILE` (the key's absolute path), in the
   environment; exactly one of the two. Under an app this is the one check
   that GitHub accepts its tokens and lets it comment, label, set statuses
   and push trunk. It refuses any repository whose name contains neither,
   and treats the one it accepts as disposable: it closes every open
   labelled pull request, deletes every queue ref, and lands commits on
   `main`. `DORFQ_LIVE_API_BASE` points it at GitHub Enterprise.
4. If trunk is protected, the credential's identity must be allowed to push
   to it (landing is a direct fast-forward, not a pull-request merge), and
   trunk's required status checks should be the names in `checks`.
5. CI must run on `dorfq/batch/**` pushes and report under the names in
   `checks`. GitHub reads a push workflow from the pushed commit, and a
   batch branch carries trunk's workflows, so the trigger has to be on
   trunk before batch branches get checks; until then batches stay pending.
   dorfq's own `.github/workflows/ci.yml` has the trigger.
6. Copy `crates/dorfq/examples/github.toml`, set `owner`, `repo` and
   `[forge.auth]`, and run it wherever outbound HTTPS is available:

   ```sh
   make optimised   # then: dorfq run --config <your github.toml>
   make run ARGS="run --config $PWD/crates/dorfq/examples/github.toml"
   ```

   `api_base` is `https://<host>/api/v3` on GitHub Enterprise;
   `tick_millis` is the polling interval.
7. To keep it running, `crates/dorfq/examples/systemd/` has a user unit and
   a system unit; each says where it expects the binary, the configuration
   and the token or the app's key. `make local-deploy` installs the binary
   where the user unit expects it and restarts the unit if it is running.

While the queue runs, nobody pushes to, deletes or creates anything under
`dorfq/` or `refs/dorfq/` by hand: the queue reads a foreign or moved ref
there as a fault and stops. Trunk moves only by fast-forward, and a push to
trunk by another hand makes every batch built on the old trunk refuse to
land. Batch ids in those names are decimal.

A queue that stops with "batch <id>'s tip is its base" is tracking a batch
that an earlier build published at its own base, because every member was
already in trunk. Stop the queue, delete `refs/dorfq/batches/<id>` and the
branch `dorfq/batch/<id>`, and start it again; the batch's members that are
still labelled rejoin the queue.

The label is what authorises a merge. Anyone who can apply it lands the pull
request, and on GitHub the Triage role is enough. The pull request lands at
whatever head it has when its batch is built, which may include commits
pushed after it was labelled. Branch protection's review rules do not apply,
because landing is a push by the queue's own identity, the token's account
or the app. Batch CI runs the pull request's own workflow files as a push to
this repository, with this repository's secrets, and that holds for pull requests from forks too. So
read a pull request's current head, workflows included, before you label it.

A stack is a pull request whose base is another pull request's branch.
Label every pull request in it: a labelled pull request is enqueued once
the pull request whose head branch it targets is enqueued, and until then
its author is told in a comment that it is blocked, and why. A pull request
whose base is a branch that is neither trunk nor any open pull request's
head is told the same, and never enqueued. A stacked pull request is held
while its head does not contain its base's current head, so after a push to
the base, rebase the pull requests stacked on it. Once a base pull request
has merged into trunk, by the queue or by hand, the queue moves the pull
requests it had queued on it onto trunk; it does this only for a stack it
had queued and only for a merge into trunk, never for a branch whose pull
request was merged elsewhere or that a labelled pull request targeted before
it was queued on it — those are told to retarget by hand. A stacked pull
request landed in the same batch as its base is moved onto trunk just
before trunk moves, so GitHub marks it merged. Every policy batches a stack
bases before children.

### As a GitHub App

1. Create the app: the account's *Settings → Developer settings → GitHub
   Apps → New GitHub App*. Give it a name (its comments appear as
   `<name>[bot]`) and any homepage URL, deactivate *Webhook* (dorfq polls),
   and under *Where can this GitHub App be installed?* choose only this
   account.
2. Repository permissions: Contents read/write, Pull requests read/write,
   Issues read, Checks read, Metadata read, Workflows read/write; add
   Commit statuses read/write if the live lane will run under the app.
3. *Generate a private key*. GitHub downloads a `.pem`; store it where the
   queue runs, mode 0600, and note the *Client ID* from the app's page.
4. *Install App* on the repository the queue serves, and only that one
   (plus the live lane's throwaway repository while you run it).
5. If trunk is protected by branch protection or a ruleset, add the app to
   *Restrict who can push* and to the bypass list of any rule requiring a
   pull request: landing is a direct fast-forward push by the app. Trunk's
   required status checks are the names in `checks`.
6. In the configuration:

   ```toml
   [forge.auth]
   kind = "app"
   client_id = "<the app's Client ID>"
   private_key_file = "<path to the .pem>"   # relative to the configuration
   ```

7. Run the live lane under the app (step 3 above) before pointing the queue
   at a repository you care about.

The queue mints an installation token scoped to the one repository at
startup, and a new one before each expires. It fails at startup, naming the
cause, if the key is unreadable or not a key, the app is not installed on
the repository, or GitHub rejects its signature (a wrong client id, a key
that is not the app's, or this host's clock more than a minute ahead or
five minutes behind GitHub's).

## Building

Install [bazelisk](https://github.com/bazelbuild/bazelisk) as `bazel`; it
reads `.bazelversion` and fetches the pinned Bazel. Every toolchain is pinned
in `MODULE.bazel` and fetched by checksum, so nothing else is needed on the
host but git and make.

```sh
make setup   # once per clone: wires the git hooks
make check   # the full gate, exactly what CI runs
make run     # build and run the dorfq binary (ARGS=... for arguments)
make help    # every target
```

The `Makefile` is a frontend: every target is a spelled-out `bazel`
invocation.

## Layout

- `crates/` — the Rust workspace. `Cargo.toml` and `Cargo.lock` are the
  source of truth for crate versions; Bazel builds them through
  `crate_universe`.
  - `crates/dorfq-core/` — the contract between scheduler and forge: the
    observation and plan types, the `Scheduler` and `Forge` traits, and the
    rules both sides keep.
  - `crates/dorfq-scheduler/` — schedulers and their configuration: the
    `AllAtOnce` stub, the `Bisect` baseline and the `Bayes` scheduler.
  - `crates/dorfq-forge-mock/` — the in-memory mock forge and its scenario
    format.
  - `crates/dorfq-forge-github/` — the GitHub forge, over the REST API, and
    the live lane (`make live-test`), which runs it against a repository
    whose name contains `scratch` or `playground` and treats that
    repository as disposable.
  - `crates/dorfq/` — the application: configuration, provider
    construction, the driver loop and the `dorfq` binary.
  - `crates/dorfq-sim/` — the simulator: a seeded world that implements the
    forge side of the contract, the loop that runs a scheduler against it,
    the metrics and the `dorfq-sim` binary.
  - `crates/dorfq-sim-wasm/` — the simulator as a WebAssembly module for
    the report site's playground.
- `site/` — the simulator report site: a static page and the Bazel rules
  that run the examples into its reports. `site/sim-worker.js` runs the
  playground's simulator off the page's thread; `site/wasm.bzl` builds it
  for wasm32, optimised.
- `tools/scrub/` — the secret scanner the commit, push and CI gates run,
  built by Bazel from its published crate with a pinned gitleaks.
- `tools/allthechanges/` — the changelog and TODO fragment tool, built by
  Bazel from its published crate.
- `changelog.d/` and `todo.d/` — the changelog and TODO fragment
  directories: each change's entry is a file here until a release (or, for
  TODOs, `make todo-gather`) folds it into `CHANGELOG.md` or `TODO.md`.
- `tools/pins/` — a test holding the version pins that appear in more than
  one file equal.
- Lock files: `MODULE.bazel.lock` pins the Bazel module graph;
  `cargo-bazel-lock.json` is the rendered view of `Cargo.lock` for the
  workspace's crates; `tools/scrub/Cargo.lock` and
  `tools/scrub/cargo-bazel-lock.json` pin the scanner's crates, and
  `tools/allthechanges/Cargo.lock` and
  `tools/allthechanges/cargo-bazel-lock.json` the fragment tool's. `make
  repin` re-renders the crate locks after a manifest change.

## License

Apache-2.0; see `LICENSE` and `NOTICE`.
