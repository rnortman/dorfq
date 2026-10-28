# Running DorfQ on GitHub

The forge is tested against an in-memory fake of GitHub. Its first contact
with GitHub is the live lane, so run that before pointing the queue at any
repository you care about.

In order:

1. Create the intent label (`queue` in the example) in the repository.
2. Choose a [credential](#credential).
3. Run the [live lane](#the-live-lane) against a throwaway repository.
4. Set up [branch protection and CI](#branch-protection-and-ci).
5. [Configure and run](#configuration-and-running) the queue.

Before labelling anything, read [what the label means](#the-label).

## Credential

The queue runs as a GitHub App or with a fine-grained personal access token.

- **GitHub App** (preferred). The queue has an identity of its own: comments
  and merge commits appear as `<app name>[bot]`, and what it holds at rest is
  a private key from which it mints its own short-lived tokens.
- **Token.** The queue acts as a person: comments and merge commits appear
  under the token's account.

Either credential needs these repository permissions:

- Contents: read/write
- Pull requests: read/write
- Issues: read
- Checks: read
- Metadata: read
- Workflows: read/write
- Administration: read/write — only with `lock_heads = true`
- Actions: read/write — only with `ci = "actions"`

Workflows is needed because GitHub refuses a merge or a ref update that adds
or changes a file under `.github/workflows/` to a credential without it, and
the queue makes both for every queued pull request that touches CI; without
it the first such batch stops the queue. Checks is what GitHub's permission
reference names for listing a commit's check runs, which the queue does on
every observation.

With `lock_heads = true` the queue owns a repository ruleset,
`dorfq/locked-heads`, that locks the head branch of every pull request in a
batch until the batch ends, and needs Administration to write it. A
credential without it stops the queue at the first batch it starts, naming
the permission. Do not edit the ruleset by
hand while the queue runs; the queue creates it with the first locked
branch, rewrites it as batches start and end, and deletes it with the last.
The ruleset outlives the process: while the queue is stopped, whether it
stopped on an error or was shut down, the branches it locked stay locked.
To release them, delete `dorfq/locked-heads`; the queue makes it again from
its batch records at its next successful observation. Delete it too when
turning `lock_heads` off or retiring the queue for good.

With `ci = "actions"` the queue cancels the workflow runs still going on a
batch's branch when it retires the batch without landing it: a batch the
scheduler gave up on, one that failed fast while its other checks were still
running, a probe trunk moved under. A retired batch then does not keep a
runner busy to the end. The cancel comes once the batch's record and branch
are gone, so a cancelled run is never read as the batch's verdict. A credential without Actions: read/write stops the
queue at the first such retire, naming the permission. A repository whose CI
is not Actions leaves the setting absent.

### As a GitHub App

1. Create the app: the account's *Settings → Developer settings → GitHub
   Apps → New GitHub App*. Give it a name (its comments appear as
   `<name>[bot]`) and any homepage URL, deactivate *Webhook* (dorfq polls),
   and under *Where can this GitHub App be installed?* choose only this
   account.
2. Give it the repository permissions above.
3. *Generate a private key*. GitHub downloads a `.pem`; store it where the
   queue runs, mode 0600, and note the *Client ID* from the app's page.
4. *Install App* on the repository the queue serves, and only that one (plus
   the live lane's throwaway repository while you run it).
5. In the configuration:

   ```toml
   [forge.auth]
   kind = "app"
   client_id = "<the app's Client ID>"
   private_key_file = "<path to the .pem>"   # relative to the configuration
   ```

The queue mints an installation token scoped to the one repository at
startup, and a new one before each expires. It fails at startup, naming the
cause, if the key is unreadable or not a key, the app is not installed on the
repository, or GitHub rejects its signature (a wrong client id, a key that is
not the app's, or this host's clock more than a minute ahead or five minutes
behind GitHub's).

### As a token

Create a fine-grained personal access token for the repository with the
permissions above, export it as `DORFQ_GITHUB_TOKEN` where the queue runs,
and set:

```toml
[forge.auth]
kind = "token"
env = "DORFQ_GITHUB_TOKEN"
```

## The label

The label puts a pull request in the pool; anyone who can apply it does, and
on GitHub the Triage role is enough. A head lands only when GitHub reports
its base branch's protection satisfied and the configured checks pass, so
review requirements, code owners and stale-review dismissal apply exactly as
the repository configures them. A pull request GitHub blocks, or a draft, is
held, not withdrawn; the queue posts nothing about it — GitHub shows why on
the pull request. A new head is held until it passes again. A pull request
held by the repository at the latest observation is not landed, even if its
batch passed. With `lock_heads` on, while a pull request is in a batch its
head branch is locked, so nobody can push to it; removing the label
withdraws it and, while the queue is running, unlocks the branch at its next
observation. A head in a fork cannot be locked. With `lock_heads` off, or for
a head in a fork, a push to a batched branch makes its batch refuse to land.
Batch CI runs the pull request's own workflow files as a push to
this repository, with this repository's secrets, and that holds for pull
requests from forks too. So read a pull request's current head, workflows
included, before you label it.

## Branch protection and CI

- If trunk is protected by branch protection or a ruleset, the credential's
  identity must be allowed to push to it: landing is a direct fast-forward,
  not a pull-request merge. For an app, add it to *Restrict who can push*
  and to the bypass list of any rule requiring a pull request.
- Trunk's required status checks should be the names in `checks`.
- CI must run on `dorfq/batch/**` pushes and report under the names in
  `batch_checks`, which defaults to `checks`. GitHub reads a push workflow from the pushed commit, and a batch
  branch carries trunk's workflows, so the trigger has to be on trunk before
  batch branches get checks; until then batches stay pending. dorfq's own
  `.github/workflows/ci.yml` has the trigger.
- With `lock_heads` on, the branch lock is a repository ruleset. On a
  private repository, rulesets need a plan that offers them.
- With `ci = "actions"`, only runs whose head branch is the batch branch are
  cancelled; a run of the same commit on trunk is left alone.

## The live lane

Create a throwaway repository whose name contains `scratch` or `playground`,
with a `main` branch holding at least one commit, and give the credential
access to it. The lane sets commit statuses, which the queue never does, so
add Commit statuses: read/write for it. That permission covers every
repository the app or token reaches, production included; remove it once
the lane passes. Then:

```sh
DORFQ_LIVE_REPO=<owner>/<name> make live-test
```

with either `DORFQ_GITHUB_TOKEN`, or `DORFQ_GITHUB_APP_CLIENT_ID` and
`DORFQ_GITHUB_APP_KEY_FILE` (the key's absolute path), in the environment;
exactly one of the two. Under an app this is the one check that GitHub
accepts its tokens and lets it comment, label, set statuses and push trunk.
`DORFQ_LIVE_API_BASE` points it at GitHub Enterprise.

With `DORFQ_LIVE_LOCK_HEADS=1` the lane also locks a batched branch with the
queue's ruleset, which needs Administration: read/write on the credential and
a public scratch repository, or a plan that offers rulesets on private ones.
The lane never touches `main`'s protection.

With `DORFQ_LIVE_CI=actions` the lane also dispatches a workflow on a batch
branch and retires the batch, which needs Actions: read/write on the
credential.

The live lane refuses any repository whose name contains neither `scratch`
nor `playground`, and treats the one it accepts as disposable: it closes
every open labelled pull request, deletes every queue ref, and lands commits
on `main`.

## Configuration and running

Copy `crates/dorfq/examples/github.toml` and set `owner`, `repo` and
`[forge.auth]`. `api_base` is `https://<host>/api/v3` on GitHub Enterprise;
`tick_millis` is the polling interval. `checks` names the checks that must
pass on a pull request's head; `batch_checks`, the checks that must pass on a
batch tip, for a repository whose merge CI reports under other names than
its pull-request CI; absent, it is `checks`. Either may name an Actions job
or a commit status context such as Buildkite's `buildkite/<pipeline>`.
`lock_heads = true` locks each batched pull request's head branch with a
repository ruleset until its batch ends, so a push cannot waste the batch's
run; it needs Administration: read/write, and is off when absent.
`ci = "actions"` cancels the workflow runs still going on a batch's branch
when the batch is retired unlanded; it needs Actions: read/write, and
absent, nothing is cancelled. The
`[scheduler]` table sets the
Bayesian scheduler's parameters, each commented in the example: set `lanes`
and `max_batch` to what the repository's CI can take, and the `p_flake` and
`p_interact` priors from its history, `p_miss` from the rate its CI has
been seen to pass a change that was broken, and `p_trunk` from how often
trunk or CI has broken between two passing runs for a reason no queued
change explains. `eject_above` is the belief at which a failed run's
culprit is ejected; `notify_above`, at most `eject_above`, the belief at
which its author is told it is probably broken without an ejection, and at
which each of two pull requests that fail only together is told so,
naming the other.

Run it wherever outbound HTTPS is available:

```sh
make run ARGS="run --config $PWD/crates/dorfq/examples/github.toml"
```

To keep it running, `crates/dorfq/examples/systemd/` has a user unit and a
system unit; each says where it expects the binary, the configuration and
the token or the app's key. `make local-deploy` installs the binary where the
user unit expects it and restarts the unit if it is running.

## While it runs

- Nobody pushes to, deletes or creates anything under `dorfq/` or
  `refs/dorfq/` by hand: the queue reads a foreign or moved ref there as a
  fault and stops. Batch ids in those names are decimal.
- Trunk moves only by fast-forward. A push to trunk by another hand makes
  every batch built on the old trunk refuse to land.
- A push to trunk by another hand also counts as trunk changes against every
  queued pull request, so one that the push broke is found by its next failed
  batch rather than batched again.
- A run on a batch with no members, a commit named `dorfq probe <id>` on
  trunk, is the queue testing trunk alone after several batches failed. One
  that fails means trunk, or CI, is broken: the queue waits, probing, until
  one passes. Failures while trunk is broken are explained by trunk rather
  than by the pull requests in the runs, so an ejection then is rare, not
  impossible: one made while probes are failing is worth a second look
  before telling its author. A probe still running when trunk moves is
  cancelled; its verdict would be about a trunk that is gone.
- A comment beginning `<!-- dorfq: notice -->` is the queue telling an
  author what it believes without acting on it: that the pull request is the
  likely cause of a failed batch, or that it and another queued pull request
  probably fail only together. The pull request stays queued. Rebasing and letting
  its own checks run again is the fastest way to settle it either way.
- A queue that stops with "batch <id>'s tip is its base" is tracking a batch
  that an earlier build published at its own base, because every member was
  already in trunk. Stop the queue, delete `refs/dorfq/batches/<id>` and the
  branch `dorfq/batch/<id>`, and start it again; the batch's members that are
  still labelled rejoin the queue.

## Stacks

A stack is a pull request whose base is another pull request's branch. Label
every pull request in it.

- A labelled pull request is enqueued once the pull request whose head branch
  it targets is enqueued; until then its author is told in a comment that it
  is blocked, and why.
- A pull request whose base is a branch that is neither trunk nor any open
  pull request's head is told the same, and never enqueued.
- A stacked pull request is held while its head does not contain its base's
  current head, so after a push to the base, rebase the pull requests stacked
  on it.
- Once a base pull request has merged into trunk, by the queue or by hand,
  the queue moves the pull requests it had queued on it onto trunk. It does
  this only for a stack it had queued and only for a merge into trunk, never
  for a branch whose pull request was merged elsewhere or that a labelled
  pull request targeted before it was queued on it — those are told to
  retarget by hand.
- A stacked pull request landed in the same batch as its base is moved onto
  trunk just before trunk moves, so GitHub marks it merged.
- If that batch then does not land — trunk moved first, a call failed, or
  the queue stopped between the move and the fast-forward — the queue puts
  the stacked pull request back on its base, at once or at its next
  observation, after a restart included.
- Every scheduler batches a stack bases before children.
- GitHub judges a stacked pull request by the protection rules of the branch
  it targets, as it would when merging it there. To require review for
  stacked pull requests, the repository's rules must target their base
  branches too; a ruleset on every branch does.
- With `lock_heads` on, while its base is in a batch, the queue's own lock on
  the base's branch blocks the stacked pull request too, and GitHub's answer
  does not say which rule blocks it. A stacked pull request outside a batch is
  then held as blocked by branch protection until its base leaves the batch;
  it is not batched on top of its base meanwhile. One already in a batch was
  ready when it was batched and is not held by the block, so a review
  dismissed on it while its base is batched does not stop it landing.
