The Bayesian scheduler grows a batch from whole units, lowest combined
chance of failing first; a unit is every pull request that must land
together, so a large `Atomic` stack at the prior is among the riskiest units
and is reached late. `grow` in `crates/dorfq-scheduler/src/bayes/policy.rs`
skips any unit that no longer fits beside the units added ahead of it: one
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
