- **Configuration change, and a known limitation.** The `bayes` scheduler's
  `p_bad` setting is removed, and a configuration that still sets it is
  rejected. Instead of one fixed prior, `bayes` now judges each pull request
  by how stale its own CI run is: a pull request whose CI passed against the
  current trunk is not suspected at all, and each pull request landed since
  then raises its chance of being broken by `p_interact`. Limitation:
  `bayes` does not yet notice trunk changes made outside the queue (a merge
  without the queue label, a direct push), so a pull request broken that
  way is retried forever instead of being ejected. The same happens to the
  mock forge's `bad = true` pull requests at the default `behind = 0`: under
  `bayes`, `crates/dorfq/examples/scenario.toml` restarts `[5]` every tick
  and `--exit-when-idle` never returns. Do not run `bayes` against a real
  forge yet; see `bayes-unobserved-trunk-change` in `TODO.md`.
