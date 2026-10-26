The `bayes` model treats a passing run as proof, not evidence. `passed` in
`crates/dorfq-scheduler/src/bayes/model.rs` moves each covered pull
request's cleared position to the run's trunk and sets each covered pair to
`FLOOR`; `blames` then excludes that pull request from every failed run
started at or before that position, and `prune` discards a failure with
nothing left to blame as a flake. A pull request's own CI, read through
`PrSnapshot::behind`, is taken as proof the same way. But CI misses a real
break at some small rate, so a pass should lower the belief that a pull
request or pair is broken, as any other observation does, not zero it. The
simulator has the same blind spot: a run over a broken tree always fails,
so the sweep cannot measure what a false green costs or what modelling one
buys. Closing this closes `bayes-unobserved-trunk-change`; it keeps more
causes alive, so it bears on `bayes-posterior-precision`. Done means: no
passing run, queue or own CI, clears a cause outright in the model; the
simulator's world produces false greens at a configured rate; and a sweep
measures the scheduler against that world.
