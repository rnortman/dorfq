- **Configuration change: every `kind = "bayes"` scheduler configuration
  must now set two new priors, `p_miss` and `p_trunk`.** A broken trunk
  or a CI outage now ejects far fewer innocent pull requests. The `bayes`
  scheduler now allows that trunk itself, or CI, may have broken since the
  last passing run on trunk (`p_trunk`, the prior for this). It blames
  failures on trunk rather than on whichever pull requests happened to be
  in the failing runs. To settle the question it runs a *probe*: a batch
  with no pull requests that tests trunk alone. A passing run also no
  longer clears a pull request entirely. CI can miss a real break
  (`p_miss`, its rate of doing so), so a pass lowers suspicion rather than
  removing it.
- When the scheduler's probability arithmetic cannot be resolved, it now
  falls back to considering fewer of the most recent runs rather than
  stopping the queue.
