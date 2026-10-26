# TODO

The master list of deferred work. Each entry has a slug, a description, and the
reason it was deferred; the matching `TODO(slug)` comment in code marks the spot
where the work happens. Slugs are the join key: adding a TODO requires both
halves, and removing one removes both.

A new entry is never written here: it is a fragment, `todo.d/<slug>.md`, which
`make todo-gather` or `make release` folds into `## Open` below. Between
gathers the open list is this file plus `todo.d/`;
`make changes ARGS="todo query"` lists the fragments.

Every entry is public writing.

## Open

### bayes-atomic-unit-resolution

An `Atomic` unit whose runs all fail blames every member equally under
`bayes`: each member's `Bad` cause is exercised by every run of the unit,
so their posteriors stay equal and `singled_out` never fires; only closure
finders of sub-prefixes can separate them, and whether one runs depends on
its score. `bisect` ejects the unit's `Atomic` members on its first own
failure instead. Decide between one cause per unit in the model and a rule
for a unit that fails alone, measured on a broken member of a unit of
seven in the simulator. Model design. The `TODO(bayes-atomic-unit-resolution)`
comment is on `singled_out` in `crates/dorfq-scheduler/src/bayes/policy.rs`.

### bayes-atomic-unit-starvation

The Bayesian scheduler grows a batch from whole units, lowest combined
chance of failing over its members' mean value first; a unit is every pull
request that must land together, so a large `Atomic` stack is among the
riskiest units and is reached late. `w_age` raises a unit's value with the
hours its members have waited, which moves a waiting unit up the order; it
does not make the wait measured, and the criterion below stands. `grow` in
`crates/dorfq-scheduler/src/bayes/policy.rs` skips any unit that no longer fits beside the units added ahead of it: one
of `max_batch` members or more starts only on an empty batch, and a smaller
one only while room is left, and only then has to raise the batch's score.
Under a standing backlog of lower-risk work, not inflow alone, a
multi-member unit can be skipped step after step. Once offered as the
extension it must still clear `speculate_above` and win a free lane against
culprit finders, and the liveness fallback starts it only on an empty chain.
It is not stuck, but how long it waits is unmeasured: the report gives means
over all pull requests, and the drain tests cannot show a wait that exists
only while the backlog stands. The `TODO(bayes-atomic-unit-starvation)`
comment sits where `step` sorts the units by risk. Done means: a measurement
of how long `Atomic` stacks of each size wait to start under `bayes` in
`crates/dorfq-sim/examples/busy.toml`, and, if the wait is material, a rule
that gives a waiting unit its turn (an age term in the unit order, or a
reserved start after some number of skips) sized in that world against the
whole wait, selection in `step` included. Deferred because the rule changes
which batch the extension emits, which is design work for the cycle that
clears `bayes` to run against a real forge, beside
`bayes-posterior-precision`.

### bayes-false-green

The `bayes` model treats a passing run as proof, not evidence. `passed` in
`crates/dorfq-scheduler/src/bayes/model.rs` moves each covered pull
request's cleared position to the run's trunk and sets each covered pair to
`FLOOR`; `blames` then excludes that pull request from every failed run
started at or before that position, and `prune` discards a failure with
nothing left to blame as a flake. A pull request's own CI, read through
`PrSnapshot::behind`, is taken as proof the same way. But CI misses a real
break at some small rate, so a pass should lower the belief that a pull
request or pair is broken, as any other observation does, not zero it. The
simulator has the same blind spot: a run over a broken tree always fails,
so the sweep cannot measure what a false green costs or what modelling one
buys. Closing this closes `bayes-unobserved-trunk-change`; it keeps more
causes alive, so it bears on `bayes-posterior-precision`. Done means: no
passing run, queue or own CI, clears a cause outright in the model; the
simulator's world produces false greens at a configured rate; and a sweep
measures the scheduler against that world.

### bayes-finder-valuation

The `bayes` chain scores a culprit finder as `p·V + (1 − p)·R − run_cost`
with `V` its members' landing value, as though its pass lands: `chain_plan`
in `crates/dorfq-scheduler/src/bayes/chain.rs` scores every finder at
`p_node = 1`, and `score` in `bayes/policy.rs` sums the members' values
into the landing term. Under load a passed finder does not land: `chain_plan`
retires it whenever a chain batch holds trunk, so `p·V` prices a landing
that will not happen, and the score has no term for what the pass actually
does — clear its suspects for the chain and strip them from the failed runs
they share with the culprit. On one 12/hr sweep point traced whole, finders
that passed and were not landed took 35% and 44% of the two chains'
run-hours, up from 21% and 31% before the walk that proposes more of them
(their count doubled); they win lanes and members over chain extensions, so
the median wait rises with every rule that proposes more of them and with
`w_age`, which grows `V`. Over the sweep's 12/hr cells measured — each CI
profile at `w_age` 0 and 0.02, and pooled over `w_age` — that walk's trade
is +15–24% correct ejections and 7–19% faster detection against a median
wait up 3–16% at `w_age = 0` (1–10% at 0.02) and full-chain runner-hours
up 7–9%. This is `bayes-joint-path-pass` seen from the other side: there
the landing value is multiplied by a node factor that treats a path of
pending batches as passing independently, here it is credited to a finder
whose pass does not land; in both the score pays `p_node · p · V` for a
landing that may not happen, and the landing term should be designed once,
not patched twice. Design: what a finder's pass is worth under each
speculation — the value of what it teaches, not the landing it does not
make — and whether the tree, where a finder on a node can land, needs the
same term. Measure on the sweep's 12/hr points against `land_p50`,
`runner_hours`, `detect_p50` and `ejected_bad`.

### bayes-joint-path-pass

The `bayes` scheduler prices a batch on a node — the chain's end, or a tree
node — as the product of its own chance of passing and the chance the node
stands: `p_end` in `crates/dorfq-scheduler/src/bayes/chain.rs`,
`Node::weight` and a node's `p` in `bayes/tree.rs`, and `p_node · p` in
`score` in `bayes/policy.rs`. Each factor is the joint posterior within one
batch, but the product across batches treats them as independent, and
batches whose trees share a failed run are not: `[10, 11]` on a pending
`[8, 9]`, right after `[8, 9, 10, 11]` failed, is priced at about a quarter
where the joint is near zero, so a batch that cannot pass is started and
later cancelled or fails. In the sweep's 12/hr traces such starts cost about
1–2% of lane-hours. The right landing term is the joint over the union of
the path's and the batch's exercised causes, with the flake factor once per
pending run, which `Inference::p_none` can give; the resolution term and a
tree node's sibling terms need their own joint
(`P(node stands ∧ this fails) = P(path) − P(path ∪ this)`, and
inclusion–exclusion over siblings). Design: the score's node factor under
both speculations. Measure on the sweep's 12/hr point.

### bayes-pair-eject-timing

When `bayes` singles out a pair of pull requests while both are still
queued, it ejects the newer at once with `InteractsWith`. Neither is broken
against trunk until the other lands, so the simulator counts that ejection
as `ejected_good`. Land the older first and eject the newer only after, or
keep the two apart until then; the design question is what holds the newer
in the meantime. The `TODO(bayes-pair-eject-timing)` comment is on
`culprit` in `crates/dorfq-scheduler/src/bayes/policy.rs`.

### bayes-pair-search

The Bayesian scheduler's culprit finders search a set of materialised cross
pairs one pair per run, in rank order, so locating the failing pair among
the `n × m` cross pairs of two halves that passed alone costs on the order
of `n · m` runs. Cross-subset finders — one side against half of the other,
recursively — would locate it in about `log2(n · m)` runs. The
`TODO(bayes-pair-search)` comment sits where finders are generated in
`crates/dorfq-scheduler/src/bayes/policy.rs`. Deferred because the search
strategy is design work: which subsets to run, how their verdicts update
the pair beliefs, and how a finder of several pull requests is scored
against the chain.

### bayes-posterior-precision

The Bayesian scheduler's exact inference sums an alternating series over
the subsets of a component's failed runs. When a component is explained
only by causes at low belief — pairs at the interaction prior, members
whose alibi set them to the floor and whose belief later landings lifted
back near that prior — the probability of
all its failures falls toward the rounding error of the terms that cancel
to give it, and the posteriors become noise. That is the shape a CI outage
or a long flaky spell produces, with the same pull requests retried on
culprit finders. Today the guard where the `TODO(bayes-posterior-precision)`
comment sits in `crates/dorfq-scheduler/src/bayes/infer.rs` estimates the
error and, once the estimate reaches 1%, has `step` return a
`SchedulerError`, which stops the process, rather than eject a pull request
on noise; the estimate runs well above the true error, so the stop comes a
run or two before the posteriors are noise. It
is reached two ways. Inferring over the evidence reaches it at a chain of
about nine or more such linked runs at the shipped priors. Scoring a
candidate batch by the posteriors its failure would give reaches it
sooner: the hypothetical run is summed with every component that a cause
the batch would blame belongs to, so one cluster reaches it a run before
the evidence does, and a batch whose members come from separate clusters
is scored over their union, whose runs count as if they were one cluster:
two clusters of four, each resolved comfortably on its own, stop the
process. The chain extension grows from the ready pull requests in
ascending order of belief, which puts members lifted off their alibi by a
landing first, so such a batch is an ordinary candidate. That scoring stop
remains under `speculation = "chain"`, the control, and is gone under
`"tree"`, where a hypothetical failure that cannot be resolved scores with
no resolution credit: the tree scores several times more hypotheticals a
step, over deeper trees, and would stop far more often. The stop on the
evidence remains under both. Until it is
resolved, `bayes` is not to be run against a real forge: a flaky spell
would stop the queue. Two recoveries are candidates: drop the component's
oldest runs until the sum is significant, or fall back to exact
enumeration over the component's causes when there are few of them.
Deferred because either changes what the posteriors are in a regime no
simulated world has reached, so choosing and sizing one is model design
with a world that exercises it. Scoring has a third, exact remedy for the
union: components share no cause, so the hypothetical run's failure can be
scored against each component separately and the results combined, which
leaves the posteriors unchanged and moves the scoring stop back to where a
single cluster puts it. It resolves neither the single-cluster case nor
inference over the evidence, and it changes when the queue stops, so it is
chosen together with the recovery above.

### bayes-unobserved-trunk-change

The `bayes` scheduler cannot blame a pull request its forge reports cleared
against the current trunk. `Model::sync` in
`crates/dorfq-scheduler/src/bayes/model.rs` reads `PrSnapshot::behind` when
it first sees a head, and after that only through a `max` that moves the
cleared position forward, so a rise in `behind` on an unchanged head is
dropped; and the trunk position (`ingest` in `bayes/policy.rs`) counts only
the departures the scheduler observes. A trunk change the queue did not
make — on GitHub, a merge without the queue label, an admin or bot push —
therefore never puts a queued pull request's cleared position behind the
trunk, and a pull request first seen at `behind = 0` that such a change
breaks is never a cause: every failed run holding it has nothing else to
blame and is pruned as a flake, it stays `Ready` and is batched again every
step, it fails its batch-mates, and it leaves only when a materialised pair
with one of them is singled out, which may eject the innocent one. The same
loop follows from own CI weaker than the queue's CI, which the model treats
as equal: the mock forge's `bad = true` at its default `behind = 0`
(`crates/dorfq-forge-mock`) is the fixture form of it, and under `bayes`
the shipped `crates/dorfq/examples/scenario.toml` never ejects pull request
5. The simulator cannot produce either case — there every landing goes
through the queue and a pull request goes bad only through a landing its
own CI did not cover — so the sweeps and examples are unaffected. This is
one of the reasons `bayes` is not yet cleared to run against a real forge,
beside `bayes-atomic-unit-starvation` and `bayes-posterior-precision`.
Design: how the scheduler learns of trunk changes it did not observe — a
trunk position the forge counts, in the contract, or a rise in `behind` on
an unchanged head read as that many changes — and whether a passing own-CI
run is trusted as the queue's CI or leaves a residual chance that the
queue's run fails. Done means: a `bayes` test on the GitHub fake in which a
pull request labelled at `behind = 0` is broken by a direct push to trunk
and is ejected for its own failure — `EjectReason::BatchFailed`, the
scheduler blaming it as the batch's culprit — and not as the newer half of
a pair with a batch-mate, which today's code already reaches after a few
shared failures and which ejects whichever of the two has the higher
number; and the mock's `bad` pull request at `behind = 0` either ejected
the same way under `bayes` in a driver test or documented in
`crates/dorfq-forge-mock` as a fixture `bayes` reads as a flake, by the
design's choice.

### chain-collapse-under-runner-queue

Both pipelining schedulers bound their chain's depth by `lanes`, counting
every batch `is_pending` in `crates/dorfq-scheduler/src/lib.rs` accepts, and
the contract does not say whether a pending batch's run has begun or is still
waiting for a runner. When the runners are saturated a started batch waits
hours for a runner. `bayes` retires any batch whose member's head changed, so
in that time a repush, withdrawal or blocking under any batch of the chain
dooms the chain above it, and the rebuilt chain joins the back of the runner
queue; `bisect` shares the bound and the collapse above a retired batch, but
finds a changed member out only at landing, and loses less. The simulator's
world no longer has a runner queue, so the condition is reached only on a
forge whose CI queues. The `TODO(chain-collapse-under-runner-queue)` comment
sits on `is_pending`. Done means either a contract field saying whether a
batch's run has begun (GitHub reports a check run `queued` or `in_progress`)
with a rule that does not extend a chain whose end has not begun running, or
a rule that bounds waiting batches without it. Deferred because the contract
change and the rule's effect on each scheduler's chain are design work.

### dependency-audit-lane

A license and advisory audit of the Rust dependency graph (for example
`cargo deny`) belongs in `make check` before the first crate is published.
Running it through Bazel needs a decision first: the advisory check fetches
the RustSec database over the network, which a sandboxed test action must not
do, and the tool shells out to `cargo metadata`, so it needs the pinned
toolchain's cargo handed to it. The options are a repository rule that
fetches the advisory database at a pinned commit so the check is a sandboxed
test; an unsandboxed `bazel run` lane like the scrub gate; or a licenses-and-
bans test now with advisories later. The `TODO(dependency-audit-lane)` comment
sits above `check` in the `Makefile`.

### efficiency-conflict-recovery-reads-for-unreachable-leftovers

When `observe` reads a batch record in the `conflict` state, the GitHub
forge in `crates/dorfq-forge-github/src/lib.rs` looks for the batch's
build and probe branches and deletes any it finds: two `get_ref` calls per
conflict batch per observation. Its own `start_batch` cannot leave either
branch behind a `conflict` record, since both are deleted before the
record says `conflict` and any failure before that leaves the record
`started`, which is cleaned up whole. So the branch is only ever there if
someone outside the queue created it, and the queue's rule for foreign
refs in its namespace is to stop with `Permanent`; this row deletes them
silently instead. Either drop the lookup or make a present build or probe
branch beside a `conflict` record `Permanent`, as a present batch branch
already is. Deferred because the recovery behaviour for this row is a
fixed part of the forge's design and changing it is a design decision.
The `TODO(efficiency-conflict-recovery-reads-for-unreachable-leftovers)`
comment sits above the lookup.

### errhandling-write-applied-but-reported-transient-tells-authors-wrong-thing

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
the latest observation. The second adds reads to every tick. Both are open
design choices, and the webhook-driven forge may change the answer. The
`TODO(errhandling-write-applied-but-reported-transient-tells-authors-wrong-thing)`
comments sit above the label removal in `eject` and above the `testing`
record write in `start_batch`.

### github-behind-counts-commits

The GitHub forge reports `PrSnapshot::behind` in commits (`compare`'s
`ahead_by`, in `fetch_world` in `crates/dorfq-forge-github/src/lib.rs`), but
the contract defines it in pull requests reaching trunk, and `bayes` reads
it as landings (`Model::sync` in `crates/dorfq-scheduler/src/bayes/model.rs`
subtracts it from a position counted in pull requests). The queue lands one
merge commit per member on trunk, so `behind` counts merge commits. Done
means: on the GitHub fake, a pull request of several commits, one queue
landing behind, is reported at `behind = 1`.

### github-behind-every-tick

The queue lands every batch itself, so it knows what merged and when
without asking GitHub, yet the GitHub forge re-reads from the API what that
record already determines: `fetch_world` in
`crates/dorfq-forge-github/src/lib.rs` calls `compare` for every enqueued
pull request on every observation to compute `PrSnapshot::behind`. Restructure
the forge, and the contract and scheduler where they need it, around what
the queue already knows: beliefs and positions move on the queue's own
landings wherever it has them, and GitHub is asked only for what the queue
cannot know — chiefly, when a new own-CI result arrives for a pull request,
which trunk that run tested (`github-behind-tested-trunk`). This cuts API use
and may change more than the one call. Done means: no observation makes an
API call for something the queue's own landing record determines.

### github-behind-tested-trunk

The GitHub forge measures `PrSnapshot::behind` from the head's merge base
with trunk (`fetch_world` in `crates/dorfq-forge-github/src/lib.rs`), which
is the trunk own CI tested only when CI tests the head as pushed. CI that
tests the head merged with trunk tested a later trunk, and which one
depends on the CI system and how it is set up. Add a configuration setting
that selects how the forge estimates the trunk a pull request's own CI ran
against, with several strategies to choose from — the merge base, and trunk
as of when the run started (a check run's `started_at`, or when a commit
status was first posted pending) among them. An estimate may be imprecise;
the model is Bayesian and absorbs that. One known imprecision: re-running
a GitHub Actions workflow tests the original event's commit, not trunk as
of the re-run. Done means: the setting exists, each strategy is tested on
the GitHub fake, and `behind` counts from the trunk the selected strategy
names.

### github-retire-cancels-run

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

### github-unlanded-moves-lost-on-crash

The GitHub forge moves each stacked member of a batch onto trunk just before
it fast-forwards trunk, because GitHub refuses the move once trunk contains
the member's head. When the batch then does not land, `fast_forward` in
`crates/dorfq-forge-github/src/lib.rs` puts each moved member back on its
base branch. A process that dies after a move and before the fast-forward
cannot do that: the base branches it moved members off live only in memory,
so the restarted queue reads those members as targeting trunk. If their base
pull requests then do not land with them, nothing stops the queue from
landing a child alone, which lands its base's commits with it. Done means:
the `landing` state of the batch record also names each stacked member's base
branch, and recovering a `landing` record whose tip is not in trunk moves each
such member still open on trunk back onto that branch before the observation
is reported, with a test that restarts the forge between the moves and the
fast-forward. The `TODO(github-unlanded-moves-lost-on-crash)` comment sits
above the moves. Deferred because it changes the batch record's grammar,
which the queue's recovery depends on, to cover a window of a few API calls.
Whether that is worth doing is for the owner and a design cycle to decide.

### quality-pages-workflow-copies-ci-bazel-setup

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

### quality-trunk-position-counter-held-twice

The `bayes` scheduler keeps its trunk position twice. `Bayes::position`
accumulates the landings `ingest` sees (`crates/dorfq-scheduler/src/bayes/policy.rs`)
and is handed to `Model::sync`, which stores its own copy; inference reads the
model's copy, while `ingest` computes each batch's covered position from the
scheduler's. Nothing outside the tests sets them apart today, but every test
that calls `sync` with its own position leaves the two disagreeing. Keep one:
`Model::sync` takes the number of pull requests that reached trunk and
advances its own position, `ingest` reads `self.model.position()`, and the
tests pass a landed count instead of an absolute position. Behaviour does not
change: the sweep's and the examples' JSON output is byte-identical before
and after.

### quality-untested-parallel-map-to-trees

`Bayes` in `crates/dorfq-scheduler/src/bayes/mod.rs` keeps `untested`, each
tracked batch's count of landings its run did not test, as a second map over
exactly the batches in `trees`. `ingest` in `bayes/policy.rs` inserts and
retains it beside `trees`, and reads it with `get_mut(..).expect` and an
index that are safe only because `trees` was filled first. Make the `trees`
value one struct holding a batch's tree members and its untested count, so
one insert and one retain cover both and the `expect` and the index go.
`finders` and `read` are subsets of the tracked batches, not data held for
each, and stay sets. Behaviour does not change: the sweep's and the
examples' JSON output is byte-identical before and after.

### security-label-grants-unreviewed-landing-on-any-later-head

The GitHub forge in `crates/dorfq-forge-github/src/lib.rs` enqueues an open
pull request because it carries the intent label. It never asks who applied
the label or which head they saw. GitHub keeps a label across new pushes, so
a pull request labelled at one head and pushed to again is built, tested and
landed at the new head. That head has not been reviewed, and the queue lands
it by a direct fast-forward that bypasses branch protection's review rules.
Anyone with the Triage role can also apply the label, although that role
cannot merge. Ways to close the gap:
- treat a head change on a labelled pull request as leaving the queue;
- require the labeller to hold write permission;
- require an approved review decision on the current head.
Each adds API calls. Admission is the forge's: a pull request it would not
land is reported `Waiting`, and `Hold::Review` is the slot for a missing
review. Which policy production needs is a design decision. The
`TODO(security-label-grants-unreviewed-landing-on-any-later-head)` comment
sits above the label check in `fetch_world`.

### soft-eject

A second, lower belief threshold at which the scheduler tells an author
that their pull request is probably broken against trunk, without ejecting
it. It needs a contract item beside `Plan.eject` (rule 9 lets the scheduler
send authors nothing else), emitted once per (pull request, head), and a
`Forge` method that the GitHub forge relays as a comment and the mock
records. In the simulator, a told author starts a fix (as a failed own-CI
run already makes them do) with a configured probability, and the report
adds the time from `bad_since` to the first notice and a count of notices
sent for pull requests that were not bad. Deferred because it is a
contract change and a forge behaviour, to be designed as one cycle on top
of the world that models breakage against trunk. The `TODO(soft-eject)`
comments sit at `BayesConfig::eject_above` in
`crates/dorfq-scheduler/src/config.rs` and where the simulated world starts
an author's fix, `begin_fix` in `crates/dorfq-sim/src/world.rs`.

### test-site-app-js-untested-against-report-schema

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

### tree-starves-resolution

Under `speculation = "tree"` a suspect batch almost never starts while
fresh pull requests are ready, so bad pull requests sit queued until their
authors withdraw them. Every candidate on every node competes on one score
and a lane takes the maximum; a suspect singleton's value is
`p + (1 − p)·w_reject = 1` at `w_reject = 1`, which nets at most
`1 − run_cost` on an empty node and about `0.08` on trunk beside a pending
batch of sixteen, while an extension of fresh pull requests on the same
node nets several. The risk-sorted unit order and `grow`'s stop at the
first unit that does not raise the score keep a high-`Bad` suspect out of
every extension too. The chain only runs its finders because the
one-extension-per-step rule reserves the remaining lanes for them. In the
speculation sweep (`crates/dorfq-sim/sweeps/speculation.toml`, four
replications) the tree ejected about 80% fewer bad pull requests than the
chain at every point and left about three times as many pull requests
unfinished at nearly every point, landing more only under overload; those
ratios were measured before the simulator modelled breakage against
trunk, when `ejected_bad` also counted a pull request whose conflicting
partner was merely queued, and the replications were cut off at a fixed
drain. A pull request left queued costs the objective nothing. Two things to
price, each as a flag measured by that sweep: `w_age`, a value per hour
waited, shipped; the sweep has it as an axis and at `[0.02, 0.1]` the
criterion was not met; and `w_reject`, which prices
a resolution at one landing against a fresh batch's several. Done means a
flag under which the tree is better than the chain on `ejected_bad` and
on `ended_hours` at more than half of the compared points, as that sweep's
summary prints them: a replication now runs until its queue empties, so
the time it takes to empty is what `unfinished` used to measure. The `TODO(tree-starves-resolution)` comment is on
`candidates` in `crates/dorfq-scheduler/src/bayes/tree.rs`.

### trunk-broken-probe

Neither `Bisect` nor `Bayes` notices that trunk itself is broken. A flaky
pass can land a bad pull request; after it, every batch built on trunk
fails, `Bisect` ejects each singleton in turn and `Bayes` singles out each
finder's suspect, so innocent pull requests are ejected one after another.
A probe on trunk (a `BatchStart` with no members, which the contract allows)
after some number of consecutive own-failures would tell. When to probe,
what to hold while trunk is broken, how to tell the operator and when to
resume on a passing probe are design work not yet done. The
`TODO(trunk-broken-probe)` comments sit at `Bisect`'s singleton ejection in
`crates/dorfq-scheduler/src/bisect.rs` and at `Bayes`'s finder generation in
`crates/dorfq-scheduler/src/bayes/policy.rs`.
