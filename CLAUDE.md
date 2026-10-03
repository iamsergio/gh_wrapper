# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

Rust CLI (edition 2024, deps: `serde`, `serde_json`, `chrono`, `chrono-humanize`) that wraps the GitHub `gh` CLI. It shells out to `gh` via `std::process::Command` rather than calling the GitHub API directly.

Startup sequence in `src/main.rs`:
1. `gh --version` — exit 1 if `gh` is missing.
2. `gh auth status` — exit 1 and print `gh auth login` if not authenticated.
3. `gh api graphql` searching `is:pr is:open author:@me` — fetches repo (`nameWithOwner`), title, URL, `updatedAt`, `isDraft` and the head commit's `statusCheckRollup` state; JSON is parsed with serde, sorted by `updatedAt` descending, and printed as a table with relative dates via `chrono-humanize` (e.g. "2 months ago"); draft PRs (unless `--draft`, which shows them with a " (draft)" title suffix), and PRs titled `chore: release…` not updated in 30+ days, are hidden (one row per PR) with a CI column (🟢 success, 🔴 failure/error, 🟡 pending, blank if no checks). With `--actions`, the query also fetches the rollup's `contexts` (CheckRuns, prefixed with their workflow name, and StatusContexts) and prints each failed/running check with its URL indented under its PR row, failures first; cancelled checks are hidden (usually fail-fast noise) and non-plain failures get a suffix like "(timed out)". With `--watched`, the repos the user watches are fetched via REST `user/subscriptions` (faster than GraphQL's `viewer.watching`) and a second aliased search (`is:pr is:open -author:@me created:>=<3 weeks ago>` plus one `repo:` qualifier per watched repo) is added to the same GraphQL query; its PRs are merged into the table, which then gains an AUTHOR column. Fetching each watched repo's `pullRequests` through GraphQL instead times out (HTTP 504) once CI rollups are included. GraphQL is used because `gh search prs --json` can't return CI status, and a search is used instead of `gh pr list` because the latter requires being inside a repo.

`gh_wrapper merge [--force] <pr-url>` instead polls a GraphQL `resource(url:)` query for the PR's CI and head branch/commit until CI finishes (shared with `wait`, below) and, only if CI is green, runs `gh pr merge <url> --rebase --match-head-commit <oid>` (non-interactive), then deletes the remote head branch via `gh api -X DELETE repos/…/git/refs/heads/…`. `--delete-branch` isn't used because it also deletes the local branch, which we keep. Failed or missing CI aborts with exit 1, as does a PR that is already merged or closed (also when that happens while waiting; `wait` behaves the same). If CI was still unfinished at the first poll, the outcome (merged or not, and why) is also shown as a desktop notification with an Open button, like `wait`'s; if it could merge right away, there's no notification. `--force` skips CI entirely: it fetches the PR once and merges right away.

`gh_wrapper rebase <pr-url>` runs `gh pr update-branch <url> --rebase`, a server-side rebase onto the latest base branch (no local checkout needed; fails on conflicts).

`gh_wrapper rerun <url>` takes an Actions job URL (`…/actions/runs/<run>/job/<job>`, as printed by `--actions`) and runs `gh run rerun --job <job> -R <host>/<owner>/<repo>` (which also reruns the job's dependencies), or a run URL (`…/actions/runs/<run>`, optionally `/attempts/<n>`) and runs `gh run rerun <run> --failed`, or a PR URL (`…/pull/<n>`, optionally followed by a tab like `/checks`), for which it fetches the head commit's CheckRuns via a GraphQL `resource(url:)` query and runs `gh run rerun <run> --failed` for every workflow run with a failed or cancelled job (`--failed` covers cancelled jobs too), continuing past runs that can't be rerun. The URL is parsed locally; anything else, e.g. non-Actions status URLs, is rejected.

`gh_wrapper wait <pr-url>` polls the same `resource(url:)` query (plus title/repo) every 30s until CI succeeds or fails, following force pushes; "no checks" counts as pending for 5 minutes per head commit, then aborts. It then shows a critical desktop notification by calling the freedesktop `org.freedesktop.Notifications.Notify` D-Bus method through `gdbus` (not `notify-send`, so libnotify isn't needed), with Merge (only when green) and Open buttons. A `gdbus monitor` started before the notification catches the `ActionInvoked`/`NotificationClosed` signal for its id; Merge merges the head commit that passed (no re-wait), Open runs `xdg-open`. With `--no-notify` (before or after the URL) no notification is shown, only the terminal output and the exit status (0 green, 1 otherwise) report the result, so it can be used in scripts. Failed CI exits 1; notification errors are only warnings. On macOS (no D-Bus) the notification is instead an `osascript` `display alert` with the actions plus a Dismiss button, and Open runs `open`.

`gh_wrapper pr <branch>` refuses `main`/`master` and anything that isn't a local branch (`refs/heads/<branch>`), then runs `git push --force origin <branch>` and `gh pr create --head <branch> --fill` (non-interactive; title/body from the commits).

Every subcommand taking a URL (`merge`, `rebase`, `wait`, `rerun`) first runs it through `normalize_url`, which prepends `https:` to a scheme-less `//host/…` URL (macOS double-click doesn't select the `https:` part).

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
