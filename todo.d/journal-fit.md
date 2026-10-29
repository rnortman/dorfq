Fit the simulator to a real queue. Add `dorfq journal fit <segment>...`,
which prints a simulator `[world]` table — arrival rate, withdrawal rate,
batch and pull-request CI durations, failure and flake rates per check —
estimated from the journal's events and summary, so a sweep can be run
against a world that behaves like the queue that wrote the journal. Done
means the table parses as a `WorldConfig`, and a journal written by the
simulator under a known world fits back to that world's parameters within
stated bounds. Deferred because it is design work: which world parameters
the journal identifies, which it cannot (truth is not observed), and the
estimators. The `TODO(journal-fit)` comment sits above `summarise` in
`crates/dorfq/src/journal/summary.rs`.
