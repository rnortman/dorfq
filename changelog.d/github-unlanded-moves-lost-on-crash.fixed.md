- The GitHub forge no longer leaves a stacked pull request on trunk when a
  landing it was moved for does not complete. The batch record now names the
  base each stacked member was moved off, and the next observation, after a
  restart included, puts the member back on that base unless the base has
  merged. A move back GitHub refuses stops the queue rather than leave a
  child able to land without its base.
