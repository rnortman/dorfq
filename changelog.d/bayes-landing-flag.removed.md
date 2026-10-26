- **Configuration change.** The `bayes` scheduler's `landing` setting is
  removed. `"decay"` is the behaviour: each landing regrows every other
  belief by `p_interact`. `"transfer"` measured indistinguishable from it
  and could not, by construction, regrow a belief by more. A configuration
  that still sets `landing` is rejected; drop the line.
