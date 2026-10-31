Make a write that skips the GitHub forge's rate-limit gate fail to compile.
Every write in `crates/dorfq-forge-github/src/lib.rs` goes through
`GitHubForge::send`, which refuses it while a pause GitHub announced is in
force, but `GitHubApi` mixes the writing methods with the reading ones and
`self.api` is in scope throughout the forge, so a new `self.api.set_base(..)`
compiles and writes through a pause; the helpers that only read (`Counted`,
`Ancestry`) are also handed a handle that can write. One shape: split the
trait into a read half and a write half, keep the handle in a private
wrapper that hands out only the read half, and make `send` the only way to
the write half, so `Counted` and `Ancestry` take the read half. Done means
no code in the forge outside `send` can call a writing method. Deferred
because it changes the trait the gate sits on and every implementation of
it, the client and the fake, which is a design decision about the API seam
rather than a fix to the gate. The `TODO(quality-write-gate-enforced-only-by-a-doc-comment)`
comment sits above `send`.
