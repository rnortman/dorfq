- The GitHub forge's optional `lock_heads` setting stops pushes to a
  batched pull request's branch while its batch runs, so a push cannot waste
  the batch. The lock is a repository ruleset the queue owns,
  `dorfq/locked-heads`; it is lifted when the batch ends or the label is
  removed, and branches in forks are not locked. It needs Administration:
  read/write on the credential and is off when absent.
- The GitHub forge's `batch_checks` setting names the checks that must pass
  on batch tips, for a repository whose merge CI reports under other names
  than its pull-request CI. Absent, it is `checks`.
