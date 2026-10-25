- The simulator now models a bad pull request as one that is broken
  against trunk, instead of one drawn bad at arrival and bad forever. Two
  pull requests may conflict: a batch run holding both fails, and whichever
  lands first breaks the other. A pull request stays bad until its author
  pushes a fix, and it is also bad while the pull request it is stacked on
  is bad. Scenario files must drop `p_bad` from `[world]`.
- Replications run until every pull request has left, not to a fixed
  horizon. Scenario files must set the new `stall_hours`: a replication
  that goes that long without a landing or an ejection is cut off and
  reported `stalled`. `drain_hours` remains only as a last-resort cut-off.
- An ejection counts as correct (`ejected_bad`) only when the pull request
  is broken against trunk at that moment. Ejecting one whose conflicting
  partner is still only queued counts as `ejected_good`.
- A pull request's own CI run now fails when it is broken against trunk,
  and its author then pushes a fix.
- New metrics: `ever_bad`; `detect_p50_hours` and `detect_p95_hours`, the
  time from a pull request becoming bad (or arriving bad) to its ejection;
  `ended_hours`, when the replication ended; `cut_off` and `stalled`.
- A pull request that leaves because the one it is stacked on left is now
  counted under that pull request's exit. The `blocked` metric is replaced
  by `left_with_base`.
- A batch run's pass or fail is decided when it starts, for the code it
  was given.
- The JSON report moves to schema 4 and the sweep document to schema 2.
  In the trace, `arrive` carries `conflicts` instead of `truth`, `blocked`
  gains `bad`, `pr_ci_end` gains `failed`, `rebase` gains the reason
  `broken`, and there are new `broken` and `fixed` events.
- The shipped speculation sweep now covers CI times of 45–120 and 90–120
  minutes, `max_batch` of 16 and 30, and a `w_reject` axis.
