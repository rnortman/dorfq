Both pipelining schedulers bound their chain's depth by `lanes`, counting
every batch `is_pending` in `crates/dorfq-scheduler/src/lib.rs` accepts, and
the contract does not say whether a pending batch's run has begun or is still
waiting for a runner; the simulator's world reports both as `Pending`. When
the runners are saturated — in the simulator, when PR CI alone fills them — a
started batch waits hours for a runner. `bayes` retires any batch whose
member's head changed, so in that time a repush, withdrawal or blocking under
any batch of the chain dooms the chain above it, and the rebuilt chain joins
the back of the runner queue; `bisect` shares the bound and the collapse above
a retired batch, but finds a changed member out only at landing, and loses
less. In a world of six runners, five arrivals an hour and PR CI of 45–120
minutes, `bayes` started 305 batches in a replication and 290 never reached a
runner; with twelve runners, 330 of 454. In
`crates/dorfq-sim/examples/busy.toml` the loss is 35 of 422, with no runner
wait over an hour. The `TODO(chain-collapse-under-runner-queue)` comment sits
on `is_pending`. Done means: a measure of starts that never reached a runner,
from the trace or as a metric, in the busy example and in a world whose PR CI
demand is near the runner capacity; and, if material at a realistic load,
either a contract field saying whether a batch's run has begun (GitHub reports
a check run `queued` or `in_progress`) with a rule that does not extend a
chain whose end has not begun running, or a rule that bounds waiting batches
without it. Deferred because the contract change and the rule's effect on
each scheduler's chain are design work.
