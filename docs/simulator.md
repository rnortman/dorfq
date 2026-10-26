# The simulator

`dorfq-sim` runs DorfQ's schedulers, and an approximation of GitHub's merge
queue, against a seeded world of pull requests whose truths the simulator
knows and the schedulers do not. Every policy in a scenario sees the same
arrivals and the same truths. The published results are at
<https://rnortman.github.io/dorfq/simulation.html>. <!-- gitleaks:allow -->

## Run a scenario

From the repository root:

```sh
make sim ARGS="run --config $PWD/crates/dorfq-sim/examples/busy.toml"
```

This prints a table of every metric for every policy, as the mean over the
replications with its range.

- `--trace` also prints the first replication of each policy step by step.
- `--json $PWD/<file>.json` also writes the report, its configuration and
  the first replication of each policy as a JSON document: the document the
  simulation page reads. `make sim` runs the binary from Bazel's runfiles
  tree, so give every path absolute, as `$PWD/` does.

The shipped scenarios are `crates/dorfq-sim/examples/busy.toml` and
`crates/dorfq-sim/examples/sim.toml`. Every line of both is commented, and
those comments are the file format's reference: copy one and edit it. A
scenario is a `[world]` table and one `[[policy]]` table per column; a
policy's `kind` names its scheduler and the rest of the table is that
scheduler's configuration, the same `[scheduler]` table `dorfq` reads.

## View it

The simulation page has a file picker beside its report select. Load a
`--json` document there and the page shows it as it shows the published
reports: the metric table, the comparison against `github-like`, and the
replay. The playground on the same page runs an edited scenario in the
browser, compiled to WebAssembly, without a checkout.

## Sweeps

```sh
make sweep ARGS="--config $PWD/F"
```

runs a grid of simulations, optimised and on every available thread. A sweep
file is a scenario file with two more top-level keys:

- `baseline`, the policy the others are compared against;
- `[[axis]]` tables, each with `keys` (fields such as
  `world.arrivals_per_hour` or `policy.bayes.max_batch`, set together),
  `values`, and optional `labels`.

The sweep runs every point of the axes' cartesian product. It prints one row
per point and policy with every metric's mean over the replications, then
each policy's mean relative delta against the baseline over the points where
the baseline's is not 0, with the counts of points at which it is better and
worse, `(B+ W- /N)`, over every point where both have the metric.

- `--json $PWD/<file>.json` writes every replication's metrics.
- `--jobs N` caps the threads.

Two sweep files ship in `crates/dorfq-sim/sweeps/`:

- `speculation.toml` compares the `bayes` flag combinations with each other
  and with `github-like` over arrival rate, CI length, flake rate,
  `max_batch`, `lanes`, `run_cost`, `w_reject` and `w_age`.
- `bisect-cap.toml` runs `bisect` over its batch cap in the busy scenario's
  world.

## The site's data

`make sim-report` runs every shipped scenario to JSON under
`bazel-bin/site/reports/` and builds the browser simulator at
`bazel-bin/site/dorfq_sim_wasm.wasm`. `make check` builds them too, so a
scenario that fails to run is a red gate. The Pages workflow
(`.github/workflows/pages.yml`) rebuilds and publishes them on every push to
`main`.

To view the site from a checkout, serve `site/` and `bazel-bin/site/`
together over HTTP, as the Pages workflow assembles them; for example:

```sh
make sim-report
mkdir -p /tmp/dorfq-site/reports
cp site/*.html site/*.js site/*.css /tmp/dorfq-site/
cp bazel-bin/site/dorfq_sim_wasm.wasm /tmp/dorfq-site/
cp bazel-bin/site/reports/*.json /tmp/dorfq-site/reports/
python3 -m http.server -d /tmp/dorfq-site
```

Opened as a file, the simulation page still loads a document through its
file picker, but the playground needs HTTP.

## What the simulator models

Pull requests arrive as stacks, each with hidden truths: whether it conflicts
with trunk, and which other pull requests it breaks when tested together.
Each pull request runs its own CI beside the queue, and a policy's admission
decides when the queue sees it. Batch runs take a uniform time and fail on a
bad member, a conflicting pair or a flake. The crate documentation,
`crates/dorfq-sim/src/lib.rs`, states the model in full.
