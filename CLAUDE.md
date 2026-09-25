# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

Rust CLI (edition 2024, deps: `serde`, `serde_json`, `chrono`, `chrono-humanize`) that wraps the GitHub `gh` CLI. It shells out to `gh` via `std::process::Command` rather than calling the GitHub API directly.

Startup sequence in `src/main.rs`:
1. `gh --version` — exit 1 if `gh` is missing.
2. `gh auth status` — exit 1 and print `gh auth login` if not authenticated.
3. `gh api graphql` searching `is:pr is:open author:@me` — fetches repo (`nameWithOwner`), title, URL, `updatedAt`, `isDraft` and the head commit's `statusCheckRollup` state; JSON is parsed with serde, sorted by `updatedAt` descending, and printed as a table with relative dates via `chrono-humanize` (e.g. "2 months ago"); draft PRs (unless `--draft`, which shows them with a " (draft)" title suffix), and PRs titled `chore: release…` not updated in 30+ days, are hidden (one row per PR) with a CI column (🟢 success, 🔴 failure/error, 🟡 pending, blank if no checks). With `--actions`, the query also fetches the rollup's `contexts` (CheckRuns, prefixed with their workflow name, and StatusContexts) and prints each failed/running check with its URL indented under its PR row, failures first; cancelled checks are hidden (usually fail-fast noise) and non-plain failures get a suffix like "(timed out)". With `--watched`, the repos the user watches are fetched via REST `user/subscriptions` (faster than GraphQL's `viewer.watching`) and a second aliased search (`is:pr is:open -author:@me created:>=<3 weeks ago>` plus one `repo:` qualifier per watched repo) is added to the same GraphQL query; its PRs are merged into the table, which then gains an AUTHOR column. Fetching each watched repo's `pullRequests` through GraphQL instead times out (HTTP 504) once CI rollups are included. GraphQL is used because `gh search prs --json` can't return CI status, and a search is used instead of `gh pr list` because the latter requires being inside a repo.

`gh_wrapper merge <pr-url>` instead looks up the PR's CI and head branch/commit via a GraphQL `resource(url:)` query and, only if CI is green, runs `gh pr merge <url> --rebase --match-head-commit <oid>` (non-interactive), then deletes the remote head branch via `gh api -X DELETE repos/…/git/refs/heads/…`. `--delete-branch` isn't used because it also deletes the local branch, which we keep. Failed, running or missing CI aborts with exit 1.

`gh_wrapper rebase <pr-url>` runs `gh pr update-branch <url> --rebase`, a server-side rebase onto the latest base branch (no local checkout needed; fails on conflicts).

`gh_wrapper wait <pr-url>` polls the same `resource(url:)` query (plus title/repo) every 30s until CI succeeds or fails, following force pushes; "no checks" counts as pending for 5 minutes per head commit, then aborts. It then shows a critical desktop notification by calling the freedesktop `org.freedesktop.Notifications.Notify` D-Bus method through `gdbus` (not `notify-send`, so libnotify isn't needed), with Merge (only when green) and Open buttons. A `gdbus monitor` started before the notification catches the `ActionInvoked`/`NotificationClosed` signal for its id; Merge runs the `merge` path, Open runs `xdg-open`. Failed CI exits 1; notification errors are only warnings.

`gh_wrapper pr <branch>` refuses `main`/`master` and anything that isn't a local branch (`refs/heads/<branch>`), then runs `git push --force origin <branch>` and `gh pr create --head <branch> --fill` (non-interactive; title/body from the commits).

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
