- `capture = "<path>"`, beside `journal`, records what each pull request head
  changes: its changed files with additions, deletions and status, the base
  sha and the totals, one JSON line keyed by `(pr, head)`, read from GitHub
  when the queue first sees the head. The file rotates with the journal and
  records a head at most once per file. It reads at most eight heads a tick
  and costs `1 + ceil(files / 100)` requests per new head, nothing
  otherwise. See `docs/running-on-github.md`.
