- The GitHub forge's configuration requires a `[forge.refresh]` table with
  `listing_secs`, `pr_secs` and `batch_secs`, each at least 1: how often the
  labelled listing and batch records, each queued pull request, and each
  testing batch's checks and trunk are read unprompted. An observation reads
  only what is due, and what the queue's own last actions changed, and
  derives the rest from what it read before; one with nothing due makes no
  GitHub call beyond retrying a comment post that failed. A blocked pull
  request's comment thread is read once per reason it is blocked, not at
  every observation.
- `[driver]` and its `tick_millis` are removed; a configuration that still
  has them is rejected. The queue ticks when something is due: at once on
  the mock, and on GitHub when the next refresh deadline passes, a webhook
  makes something due sooner, or a comment a transient failure kept from
  posting is due to be retried. After a transient failure the GitHub forge
  waits five seconds, doubling to a minute while failures continue, and a
  rate limit holds it until the time GitHub's response gave; a comment post
  that fails transiently is retried on the same five-second-to-a-minute
  schedule without holding the next refresh, and each failed attempt writes
  a line on stderr naming the comment and GitHub's error. A comment an observation owes
  is posted in that observation's tick.
