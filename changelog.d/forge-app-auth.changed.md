- Breaking: the GitHub forge's credential moves into a `[forge.auth]`
  table, and a configuration with `token_env = "X"` under `[forge]` no
  longer loads. Replace that line with a `[forge.auth]` table holding
  `kind = "token"` and `env = "X"`. A GitHub App is `kind = "app"` with
  `client_id` and `private_key_file`.
- The README's token permission list gains Checks: read, which GitHub's
  permission reference names for listing a commit's check runs.
