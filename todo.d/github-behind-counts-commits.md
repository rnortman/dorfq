The GitHub forge reports `PrSnapshot::behind` in commits (`compare`'s
`ahead_by`, in `fetch_world` in `crates/dorfq-forge-github/src/lib.rs`), but
the contract defines it in pull requests reaching trunk, and `bayes` reads
it as landings (`Model::sync` in `crates/dorfq-scheduler/src/bayes/model.rs`
subtracts it from a position counted in pull requests). The queue lands one
merge commit per member on trunk, so `behind` counts merge commits. Done
means: on the GitHub fake, a pull request of several commits, one queue
landing behind, is reported at `behind = 1`.
