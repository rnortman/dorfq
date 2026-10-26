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

Workflows is needed because GitHub refuses a merge or a ref update that adds
or changes a file under `.github/workflows/` to a credential without it, and
the queue makes both for every queued pull request that touches CI; without
it the first such batch stops the queue. Checks is what GitHub's permission
reference names for listing a commit's check runs, which the queue does on
every observation.

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

The label is what authorises a merge. Anyone who can apply it lands the pull
request, and on GitHub the Triage role is enough. The pull request lands at
whatever head it has when its batch is built, which may include commits
pushed after it was labelled. Branch protection's review rules do not apply,
because landing is a push by the queue's own identity, the token's account or
the app. Batch CI runs the pull request's own workflow files as a push to
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
  `checks`. GitHub reads a push workflow from the pushed commit, and a batch
  branch carries trunk's workflows, so the trigger has to be on trunk before
  batch branches get checks; until then batches stay pending. dorfq's own
  `.github/workflows/ci.yml` has the trigger.

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

The live lane refuses any repository whose name contains neither `scratch`
nor `playground`, and treats the one it accepts as disposable: it closes
every open labelled pull request, deletes every queue ref, and lands commits
on `main`.

## Configuration and running

Copy `crates/dorfq/examples/github.toml` and set `owner`, `repo` and
`[forge.auth]`. `api_base` is `https://<host>/api/v3` on GitHub Enterprise;
`tick_millis` is the polling interval. The `[scheduler]` table sets the
Bayesian scheduler's parameters, each commented in the example: set `lanes`
and `max_batch` to what the repository's CI can take, and the `p_flake` and
`p_interact` priors from its history.

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
- Every scheduler batches a stack bases before children.
