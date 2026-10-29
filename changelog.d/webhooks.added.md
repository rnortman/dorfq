- The GitHub forge can receive GitHub webhooks, so a large repository no
  longer has to be polled often to react quickly. Add a `[forge.webhook]`
  table naming the address to `listen` on and the environment variable
  holding the webhook secret (`secret_env`). Each delivery's signature is
  checked, and a pull request, check, status or trunk push delivery makes
  the queue re-read what it names at once instead of at its next refresh.
  Deliveries are only hints, never trusted as facts, and the refresh
  intervals stay as the backstop for a dropped one. Webhooks are optional:
  an installation that cannot receive them keeps short intervals.
- The receiver speaks plain HTTP, one connection at a time under a
  ten-second deadline; put a reverse proxy terminating TLS in front of it.
  If it dies the queue stops with an error rather than silently falling
  back to polling. Each verified delivery logs one stderr line saying what
  it was and what the queue did with it.
