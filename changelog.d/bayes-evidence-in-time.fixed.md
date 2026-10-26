- `bayes` now judges each failed run against the trunk it started on, so a
  later landing no longer retroactively undoes an earlier pass, and culprits
  are singled out with fewer repeated runs. Its culprit-finding batches also
  now cover a culprit at the bottom of a stack, and a busy stack no longer
  starves an unrelated failing stack of them. Under `landing = "decay"`,
  merges made outside the queue now count as trunk movement too.
