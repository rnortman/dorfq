- The GitHub forge can run as a GitHub App: the queue's comments and merges
  appear as `<app>[bot]`, and it mints and renews its own short-lived
  installation tokens from the app's private key.
  `docs/running-on-github.md` has a section, "As a GitHub App", that walks
  through creating, permissioning and installing the app. The live lane
  accepts the app's credential as well (`DORFQ_GITHUB_APP_CLIENT_ID` with
  `DORFQ_GITHUB_APP_KEY_FILE`).
