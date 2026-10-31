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
6. Optionally, point [webhooks](#webhooks) at it, so it hears of changes as
   they happen instead of polling for them.

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
- Issues: read/write — only with `status_issue`; read otherwise
- Checks: read
- Metadata: read
- Workflows: read/write
- Administration: read/write — only with `lock_heads = true`
- Actions: read/write — only with `ci = "actions"`

Workflows is needed because GitHub refuses a merge or a ref update that adds
or changes a file under `.github/workflows/` to a credential without it, and
the queue makes both for every queued pull request that touches CI; without
it the first such batch stops the queue. Checks is what GitHub's permission
reference names for listing a commit's check runs, which the queue does each
time it reads a pull request or a testing batch.

With `status_issue` set the queue edits one issue's body with Issues: write
(see *The status issue* below). It writes that body once at start, so a
credential without the permission stops the queue at start with GitHub's
answer: `cannot start the GitHub forge: permanent GitHub error: PATCH
/issues/<n>: HTTP 403: …`.

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
   `<name>[bot]`) and any homepage URL. Under *Webhook*, activate it as
   [Webhooks](#webhooks) describes, or deactivate it and let the queue poll.
   Under *Where can this GitHub App be installed?* choose only this
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
held by the repository when the queue last read it, or when it reads it
again at the moment of landing, is not landed, even if its batch passed. With `lock_heads` on, while a pull request is in a batch its
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
`[forge.auth]`. `api_base` is `https://<host>/api/v3` on GitHub Enterprise.
`[forge.refresh]` sets how often each thing the queue reads is read again
unprompted, and `[forge.webhook]` whether GitHub's deliveries prompt it;
[Webhooks](#webhooks) says what each costs. `checks` names the checks that must
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
naming the other. A `[telemetry]` table pushes the queue's gauges and
events to an OpenTelemetry collector; [Telemetry](#telemetry) describes it.

Run it wherever outbound HTTPS is available:

```sh
make run ARGS="run --config $PWD/crates/dorfq/examples/github.toml"
```

To keep it running, `crates/dorfq/examples/systemd/` has a user unit and a
system unit; each says where it expects the binary, the configuration and
the token or the app's key. `make local-deploy` installs the binary where the
user unit expects it and restarts the unit if it is running. For a server
whose glibc the optimised build does not match, or a container image with no
libc, `make musl` builds a statically linked binary that runs on any x86_64
Linux host and prints its path, `bazel-out/k8-opt/bin/crates/dorfq/dorfq-musl`.

## Webhooks

Without webhooks the queue learns of every change by reading GitHub again:
each queued pull request every `pr_secs`, each testing batch and trunk every
`batch_secs`, the labelled pull requests and the batch records every
`listing_secs`. With webhooks GitHub tells it what moved, and it reads that
one thing at once; the intervals become a backstop. A delivery is only ever
a prompt to read: the queue believes GitHub's API, never a payload, and
ignores every delivery whose signature does not verify.

### Choosing a profile

- **Webhooks** (`examples/github.toml` as shipped): `listing_secs = 300`,
  `pr_secs = 1800`, `batch_secs = 300`, and a `[forge.webhook]` table. The
  intervals bound how long a dropped delivery goes unnoticed: GitHub does
  not retry a delivery that failed, and the queue does not ask for
  redelivery.
- **Polling** (the commented block in the example): no `[forge.webhook]`,
  and `listing_secs = 60`, `pr_secs = 120`, `batch_secs = 30`. Use it when
  GitHub cannot reach the queue, behind a firewall for instance; the queue
  then needs only outbound HTTPS. Every change reaches it within the
  intervals, at the cost of more API calls: each queued pull request costs
  about three calls every `pr_secs`.

What a stale read costs, interval by interval:

- `pr_secs`: a push to a queued head that the queue has not read yet can
  be batched at its old head; the batch then refuses to land that member
  and its run is wasted. A queued pull request's mergeability is as of its last read: one that
  trunk's latest landing put in conflict is still batched as clean until
  then, its merge onto the batch branch fails, and the queue ejects it
  before any CI runs, at the cost of one build attempt for the other
  members. A draft or a dismissed approval is always caught at landing,
  whatever the interval.
- `batch_secs`: a batch's verdict, and a push to trunk by another hand, are
  seen this late.
- `listing_secs`: a newly labelled pull request joins the pool this late.

### Setting up the hook

Generate a secret (`openssl rand -hex 32`), export it where the queue runs
under the name `secret_env` gives (`DORFQ_WEBHOOK_SECRET` in the example;
the systemd units read it from the same environment file as the
credential), and set:

```toml
[forge.webhook]
listen = "127.0.0.1:8787"
secret_env = "DORFQ_WEBHOOK_SECRET"
```

The queue refuses to start, naming the variable, if it is unset, and naming
the address if it cannot bind it.

Then point GitHub at it. The payload URL is the proxy's public URL (below);
any path reaches the queue. Set the secret above and the content type
`application/json`, and subscribe to these events:

- Pull requests
- Pull request reviews
- Check runs
- Check suites
- Pushes
- Statuses — only when a name in `checks` or `batch_checks` is a commit
  status context rather than a check run, Buildkite's for instance

For an app, this is the app's own webhook: on the app's settings page,
activate *Webhook*, fill in the URL and secret, and under *Permissions &
events → Subscribe to events* tick the events. GitHub offers an event only
once the app has the permission it belongs to; Statuses is offered with
Commit statuses: read.

For a token, which has no webhook of its own, add a repository webhook:
*Settings → Webhooks → Add webhook* on the repository, which needs admin on
it. Choose *Let me select individual events* and tick the events above.

An app's webhook receives the deliveries of every repository the app is
installed on, the live lane's throwaway repository included. The queue
answers them all and acts only on those whose repository is the configured
`owner/repo`.

GitHub sends a `ping` when the hook is saved. *Recent Deliveries* (under
the app's *Advanced* tab, or on the repository webhook's page) shows each
delivery's response: `204` means the queue verified it. `401` is a secret
that does not match, `411` a proxy that forwarded the body without its
length, `413` a body over 25 MiB; the queue writes one line on stderr for
each of these three. Nothing in `make live-test` exercises the hook: the
first real delivery the receiver sees is the one this `ping` sends.

### Following a delivery through the log

The queue writes its diagnostics to stderr and its tick lines to stdout;
a log that captures only one stream shows half the chain (`journalctl`
captures both under the shipped units). In order, a delivery leaves:

- at startup, `webhook receiver listening on <address>`;
- per verified delivery, one line with its id (the id *Recent Deliveries*
  shows), its event, its action, and either the hint it became or why it
  became none — for the `ping`, `no hint: the queue does not watch this
  event`;
- per hint, what the queue made due: a pull request, a batch or trunk,
  `due now` or `due in <n> s` when it was read less than five seconds ago,
  or that a commit matched nothing the queue holds;
- when a rate-limit pause, a backoff after a failure, the retry of a
  comment post or status edit that failed, or the five-second floor holds the next tick
  past the hints, `next tick in <n> s` and why;
- when a hinted pull request the queue did not hold is read and found not
  open or not labelled, `read; not open and labelled "<label>"; not queued`;
- then, on stdout, `message to pr <n>: <what>` for each comment the
  observation owes the author — a greeting, why the pull request is not
  enqueued, what holds it — and for each a batch started, a landing or an
  ejection posts in the same tick;
- then the tick line on stdout, printed when the observation changed.

What each line, or its absence, means:

- No `listening on` line with `[forge.webhook]` set: the process did not get
  that far; the startup error says why.
- `502` or `504` in *Recent Deliveries*: the request did not reach the
  receiver. Check the proxy's upstream; with `listen` on loopback, the proxy
  must run on the same host.
- `204` in *Recent Deliveries* and no delivery line: a receiver holding the
  secret verified it, but not the one whose log you are reading. Check
  which process listens on the proxy's upstream address — another or an
  older instance, or a binary from before these lines existed — and that
  the log captures stderr.
- `no hint: repository … is not the queue's …`: `owner` and `repo` in the
  configuration do not name the repository the hook is on.
- `no hint: the pull request does not carry the label …`: the label on the
  pull request is not `forge.label`.
- `read; not open and labelled …; not queued`: what GitHub's API reported
  for a hinted pull request the queue did not hold. After an `unlabeled` or
  `closed` delivery it is routine: the queue removes the label itself when it
  lands or ejects a pull request, and reads that pull request once more when
  the delivery's hint arrives. After a `labeled` or `opened` delivery it means
  the delivery said the pull request carried the label and the API did not;
  look at the pull request on GitHub.
- A tick line and no `message to pr` line before it: the queue observed the
  pull request and owes nothing — its thread already says why it is
  blocked, or what holds it — or the post failed transiently and is being
  retried: each failed attempt writes `kept the comment for message to pr
  <n>: …; retrying` on stderr with GitHub's error, and after a hint `next
  tick in <n> s: retrying a comment post` says when the next attempt is. A
  post GitHub answered with a rate-limit pause is kept the same way, but the
  tick's effects wait for the pause (see *While it runs*). A post GitHub
  refused for good is `dropped the comment for …` on stderr.

### The reverse proxy

The receiver speaks plain HTTP and serves one connection at a time. Bind it
to loopback or a private address and put a reverse proxy in front that
terminates TLS. With `nginx`:

```nginx
location /dorfq/webhook {
    client_max_body_size 25m;
    client_body_timeout 10s;
    proxy_pass http://127.0.0.1:8787;
}
```

With `caddy`:

```caddyfile
handle /dorfq/webhook {
    request_body {
        max_size 25MB
    }
    reverse_proxy 127.0.0.1:8787
}
```

with the read timeout set in the server's global options
(`servers { timeouts { read_body 10s } }`).

The receiver requires a `Content-Length` and answers a chunked body `411`,
so the proxy must forward the body with its length. `nginx` does by
default: leave `proxy_request_buffering` at `on`. `caddy` forwards the
length it received; there is nothing to set.

The body limit and the body timeout protect the receiver's time, not its
life. It already refuses any declared body over 25 MiB before reading it and
drops any connection still unfinished ten seconds after it was accepted, so
nothing a client sends can stop the queue. But a client that holds a
connection for those ten seconds delays every delivery queued behind it,
and GitHub gives up on a delivery after ten seconds; the proxy drops such
traffic before it reaches the receiver. A receiver reachable without the
proxy can be slowed, so that the queue falls back on its intervals, but not
stopped.

If the receiver's thread ever stops, the queue stops with it, with the
cause on stderr: a queue configured for webhooks does not run on without
them.

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
- While a rate-limit pause GitHub announced is in force, the queue writes
  nothing: no merge, branch update, comment or status edit. A comment post,
  or with a status issue a status edit, answered with a pause is kept
  (`kept the …` on stderr). When that tick's plan has an effect that
  writes, it fails at its first write: after the tick line, stderr carries
  `dorfq: transient forge error: rate limited until unix time <t>; nothing
  is written before then; retrying`, and the tick's journal line carries
  that error. A notice is kept instead, with `kept the comment for …` on
  stderr, and posted when the pause ends. A hint that arrives during the
  pause writes `next tick in <n> s: rate-limit pause`. The kept comment and
  body go out at the first tick after the pause, and that tick plans again.
  A landing interrupted by a pause is finished at the next tick, so a
  stacked pull request it had moved onto trunk stays there until then.
- A queue that stops with "batch <id>'s tip is its base" is tracking a batch
  that an earlier build published at its own base, because every member was
  already in trunk. Stop the queue, delete `refs/dorfq/batches/<id>` and the
  branch `dorfq/batch/<id>`, and start it again; the batch's members that are
  still labelled rejoin the queue.

## The journal

The repository forgets: a retired batch's record and branch are deleted, and
nothing in it says when anything happened. The journal is the queue's own
record. Set `journal = "<path>"` at the top of the configuration, above
`[forge]`; a relative path is resolved against the configuration file's
directory. With it set, `dorfq run` appends every tick to that file as one
JSON line: the tick's observation and plan when either says something new, a
short `same` line when the tick saw nothing new and planned nothing, and the
error when a tick failed, including the fatal one the process exits on.
Each line is flushed before the queue waits, so a crash loses at most the
tick in progress. Without the key nothing is written.

At start the queue reads the current segment once, line by line, so that
the event stream it derives as it runs, and the status issue, continue from
what the segment recorded. The scheduler is still built fresh, as it is without a journal:
nothing it decided is recovered. A restart appends to the file: each
process begins its lines with a header naming the dorfq version, the forge
and the `[scheduler]` settings it ran with. A last line cut off by a crash is
cut back at the next start, with a line on stderr saying so. A file at the
path that dorfq did not write stops the run at start, before any API call,
and is left as it was; point `journal` elsewhere. A segment with a line that
does not read stops the run at start the same way, naming the path and the
line, as `dorfq journal` refuses it; point `journal` elsewhere or repair the
file.

The current file is one *segment*. A segment is closed and a fresh one
opened at the same path when the queue goes idle after work — the tick that
sees no pull requests and no batches left is the closed segment's last line,
and the fresh segment holds only its header until the next tick — or when it
passes 64 MiB. A closed segment is renamed `<path>.<opened>`, `<opened>`
being the Unix time its first header was written; a name already taken gets
`.1`, `.2` and so on, so a closed segment is never overwritten. A segment
that began just after an idle tick and was closed at idle holds every pull
request and batch it mentions from arrival to departure. Any other segment
continues from, or into, its neighbours: read it with them.

dorfq never deletes a closed segment: they are the record, and retention is
yours. A cron job that deletes the oldest of `<path>.*`, or moves them out
of the journal's directory to compress or archive them there, does it;
leave `<path>` itself alone. `dorfq journal` reads only segments as dorfq
wrote them, so keep compressed ones out of the glob below. A `logrotate`
rule renames the files it rotates, and its names collide with dorfq's own
`.1`, `.2` suffixes; do not point one at the journal.

An upgrade that changes the journal's format closes a segment written in the
old format as a rotation would, with the events and capture files, opens fresh ones, and
says so on stderr; the run does not stop. The new build's `dorfq journal`
still reads the old segment. A segment in a format newer than the build reading it is refused,
naming the formats that build reads.

### The events file

Set `events = "<path>"` beside `journal`, resolved the same way, and the
queue also writes the event stream as it runs: each tick's events, one JSON
object per line, exactly the lines `dorfq journal events` prints. `events`
without `journal` is refused at load, and an `events` path that is the
journal's own file, under any name, stops the run at start. Each tick's events are appended and
flushed after its journal line, before the queue waits; the file has no
header. It rotates with the journal: when a journal segment is closed as
`<path>.<opened>`, the events file is closed as `<events>.<opened>` with the
same `<opened>`, and a fresh one is opened empty. When either name is
taken, both get the first `.1`, `.2` suffix free for both, so the names
always pair. A tailing reader sees appends, then a rename at rotation.

The events segments, concatenated in order, are `dorfq journal events` over
the journal segments read together. One events segment is not by itself the
stream of the journal segment beside it: it continues the one before, as a
journal segment does, so a pull request queued across a rotation has its
`arrive` in an earlier events segment, while `dorfq journal events` over the
later journal segment alone would say it arrived there. The file begins
where `events` was first set; what the journal recorded before is only in
the journal.

A process continues the stream from the current segment alone. When it
starts on a segment that has no observation yet — the queue stopped after a
rotation and before its next tick — what came before the rotation is not
read: after an idle rotation nothing is queued and the stream continues
exactly, but after a 64 MiB rotation the pull requests still queued arrive
again, the batches it adopts start again, and those gone meanwhile get no
`lost`, as `dorfq journal events` over that segment alone says. The segment
opened fresh after a format change starts the stream the same way. Across
such a restart the events segments and `dorfq journal events` over the
journal segments read together differ; the journal is the record.

An event is written once it has a time. The error of a tick that failed
after a start or a rotation and before the next observation, and the
`restart` it follows, take the time
of the next observation, so they appear when a process next observes — the
one after a restart, if this one exits first. Until then the
events file lacks them; `dorfq journal events` over the journal places them
at the last observation's time. An events file whose last line was cut off
by a crash is cut back at the next start, with a line on stderr. A failure to
write or rename the events file stops the queue, as a journal failure does.
The tick's journal line is written first, so the events of the tick that
failed, or those after the last line written, are only in the journal: no
later process writes them, and `dorfq journal events` is the authority.
Retention is yours, as for the journal: the same job can treat `<events>.*`
as it treats `<path>.*`.

The journal holds pull request numbers, commit shas, check names, batch ids,
the reasons the queue gave and its scheduler settings: no titles, no logins,
nothing from the credential.

### The capture file

Set `capture = "<path>"` beside `journal`, resolved the same way, and the
queue records what each pull request head changes, read from GitHub when
the queue first sees the head: one JSON object per line, keyed by the pull
request's number and the head's sha. A head is first seen at its `arrive`
or `repush` event. `capture` without `journal` is refused at load, and a
`capture` path that is the journal's or the events file's own file stops
the run at start. The file has no header; each line carries its format
number, `capture`, now `1`:

```json
{"capture":1,"pr":42,"head":"<40 hex>","t":1730000000,"at":1730000003,"base":"<40 hex>","additions":120,"deletions":7,"changed_files":3,"files":[{"path":"crates/x/src/lib.rs","additions":100,"deletions":7,"status":"modified"},{"path":"docs/new.md","additions":20,"deletions":0,"status":"renamed","previous":"docs/old.md"}]}
{"capture":1,"pr":43,"head":"<40 hex>","t":1730000000,"at":1730000003,"error":"GET /pulls/43: HTTP 404: Not Found"}
```

`pr` and `head` are the key. `t` is the forge time of the event that first
showed the head, so a record joins the `arrive` or `repush` at the same
`t`; `at` is the host time of the tick that read it, the `at` of that
tick's journal line, so every record a tick writes carries the same `at`.
`base`, `additions`, `deletions` and `changed_files` are what
`GET /pulls/{n}` reported at the read: the
base branch's sha and the pull request's totals. `files` is
`GET /pulls/{n}/files`, in the order GitHub lists it; `status` is GitHub's
word (`added`, `removed`, `modified`, `renamed`, `copied`, `changed`,
`unchanged`), and `previous`, present only on a renamed or copied file, is
the path before. GitHub lists at most 3,000 files: a `changed_files` above
the length of `files` means the listing was cut there. A record holds
paths, counts, shas and times: no titles, no logins.

Before listing the files the queue reads the pull request and checks that
its head is still the head first seen; the listing names no sha, so it
would otherwise be another head's. A head found moved gets no record and
one line on stderr; the journal's `repush` of the new head records the
move, and the new head gets its own record. The moved head is read again
only when its segment shows it arriving again: a `repush` back to it, or a
restart's replay of its `arrive` or `repush`. A push between the check and
the listing, milliseconds apart, puts the new head's files under the old
key; the new head's own record, with the same files under another key, is
the tell.

A read GitHub refuses for good — a pull request it no longer serves, or a
response without one of the three totals — is recorded with `error` in
place of `base` … `files`, the failure's text, and a line on stderr, and is
not asked again in that file. A read that fails transiently, a rate limit
included, leaves the head owed, prints a line on stderr ending `retrying`,
and ends that tick's reading; the next tick starts with the head after it,
so a head that keeps failing never holds back the others. A rate limit's
pause is waited out before the next tick, as for every other read. The
queue reads at most eight heads a tick, after the tick's journal line,
events, status write and export, and only after a tick whose own reads and
writes all succeeded: a tick that failed, permanently or transiently, reads
none, so a lost permission or a rate limit the tick ran into costs no
capture request and no record. The rest wait for the next tick, so a
backlog — `capture` first set against a journal that already has a
segment, or a restart onto a segment opened by a 64 MiB rotation — drains
over the following ticks. The heads the current segment showed arriving
before the start and the capture file lacks are read at the first ticks
after it, so a crash between a head's sighting and its record loses
nothing.

The capture file rotates with the journal exactly as the events file does:
closed as `<capture>.<opened>` beside `<path>.<opened>` under the same
suffix, and opened fresh and empty. A head is recorded at most once per
capture file, and the file is fresh whenever its journal segment is, so
the heads a fresh segment shows arriving are read again into the fresh
file. In practice: a restart onto a segment opened by a 64 MiB rotation, or
after a format change, reads the still-queued heads again; a pull request
queued again with the same head after an idle rotation is read again; a
head first seen in a tick that rotates, or still owed across a rotation, is
recorded in the next file; and a head still owed when an idle rotation
ends its pull request's segment is lost if the process then stops, except
at the exit `--exit-when-idle` makes: that exit first reads every head
still owed, past the eight a tick, until none is left or a read fails
transiently, and prints a line for each head it leaves unread. A
reader of several capture files dedups by `(pr, head)`. A last line cut
off by a crash is cut back at the next start, with a line on stderr, and
its head read again; a line without `pr` and `head` stops the run at
start, naming the path and the line. A failure to write or rename the
capture file stops the queue. Retention is yours, as for the events file.

Each head costs `1 + ceil(files / 100)` requests, at most eight heads a
tick, and nothing otherwise in a running process. After a restart, each
head of the current segment that was found moved and so has no record
costs one `GET /pulls/{n}`, at each restart until the segment rotates,
within the same eight a tick. Every head the segment shows superseded — a
`repush` of the same pull request followed it, as for every head of a
landed pull request but the last — is such a head. The mock forge answers
every read with one file, `pr/<n>`, one line added, under an all-zero base.

The stderr lines, none for a record of files; a tick prints at most one
per head, and the exit's line for a head it leaves unread may follow that
head's `retrying`:

- `capture of #<pr> at <head>: <error>; retrying`
- `capture of #<pr> at <head> recorded as failed: <error>`
- `capture of #<pr> at <head> not read: the head moved to <now>`
- `capture of #<pr> at <head> not read before the exit`, at the
  `--exit-when-idle` exit only

## Reading the journal

`dorfq journal` reads segments, in the order given. For a queue that has
rotated, that is the closed segments in name order and then the current one:

```sh
dorfq journal metrics queue.journal.* queue.journal
```

- `dorfq journal events <segment>...` prints what happened as one JSON object
  per line: arrivals, pushes, holds, CI on pull requests and batches,
  landings, ejections and their reasons, notices, trunk moves, restarts and
  errors. Times are the forge's, in seconds; a pull request that leaves and
  comes back under the same number is a new arrival.
- `dorfq journal metrics [--json] <segment>...` prints a summary over the
  whole input, one row per figure, or one JSON object with `--json`: counts
  of arrivals, landings, ejections, withdrawals and the rest; batch runs and
  their run time; how much of it went on batches that never landed; and
  percentiles of the time from arrival to landing, from arrival to ejection,
  of batch runs and of pull requests' own CI. A batch whose build conflicted
  never ran CI and counts in no run figure. A batch's verdict is read at its
  next refresh, so run times are as fine as `[forge.refresh]` makes them.
- `dorfq journal report [--name <name>] <segment>...` prints a document the
  simulation page opens through its file picker: the summary as the metric
  table and the events as the replay. The page has no truth for a real
  queue, so it says "truth unknown" where it would say whether a pull request
  was broken or an ejection justified.

A segment that is not a journal, or a line that does not read, stops the
command with exit 1 and `dorfq: <path>:<line>: …` on stderr. The current
segment's last line may be a tick still being written; it is skipped, with a
line on stderr.

## The status issue

The queue can keep one issue's body as its status, for anyone who can read
the repository's issues. Create an issue — any title; the title is yours —
and set its number under `[forge]`:

```toml
journal = "dorfq.journal"

[forge]
# ...
status_issue = 12
```

`status_issue` needs `journal`: the body is drawn from the event stream the
queue derives as it journals. It is refused at load without `journal`, as 0,
and under the mock forge. The credential needs Issues: read/write. At start
the queue reads the issue and stops, naming it, if there is no such issue or
the number is a pull request's; it then writes a starting body saying the
queue has started and has not observed the repository yet. A failure of
either stops the queue at start with GitHub's answer, before the first tick.

The body begins with the marker line `<!-- dorfq: status -->` and shows:

- one line: the dorfq version, when this process started, when the body last
  changed, and how many pull requests are queued and batches in flight;
- **Queue**: each queued pull request, oldest arrival first, linked by
  number, the pull request it is stacked on, its status as the tick line
  words it (`ready`, `waiting:<holds>`, `conflicting:<with>`), how long it
  has been in that status, how long it has been queued and the batch
  holding it;
- **Batches in flight**: each batch, what it was built on, its members, its
  state and how long it has run;
- **Last 24 hours**: landings, ejections, refusals and runs passed, failed
  and cancelled; the median and 95th percentile of batch run time and of the
  time from arrival to landing; and the events a reader cares about, newest
  first — landings, ejections and their reasons, failed runs, refused
  landings, notices, pull requests merged outside the queue, withdrawn, left
  or lost, batches that could not be built or were cancelled, trunk moved by
  another hand, restarts and errors. A figure counts what ended in the last
  24 hours of the forge's clock.

Times are UTC, the date left out on the day of the last change. No titles,
no logins and nothing from the credential are written: GitHub shows a
linked pull request's title on hover.

The body is rewritten only when what it shows changes — an arrival, a push,
a status change, a batch starting, a verdict, a landing, an ejection, a
notice, an error, an event leaving the 24-hour window — and at most once in
30 seconds: a change inside that interval is written at the first tick after
it, and only the newest body is written. Nothing is written during a
rate-limit pause GitHub announced; the newest body goes out at the first
tick after the pause ends. The queue owns the body: an edit by hand is
overwritten at the next change. Edit the title instead.

A failed edit never stops the queue. A transient failure keeps the body and
retries it with the comment outbox's backoff, 5 seconds doubling to a
minute, a newer body taking its place: `dorfq: kept the status body for
issue #<n>: <error>; retrying` on stderr; the queue wakes for the retry,
and a hint arriving before it writes `next tick in <n> s: writing the status
issue`. Any other failure —
the issue deleted or transferred, issues disabled, the body refused — drops
the body: `dorfq: dropped the status body for issue #<n>: <error>`; the next
change tries again.

The body says when it last changed, not whether the queue is running: a
queue that stopped leaves its last body in place. A fatal error is not
written to it — the error is in the journal and on stderr, and the next
process's body shows it — and a change held by the 30-second limit when the
queue stops is never written. A quiet queue changes
nothing, so a body hours old is not by itself a sign the queue has stopped.

After a restart the body continues from what the current journal segment
recorded: each queued pull request keeps its arrival and the time it entered
its status, each batch its start, and the window the events since the
segment began. The segment begins at the last time the queue went idle, or
at the last 64 MiB rotation, so a restart forgets what happened before
that; while that is inside the window the body says `the record begins
<time>; earlier events are in the closed journal segments`, and `dorfq
journal` over the closed segments has the rest. After a 64 MiB rotation a
pull request queued across it reads as queued from the rotation.

GitHub accepts a body of at most 65,536 characters, and the queue keeps its
own under 60,000. A body over that loses, in order, the oldest rows of the
event table, then the event table itself (the figure lines stay), then the
queue table's newest rows, replaced by `… and N more`. The batches table is
never cut.

## Telemetry

The queue can push its gauges and its events to an OpenTelemetry collector
over OTLP/HTTP, as protobuf (the encoding every OTLP/HTTP collector accepts)
or as JSON:

```toml
journal = "dorfq.journal"

[telemetry]
metrics_endpoint = "http://127.0.0.1:4318/v1/metrics"
logs_endpoint = "http://127.0.0.1:4318/v1/logs"
interval_secs = 60
encoding = "protobuf"
authorization_env = "DORFQ_OTLP_AUTHORIZATION"
```

- `metrics_endpoint` and `logs_endpoint` are the complete URLs the metrics
  and the logs requests are posted to, `http://` or `https://`, used as
  given: nothing is appended. The OpenTelemetry Collector takes both on one
  port, `http://host:4318/v1/metrics` and `http://host:4318/v1/logs`; a
  collector that serves the two signals from different ports or paths
  (VictoriaMetrics at `/opentelemetry/v1/metrics`, VictoriaLogs at
  `/insert/opentelemetry/v1/logs`) gets each URL as it documents it.
- `interval_secs` is the time between exports, from 1 to 86400 (a day).
- `encoding`, optional, is `protobuf` (the default; bodies are posted as
  `application/x-protobuf`) or `json` (`application/json`). Protobuf is the
  encoding the OTLP/HTTP specification requires every collector to accept;
  `json` is for a collector that only takes OTLP/JSON.
- `authorization_env`, optional, names the environment variable holding the
  `Authorization` header's whole value: `Basic <base64>` for Grafana Cloud,
  `Bearer <token>` elsewhere. Absent, no header is sent. Over `http://` the
  header travels unencrypted, so a collector reached across a network you do
  not control, a hosted one, needs `https://`.

`[telemetry]` needs `journal`: the gauges and the events come from the
stream the queue derives as it journals. It works under either forge.

A configuration the queue cannot use stops it at load: `telemetry needs
journal`; `telemetry.metrics_endpoint "<url>" is not a URL: …`, `… is not
an http:// or https:// URL`, `… has no host` or `… has an invalid port`;
`telemetry.metrics_endpoint carries credentials; use authorization_env` or
`telemetry.metrics_endpoint has a query or fragment` (an endpoint that could
hold a secret is not repeated), and the same for `telemetry.logs_endpoint`; `telemetry.interval_secs is 0` or `… is above 86400`;
`telemetry.authorization_env is empty`. `encoding` set to anything else is
refused by the parser: ``unknown variant `xml`, expected `protobuf` or
`json` ``. At start, before the forge is
reached, the variable `authorization_env` names must be set:
`telemetry.authorization_env <var> is unset, empty or not UTF-8`, or
`… holds a value that is not a valid HTTP header value` for one with a
trailing newline or another byte a header cannot carry. Neither message
shows the value. Whether the collector answers is not checked at start.

### The metrics

Every export carries these, in the scope `dorfq`, under the resource
`service.name = dorfq`, `service.version = <the dorfq version>`:

| Metric | Unit | Attributes | Value |
|---|---|---|---|
| `dorfq_queue_prs` | `{pull_request}` | `status`: `ready`, `waiting`, `conflicting` | pull requests queued, by status |
| `dorfq_batches` | `{batch}` | `state`: `testing`, `passed`, `failed`, `landed`, `refused`, `conflict` | batches in flight, by state |
| `dorfq_oldest_queued_seconds` | `s` | | how long the oldest queued pull request has been queued; 0 when none is |
| `dorfq_window_landed_prs` | `{pull_request}` | `window_seconds` | pull requests landed |
| `dorfq_window_landed_batches` | `{batch}` | `window_seconds` | batches landed |
| `dorfq_window_ejected_prs` | `{pull_request}` | `window_seconds` | pull requests ejected |
| `dorfq_window_refused_landings` | `{batch}` | `window_seconds` | landings GitHub refused |
| `dorfq_window_runs` | `{run}` | `verdict`: `passed`, `failed`, `cancelled`; `window_seconds` | batch runs ended |
| `dorfq_window_batch_run_seconds` | `s` | `quantile`: `0.5`, `0.95`; `window_seconds` | batch run time |
| `dorfq_window_land_seconds` | `s` | `quantile`; `window_seconds` | time from arrival to landing |
| `dorfq_tick_age_seconds` | `s` | | time since the queue last observed the repository |
| `dorfq_queue_blocked_prs` | `{pull_request}` | | pull requests labelled but not enqueued, told they are blocked; GitHub only |
| `dorfq_api_rate_limit_remaining` | `{request}` | | the last `x-ratelimit-remaining` GitHub sent for a repository request; GitHub only |
| `dorfq_telemetry_export_failures_total` | `{request}` | | export requests that failed, since start (a cumulative sum) |
| `dorfq_telemetry_events_dropped_total` | `{event}` | | events dropped from a full buffer, since start (a cumulative sum) |

The status of a queued pull request and the state of a batch are those the
status issue shows, without what follows the `:`. The `window_` figures are
the status issue's last-24-hours figures, and `window_seconds` is 86400 on
each: a figure counts what ended in the trailing 24 hours of the forge's
clock, and a run or a lifetime still open is in none. The pool gauges are
not windowed. Every status, state and verdict is sent, zero included, so
zero is never confused with absent; a metric or point is absent only when
its value is unknown: a percentile with no samples, `dorfq_tick_age_seconds`
before the first observation, the two GitHub-only gauges under the mock
forge, and the rate limit before the first repository request.

With `capture` set, the capture file spends `1 + ceil(files / 100)`
requests of the rate limit per new pull request head, at most eight heads
a tick, and nothing otherwise while the process runs; a restart re-asks
the current segment's heads found moved, see [the capture
file](#the-capture-file).

A metric's time is the host time of the export that carries it. The queue's
figures change only when it ticks, so between ticks each export repeats
them with its own time, and `dorfq_tick_age_seconds` says how old they are.
No metrics are sent before the first tick that observes the repository.

### The log records

Every event the events file would hold is sent as one log record, in order:
its body is the events file's line, its time the event's own (the forge's
clock), its observed time the host time of the tick that produced it. Its
attributes are `kind`, and `pr` and `batch` when the event names them. An
`error` event is severity `ERROR`, `FATAL` when fatal; every other event
`INFO`. Events the queue replays from the journal at start are not sent
again.

### Delivery

The queue never waits on the collector: a thread of its own exports every
`interval_secs`, whatever the queue is doing, and once more when the queue
exits, idle or fatal. An export is one metrics request, then the
pending log records in requests of 1,000. A request is bounded by 10
seconds; any answer but a 2xx is a failure, and a redirect is not followed.

A failed request is counted in `dorfq_telemetry_export_failures_total` and
tried again at the next interval; a refused log request keeps its records
and the ones after it for then. Each signal says it once on stderr when it
starts failing, `dorfq: telemetry: logs export failed: <error>; retrying
every 60s`, with a refused answer's status and its first 200 bytes on one
line, and once when it recovers, `dorfq: telemetry: logs export resumed
after <n> failed requests`. A collector that takes metrics and refuses logs
is reported for logs alone, once. A refusal whose body
is not text (a protobuf `Status`, for instance) is noted as `HTTP <status>:
<type> body omitted`, its media type (its first 200 characters) in place
of its bytes; one with an empty media type is quoted as one with none.

Up to 10,000 events wait for delivery. Past that the oldest are dropped and
counted in `dorfq_telemetry_events_dropped_total`, with one line, `dorfq:
telemetry: 10000 events pending; dropping the oldest until the collector
takes them`; when log records are next delivered the count is written,
appended to the `resumed` line or as `dorfq: telemetry: <m> events dropped
while the buffer was full`. Every event stays in the events file whatever
the collector does.

The export at exit can delay the exit. Against an unreachable collector it
waits for the export in flight, if any, and then the final one: up to four
request timeouts, 40 seconds. A slow collector working through a full
buffer can extend this to a few minutes: each export is one metrics request
and up to 10,000 / 1,000 log requests, each up to 10 seconds.

A stop by signal runs no export: `systemctl stop` and `restart`, and so
`make local-deploy`, end the process at once, during an exit export or not.
The events not yet delivered reach the events file only, uncounted in
`dorfq_telemetry_events_dropped_total`, and the next start does not send
them: up to `interval_secs` of log records per stop, more while the
collector is failing.

### What to alert on

- `dorfq_tick_age_seconds` above a few refresh intervals: the queue no
  longer observes the repository — observing fails, or the loop is stuck.
- No samples for two intervals: the process has stopped, by a stop or on a
  fatal error: a scheduler error or a permanent forge error.
- Log records with `kind = error`: a tick that failed. A failure after the
  queue observed, such as a merge or a CI start GitHub refused, does not age
  `dorfq_tick_age_seconds`; these records are its signal.
- A rising `dorfq_telemetry_export_failures_total`: the collector, not the
  queue.

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
