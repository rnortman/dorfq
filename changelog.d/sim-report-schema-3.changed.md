- The report page's replay now draws one lane per batch lane of the policy
  and grows taller with the lane count, so many-lane runs stay readable.
- The simulator's JSON report moves to schema 3, and the report page reads
  only schema 3: `pr_ci_runs` and `pr_ci_hours` become `billed_pr_ci_runs`
  and `billed_pr_ci_hours`, `utilisation` is removed, the trace's `pr_ci`
  event gains `billed`, and the `run_begin` and `pr_ci_begin` events are
  gone.
