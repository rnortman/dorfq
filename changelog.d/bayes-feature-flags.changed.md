- The `bayes` scheduler takes three required flags: `speculation`
  (`"chain"`, one chain of batches with culprit finders on trunk beside it,
  or `"tree"`, below), `charge` (`"discounted"`, the cost of a run weighed
  by the chance its base stands, or `"full"`, the cost paid whole) and
  `landing` (`"decay"`, every other belief regrown by `p_interact` per
  landed pull request, or `"transfer"`, each belief regrown by its pair
  belief with each pull request that reached trunk, including one merged by
  another hand). Existing configurations must add `speculation = "chain"`,
  `charge = "discounted"` and `landing = "decay"` to keep their behaviour.
