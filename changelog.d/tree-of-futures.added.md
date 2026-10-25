- A new speculation strategy for the `bayes` scheduler, the tree of
  futures, chosen with `speculation = "tree"`. Instead of one chain of
  batches plus separate culprit finders, it treats trunk and the tip of
  every batch still in flight as possible future bases, and gives each
  free CI lane the most valuable batch on any of them, weighed by the
  chance that base actually lands. Building on a pending batch speculates
  that it passes; building beside it speculates that it fails. Batches
  that narrow down a failure's culprit compete on the same score, and a
  batch whose base can no longer land is cancelled. The existing
  behaviour stays available as `speculation = "chain"`.
