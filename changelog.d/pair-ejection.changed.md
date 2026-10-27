- The `bayes` scheduler no longer ejects a pull request merely because it
  fails together with another queued one. Both authors are warned, each
  told about the other; the two are kept out of the same batch, and once
  one lands the other is ejected only if it is then broken against trunk.
