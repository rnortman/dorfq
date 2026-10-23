# TODO

The master list of deferred work. Each entry has a slug, a description, and the
reason it was deferred; the matching `TODO(slug)` comment in code marks the spot
where the work happens. Slugs are the join key: adding a TODO requires both
halves, and removing one removes both.

Every entry is public writing.

## Open

### quality-check-land-returns-status-callers-re-match

`Ledger::check_land` in `crates/dorfq-core/src/ledger.rs` matches
`Testing { ci: Passed, .. }` and then returns the whole `BatchStatus`, so
each forge's `land` matches the state again to read the tip and carries an
`unreachable!` for the other arms (the mock's `land` in
`crates/dorfq-forge-mock/src/lib.rs` does today; the GitHub forge's `land`
will need the same). Returning what the check proved — the tip alongside
the status, as a pair or a small struct — removes the panic branch from
every forge. Deferred because the shape of `check_land`'s return is a
fixed interface of the forge work in progress and changing it is a design
decision. The `TODO(quality-check-land-returns-status-callers-re-match)`
comment sits above `check_land`.

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
Each adds API calls and puts review policy into the forge. Which one
production needs, and whether the forge or the scheduler should own it, is a
design decision. The
`TODO(security-label-grants-unreviewed-landing-on-any-later-head)` comment
sits above the label check in `fetch_world`.
### eject-reason-display

`EjectReason` has no rendering of its own in `dorfq-core`, so three crates
spell it by hand: the driver's plan log (`crates/dorfq/src/driver.rs`), the
mock's author messages (`crates/dorfq-forge-mock/src/lib.rs`) and the
simulator's trace, where the `TODO(eject-reason-display)` comment sits. The
first two use the same words independently; the trace needs a form without
spaces. Give `EjectReason` a `Display` (and a token form if the trace keeps
one) and have all three call it, so a new variant is worded once. Deferred
because it changes `dorfq-core`, which is a coordinated contract change
landed on `main`, and which form each audience gets is a decision for that
change.

### eject-reason-interacts-with

When the Bayesian scheduler singles out a pair of pull requests that fail
only together, it ejects the newer one with `EjectReason::BatchFailed`
naming the run that singled the pair out. The honest reason is "fails
together with #N": the author hears less than the scheduler knows. Add
`EjectReason::InteractsWith { pr, batch }` to `dorfq-core`, render it in the
mock forge's author messages and the driver's plan log, and have the
scheduler emit it where the `TODO(eject-reason-interacts-with)` comment sits
in `crates/dorfq-scheduler/src/bayes/policy.rs`. Deferred because it is a
contract change, landed on `main` with both sides of the boundary.

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
error and stops the process once the estimate reaches 1%, rather than
eject a pull request on noise; the estimate runs well above the true
error, so the stop comes a run or two before the posteriors are noise. It
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
landing first, so such a batch is an ordinary candidate. Until it is
resolved, `bayes` is not to be run against a real forge: a flaky spell
would stop the queue. Two recoveries are candidates: drop the component's
oldest runs until the sum is significant, or fall back to exact
enumeration over the component's causes when there are few of them.
Deferred because either changes what the posteriors are in a regime no
simulated world has reached, so choosing and sizing one is model design
with a world that exercises it; and because the scheduler contract gives
`step` no way to report that it cannot compute, so the alternative to a
stop is a contract change. Scoring has a third, exact remedy for the
union: components share no cause, so the hypothetical run's failure can be
scored against each component separately and the results combined, which
leaves the posteriors unchanged and moves the scoring stop back to where a
single cluster puts it. It resolves neither the single-cluster case nor
inference over the evidence, and it changes when the queue stops, so it is
chosen together with the recovery above.
