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
