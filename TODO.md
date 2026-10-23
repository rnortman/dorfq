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
