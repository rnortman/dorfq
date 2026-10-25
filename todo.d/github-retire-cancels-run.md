`GitHubForge::retire` deletes a batch's record and branch and never cancels
the CI run on its tip, so a retired pending batch keeps a runner busy to the
end. The contract's rule 7 makes `retire` of a pending batch its
cancellation where the platform allows, and GitHub allows it: list the runs
on the tip (`GET /repos/{owner}/{repo}/actions/runs?head_sha=<tip>`) and
cancel each in progress (`POST /repos/{owner}/{repo}/actions/runs/{id}/cancel`),
which needs the Actions: write permission. The `bayes` scheduler under
`speculation = "tree"` retires pending batches as a matter of course, so on
GitHub it pays for every cancelled batch's full run until this is done.
Done means: `retire` of a `Testing { Pending }` batch cancels every
in-progress run on its tip before deleting the refs, the fake models runs
and their cancellation, the README's permission lists gain Actions: write
under both credentials, and `make live-test` exercises it once. Deferred
because it is forge design: a permission every installation must add, a
fake that models runs, and what to do when a cancel is refused. The
`TODO(github-retire-cancels-run)` comment sits in `GitHubForge::retire` in
`crates/dorfq-forge-github/src/lib.rs`.
