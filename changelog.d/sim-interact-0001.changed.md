- The simulator's shipped examples and sweeps now use an interaction rate of
  `p_interact = 0.001` (was 0.005), and the busy example's published figures
  are re-measured: `bayes` lands 322, `bayes-tree` 325, `bisect` 270 and
  `github-like` 277 (previously 275, 278, 238, 239), with `bayes` ejecting
  10 bad and no good pull requests at 1.0 runner-hours per landing.
