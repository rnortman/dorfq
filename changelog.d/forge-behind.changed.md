- Forges now report, for each pull request, how far trunk has moved since
  its last own-CI run (`PrSnapshot::behind`, a new required field and fifth
  argument to `PrSnapshot::new`). The simulator counts landings exactly; the
  mock forge reads a new `behind` field in its scenario, raised by every
  landing and reset by a push; GitHub counts trunk commits the head lacks,
  at one extra API call per queued pull request per observation.
