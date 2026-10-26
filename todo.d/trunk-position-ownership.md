How far trunk has moved is one number held by two parties in two units. The
scheduler counts the departures it observes (`ingest` in
`crates/dorfq-scheduler/src/bayes/policy.rs`) and the model keeps a second
copy that `Model::sync` sets from it
(`crates/dorfq-scheduler/src/bayes/model.rs`); nothing outside the tests
sets the two apart, but every test that calls `sync` with its own position
leaves them disagreeing. The GitHub forge reports `PrSnapshot::behind` by
one `compare` per enqueued pull request per observation (`fetch_world` in
`crates/dorfq-forge-github/src/lib.rs`), re-reading from the API what the
queue's own landing record already determines; in commits — the queue lands
one merge commit per member, so one queue landing of one pull request of
several commits reads as more than 1 — where the contract, the simulator and
the mock forge count pull requests; and always from the head's merge base
with trunk, which is the trunk own CI tested only when CI tests the head as
pushed rather than merged with trunk (a check run's `started_at`, already
fetched by `checks` and discarded, would name trunk as of the run; a re-run
of a GitHub Actions workflow tests the original event's commit, not trunk as
of the re-run). The contract already says `behind` grows with every change
to trunk whether or not the queue made it and is the scheduler's only fact
about the changes it did not make, but `Model::sync` reads it when it first
sees a head and afterwards only through a `max` that moves the cleared
position forward, so a rise in `behind` on an unchanged head is dropped. A
trunk change the queue did not make — on GitHub, a merge without the queue
label, an admin or bot push — therefore never puts a queued pull request's
cleared position behind the trunk, and a pull request first seen at
`behind = 0` that such a change breaks is never a cause: every failed run
holding it has nothing else to blame and is pruned as a flake, it stays
`Ready` and is batched again every step, it fails its batch-mates, and it
leaves only when a materialised pair with one of them is singled out, which
may eject the innocent one. The simulator cannot produce this — there every
landing goes through the queue — so the sweeps and examples are unaffected.
One shape that would close all of it, not yet checked against the code in
detail: the trunk position becomes a fact the forge reports in the
observation — pull requests that reached trunk by any hand, as the forge
best counts them, the GitHub forge counting its own landings exactly and
asking GitHub only when trunk moved to a sha it did not land — and `behind`
is derived from it and from the position own CI started against, read once
per new own-CI result with the merge base as the fallback the contract
already allows; the scheduler and the model then keep no position of their
own, and a foreign push regrows every queued pull request by the model's
rule. That is a required contract change: one pull request in `dorfq-core`
and one site per consumer (the GitHub forge, the fake, the mock, the
simulator's world, test support). Whether a passing own-CI run is trusted as
the queue's CI is a separate question, under `bayes-false-green`. Done
means: on the GitHub fake, a pull request of several commits one queue
landing behind is reported at `behind = 1`; no observation makes an API call
for something the queue's own landing record determines; on the GitHub fake,
a pull request whose own CI tested its head merged with trunk is reported at
a `behind` counted from the trunk that run tested rather than from the merge
base, by whichever estimate the design settles on; a `bayes` test on the
GitHub fake in which a pull request labelled at `behind = 0` is broken by a
direct push to trunk and is ejected for its own failure —
`EjectReason::BatchFailed`, the scheduler blaming it as the batch's culprit
— and not as the newer half of a pair with a batch-mate, which today's code
reaches after a few shared failures and which ejects whichever of the two
has the higher number; and one trunk position in the scheduler, which on its
own leaves the sweep's and the examples' JSON output byte-identical. The
`TODO(trunk-position-ownership)` comments sit at the `compare` call in
`fetch_world`, at the `behind` read in `Model::sync`, and where `ingest`
advances the position.
