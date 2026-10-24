# Repo-root Makefile: a frontend to Bazel, the check gate, and hook wiring.
#
# Bazel is the build. Every recipe here is a spelled-out Bazel invocation (or
# git plumbing, or `local-deploy`'s install and systemctl, or `release`'s edit
# of the two version manifests), so `check` is one definition that the
# pre-commit hook and CI both run, reproducible on a fresh clone with nothing
# installed but bazelisk.

# Recipes run under bash with -e and pipefail, so a failing command anywhere in a
# chain or a pipeline fails the target. Without this a broken step degrades into a
# green result, which is the worst thing a gate can do.
SHELL := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c

.DEFAULT_GOAL := help

SCRUB := bazel run //tools/scrub:brenn-scrub --

CHANGES := bazel run //tools/allthechanges --

.PHONY: help
help:
	@echo "Repo-root targets (all of them run Bazel):"
	@echo "  make check         the gate: scrub kit, fragment lint, build, test, clippy, rustfmt, buildifier, tree scan"
	@echo "  make build         build every target"
	@echo "  make test          run every test, without the lint aspects"
	@echo "  make run           run the dorfq binary (ARGS=... for arguments)"
	@echo "  make live-test     run the GitHub forge against a disposable repository (DORFQ_GITHUB_TOKEN, DORFQ_LIVE_REPO)"
	@echo "  make optimised     build the dorfq binary optimised"
	@echo "  make release VERSION=X.Y.Z"
	@echo "                     cut a release: gather changelog.d and todo.d, bump the crate version, repin"
	@echo "  make todo-gather   fold todo.d fragments into TODO.md, outside a release"
	@echo "  make changes       run allthechanges (ARGS=\"query\", ARGS=\"todo query\", ...)"
	@echo "  make local-deploy  install the optimised binary to ~/.local/bin and restart the user unit if running"
	@echo "  make sim           run the dorfq-sim simulator (ARGS=... for arguments)"
	@echo "  make fix           rustfmt and buildifier the tree in place"
	@echo "  make repin         re-render every crate hub's lock files after a Cargo.toml change or scrub bump"
	@echo "  make ide           write rust-project.json for rust-analyzer"
	@echo "  make setup         wire git at .githooks (once per clone)"
	@echo "  make scrub-tree    whole-tree secret sweep"
	@echo "  make clean         drop Bazel's outputs for this workspace"

# The gate, in four steps. The scrub kit check is cheap and its failure the
# most important, so it runs first. The fragment lint follows: a malformed
# changelog.d or todo.d fragment fails here rather than at release time. One
# `bazel test` then carries the build of every target, every test, and (via
# the config) the clippy and rustfmt aspects over every Rust target. The tree scan runs last: it is the backstop
# that sees the whole tracked tree. Every tool is pinned in MODULE.bazel and
# fetched by checksum, so no lane can skip.
# TODO(dependency-audit-lane): add a license and advisory audit of the Rust
# dependency graph, run through Bazel.
.PHONY: check
check:
	$(SCRUB) init --check
	$(CHANGES) query
	$(CHANGES) todo query
	bazel test --config=lint //...
	$(SCRUB) tree

.PHONY: build
build:
	bazel build //...

.PHONY: test
test:
	bazel test //...

ARGS ?=
.PHONY: run
run:
	bazel run //crates/dorfq -- $(ARGS)

# The live lane: the GitHub forge against a real, disposable repository whose
# name contains "scratch" or "playground". Not part of `check`: it needs the network and a
# token, which `bazel run` leaves to the operator's environment.
.PHONY: live-test
live-test:
	bazel run //crates/dorfq-forge-github:live
.PHONY: sim
sim:
	bazel run //crates/dorfq-sim -- $(ARGS)

.PHONY: optimised
optimised:
	bazel build --config=release //crates/dorfq

# Passthrough to allthechanges for queries and dry runs:
# `make changes ARGS="todo query"`,
# `make changes ARGS="gather --version 0.1.0 --dry-run"`.
.PHONY: changes
changes:
	$(CHANGES) $(ARGS)

# Folds every todo.d fragment into TODO.md. Independent of a release; the
# tool refuses (exit 1) when there is nothing to gather.
.PHONY: todo-gather
todo-gather:
	$(CHANGES) todo gather

# Cuts release VERSION (bare X.Y.Z, given on the command line): gathers
# changelog.d into a new CHANGELOG.md section and todo.d into TODO.md, writes
# the version into Cargo.toml and crates/dorfq/BUILD.bazel, checks it back
# with the patterns //tools/pins reads them by, and re-renders the lock
# files. It starts from a clean tree, untracked files included, so
# `git checkout -- .` undoes a failure at any step; it runs no git write, so
# the result is a diff to review and commit.
.PHONY: release
release:
	@if [[ "$(origin VERSION)" != "command line" \
	      || ! "$(VERSION)" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$$ ]]; then \
	    echo "usage: make release VERSION=X.Y.Z (bare version, no v prefix)" >&2; \
	    exit 1; fi
	@if [[ -n "$$(git status --porcelain)" ]]; then \
	    echo "make release: the working tree is not clean; commit or stash first" \
	        "(untracked files count: gather would fold and delete them)" >&2; \
	    exit 1; fi
	$(CHANGES) gather --version $(VERSION)
	@if compgen -G 'todo.d/*.md' >/dev/null; then \
	    $(CHANGES) todo gather || { \
	        echo "make release: todo gather refused after CHANGELOG.md was rewritten;" \
	            "\`git checkout -- .\` restores the tree" >&2; \
	        exit 1; }; fi
	sed -i '/^\[workspace\.package\]$$/,/^\[/s/^version = "[^"]*"$$/version = "$(VERSION)"/' Cargo.toml
	sed -i 's/^VERSION = "[^"]*"$$/VERSION = "$(VERSION)"/' crates/dorfq/BUILD.bazel
	@[[ "$$(sed -n '/^\[workspace\.package\]$$/,/^\[/s/^version = "\([^"]*\)"$$/\1/p' Cargo.toml)" \
	      == "$(VERSION)" ]] || { \
	    echo "make release: [workspace.package] version in Cargo.toml is not $(VERSION);" \
	        "\`git checkout -- .\` restores the tree" >&2; \
	    exit 1; }
	@[[ "$$(sed -n 's/^VERSION = "\([^"]*\)"$$/\1/p' crates/dorfq/BUILD.bazel)" \
	      == "$(VERSION)" ]] || { \
	    echo "make release: VERSION in crates/dorfq/BUILD.bazel is not $(VERSION);" \
	        "\`git checkout -- .\` restores the tree" >&2; \
	    exit 1; }
	@$(MAKE) repin || { \
	    echo "make release: repin failed after CHANGELOG.md and the manifests were" \
	        "rewritten; rerun \`make repin\` if the failure was transient, else" \
	        "\`git checkout -- .\` restores the tree" >&2; \
	    exit 1; }
	@echo
	@echo "Release $(VERSION) is in the working tree. Next:"
	@echo "  1. review the diff (git status; git diff)"
	@echo "  2. commit it on a branch; the pre-commit make check runs //tools/pins:pins_test"
	@echo "  3. land it, tag the landed main commit v$(VERSION), and push the tag"

# Installs the optimised binary where the example user unit expects it and
# restarts the unit if it is running. Restarting is safe at any point: the
# queue recovers its state from the repository on the next observation.
.PHONY: local-deploy
local-deploy:
	bazel build --config=release //crates/dorfq
	install -D "$$(bazel cquery --config=release --output=files //crates/dorfq)" ~/.local/bin/dorfq
	systemctl --user try-restart dorfq

# Scoped to what `check` enforces that has a fixer, so `make fix && make check`
# is a clean cycle. Clippy findings are fixed by hand.
.PHONY: fix
fix:
	bazel run @rules_rust//:rustfmt
	bazel run //:buildifier

# crate_universe renders each Cargo.lock into a cargo-bazel-lock.json once and
# then trusts it. CARGO_BAZEL_REPIN re-resolves every hub the crate extension
# declares, the workspace's, tools/scrub's and tools/allthechanges's, with the
# pinned cargo; a hub whose inputs did not change renders byte-identically.
# Commit every lock file that changed.
.PHONY: repin
repin:
	CARGO_BAZEL_REPIN=1 bazel fetch --repo=@crates --repo=@scrub_crates --repo=@allthechanges_crates

# rust-analyzer's view of the build. Not part of `check`: it writes into the
# source tree, and the file it writes is ignored.
.PHONY: ide
ide:
	bazel run @rules_rust//tools/rust_analyzer:gen_rust_project

# Wire git at the tracked hooks directory through the scrub kit's own init,
# which sets core.hooksPath, removes a stale .git/hooks/pre-commit and lays
# nothing that is already present. Idempotent; run once per clone.
.PHONY: setup
setup:
	@command -v bazel >/dev/null 2>&1 || { \
	    echo "bazel not found on PATH: install bazelisk as 'bazel'; every gate runs through it." >&2; \
	    exit 1; }
	$(SCRUB) init

.PHONY: scrub-tree
scrub-tree:
	$(SCRUB) tree

.PHONY: clean
clean:
	bazel clean
