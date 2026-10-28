- The GitHub integration now follows the repository's own rules on who may
  merge. Before, the queue label alone authorised a landing, at whatever
  head the pull request had by then, bypassing review rules. Now the label
  only puts a pull request in the pool: a draft, or a pull request GitHub
  reports blocked by branch protection once its configured checks pass, is
  held, and a held pull request is not landed even when its batch passed.
  Review requirements, code owners and stale-review dismissal apply to the
  queue as the repository configures them.
- The hold "review required" is replaced by "blocked by branch protection",
  which names every protection rule GitHub reports unmet.
