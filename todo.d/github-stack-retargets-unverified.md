The GitHub forge changes a pull request's base to trunk in two places, and
neither has been run against GitHub. On observe, `fetch_world` in
`crates/dorfq-forge-github/src/lib.rs` moves a pull request the queue had
reported stacked on a pull request that has since merged into trunk; at
landing completion, `move_onto_trunk` in the same file moves a landed
stacked member that is still open against its base's branch. The live
lane's stack steps (step 7 in `crates/dorfq-forge-github/src/bin/live.rs`)
drive both, but were only built and unit-tested: running them needs a
disposable repository and credentials. Run the lane and record what GitHub
did with each move. The open doubt is the landing-time one: the member's
head is already in trunk when the base is changed, and GitHub may refuse a
base change that leaves no commits between base and head. If GitHub accepts
both, remove this entry. If it refuses the landing-time move, drop that
write (`move_onto_trunk` and its call in `complete_landing`), the unit
tests in `crates/dorfq-forge-github/src/tests.rs` that assert it (delete
the ones that exist only to exercise the write, such as the failed-retarget
test, rather than flipping their assertions), the lane's poll for the
landed member's base reading trunk, and every sentence that promises a
landed stacked member is moved onto trunk: the crate docs' `# Stacks`
section, `complete_landing`'s doc comment, the stacks paragraph in
`README.md` under "Running against GitHub", and
`changelog.d/forge-stacks.added.md` if it is still a fragment, else the
gathered `CHANGELOG.md` entry gets a `changed` fragment saying so. A landed
stacked member then stays open until someone deletes its base branch, at
which point GitHub retargets it itself; the pull requests the queue had
queued on it are still moved onto trunk on observe, which reads the
queue's own record that it landed the member, not the member's base on
GitHub; keeping a write that never succeeds would be a stderr line on
every stacked landing. If GitHub refuses the observe-time move, that is a
design change: the alternative is to stop retargeting on observe and
depart such a pull request `Blocked`, telling its author to retarget by
hand. The `TODO(github-stack-retargets-unverified)` comment sits in
`move_onto_trunk`. Deferred because running the lane needs credentials and
a scratch repository the owner must supply, and because what GitHub
answers decides which of the three outcomes above to build.
