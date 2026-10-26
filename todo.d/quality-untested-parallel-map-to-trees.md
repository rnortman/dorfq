`Bayes` in `crates/dorfq-scheduler/src/bayes/mod.rs` keeps `untested`, each
tracked batch's count of landings its run did not test, as a second map over
exactly the batches in `trees`. `ingest` in `bayes/policy.rs` inserts and
retains it beside `trees`, and reads it with `get_mut(..).expect` and an
index that are safe only because `trees` was filled first. Make the `trees`
value one struct holding a batch's tree members and its untested count, so
one insert and one retain cover both and the `expect` and the index go.
`finders` and `read` are subsets of the tracked batches, not data held for
each, and stay sets. Behaviour does not change: the sweep's and the
examples' JSON output is byte-identical before and after.
