# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

Rust CLI (edition 2024, deps: `serde`, `serde_json`) that wraps the GitHub `gh` CLI. It shells out to `gh` via `std::process::Command` rather than calling the GitHub API directly.

Startup sequence in `src/main.rs`:
1. `gh --version` — exit 1 if `gh` is missing.
2. `gh auth status` — exit 1 and print `gh auth login` if not authenticated.
3. `gh search prs --author @me --state open --json title,url,updatedAt` — JSON is parsed with serde, sorted by `updatedAt` descending, and printed as a table (one row per PR). `gh search` is used instead of `gh pr list` because the latter requires being inside a repo.

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
