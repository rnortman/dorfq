- Stacked pull requests are now merged through the queue. The `bisect` and
  `bayes` schedulers (the two batching strategies) batch a pull request
  together with the pull requests it is stacked on, always testing and
  landing the bases first. A pull request marked `Atomic` lands together
  with everything beneath it or not at all: it is batched whole, even past
  `max_batch`, and a failure never splits it apart. When an `Atomic` group
  fails on its own, its `Atomic` members are ejected and the pull requests
  beneath them are retested. `bayes` also counts a child's own CI run as a
  test of its base, and tests a suspected stacked culprit on top of the
  bases it needs.
