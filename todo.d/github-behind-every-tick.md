The queue lands every batch itself, so it knows what merged and when
without asking GitHub, yet the GitHub forge re-reads from the API what that
record already determines: `fetch_world` in
`crates/dorfq-forge-github/src/lib.rs` calls `compare` for every enqueued
pull request on every observation to compute `PrSnapshot::behind`. Restructure
the forge, and the contract and scheduler where they need it, around what
the queue already knows: beliefs and positions move on the queue's own
landings wherever it has them, and GitHub is asked only for what the queue
cannot know — chiefly, when a new own-CI result arrives for a pull request,
which trunk that run tested (`github-behind-tested-trunk`). This cuts API use
and may change more than the one call. Done means: no observation makes an
API call for something the queue's own landing record determines.
