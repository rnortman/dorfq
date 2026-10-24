- Upgrading a running GitHub queue: batch records are unchanged and the
  queue picks up where it left off, with one exception. A batch that an
  earlier build published at its own base (every member already in trunk)
  now stops the queue with "batch <id>'s tip is its base"; "Running against
  GitHub" in the README says how to clear it.
- The contract between scheduler and forge (`dorfq-core`, the interface
  between the part that decides what to test and land and the part that
  talks to GitHub) is rewritten to cover what the first production release
  needs, so both sides can keep growing without breaking each other. It
  can now represent stacked pull requests (a pull request whose base is
  another queued pull request), say why a pull request is waiting or
  conflicting, tell how each one left the queue, name the checks a failed
  batch broke, and run a probe: a batch with no members that tests trunk on
  its own and never lands.
- Stacks: the `all-at-once` scheduler lands stacked pull requests, keeping
  together stacks marked to land only whole. `bisect` and `bayes` do not
  batch stacked pull requests yet.
- Authors are told in a comment when their pull request is waiting and
  what holds it.
- A scheduler that cannot reach a decision now stops dorfq with an error
  instead of crashing.
- CI also runs on pushes to `dorfq/batch/**`, so batches the queue builds get
  the `check` it waits for before landing.
