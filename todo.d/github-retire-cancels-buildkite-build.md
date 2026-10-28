`retire` of a testing batch cancels its runs only when `ci = "actions"`
(`Ci::Actions` in `crates/dorfq-forge-github/src/lib.rs`). A repository
whose CI is Buildkite reports to GitHub as a commit status,
`buildkite/<pipeline>`, and its builds are cancelled only through
Buildkite's own API with its own token: `GET
/v2/organizations/{org}/pipelines/{pipeline}/builds?commit=<tip>` to find
the builds on the tip, `PUT …/builds/{number}/cancel` to cancel each that
is not finished. Done means: a `Ci::Buildkite` variant with its own
settings (organization, pipeline slugs, a token read from the environment
as `[forge.auth] kind = "token"` reads its own), `retire` of a
`Testing` batch cancelling the builds on its tip through them, after the
record and branch are deleted as the Actions route does, the fake
modelling builds and their cancellation, the token
documented in docs/running-on-github.md, and `make live-test` exercising
it once behind an environment variable, as `DORFQ_LIVE_CI=actions` does
for Actions. Deferred because it is design: a second credential beside
the forge's, where it lives at rest, what a refused or failed cancel does
to `retire`, and whether Buildkite reports the batch branch in a build so
a trunk build of the same commit is left alone. The
`TODO(github-retire-cancels-buildkite-build)` comment sits on the `Ci`
enum in `crates/dorfq-forge-github/src/lib.rs`.
