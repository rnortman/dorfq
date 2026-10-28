# TODO

The master list of deferred work. Each entry has a slug, a description, and the
reason it was deferred; the matching `TODO(slug)` comment in code marks the spot
where the work happens. Slugs are the join key: adding a TODO requires both
halves, and removing one removes both.

A new entry is never written here: it is a fragment, `todo.d/<slug>.md`, which
`make todo-gather` or `make release` folds in below as a `` ## `<slug>` ``
entry. Between gathers the open list is this file plus `todo.d/`;
`make changes ARGS="todo query"` lists the fragments.

Every entry is public writing.

## `bayes-atomic-unit-resolution`

An `Atomic` unit whose runs all fail blames every member equally under
`bayes`: each member's `Bad` cause is exercised by every run of the unit,
so their posteriors stay equal and `singled_out` never fires; only closure
finders of sub-prefixes can separate them, and whether one runs depends on
its score. `bisect` ejects the unit's `Atomic` members on its first own
failure instead. Decide between one cause per unit in the model and a rule
for a unit that fails alone, measured on a broken member of a unit of
seven in the simulator. Model design. The `TODO(bayes-atomic-unit-resolution)`
comment is on `singled_out` in `crates/dorfq-scheduler/src/bayes/policy.rs`.

## `bayes-atomic-unit-starvation`

The Bayesian scheduler grows a batch from whole units, lowest combined
chance of failing over its members' mean value first; a unit is every pull
request that must land together, so a large `Atomic` stack is among the
riskiest units and is reached late. `w_age` raises a unit's value with the
hours its members have waited, which moves a waiting unit up the order; it
does not make the wait measured, and the criterion below stands. `grow` in
`crates/dorfq-scheduler/src/bayes/policy.rs` skips any unit that no longer
fits beside the units added ahead of it: one of `max_batch` members or more
starts only on an empty batch, and a smaller one only while room is left,
and only then has to raise the batch's score. Under a standing backlog of
lower-risk work, not inflow alone, a multi-member unit can be skipped step
after step. Once offered as the extension it must still clear
`speculate_above` and win a free lane against culprit finders, and the
liveness fallback starts it only on an empty chain. It is not stuck, but how
long it waits is unmeasured: the report gives means over all pull requests,
and the drain tests cannot show a wait that exists only while the backlog
stands. The `TODO(bayes-atomic-unit-starvation)` comment sits where `step`
sorts the units by risk. Done means: a measurement of how long `Atomic`
stacks of each size wait to start under `bayes` in
`crates/dorfq-sim/examples/busy.toml`, and, if the wait is material, a rule
that gives a waiting unit its turn (an age term in the unit order, or a
reserved start after some number of skips) sized in that world against the
whole wait, selection in `step` included. Deferred because the rule changes
which batch the extension emits, which is design work.

## `bayes-pair-search`

The `bayes` scheduler's suspect batches search a set of materialised cross
pairs one pair per run, in rank order — under `speculation = "chain"` as
culprit finders on trunk, under `"tree"` as the same suspects on each node —
so locating the failing pair among the `n × m` cross pairs of two halves
that passed alone costs on the order of `n · m` runs. Cross-subset suspects
— one side against half of the other, recursively — would locate it in about
`log2(n · m)` runs. It matters only where cross pairs are common. The
`TODO(bayes-pair-search)` comment sits where suspects are generated in
`crates/dorfq-scheduler/src/bayes/policy.rs`. Deferred because the search
strategy is design work: which subsets to run, how their verdicts update the
pair beliefs, and how a suspect batch of several pull requests is scored
against the extensions.

## `chain-collapse-under-runner-queue`

Both pipelining schedulers bound their chain's depth by `lanes`, counting
every batch `is_pending` in `crates/dorfq-scheduler/src/lib.rs` accepts, and
the contract does not say whether a pending batch's run has begun or is
still waiting for a runner. When the runners are saturated a started batch
waits hours for a runner. `bayes` retires any batch whose member's head
changed, so in that time a withdrawal, blocking, or a push to a member's
head (with `lock_heads` on, the GitHub forge locks every batched head branch
in the repository itself, so only a fork head can be pushed) under any batch
of the chain dooms the chain above it, and the rebuilt chain joins the back of
the runner queue; `bisect` shares the bound and the collapse above a retired
batch, but finds a changed member out only at landing, and loses less. The
simulator's world no longer has a runner queue, so the condition is reached
only on a forge whose CI queues. The fact is already fetched: `checks` in
`crates/dorfq-forge-github/src/api.rs` reads each check run's `status` and
folds `queued` and `in_progress` alike into `Pending`. The
`TODO(chain-collapse-under-runner-queue)` comment sits on `is_pending`. Done
means either a contract field saying whether a batch's run has begun (GitHub
reports a check run `queued` or `in_progress`) with a rule that does not
extend a chain whose end has not begun running, or a rule that bounds
waiting batches without it. Deferred because the contract change and the
rule's effect on each scheduler's chain are design work.

## `dependency-audit-lane`

A license and advisory audit of the Rust dependency graph (for example
`cargo deny`), fetched through Bazel like every other tool, run by a
separate nightly CI job: not in `make check` and not on pull requests,
because an advisory or a yank published an hour ago must never block a
merge. The advisory check fetches the RustSec database over the network,
which the nightly job's own invocation may do and a sandboxed test action
must not; the tool shells out to `cargo metadata`, so it needs the pinned
toolchain's cargo handed to it. Done means: a make target that runs the
audit through Bazel, a workflow that runs it nightly and on demand, and
nothing new in `check`. The `TODO(dependency-audit-lane)` comment sits above
`check` in the `Makefile`, beside where that target goes.

## `quality-pages-workflow-copies-ci-bazel-setup`

Share the Bazel setup between `.github/workflows/ci.yml` and
`.github/workflows/pages.yml`. Both restore the repository and disk caches
under the same `hashFiles(...)` keys and write the same `.bazelrc.user`
lines, copied by hand so that Pages is served from what CI built. A cache
key or Bazel flag changed in one file and not the other fails silently: the
Pages job restores cold by prefix or writes entries CI never reads. Move
both cache steps and the `.bazelrc.user` lines into a local composite
action (for example `.github/actions/bazel-ci/action.yml`, with an input for
CI's extra `test --keep_going`) and call it from both workflows. This
rewrites the gate's own workflow, so it waits for a moment when no other
branch is editing `ci.yml`.
The `TODO(quality-pages-workflow-copies-ci-bazel-setup)` comment sits above the cache steps in `.github/workflows/pages.yml`.

## `test-site-app-js-untested-against-report-schema`

Test the report page against the documents it reads. Nothing in the gate
checks that `site/app.js` parses, or that the fields it reads from a report
(`schema`, `config`, each policy's `replications` and `trace`, every event
kind's fields, every metric key in `ROWS`) exist in what `dorfq-sim run
--json` writes, so a rename in `crates/dorfq-sim/src/report.rs`,
`metrics.rs` or `trace.rs`, or a syntax error in `app.js`, passes
`make check` and breaks the published page; a metric added to `Metrics` gets
no table row and no glossary definition. The test loads `app.js`'s
data-handling functions in a JavaScript runtime against
`//site:report_sim`'s output and asserts that the policy list and the replay
fold run without error and that `ROWS` names exactly the metric keys of
each policy's replications. It needs a JavaScript runtime pinned in
`MODULE.bazel`, which is a toolchain decision for the owner: worth taking
once the page grows past its first version or becomes a production replay
tool.
The `TODO(test-site-app-js-untested-against-report-schema)` comment sits in `site/BUILD.bazel`, above the report rules.
