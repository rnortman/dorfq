- The simulator examples run at realistic settings: batch and PR CI of 45 to
  120 minutes in both; the busy world at a flake rate of 0.03 and an
  interaction rate of 0.005. `github-like` runs on sixteen lanes, the
  concurrency GitHub's queue is given in practice, while dorfq's columns
  share one capacity (eight in the busy world, two in the small one). Each
  example has two `bayes` columns: `bayes`, the status quo at its best
  measured settings, and `bayes-tree`, the tree of futures at the same
  settings. The busy world's bayes columns run at `max_batch = 8`; its
  settings come from the shipped sweeps.
