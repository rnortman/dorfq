- `dorfq-sim sweep --config FILE [--json PATH] [--jobs N]`, and
  `make sweep ARGS=...` to run it optimised: a sweep file is a run file with
  a `baseline` policy and `[[axis]]` tables, each setting one or more world
  or policy fields (`world.<field>`, `policy.<name>.<field>`, dotted into
  nested tables) to each of its values. Every point of the grid runs every
  policy at every replication on a pool of threads, and the sweep prints one
  row per point and policy with every metric's mean, then each policy's mean
  relative delta against the baseline over the points where the baseline's
  is not 0, with the counts of points where it is better and worse out of
  those where both have the metric. `--json` writes every replication's
  metrics at every point.
- The `cancelled_runs` metric: batch runs stopped by their batch's
  retirement, in the run table, the JSON document and the report site.
- Two shipped sweep files under `crates/dorfq-sim/sweeps/`:
  `speculation.toml` runs the four `bayes` flag combinations (the status-quo
  chain, the chain with `charge = "full"`, the tree, and the tree with
  `landing = "transfer"`) and a `github-like` column over arrival rate, CI
  length, flake rate, `max_batch`, `lanes`, `run_cost`, `w_reject` and
  `w_age`, at an interaction rate of 0.001;
  `bisect-cap.toml` runs `bisect`'s batch cap at two capacities in the busy
  example's world.
