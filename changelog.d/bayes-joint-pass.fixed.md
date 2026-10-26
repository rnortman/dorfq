- `bayes` no longer keeps restarting a batch that has already failed twice
  with the same members (such as an `Atomic` stack with a broken member):
  it used to rate such a batch a one-in-four pass and hold a lane with it
  until an author intervened; it now recognises the near-certain failure
  and runs culprit finders instead.
