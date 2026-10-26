The `bayes` scheduler keeps its trunk position twice. `Bayes::position`
accumulates the landings `ingest` sees (`crates/dorfq-scheduler/src/bayes/policy.rs`)
and is handed to `Model::sync`, which stores its own copy; inference reads the
model's copy, while `ingest` computes each batch's covered position from the
scheduler's. Nothing outside the tests sets them apart today, but every test
that calls `sync` with its own position leaves the two disagreeing. Keep one:
`Model::sync` takes the number of pull requests that reached trunk and
advances its own position, `ingest` reads `self.model.position()`, and the
tests pass a landed count instead of an absolute position. Behaviour does not
change: the sweep's and the examples' JSON output is byte-identical before
and after.
