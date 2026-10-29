- `status_issue` under `[forge]` (GitHub, with `journal`) names an issue
  whose body the queue keeps as its status: the queue, the batches in
  flight, and the last 24 hours' landings, ejections, failures, run times
  and time to land. The body is rewritten when what it shows changes, at
  most twice a minute and never during a rate-limit pause. The issue is
  checked and given a starting body at start, so a missing issue or a
  credential without Issues: read/write stops the queue at start.
