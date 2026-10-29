A simulator world started from a given state and driven by a scripted
history. Today `World::new` in `crates/dorfq-sim/src/world.rs` starts
empty at time 0 and draws every arrival, push and verdict from its random
streams. Add a constructor that takes the pull requests and batches in
flight at some instant and a scripted sequence of exogenous events
(arrivals, pushes, withdrawals, CI verdicts) to play from there, so a
journal's real history can be replayed under a different scheduler.
Done means a world built from a journal's first observation and its
events, under the scheduler that wrote the journal, reproduces the
journal's plans. Deferred because it is design work: what is exogenous in
a real history once a different scheduler changes which batches run, and
how verdicts of batches that never ran are drawn. The
`TODO(sim-scripted-world)` comment sits on `World::new`.
