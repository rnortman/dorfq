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
changed, so in that time a repush, withdrawal or blocking under any batch of
the chain dooms the chain above it, and the rebuilt chain joins the back of
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

## `errhandling-write-applied-but-reported-transient-tells-authors-wrong-thing`

A GitHub write can be applied while the client sees a failure (a timeout, a
`5xx` or a reset connection after the request was processed), which the
client reports as `Transient`. The GitHub forge in
`crates/dorfq-forge-github/src/lib.rs` treats such a failure as "nothing
happened", and two of its messages to authors then go wrong. An `eject`
whose label removal was applied leaves the pull request unlabelled and still
in `known`, so the next observation tells the author they withdrew, and the
scheduler's reason and next step are never sent. A `start_batch` whose final
`testing` record write was applied is reported `Testing` by the next
observation, but no member is ever told it is being tested. Closing this
needs either memory of what the forge emitted, kept in the process, or a
read of each member's comment thread to reconcile it, carried across
restarts. The first breaks the forge's property of holding no state beyond
the latest observation. The second adds reads to every tick. A third is
bounded: every comment the forge posts, a `notice` included, begins with a
`<!-- dorfq: <event> -->` marker, and the forge already reads a pull
request's thread to decide whether it is entering the queue
(`crates/dorfq-forge-github/src/messages.rs`), so reconciliation can run
only when something is unexplained — a departure the ledger cannot account
for reads the thread's last marker, and `ejected` means the author was told
and the departure is reported `Ejected`; a `testing` batch the ledger did
not start sends `Testing` to every member whose last marker is not
`testing`. For the first case it needs `eject` to post its comment before it
removes the label, where today the comment is queued after the removal, so
that a removal applied with its response lost leaves the marker to find.
Which of the three to take is the design, and the webhook-driven forge may
change the answer. The
`TODO(errhandling-write-applied-but-reported-transient-tells-authors-wrong-thing)`
comments sit above the label removal in `eject` and above the `testing`
record write in `start_batch`.

## `github-retire-cancels-run`

`GitHubForge::retire` deletes a batch's record and branch and never cancels
the CI run on its tip, so a retired pending batch keeps a runner busy to the
end. The contract's rule 7 makes `retire` of a pending batch its
cancellation where the platform allows, and GitHub allows it: list the runs
on the tip (`GET /repos/{owner}/{repo}/actions/runs?head_sha=<tip>`) and
cancel each in progress (`POST /repos/{owner}/{repo}/actions/runs/{id}/cancel`),
which needs the Actions: write permission. The `bayes` scheduler under
`speculation = "tree"` retires pending batches as a matter of course, so on
GitHub it pays for every cancelled batch's full run until this is done.
Done means: `retire` of a `Testing { Pending }` batch cancels every
in-progress run on its tip before deleting the refs, the fake models runs
and their cancellation, the permission lists in docs/running-on-github.md
gain Actions: write under both credentials, and `make live-test` exercises it once. Deferred
because it is forge design: a permission every installation must add, a
fake that models runs, and what to do when a cancel is refused. The
`TODO(github-retire-cancels-run)` comment sits in `GitHubForge::retire` in
`crates/dorfq-forge-github/src/lib.rs`.

## `github-unlanded-moves-lost-on-crash`

The GitHub forge moves each stacked member of a batch onto trunk just before
it fast-forwards trunk, because GitHub refuses the move once trunk contains
the member's head. When the batch then does not land, `fast_forward` in
`crates/dorfq-forge-github/src/lib.rs` puts each moved member back on its
base branch. A process that dies after a move and before the fast-forward
cannot do that: the base branches it moved members off live only in memory,
so the restarted queue reads those members as targeting trunk. If their base
pull requests then do not land with them, nothing stops the queue from
landing a child alone, which lands its base's commits with it. Done means:
the `landing` state of the batch record also names each stacked member's
base branch, and recovering a `landing` record whose tip is not in trunk
moves each such member still open on trunk back onto that branch before the
observation is reported, with a test that restarts the forge between the
moves and the fast-forward. The `TODO(github-unlanded-moves-lost-on-crash)`
comment sits above the moves. The same state is reached without a crash:
when the fast-forward's error leaves it unknown whether trunk moved and the
trunk read that would tell fails too, `fast_forward` leaves the moved
members on trunk with a line on stderr. The grammar change is accepted; the
one compatibility requirement is that a restarted queue reads a `landing`
record written before the change without stopping. Deferred because the
window is a few API calls and nothing at today's load has reached it.

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

## `security-label-grants-unreviewed-landing-on-any-later-head`

The GitHub forge in `crates/dorfq-forge-github/src/lib.rs` enqueues an open
pull request because it carries the intent label. It never asks which head
the labeller saw or whether that head was reviewed: GitHub keeps a label
across new pushes, so a pull request labelled at one head and pushed to
again is built, tested and landed at the new head, by a direct fast-forward
that bypasses branch protection's review rules; anyone with the Triage role
can apply the label, although that role cannot merge.

The standard this project sets: a private repository where everyone who can
review or label is a trusted, logged employee — the queue guards against
mistakes, not malice — and dorfq does as well as GitHub's own merge queue on
policy while doing better on batching. So everyone who can review, or add
and remove the label, is trusted to decide pool admission; whether a new
head needs a new review is the repository's GitHub configuration, not
dorfq's; and pool membership is not tied to a head — the label puts a pull
request in the pool until it lands or is ejected, and a new head makes it
ineligible until it passes the gate again rather than taking it out of the
pool.

A candidate shape for the design to start from, not yet checked against
GitHub's API: eligibility follows GitHub's own verdict plus the configured
checks — a pull request is eligible for a batch when `mergeable_state`,
carried by the `GET /pulls/{n}` the forge already makes every tick (it reads
only `mergeable` from it today), says branch protection is satisfied as the
repository configured it, and the pull-request checks pass on its head; a
second required-checks list for batch commits, defaulting to the
pull-request one, where today one `checks` list serves both; and a member's
branch locked while it is batched, so a push is refused and an author who
wants to push removes the label, which cancels the batch. For the design to
check, not this entry to decide: the exact semantics of `mergeable_state`
(`behind` has to count as eligible, since the queue tests against the latest
trunk; and what it reads as under a credential on the bypass list); how
GitHub's queue treats stacked pull requests, which is the answer for dorfq
too; whether a lock exists at all for the queue's credential, and fork
branches, which live where the queue cannot lock; and which list governs
when branch protection's own required checks differ from the configured one.
The `TODO(security-label-grants-unreviewed-landing-on-any-later-head)`
comment sits above the label check in `enqueued_bases`.

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
