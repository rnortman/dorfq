# Changelog

Notable changes to dorfq. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-10-26

### Added

- Changelog and TODO entries are now small fragment files under `changelog.d/`
  and `todo.d/`, so concurrent branches no longer conflict on `CHANGELOG.md`
  and `TODO.md`. The fragments are handled by allthechanges, a tool built
  through Bazel: `make release VERSION=X.Y.Z` gathers both kinds into
  `CHANGELOG.md` and `TODO.md` and bumps the crate version, `make todo-gather`
  folds in the TODO fragments alone (for a burndown before a release), and
  `make check` lints every fragment.
- The `bayes` scheduler has a new required setting, `w_age`: how much each
  hour a pull request has waited adds to its value, so long-waiting pull
  requests are batched and their failures resolved ahead of fresh ones at
  the same risk. `w_age = 0` keeps the previous behaviour; the shipped
  examples use 0.02.
- A GitHub forge (`kind = "github"` in the configuration): dorfq can run a
  queue on a GitHub or GitHub Enterprise repository through the REST API,
  polling once per tick. A label marks a pull request as queued; batches are
  built from merge commits on `dorfq/batch/<id>` branches and landed by
  fast-forwarding trunk once their required checks pass; authors are told
  each step in a pull request comment. Queue state lives in the repository
  itself, so a restarted queue picks up where it left off. See
  `crates/dorfq/examples/github.toml` and `docs/running-on-github.md`.
  Example systemd units, user and system, are in
  `crates/dorfq/examples/systemd/`.
- Operators should know that the queue label is the landing authority:
  anyone who can apply it (Triage role included) lands the pull request at
  the head it has when batched, without branch protection's review rules,
  and batch CI runs that code with the repository's secrets, fork pull
  requests included. Restrict who can label accordingly.
- `make live-test`: exercises the GitHub forge end to end against a
  disposable repository whose name contains `scratch` or `playground`.
- A `bayes` scheduler: keeps a probability that each queued pull request
  breaks on its own and that a pair breaks only together, updates it
  exactly from every run that failed or passed, chooses batches by expected
  value against the cost of a run, runs suspects alone to find the culprit
  while the chain keeps landing, and ejects a pull request only when a
  failed run singles it out. If clusters of linked failures grow large
  enough that its probabilities can no longer be computed precisely, it
  stops the process rather than act on noise (see
  `bayes-posterior-precision` in `TODO.md`).
- A `bisect` scheduler: the conventional pipelined bisect. Batches run as one
  chain, each on the previous one's tip; a failed batch is split in half and
  the halves run next; no batch is built from pull requests already known
  to fail together.
- `dorfq-sim run --config <path> [--trace]`: a deterministic, seeded
  simulator that runs schedulers against a world of pull requests whose
  truth it knows (which break on their own, which pairs break only
  together, which runs flake) on a fixed number of CI runners, and prints a
  table comparing them: runner-hours per landed pull request, time to land
  and to reject, wasted runner time, false ejections. Two policies at one
  seed see the same arrivals and truths. `make sim` runs it; the shipped
  example is `crates/dorfq-sim/examples/sim.toml`.
- Scheduler configuration (`[scheduler]`, `kind = "bisect" | "bayes"`) now
  lives in the scheduler crate and is shared by `dorfq` and the simulator;
  out-of-range parameters are rejected by name.
- `dorfq run --config <path>`: the first end-to-end loop. It reads a TOML
  configuration naming a forge (the service hosting the repository and its
  pull requests) and a scheduler (the part that decides what to batch, land
  or eject), then repeatedly asks the forge what has changed and carries out
  the scheduler's plan. `--exit-when-idle` stops once there is nothing left
  to do. Running `dorfq` with no subcommand is now a usage error.
- Only stand-ins exist on either side so far: a placeholder scheduler that
  tests every ready pull request in one batch and ejects them all if it
  fails, and an in-memory `mock` forge that plays a scripted scenario. Try
  it with the shipped example, `crates/dorfq/examples/mock.toml`.
- The contract between scheduler and forge (`dorfq-core`) is fixed, so the
  two sides can now be developed independently. Its rules are written down
  in `crates/dorfq-core`.
- Initial repository scaffolding.
- A Bazel-only build: Rust, LLVM and buildtools toolchains and every tool the
  gate runs are pinned and fetched by checksum, so a clone needs only
  bazelisk, git and make. `make check` is the single gate (build, tests,
  clippy, rustfmt, buildifier, secret scan).
- Secret scanning with brenn-scrub, built through Bazel, wired into the
  pre-commit and pre-push git hooks (`make setup`) and Claude Code write hooks.
- GitHub Actions CI running `make check` on every push and pull request.
- The GitHub forge can run as a GitHub App: the queue's comments and merges
  appear as `<app>[bot]`, and it mints and renews its own short-lived
  installation tokens from the app's private key.
  `docs/running-on-github.md` has a section, "As a GitHub App", that walks
  through creating, permissioning and installing the app. The live lane
  accepts the app's credential as well (`DORFQ_GITHUB_APP_CLIENT_ID` with
  `DORFQ_GITHUB_APP_KEY_FILE`).
- The GitHub forge queues stacked pull requests: a labelled pull request
  whose base is the branch of another queued pull request joins the queue
  on it, in stacks and trees of any depth. It is held while its head does
  not contain its base's current head, and reported conflicting with its
  base when it does not merge into it.
- A labelled pull request the queue cannot take, because the pull request
  it is based on is not queued or because it targets a branch that is
  neither trunk nor a queued pull request's, is told so in a comment,
  once for each reason, with what to do about it.
- Once a base pull request has merged into trunk, by the queue or by hand,
  the GitHub forge moves the pull requests it had queued on it onto trunk; a
  stacked pull request landed in the same batch as its base is moved onto
  trunk as its batch lands, before trunk moves, so GitHub marks it merged.
- Stacked pull requests are now merged through the queue. The `bisect` and
  `bayes` schedulers (the two batching strategies) batch a pull request
  together with the pull requests it is stacked on, always testing and
  landing the bases first. A pull request marked `Atomic` lands together
  with everything beneath it or not at all: it is batched whole, even past
  `max_batch`, and a failure never splits it apart. When an `Atomic` group
  fails on its own, its `Atomic` members are ejected and the pull requests
  beneath them are retested. `bayes` also counts a child's own CI run as a
  test of its base, and tests a suspected stacked culprit on top of the
  bases it needs.
- The simulator report site has a playground: edit a report's
  configuration and rerun the simulator in the browser (compiled to
  WebAssembly), with the result rendered like any published report.
- The README links the published report site.
- The simulator report site's playground can download the document its run
  produced — the JSON `dorfq-sim run --json` writes, with every replication's
  metrics, the configuration and the first replication's trace — to open
  later through the page's file picker or to hand on.
- The simulator now models each pull request's own CI, not just the merge
  queue's batch runs. A pull request runs its CI on the shared runners
  whenever it gets a new head (on arrival, on a re-push, on a rebase) and
  cannot enter a batch until that run passes. The cost shows up in the new
  `pr_ci_runs` and `pr_ci_hours` metrics and in `runner_hours`.
  **Breaking for configurations:** `[world]` now requires `pr_ci_minutes`
  and `rebase_minutes`.
- Simulator policies now choose how pull requests are admitted to the
  queue (`admission`, required): `label` (dorfq's way, from arrival) or
  `github` (GitHub's way: only once on trunk with its own CI passed, so a
  stacked pull request waits for its base to land, is rebased and re-checked
  first). This makes the cost of GitHub's one-at-a-time handling of stacks
  visible in the comparison.
- A conflict with trunk is now simulated as a rebase and a return to the
  queue rather than a final ejection; the new `rebases` metric counts these
  and `ejected_bad` no longer does.
- A simulator report site under `site/`, published to GitHub Pages by a
  new workflow, answers "how does this compare to a simpler merge queue?":
  each policy's headline numbers against a `github-like` baseline, the full
  metric table, and an animated replay of one simulated run showing
  runners, batches, the queue with its stacks, landings, ejections and
  trunk. `make sim-report` builds the reports locally.
- The simulator examples gain the `github-like` baseline, an approximation
  of GitHub's built-in merge queue (one pull request per run, five in
  flight), and a second example, `busy.toml`, a high-velocity team of five
  arrivals an hour on six runners.
- `dorfq-sim run --json PATH` writes the whole report as one JSON document:
  configuration, each replication's metrics or error, and a structured
  event trace of the first replication per policy. `--trace` prints the
  same events as text.
- The simulator now models stacked pull requests. `p_stacked` sets how
  often an arrival is stacked on a recent, still-queued one and `p_atomic`
  how often such an arrival is `Atomic`; both are required in `[world]`, so
  existing configurations must add them. A base that leaves the queue
  without landing blocks everything stacked on it, counted in a new
  `blocked` metric; a re-push restacks its descendants.
- `dorfq-sim sweep --config FILE [--json PATH] [--jobs N]`, and
  `make sweep ARGS=...` to run it optimised: a sweep file is a run file with
  a `baseline` policy and `[[axis]]` tables, each setting one or more world
  or policy fields (`world.<field>`, `policy.<name>.<field>`, dotted into
  nested tables) to each of its values. Every point of the grid runs every
  policy at every replication on a pool of threads, and the sweep prints one
  row per point and policy with every metric's mean, then each policy's mean
  relative delta against the baseline over the points where the baseline's
  is not 0, with the counts of points where it is better and worse out of
  those where both have the metric. `--json` writes every replication's
  metrics at every point.
- The `cancelled_runs` metric: batch runs stopped by their batch's
  retirement, in the run table, the JSON document and the report site.
- Two shipped sweep files under `crates/dorfq-sim/sweeps/`:
  `speculation.toml` runs the four `bayes` flag combinations (the status-quo
  chain, the chain with `charge = "full"`, the tree, and the tree with
  `landing = "transfer"`) and a `github-like` column over arrival rate, CI
  length, flake rate, `max_batch`, `lanes`, `run_cost`, `w_reject` and
  `w_age`, at an interaction rate of 0.001;
  `bisect-cap.toml` runs `bisect`'s batch cap at two capacities in the busy
  example's world.
- A new speculation strategy for the `bayes` scheduler, the tree of
  futures, chosen with `speculation = "tree"`. Instead of one chain of
  batches plus separate culprit finders, it treats trunk and the tip of
  every batch still in flight as possible future bases, and gives each
  free CI lane the most valuable batch on any of them, weighed by the
  chance that base actually lands. Building on a pending batch speculates
  that it passes; building beside it speculates that it fails. Batches
  that narrow down a failure's culprit compete on the same score, and a
  batch whose base can no longer land is cancelled. The existing
  behaviour stays available as `speculation = "chain"`.

### Changed

- The `bayes` scheduler takes three required flags: `speculation`
  (`"chain"`, one chain of batches with culprit finders on trunk beside it,
  or `"tree"`, below), `charge` (`"discounted"`, the cost of a run weighed
  by the chance its base stands, or `"full"`, the cost paid whole) and
  `landing` (`"decay"`, every other belief regrown by `p_interact` per
  landed pull request, or `"transfer"`, each belief regrown by its pair
  belief with each pull request that reached trunk, including one merged by
  another hand). Existing configurations must add `speculation = "chain"`,
  `charge = "discounted"` and `landing = "decay"` to keep their behaviour.
- **Configuration change, and a known limitation.** The `bayes` scheduler's
  `p_bad` setting is removed, and a configuration that still sets it is
  rejected. Instead of one fixed prior, `bayes` now judges each pull request
  by how stale its own CI run is: a pull request whose CI passed against the
  current trunk is not suspected at all, and each pull request landed since
  then raises its chance of being broken by `p_interact`. Limitation:
  `bayes` does not yet notice trunk changes made outside the queue (a merge
  without the queue label, a direct push), so a pull request broken that
  way is retried forever instead of being ejected. The same happens to the
  mock forge's `bad = true` pull requests at the default `behind = 0`: under
  `bayes`, `crates/dorfq/examples/scenario.toml` restarts `[5]` every tick
  and `--exit-when-idle` never returns. Do not run `bayes` against a real
  forge yet; see `bayes-unobserved-trunk-change` in `TODO.md`.
- The README covers what DorfQ is, whether it suits you, its status and how
  to start; the operator's guide, the simulator guide and the contributor
  notes move to `docs/`. The Pages site gains a home page, and the
  simulation results move to `simulation.html`, without commentary on the
  numbers, comparing only the policies a repository would run, with a
  glossary of the less obvious metrics and how to run your own scenario at
  the top. The GitHub example configuration runs the `bayes` scheduler.
- Upgrading a running GitHub queue: batch records are unchanged and the
  queue picks up where it left off, with one exception. A batch that an
  earlier build published at its own base (every member already in trunk)
  now stops the queue with "batch <id>'s tip is its base"; *While it runs*
  in `docs/running-on-github.md` says how to clear it.
- The contract between scheduler and forge (`dorfq-core`, the interface
  between the part that decides what to test and land and the part that
  talks to GitHub) is rewritten to cover what the first production release
  needs, so both sides can keep growing without breaking each other. It
  can now represent stacked pull requests (a pull request whose base is
  another queued pull request), say why a pull request is waiting or
  conflicting, tell how each one left the queue, name the checks a failed
  batch broke, and run a probe: a batch with no members that tests trunk on
  its own and never lands.
- Stacks: stacked pull requests are batched bases before children, and
  stacks marked to land only whole are kept together.
- Authors are told in a comment when their pull request is waiting and
  what holds it.
- A scheduler that cannot reach a decision now stops dorfq with an error
  instead of crashing.
- CI also runs on pushes to `dorfq/batch/**`, so batches the queue builds get
  the `check` it waits for before landing.
- Breaking: the GitHub forge's credential moves into a `[forge.auth]`
  table, and a configuration with `token_env = "X"` under `[forge]` no
  longer loads. Replace that line with a `[forge.auth]` table holding
  `kind = "token"` and `env = "X"`. A GitHub App is `kind = "app"` with
  `client_id` and `private_key_file`.
- The permission list in `docs/running-on-github.md` gains Checks: read,
  which GitHub's permission reference names for listing a commit's check
  runs.
- Forges now report, for each pull request, how far trunk has moved since
  its last own-CI run (`PrSnapshot::behind`, a new required field and fifth
  argument to `PrSnapshot::new`). The simulator counts landings exactly; the
  mock forge reads a new `behind` field in its scenario, raised by every
  landing and reset by a push; GitHub counts trunk commits the head lacks,
  at one extra API call per queued pull request per observation.
- The GitHub forge lists every open pull request on each tick, not only
  those against trunk, and may change a queued pull request's base to trunk
  once the queued pull request it was based on has merged into trunk. This
  needs no new permission: changing a base is Pull requests: write, which
  the queue already requires.
- A pull request that leaves the queue is told "Left the merge queue (label
  removed, pull request closed, or retargeted)".
- The GitHub forge's error messages now carry GitHub's error details (the
  `errors` of a 422 "Validation Failed"), not only its summary line.
- `make release` no longer builds the optimised `dorfq` binary; that target is
  now `make optimised`. `make release VERSION=X.Y.Z` instead cuts a release.
- The simulator examples run at realistic settings: batch and PR CI of 45 to
  120 minutes in both; the busy world at a flake rate of 0.03 and an
  interaction rate of 0.005. `github-like` runs on sixteen lanes, the
  concurrency GitHub's queue is given in practice, while dorfq's columns
  share one capacity (eight in the busy world, two in the small one). Each
  example has two `bayes` columns: `bayes`, the status quo at its best
  measured settings, and `bayes-tree`, the tree of futures at the same
  settings. The busy world's bayes columns run at `max_batch = 8`; its
  settings come from the shipped sweeps.
- The simulator's shipped examples and sweeps now use an interaction rate of
  `p_interact = 0.001` (was 0.005), and the busy example's published figures
  are re-measured: `bayes` lands 322, `bayes-tree` 325, `bisect` 270 and
  `github-like` 277 (previously 275, 278, 238, 239), with `bayes` ejecting
  10 bad and no good pull requests at 1.0 runner-hours per landing.
- The report page shows each run's parameters, with a comment explaining
  every line, and its replay now draws pull-request CI runs, pull requests
  held for their own CI, and those not yet admitted to the queue.
- The simulator's JSON report moves to schema 2 (adds the configuration
  text and the new trace events); the report page reads schema 2 only.
- The report page's replay now draws one lane per batch lane of the policy
  and grows taller with the lane count, so many-lane runs stay readable.
- The simulator's JSON report moves to schema 3, and the report page reads
  only schema 3: `pr_ci_runs` and `pr_ci_hours` become `billed_pr_ci_runs`
  and `billed_pr_ci_hours`, `utilisation` is removed, the trace's `pr_ci`
  event gains `billed`, and the `run_begin` and `pr_ci_begin` events are
  gone.
- The simulator now models a bad pull request as one that is broken
  against trunk, instead of one drawn bad at arrival and bad forever. Two
  pull requests may conflict: a batch run holding both fails, and whichever
  lands first breaks the other. A pull request stays bad until its author
  pushes a fix, and it is also bad while the pull request it is stacked on
  is bad. Scenario files must drop `p_bad` from `[world]`.
- Replications run until every pull request has left, not to a fixed
  horizon. Scenario files must set the new `stall_hours`: a replication
  that goes that long without a landing or an ejection is cut off and
  reported `stalled`. `drain_hours` remains only as a last-resort cut-off.
- An ejection counts as correct (`ejected_bad`) only when the pull request
  is broken against trunk at that moment. Ejecting one whose conflicting
  partner is still only queued counts as `ejected_good`.
- A pull request's own CI run now fails when it is broken against trunk,
  and its author then pushes a fix.
- New metrics: `ever_bad`; `detect_p50_hours` and `detect_p95_hours`, the
  time from a pull request becoming bad (or arriving bad) to its ejection;
  `ended_hours`, when the replication ended; `cut_off` and `stalled`.
- A pull request that leaves because the one it is stacked on left is now
  counted under that pull request's exit. The `blocked` metric is replaced
  by `left_with_base`.
- A batch run's pass or fail is decided when it starts, for the code it
  was given.
- The JSON report moves to schema 4 and the sweep document to schema 2.
  In the trace, `arrive` carries `conflicts` instead of `truth`, `blocked`
  gains `bad`, `pr_ci_end` gains `failed`, `rebase` gains the reason
  `broken`, and there are new `broken` and `fixed` events.
- The shipped speculation sweep now covers CI times of 45–120 and 90–120
  minutes, `max_batch` of 16 and 30, and a `w_reject` axis.
- The simulator now models a monorepo with effectively unlimited CI
  capacity. There is no runner pool any more: each policy's `lanes`, the
  number of batches the merge queue will run at once, is the only capacity
  limit. Efficiency is measured in runner-hours, latency in time to merge.
  PR CI never takes a lane. It is billed to the queue only when the queue
  causes it: when a pull request's parent lands and its base changes, it
  re-runs PR CI, so splitting a stack costs runner-hours and landing it
  whole does not. A new world switch, `pr_ci_on_base_landed`, controls this.
- Pull requests now arrive as whole stacks, several at once, with sizes
  drawn from a weighted table (`stack_sizes`). A stack may later grow once
  (`p_stack_grows`, `stack_growth_sizes`, `stack_growth_minutes`).
  `p_conflict` is now the rate at which a stack's bottom pull request
  conflicts with trunk.
- Breaking for simulator configurations: `runners` and `p_stacked` are
  removed, and the five new keys above are required. The bundled examples
  are updated, and their `github-like` column now gets as many lanes as the
  policies it is compared against.

### Fixed

- `bayes` now judges each failed run against the trunk it started on, so a
  later landing no longer retroactively undoes an earlier pass, and culprits
  are singled out with fewer repeated runs. Its culprit-finding batches also
  now cover a culprit at the bottom of a stack, and a busy stack no longer
  starves an unrelated failing stack of them. Under `landing = "decay"`,
  merges made outside the queue now count as trunk movement too.
- `bayes` no longer keeps restarting a batch that has already failed twice
  with the same members (such as an `Atomic` stack with a broken member):
  it used to rate such a batch a one-in-four pass and hold a lane with it
  until an author intervened; it now recognises the near-certain failure
  and runs culprit finders instead.
- `bayes` no longer treats a passing batch as proof that a pull request is
  sound when trunk moved after the batch started. Before, a pull request
  broken by one of those later landings could keep failing batch after batch
  while its innocent batch-mates were ejected in its place.
- `dorfq-sim run --json PATH` prints the table even when the document
  cannot be written, then exits 1.
