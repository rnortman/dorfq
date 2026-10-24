- The simulator now models a monorepo with effectively unlimited CI
  capacity. There is no runner pool any more: each policy's `lanes`, the
  number of batches the merge queue will run at once, is the only capacity
  limit. Efficiency is measured in runner-hours, latency in time to merge.
  PR CI never takes a lane. It is billed to the queue only when the queue
  causes it: when a pull request's parent lands and its base changes, it
  re-runs PR CI, so splitting a stack costs runner-hours and landing it
  whole does not. A new world switch, `pr_ci_on_base_landed`, controls this.
- Pull requests now arrive as whole stacks, several at once, with sizes
  drawn from a weighted table (`stack_sizes`). A stack may later grow once
  (`p_stack_grows`, `stack_growth_sizes`, `stack_growth_minutes`).
  `p_conflict` is now the rate at which a stack's bottom pull request
  conflicts with trunk.
- Breaking for simulator configurations: `runners` and `p_stacked` are
  removed, and the five new keys above are required. The bundled examples
  are updated, and their `github-like` column now gets as many lanes as the
  policies it is compared against.
