- The simulator now models each pull request's own CI, not just the merge
  queue's batch runs. A pull request runs its CI on the shared runners
  whenever it gets a new head (on arrival, on a re-push, on a rebase) and
  cannot enter a batch until that run passes. The cost shows up in the new
  `pr_ci_runs` and `pr_ci_hours` metrics and in `runner_hours`.
  **Breaking for configurations:** `[world]` now requires `pr_ci_minutes`
  and `rebase_minutes`.
- Simulator policies now choose how pull requests are admitted to the
  queue (`admission`, required): `label` (dorfq's way, from arrival) or
  `github` (GitHub's way: only once on trunk with its own CI passed, so a
  stacked pull request waits for its base to land, is rebased and re-checked
  first). This makes the cost of GitHub's one-at-a-time handling of stacks
  visible in the comparison.
- A conflict with trunk is now simulated as a rebase and a return to the
  queue rather than a final ejection; the new `rebases` metric counts these
  and `ejected_bad` no longer does.
