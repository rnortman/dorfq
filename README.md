# dorfq

DorfQ (Dorfman + queue, after Robert Dorfman's pooled testing) is a merge queue
for large monorepos with long CI runs and high PR velocity. It pools queued pull
requests into batched CI runs, so one passing run lands many changes. When a
batch fails, it infers the culprit from what failed using Bayesian reasoning
instead of bisecting blind. It lands only the exact tree that passed CI, by
fast-forwarding trunk.

It uses Bayesian reasoning, runs speculative batches when the expected value
exceeds the cost, and reasons about false positives, false negatives, and
infrastructure flakes. Every belief is probabilistic; nothing is absolute,
because that's how the real world works.

DorfQ stores its state *in git*, in refs on the tree like `refs/dorfq/...`. No
separate database is needed. DorfQ reconstructs its state from the tree on
restart. Deploying it is just running a single binary on some server with access
to your Github installation and bot credentials.

## Status

DorfQ is operating as its own merge queue. The core functionality is there;
operational support for managing it in the real world is somewhat lacking, as is
developer-facing tooling. There are a few inefficiencies in the Github API usage
and in the Bayesian reasoning that are still being improved. Only Github is
supported as a backend forge, though the forge interface is abstracted so
others can be plugged in later.

Simulation results on very large/difficult merge queues shows it outperforming
other strategies in most metrics that are important. It particularly outperforms
Github's own merge queue by quite a lot.

## Philosophy, Priorities, and Why Build This

Trivial merge queues like Github's built-in one, or the various flavors of Bors
which do bisection to find bad PRs, do fine in unchallenging environments. If
you get 4 PRs per hour and CI takes 10 minutes and you have runner capacity to
run several CI lanes in parallel, and you don't have a very high false-positive
rate (infrastructure flakes), they will hold up just fine. Developers will spend
some time hand-holding/managing the queue instead of writing code.

If you need to merge 20+ PRs per hour and CI sometimes takes 3 hours and the
infrastructure flake rate is more than a fraction of a percent, these things
fall over *immediately*. DorfQ is built for that environment.

### Optimize for the hard cases

Optimize for CI that runs long and often fails spuriously because of an
infrastructure failure. "long" here means relative to your PR rate
(`ci_wall_time > 1/rate`) and "often" means roughly more than 1% of runs fail
for infrastructure flakes. Both are common in large organizations with complex
monorepos and complex, exhaustive CI. Both make trivial merge queue strategies
untenable.

You want a merge strategy that *shines* when a terraform script accidentally
took CI offline for half a day but developers didn't stop submitting PRs and now
you have a massive backlog and the PRs are still coming.

You also, in the AI era, want a merge queue that shines with *high velocity* of
submissions and *low confidence in their quality*. High velocity increases
`rate`. Low confidence in quality means your CI had better be exhaustive -- more
unit tests, more system-wide integration tests, extensive simulation testing
(which is a very expensive kind of integration test), which makes `ci_wall_time`
higher, and both of those conspire to make `ci_wall_time >> 1/rate`.

System-wide integration/simulation testing makes dependency analysis (only run
what changed) less effective at reducing CI time; most advanced queue strategies
rely on dependency analysis as the core of their scalability. Dependency
analysis and only running what you must is necessary but not sufficient in an
environment with a lot of integration testing.

### Optimize developer experience: Take ownership when a PR enters the pool

When dorfq accepts a PR into the pool, it takes responsibility for that PR until
it explicitly hands responsibility back to the developer. Developers do not need
to babysit; if dorfq accepted it, dorfq owns it, and it'll let you know when you
need to take action. Or it'll just get it landed, if it can.

You can, of course, put the PR into the merge pool before the PR has passed its
own PR CI. DorfQ will hold it until it passes PR CI before testing it in merge
batches. It's **fire and forget** for the most part.

### Support PR stacks and trees natively

In the real world, particularly in high-velocity organizations, PRs are often
assembled into stacks or trees of related changes that build on one another.
Many merge queues have no notion of this, but it's a critical thing for the
queue to handle. DorfQ is stack-aware (and tree-aware), lets you control for
each stack whether it must merge atomically or can be split to land, and
includes stack information in its Bayesian reasoning (e.g., a stack of PRs that
has passed CI together is known not to have self-interacting PRs.)

### Be efficient; balance throughput, latency, and resource utilization

DorfQ is built on the assumption that CI runner resources are either a finite
pool, probably shared with other tasks so there is an opportunity cost to using
the resources, or growing the pool dynamically costs money. It models CI costs
directly via tunable parameters, and also lets you tune the value of landing
PRs, and the value of proving a PR "bad" and handing it back to the developer to
rebase/fix. It runs speculative batches but only when the expected value exceeds
the cost (based on those tuning parameters). It balances batch size (more PRs
tested at a one-CI-run price) against risk (bigger batches more likely to
interact, give less information per PR when they fail).

### Optimize for the case where DorfQ is the only thing merging PRs

Any real organization operating under the conditions described above is merging
PRs manually only very very rarely. DorfQ can handle this but optimizes for the
case where it's in charge.

## Simulation results

The simulator runs every policy over the same arrivals and hidden truths, side
by side with an approximation of GitHub's own merge queue. The published
results, at <https://rnortman.github.io/dorfq/simulation.html>, show each <!-- gitleaks:allow -->
shipped scenario's metrics, a replay of each policy's run, and a playground
that runs an edited scenario in the browser. Running your own scenarios and
sweeps from a checkout is in [`docs/simulator.md`](docs/simulator.md).

## Getting started

1. Install [bazelisk](https://github.com/bazelbuild/bazelisk) as `bazel`; git
   and make are the only other host prerequisites.
2. Create the queue's label in the repository and choose its credential: a
   GitHub App, or a fine-grained token.
3. Run the live lane against a throwaway repository before pointing the queue
   at one you care about.
4. Copy `crates/dorfq/examples/github.toml`, set `owner`, `repo` and
   `[forge.auth]`, and run `make run ARGS="run --config $PWD/<file>"`.
5. Make CI run on pushes to `dorfq/batch/**`.

[`docs/running-on-github.md`](docs/running-on-github.md) has every step in
full: permissions, branch protection, the live lane, the systemd units, and the
rules the queue holds you to while it runs.

## Documentation

- [`docs/running-on-github.md`](docs/running-on-github.md) — operating the
  queue on a GitHub repository.
- [`docs/simulator.md`](docs/simulator.md) — running scenarios and sweeps.
- [`docs/developing.md`](docs/developing.md) — building, testing and the
  source layout.
- The scheduler–forge contract and its rules:
  [`crates/dorfq-core/src/lib.rs`](crates/dorfq-core/src/lib.rs).
- The GitHub forge's repository layout:
  [`crates/dorfq-forge-github/src/lib.rs`](crates/dorfq-forge-github/src/lib.rs).

## License

Apache-2.0; see `LICENSE` and `NOTICE`.
