- The `bayes` scheduler has a new required setting, `w_age`: how much each
  hour a pull request has waited adds to its value, so long-waiting pull
  requests are batched and their failures resolved ahead of fresh ones at
  the same risk. `w_age = 0` keeps the previous behaviour; the shipped
  examples use 0.02.
