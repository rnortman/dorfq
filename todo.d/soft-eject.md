A second, lower belief threshold at which the scheduler tells an author
that their pull request is probably broken against trunk, without ejecting
it. It needs a contract item beside `Plan.eject` (rule 9 lets the scheduler
send authors nothing else), emitted once per (pull request, head), and a
`Forge` method that the GitHub forge relays as a comment and the mock
records. In the simulator, a told author starts a fix (as a failed own-CI
run already makes them do) with a configured probability, and the report
adds the time from `bad_since` to the first notice and a count of notices
sent for pull requests that were not bad. Deferred because it is a
contract change and a forge behaviour, to be designed as one cycle on top
of the world that models breakage against trunk. The `TODO(soft-eject)`
comments sit at `BayesConfig::eject_above` in
`crates/dorfq-scheduler/src/config.rs` and where the simulated world starts
an author's fix, `begin_fix` in `crates/dorfq-sim/src/world.rs`.
