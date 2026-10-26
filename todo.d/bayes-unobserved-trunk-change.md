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
