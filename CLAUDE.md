# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

Rust CLI (edition 2024, deps: `serde`, `serde_json`, `chrono`, `chrono-humanize`) that wraps the GitHub `gh` CLI. It shells out to `gh` via `std::process::Command` rather than calling the GitHub API directly.

Startup sequence in `src/main.rs`:
1. `gh --version` — exit 1 if `gh` is missing.
2. `gh auth status` — exit 1 and print `gh auth login` if not authenticated.
3. `gh api graphql` searching `is:pr is:open author:@me` — fetches repo (`nameWithOwner`), title, URL, `updatedAt` and the head commit's `statusCheckRollup` state; JSON is parsed with serde, sorted by `updatedAt` descending, and printed as a table with relative dates via `chrono-humanize` (e.g. "2 months ago"); PRs titled `chore: release…` not updated in 30+ days are hidden (one row per PR) with a CI column (🟢 success, 🔴 failure/error, 🟡 pending, blank if no checks). GraphQL is used because `gh search prs --json` can't return CI status, and a search is used instead of `gh pr list` because the latter requires being inside a repo.

`gh_wrapper merge <pr-url>` instead looks up the PR's CI via a GraphQL `resource(url:)` query and runs `gh pr merge <url>` (stdio inherited, so gh prompts for the merge method) only if CI is green; failed, running or missing CI aborts with exit 1.

## Commands

CI (`.github/workflows/build.yml`, Linux/macOS/Windows) runs these; keep them passing:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --locked
cargo test --locked
```

Single test: `cargo test <name>`. `Cargo.lock` must be committed since CI uses `--locked`.

## Version control

This repo is managed by git-loom: use `git loom` for commits and history edits, not raw `git commit`/`rebase`/`amend`.
