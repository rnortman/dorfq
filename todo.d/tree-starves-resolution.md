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
price, each as a flag measured by that sweep: a cost in the objective per
pull request per hour it waits in the queue; and `w_reject`, which prices
a resolution at one landing against a fresh batch's several. Done means a
flag under which the tree is better than the chain on `ejected_bad` and
on `ended_hours` at more than half of the compared points, as that sweep's
summary prints them: a replication now runs until its queue empties, so
the time it takes to empty is what `unfinished` used to measure. The `TODO(tree-starves-resolution)` comment is on
`candidates` in `crates/dorfq-scheduler/src/bayes/tree.rs`.
