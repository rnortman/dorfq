- The simulator now models stacked pull requests. `p_stacked` sets how
  often an arrival is stacked on a recent, still-queued one and `p_atomic`
  how often such an arrival is `Atomic`; both are required in `[world]`, so
  existing configurations must add them. A base that leaves the queue
  without landing blocks everything stacked on it, counted in a new
  `blocked` metric; a re-push restacks its descendants.
