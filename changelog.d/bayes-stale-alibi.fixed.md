- `bayes` no longer treats a passing batch as proof that a pull request is
  sound when trunk moved after the batch started. Before, a pull request
  broken by one of those later landings could keep failing batch after batch
  while its innocent batch-mates were ejected in its place.
