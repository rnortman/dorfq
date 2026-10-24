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
checks a batch failed, and the forge's clock. The `all-at-once` stub lands
stacks; `bisect` and `bayes` do not yet batch a stacked pull request, and
the GitHub forge does not yet enqueue one: it reports only pull requests
whose base is trunk.

On the scheduler side there are two real policies beside the stub: `bisect`,
the conventional pipelined bisect with memory that every number is compared
to, and `bayes`, which keeps a belief about every queued pull request,
infers culprits exactly from the runs that failed and passed, and chooses
batches by expected value. Both are measured with `dorfq-sim`, a seeded,
event-driven simulator that runs any scheduler against a world of pull
requests with hidden truths (bad ones, pairs that break only together,
flaky runs, a fixed number of runners) and reports runner-hours per landed
pull request, time to land and to reject, wasted runner time and false
ejections.

```sh
make run ARGS="run --config $PWD/crates/dorfq/examples/mock.toml --exit-when-idle"
```

plays the shipped scenario: one pull request ejected for conflicting with
trunk, three landed in one batch, and one ejected when its batch fails.

```sh
make sim ARGS="run --config $PWD/crates/dorfq-sim/examples/sim.toml"
```

runs the shipped simulator example, a week of arrivals on two runners, and
prints one column per policy.

## Running against GitHub

The forge is tested against an in-memory fake of GitHub. Its first contact
with GitHub is the live lane, so run that before pointing the queue at any
repository you care about. The steps, all done by the operator:

1. Create the intent label (`queue` in the example) in the repository.
2. Create a fine-grained personal access token for the repository with
   Contents: read/write, Pull requests: read/write, Issues: read, Metadata:
   read and Workflows: read/write, and export it as `DORFQ_GITHUB_TOKEN`
   where the queue runs. Comments and merge commits appear under the
   token's account. Workflows is needed because GitHub refuses a merge or a
   ref update that adds or changes a file under `.github/workflows/` to a
   token without it, and the queue makes both for every queued pull request
   that touches CI; without it the first such batch stops the queue.
3. Run the live lane: create a throwaway repository whose name contains
   `scratch` or `playground`, with a `main` branch holding at least one
   commit; give the
   token access to it as well, with Commit statuses: read/write added; then

   ```sh
   DORFQ_LIVE_REPO=<owner>/<name> make live-test
   ```

   It refuses any repository whose name contains neither, and
   treats the one it accepts as disposable: it closes every open labelled
   pull request, deletes every queue ref, and lands commits on `main`.
   `DORFQ_LIVE_API_BASE` points it at GitHub Enterprise.
4. If trunk is protected, the token's identity must be allowed to push to it
   (landing is a direct fast-forward, not a pull-request merge), and trunk's
   required status checks should be the names in `checks`.
5. CI must run on `dorfq/batch/**` pushes and report under the names in
   `checks`. GitHub reads a push workflow from the pushed commit, and a
   batch branch carries trunk's workflows, so the trigger has to be on
   trunk before batch branches get checks; until then batches stay pending.
   dorfq's own `.github/workflows/ci.yml` has the trigger.
6. Copy `crates/dorfq/examples/github.toml`, set `owner` and `repo`, and run
   it wherever outbound HTTPS is available:

   ```sh
   make optimised   # then: dorfq run --config <your github.toml>
   make run ARGS="run --config $PWD/crates/dorfq/examples/github.toml"
   ```

   `api_base` is `https://<host>/api/v3` on GitHub Enterprise;
   `tick_millis` is the polling interval.
7. To keep it running, `crates/dorfq/examples/systemd/` has a user unit and
   a system unit; each says where it expects the binary, the configuration
   and the token. `make local-deploy` installs the binary where the user
   unit expects it and restarts the unit if it is running.

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
because landing is a push by the token's identity. Batch CI runs the pull
request's own workflow files as a push to this repository, with this
repository's secrets, and that holds for pull requests from forks too. So
read a pull request's current head, workflows included, before you label it.

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
