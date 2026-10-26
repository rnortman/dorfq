- **Configuration change.** The `bayes` scheduler's `charge` setting is
  removed. A candidate run is now always weighed against its full cost,
  which the simulator sweeps measured as fewer runner-hours for the same
  landings than the discounted charge. A queue that ran
  `charge = "discounted"` changes behaviour only under load: a candidate
  built on an unlikely base no longer starts. While every batch starts on
  trunk, nothing changes. A configuration that still sets `charge` is
  rejected; drop the line.
