The GitHub forge measures `PrSnapshot::behind` from the head's merge base
with trunk (`fetch_world` in `crates/dorfq-forge-github/src/lib.rs`), which
is the trunk own CI tested only when CI tests the head as pushed. CI that
tests the head merged with trunk tested a later trunk, and which one
depends on the CI system and how it is set up. Add a configuration setting
that selects how the forge estimates the trunk a pull request's own CI ran
against, with several strategies to choose from — the merge base, and trunk
as of when the run started (a check run's `started_at`, or when a commit
status was first posted pending) among them. An estimate may be imprecise;
the model is Bayesian and absorbs that. One known imprecision: re-running
a GitHub Actions workflow tests the original event's commit, not trunk as
of the re-run. Done means: the setting exists, each strategy is tested on
the GitHub fake, and `behind` counts from the trunk the selected strategy
names.
