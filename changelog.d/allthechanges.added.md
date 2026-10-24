- Changelog and TODO entries are now small fragment files under `changelog.d/`
  and `todo.d/`, so concurrent branches no longer conflict on `CHANGELOG.md`
  and `TODO.md`. The fragments are handled by allthechanges, a tool built
  through Bazel: `make release VERSION=X.Y.Z` gathers both kinds into
  `CHANGELOG.md` and `TODO.md` and bumps the crate version, `make todo-gather`
  folds in the TODO fragments alone (for a burndown before a release), and
  `make check` lints every fragment.
