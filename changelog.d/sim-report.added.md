- A simulator report site under `site/`, published to GitHub Pages by a
  new workflow, answers "how does this compare to a simpler merge queue?":
  each policy's headline numbers against a `github-like` baseline, the full
  metric table, and an animated replay of one simulated run showing
  runners, batches, the queue with its stacks, landings, ejections and
  trunk. `make sim-report` builds the reports locally.
- The simulator examples gain the `github-like` baseline, an approximation
  of GitHub's built-in merge queue (one pull request per run, five in
  flight), and a second example, `busy.toml`, a high-velocity team of five
  arrivals an hour on six runners.
- `dorfq-sim run --json PATH` writes the whole report as one JSON document:
  configuration, each replication's metrics or error, and a structured
  event trace of the first replication per policy. `--trace` prints the
  same events as text.
