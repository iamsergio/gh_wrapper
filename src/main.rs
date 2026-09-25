use std::io::{BufRead, BufReader};
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use chrono_humanize::HumanTime;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq)]
enum CiStatus {
    Success,
    Failure,
    Running,
    /// No checks configured for the PR's head commit.
    None,
}

impl CiStatus {
    fn from_rollup_state(state: Option<&str>) -> Self {
        match state {
            Some("SUCCESS") => Self::Success,
            Some("FAILURE" | "ERROR") => Self::Failure,
            Some("PENDING" | "EXPECTED") => Self::Running,
            _ => Self::None,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Success => "🟢",
            Self::Failure => "🔴",
            Self::Running => "🟡",
            // Two spaces: the same terminal width as the emoji, so columns stay aligned.
            Self::None => "  ",
        }
    }
}

#[derive(Debug, PartialEq)]
struct PullRequest {
    repo: String,
    author: String,
    title: String,
    url: String,
    updated_at: String,
    ci: CiStatus,
    is_draft: bool,
    /// Failed or running checks; only fetched with `--actions`.
    checks: Vec<Check>,
}

#[derive(Debug, PartialEq)]
struct Check {
    name: String,
    ci: CiStatus,
    url: String,
}

// Mirrors the shape of the GraphQL response in `list_open_prs`.
#[derive(Deserialize)]
struct Response {
    data: Data,
}

#[derive(Deserialize)]
struct Data {
    search: Search,
    /// Absent unless the query asked for it.
    watched: Option<Search>,
}

#[derive(Deserialize)]
struct Search {
    nodes: Vec<PrNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrNode {
    repository: Repository,
    /// Null for deleted accounts.
    author: Option<Author>,
    title: String,
    url: String,
    updated_at: String,
    is_draft: bool,
    commits: Commits,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    name_with_owner: String,
}

#[derive(Deserialize)]
struct Author {
    login: String,
}

#[derive(Deserialize)]
struct Commits {
    nodes: Vec<CommitNode>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: Commit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    status_check_rollup: Option<Rollup>,
}

#[derive(Deserialize)]
struct Rollup {
    state: String,
    /// Absent unless the query asked for it.
    contexts: Option<Contexts>,
}

#[derive(Deserialize)]
struct Contexts {
    nodes: Vec<CheckContext>,
}

// GitHub Actions jobs are CheckRuns; StatusContexts come from external CI via the statuses API.
#[derive(Deserialize)]
#[serde(tag = "__typename", rename_all_fields = "camelCase")]
enum CheckContext {
    CheckRun {
        name: String,
        status: String,
        conclusion: Option<String>,
        details_url: Option<String>,
        check_suite: Option<CheckSuite>,
    },
    StatusContext {
        context: String,
        state: String,
        target_url: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckSuite {
    workflow_run: Option<WorkflowRun>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRun {
    database_id: u64,
    workflow: Workflow,
}

#[derive(Deserialize)]
struct Workflow {
    name: String,
}

impl From<CheckContext> for Check {
    fn from(context: CheckContext) -> Self {
        match context {
            CheckContext::CheckRun {
                name,
                status,
                conclusion,
                details_url,
                check_suite,
            } => {
                let ci = match (status.as_str(), conclusion.as_deref()) {
                    ("COMPLETED", c) if is_passing(c) => CiStatus::Success,
                    // Usually fail-fast cancelling sibling jobs after one failed, so it's noise.
                    ("COMPLETED", Some("CANCELLED")) => CiStatus::None,
                    ("COMPLETED", _) => CiStatus::Failure,
                    _ => CiStatus::Running,
                };
                // Job names like "build" are ambiguous across workflows, so prefix the workflow.
                let mut name = match check_suite.and_then(|s| s.workflow_run) {
                    Some(run) => format!("{} / {name}", run.workflow.name),
                    None => name,
                };
                // A plain failure needs no explanation; the rarer conclusions do.
                if let (CiStatus::Failure, Some(c)) = (ci, conclusion.as_deref())
                    && c != "FAILURE"
                {
                    name = format!("{name} ({})", c.to_lowercase().replace('_', " "));
                }
                Self {
                    name,
                    ci,
                    url: details_url.unwrap_or_default(),
                }
            }
            CheckContext::StatusContext {
                context,
                state,
                target_url,
            } => Self {
                name: context,
                ci: CiStatus::from_rollup_state(Some(&state)),
                url: target_url.unwrap_or_default(),
            },
        }
    }
}

fn is_passing(conclusion: Option<&str>) -> bool {
    matches!(
        conclusion,
        Some("SUCCESS" | "NEUTRAL" | "SKIPPED" | "STALE")
    )
}

impl Commits {
    fn ci_status(&self) -> CiStatus {
        let state = self
            .nodes
            .first()
            .and_then(|c| c.commit.status_check_rollup.as_ref())
            .map(|r| r.state.as_str());
        CiStatus::from_rollup_state(state)
    }

    fn into_contexts(self) -> Vec<CheckContext> {
        self.nodes
            .into_iter()
            .next()
            .and_then(|c| c.commit.status_check_rollup)
            .and_then(|r| r.contexts)
            .map_or_else(Vec::new, |c| c.nodes)
    }

    fn failed_or_running_checks(self) -> Vec<Check> {
        let mut checks: Vec<Check> = self
            .into_contexts()
            .into_iter()
            .map(Check::from)
            .filter(|c| matches!(c.ci, CiStatus::Failure | CiStatus::Running))
            .collect();
        // Stable sort: failures first, otherwise keep GitHub's order.
        checks.sort_by_key(|c| c.ci != CiStatus::Failure);
        checks
    }

    /// Actions runs with a failed or cancelled job, in GitHub's order.
    fn failed_run_ids(self) -> Vec<u64> {
        let mut ids = Vec::new();
        for context in self.into_contexts() {
            if let CheckContext::CheckRun {
                status,
                conclusion,
                check_suite:
                    Some(CheckSuite {
                        workflow_run: Some(run),
                    }),
                ..
            } = context
                && status == "COMPLETED"
                && !is_passing(conclusion.as_deref())
                && !ids.contains(&run.database_id)
            {
                ids.push(run.database_id);
            }
        }
        ids
    }
}

impl From<PrNode> for PullRequest {
    fn from(node: PrNode) -> Self {
        Self {
            ci: node.commits.ci_status(),
            repo: node.repository.name_with_owner,
            author: node.author.map_or_else(|| "ghost".to_string(), |a| a.login),
            title: node.title,
            url: node.url,
            updated_at: node.updated_at,
            is_draft: node.is_draft,
            checks: node.commits.failed_or_running_checks(),
        }
    }
}

// GraphQL rather than `gh search prs`, whose --json can't return CI status.
// Search works across all repos, unlike `gh pr list` which needs a repo context.
const QUERY: &str = "query {
  search(query: \"is:pr is:open author:@me\", type: ISSUE, first: 100) {
    nodes { ...Pr }
  }
  WATCHED
}
fragment Pr on PullRequest {
  repository { nameWithOwner }
  author { login }
  title
  url
  updatedAt
  isDraft
  commits(last: 1) { nodes { commit { statusCheckRollup { state CONTEXTS } } } }
}";

/// How recently a watched repo's PR must have been opened to be listed.
const WATCHED_MAX_AGE: TimeDelta = TimeDelta::weeks(3);

/// Search has no "repos I watch" qualifier, so they're listed as `repo:` qualifiers.
/// Fetching each watched repo's PRs through GraphQL instead times out (HTTP 504).
fn watched_search(repos: &[String], now: DateTime<Utc>) -> String {
    let since = (now - WATCHED_MAX_AGE).format("%Y-%m-%d");
    let repos: String = repos.iter().map(|r| format!(" repo:{r}")).collect();
    format!(
        "watched: search(query: \"is:pr is:open -author:@me created:>={since}{repos}\", \
         type: ISSUE, first: 100) {{ nodes {{ ...Pr }} }}"
    )
}

/// Full names (owner/repo) of the repos the user watches. REST rather than GraphQL's
/// `viewer.watching`, which is twice as slow.
fn watched_repos() -> Result<Vec<String>, String> {
    let output = Command::new("gh")
        .args(["api", "user/subscriptions?per_page=100", "--paginate"])
        .args(["--jq", ".[].full_name"])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("failed to run gh: {e}"))?;
    if !output.status.success() {
        return Err("`gh api user/subscriptions` failed".to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

// Substituted into QUERY for `--actions`. Plain text substitution rather than
// `@include(if: $var)` because `gh_graphql` only passes string variables.
const CONTEXTS: &str = "contexts(first: 100) {
  nodes {
    __typename
    ... on CheckRun {
      name status conclusion detailsUrl
      checkSuite { workflowRun { databaseId workflow { name } } }
    }
    ... on StatusContext { context state targetUrl }
  }
}";

const PR_INFO_QUERY: &str = "query($url: URI!) {
  resource(url: $url) {
    ... on PullRequest {
      title
      repository { nameWithOwner }
      headRefName
      headRefOid
      headRepository { nameWithOwner }
      commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
    }
  }
}";

// CONTEXTS is substituted as in QUERY.
const PR_RUNS_QUERY: &str = "query($url: URI!) {
  resource(url: $url) {
    ... on PullRequest {
      commits(last: 1) { nodes { commit { statusCheckRollup { state CONTEXTS } } } }
    }
  }
}";

#[derive(Deserialize)]
struct ResourceResponse {
    data: ResourceData,
}

#[derive(Deserialize)]
struct ResourceData {
    resource: Option<Resource>,
}

// Non-PR resources match no fragment and deserialize as `{}`, so every field is optional.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Resource {
    title: Option<String>,
    repository: Option<Repository>,
    head_ref_name: Option<String>,
    head_ref_oid: Option<String>,
    head_repository: Option<Repository>,
    commits: Option<Commits>,
}

#[derive(Debug, PartialEq)]
struct PrInfo {
    ci: CiStatus,
    title: String,
    repo: String,
    head_branch: String,
    head_oid: String,
    /// None if the head repository (e.g. a fork) was deleted.
    head_repo: Option<String>,
}

fn parse_pr_info(json: &str) -> Result<PrInfo, String> {
    let response: ResourceResponse =
        serde_json::from_str(json).map_err(|e| format!("failed to parse gh output: {e}"))?;
    let not_a_pr = || "not a pull request URL".to_string();
    let resource = response.data.resource.ok_or_else(not_a_pr)?;
    Ok(PrInfo {
        ci: resource.commits.ok_or_else(not_a_pr)?.ci_status(),
        title: resource.title.ok_or_else(not_a_pr)?,
        repo: resource.repository.ok_or_else(not_a_pr)?.name_with_owner,
        head_branch: resource.head_ref_name.ok_or_else(not_a_pr)?,
        head_oid: resource.head_ref_oid.ok_or_else(not_a_pr)?,
        head_repo: resource.head_repository.map(|r| r.name_with_owner),
    })
}

fn parse_failed_run_ids(json: &str) -> Result<Vec<u64>, String> {
    let response: ResourceResponse =
        serde_json::from_str(json).map_err(|e| format!("failed to parse gh output: {e}"))?;
    let not_a_pr = || "not a pull request URL".to_string();
    let commits = response
        .data
        .resource
        .and_then(|r| r.commits)
        .ok_or_else(not_a_pr)?;
    Ok(commits.failed_run_ids())
}

fn merge_pr(url: &str) -> Result<(), String> {
    let pr = parse_pr_info(&gh_graphql(PR_INFO_QUERY, &[("url", url)])?)?;
    match pr.ci {
        CiStatus::Success => {}
        CiStatus::Failure => return Err("CI failed, not merging".to_string()),
        CiStatus::Running => return Err("CI is still running, not merging".to_string()),
        CiStatus::None => return Err("PR has no CI checks, not merging".to_string()),
    }

    // Passing a merge method makes gh non-interactive. --delete-branch isn't used because it
    // also deletes the local branch; the remote one is deleted below instead.
    // --match-head-commit refuses the merge if someone pushed after the CI check above.
    let status = Command::new("gh")
        .args(["pr", "merge", url, "--rebase", "--match-head-commit"])
        .arg(&pr.head_oid)
        .status()
        .map_err(|e| format!("failed to run gh: {e}"))?;
    if !status.success() {
        return Err("`gh pr merge` failed".to_string());
    }

    if let Some(repo) = &pr.head_repo {
        delete_remote_branch(repo, &pr.head_branch);
    }
    Ok(())
}

/// Server-side rebase onto the latest base branch; no local checkout needed.
fn rebase_pr(url: &str) -> Result<(), String> {
    let status = Command::new("gh")
        .args(["pr", "update-branch", url, "--rebase"])
        .status()
        .map_err(|e| format!("failed to run gh: {e}"))?;
    if !status.success() {
        return Err("`gh pr update-branch` failed".to_string());
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
struct RerunTarget {
    /// In `gh -R` form: `host/owner/repo`.
    repo: String,
    run_id: String,
    job_id: Option<String>,
}

/// Accepts `https://<host>/<owner>/<repo>/actions/runs/<run>` (optionally `/attempts/<n>`) or
/// `…/runs/<run>/job/<job>`, as printed by `--actions`.
fn parse_rerun_url(url: &str) -> Result<RerunTarget, String> {
    let err = || format!("not a pull request or GitHub Actions run or job URL: {url}");
    let url = url.split(['?', '#']).next().unwrap_or_default();
    let path = url.strip_prefix("https://").ok_or_else(err)?;
    let segments: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    let [host, owner, repo, "actions", "runs", run_id, rest @ ..] = segments.as_slice() else {
        return Err(err());
    };
    let job_id = match rest {
        [] | ["attempts", _] => None,
        ["job", job_id] => Some(*job_id),
        _ => return Err(err()),
    };
    let is_id = |id: &str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
    if !is_id(run_id) || !job_id.is_none_or(is_id) {
        return Err(err());
    }
    Ok(RerunTarget {
        repo: format!("{host}/{owner}/{repo}"),
        run_id: run_id.to_string(),
        job_id: job_id.map(str::to_string),
    })
}

/// Accepts `https://<host>/<owner>/<repo>/pull/<n>`, optionally followed by a tab such as
/// `/checks`. Returns the repo in `gh -R` form and the PR URL without the tab.
fn parse_pr_url(url: &str) -> Option<(String, String)> {
    let url = url.split(['?', '#']).next().unwrap_or_default();
    let path = url.strip_prefix("https://")?;
    let segments: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    let [host, owner, repo, "pull", number, ..] = segments.as_slice() else {
        return None;
    };
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let repo = format!("{host}/{owner}/{repo}");
    let url = format!("https://{repo}/pull/{number}");
    Some((repo, url))
}

/// Reruns the failed and cancelled jobs (plus their dependents) of every Actions run on the
/// PR's head commit. `gh run rerun --failed` covers cancelled jobs too.
fn rerun_pr(repo: &str, url: &str) -> Result<(), String> {
    let query = PR_RUNS_QUERY.replace("CONTEXTS", CONTEXTS);
    let run_ids = parse_failed_run_ids(&gh_graphql(&query, &[("url", url)])?)?;
    if run_ids.is_empty() {
        return Err("no failed or cancelled Actions jobs to rerun".to_string());
    }
    // Keep going, so that one run that can't be rerun (e.g. still in progress) doesn't block
    // the others.
    let mut failures = 0;
    for id in &run_ids {
        let id = id.to_string();
        if let Err(e) = run("gh", &["run", "rerun", &id, "--failed", "-R", repo]) {
            eprintln!("error: {e}");
            failures += 1;
        }
    }
    if failures > 0 {
        return Err(format!(
            "{failures} of {} runs failed to rerun",
            run_ids.len()
        ));
    }
    Ok(())
}

/// Reruns a PR's failed and cancelled jobs, a single job (plus the jobs it depends on), or a
/// run's failed jobs.
fn rerun(url: &str) -> Result<(), String> {
    if let Some((repo, pr_url)) = parse_pr_url(url) {
        return rerun_pr(&repo, &pr_url);
    }
    let target = parse_rerun_url(url)?;
    // gh refuses a run id together with --job.
    let args = match &target.job_id {
        Some(job_id) => ["run", "rerun", "--job", job_id, "-R", &target.repo],
        None => [
            "run",
            "rerun",
            &target.run_id,
            "--failed",
            "-R",
            &target.repo,
        ],
    };
    run("gh", &args)
}

/// How often `wait` polls the PR's CI.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How long `wait` keeps polling a head commit without checks, since CI takes a moment to
/// register them after a push.
const NO_CHECKS_GRACE: Duration = Duration::from_secs(5 * 60);

/// Polls the PR until CI finishes, then shows a desktop notification whose buttons merge or
/// open the PR. Each poll looks at the current head commit, so force pushes are followed.
fn wait_pr(url: &str) -> Result<(), String> {
    let fetch = || -> Result<PrInfo, String> {
        parse_pr_info(&gh_graphql(PR_INFO_QUERY, &[("url", url)])?)
    };
    // Fail fast on a bad URL; later failures are likely network hiccups, so they're retried.
    let mut pr = fetch()?;
    let mut head_since = Instant::now();
    println!("Waiting for CI of {}: {}", pr.repo, pr.title);
    loop {
        match pr.ci {
            CiStatus::Success | CiStatus::Failure => break,
            CiStatus::None if head_since.elapsed() > NO_CHECKS_GRACE => {
                return Err("PR has no CI checks".to_string());
            }
            CiStatus::Running | CiStatus::None => {}
        }
        thread::sleep(POLL_INTERVAL);
        match fetch() {
            Ok(new) => {
                if new.head_oid != pr.head_oid {
                    head_since = Instant::now();
                }
                pr = new;
            }
            Err(e) => eprintln!("warning: {e}, retrying"),
        }
    }

    let passed = pr.ci == CiStatus::Success;
    let (summary, icon) = match passed {
        true => ("CI passed", "emblem-success"),
        false => ("CI failed", "dialog-error"),
    };
    println!("{} {summary}", pr.ci.icon());
    let actions = [("merge", "Merge"), ("open", "Open")];
    let actions = if passed { &actions[..] } else { &actions[1..] };
    let body = format!("{}: {}", pr.repo, pr.title);
    match notify(summary, &body, icon, actions) {
        Ok(Some(action)) if action == "merge" => merge_pr(url)?,
        Ok(Some(action)) if action == "open" => run("xdg-open", &[url])?,
        Ok(_) => {}
        // e.g. no desktop session; the result was already printed above.
        Err(e) => eprintln!("warning: failed to show notification: {e}"),
    }
    match passed {
        true => Ok(()),
        false => Err("CI failed".to_string()),
    }
}

const NOTIFICATIONS: [&str; 4] = [
    "--dest",
    "org.freedesktop.Notifications",
    "--object-path",
    "/org/freedesktop/Notifications",
];

/// Shows a desktop notification through the freedesktop D-Bus API, like notify-send but
/// without needing libnotify installed. Blocks until it's closed, returning the key of the
/// clicked action, if any.
fn notify(
    summary: &str,
    body: &str,
    icon: &str,
    actions: &[(&str, &str)],
) -> Result<Option<String>, String> {
    let mut monitor = Command::new("gdbus")
        .args(["monitor", "--session"])
        .args(NOTIFICATIONS)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run gdbus: {e}"))?;
    let result = show_notification(&mut monitor, summary, body, icon, actions);
    let _ = monitor.kill();
    let _ = monitor.wait();
    result
}

fn show_notification(
    monitor: &mut Child,
    summary: &str,
    body: &str,
    icon: &str,
    actions: &[(&str, &str)],
) -> Result<Option<String>, String> {
    let stdout = monitor.stdout.take().expect("stdout is piped");
    let mut lines = BufReader::new(stdout).lines();
    // gdbus prints this once subscribed, so a click can't come before we listen for it.
    lines.next();

    let actions: Vec<String> = actions
        .iter()
        .flat_map(|(key, label)| [gvariant_string(key), gvariant_string(label)])
        .collect();
    let output = Command::new("gdbus")
        .args(["call", "--session"])
        .args(NOTIFICATIONS)
        .args(["--method", "org.freedesktop.Notifications.Notify"])
        .args([
            gvariant_string("gh_wrapper"),
            // replaces_id: 0 for a new notification.
            "0".to_string(),
            gvariant_string(icon),
            gvariant_string(summary),
            gvariant_string(&escape_markup(body)),
            format!("[{}]", actions.join(", ")),
            // Critical notifications stay on screen until dismissed.
            "{'urgency': <byte 2>}".to_string(),
            // expire_timeout: never. The -1 "server default" would be taken as an option.
            "0".to_string(),
        ])
        .output()
        .map_err(|e| format!("failed to run gdbus: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let reply = String::from_utf8_lossy(&output.stdout);
    let id = parse_notification_id(&reply)
        .ok_or_else(|| format!("unexpected gdbus reply: {}", reply.trim()))?;

    for line in lines {
        let Ok(line) = line else { break };
        match parse_notification_signal(&line, id) {
            Some(NotificationSignal::Action(key)) => return Ok(Some(key)),
            Some(NotificationSignal::Closed) => return Ok(None),
            None => {}
        }
    }
    Ok(None)
}

/// gdbus parses each argument as GVariant text, so strings are quoted to be taken literally.
fn gvariant_string(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Notification bodies may contain HTML-like markup.
fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Parses the Notify reply, e.g. "(uint32 42,)".
fn parse_notification_id(reply: &str) -> Option<u32> {
    reply
        .trim()
        .strip_prefix("(uint32 ")?
        .strip_suffix(",)")?
        .parse()
        .ok()
}

#[derive(Debug, PartialEq)]
enum NotificationSignal {
    Action(String),
    Closed,
}

/// Parses a `gdbus monitor` line about notification `id`, e.g.
/// "/org/freedesktop/Notifications: org.freedesktop.Notifications.ActionInvoked (uint32 42, 'merge')".
fn parse_notification_signal(line: &str, id: u32) -> Option<NotificationSignal> {
    let (_, signal) = line.split_once("org.freedesktop.Notifications.")?;
    let (name, args) = signal.split_once(' ')?;
    let args = args.strip_prefix('(')?.strip_suffix(')')?;
    let (signal_id, rest) = args.strip_prefix("uint32 ")?.split_once(", ")?;
    if signal_id.parse() != Ok(id) {
        return None;
    }
    match name {
        "ActionInvoked" => {
            let key = rest.strip_prefix('\'')?.strip_suffix('\'')?;
            Some(NotificationSignal::Action(key.to_string()))
        }
        "NotificationClosed" => Some(NotificationSignal::Closed),
        _ => None,
    }
}

fn check_pr_branch(branch: &str) -> Result<(), String> {
    match branch {
        "main" | "master" => Err(format!("refusing to force push {branch}")),
        _ => Ok(()),
    }
}

/// Force pushes a local branch to origin and opens a PR for it, filled from its commits.
fn create_pr(branch: &str) -> Result<(), String> {
    check_pr_branch(branch)?;
    // Otherwise `git push` would also accept tags, remote branches or commits.
    let is_local_branch = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !is_local_branch {
        return Err(format!("{branch} is not a local branch"));
    }

    run("git", &["push", "--force", "origin", branch])?;
    run("gh", &["pr", "create", "--head", branch, "--fill"])
}

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    if !status.success() {
        return Err(format!("`{program} {}` failed", args.join(" ")));
    }
    Ok(())
}

/// Best effort: the PR is already merged, so failures are only warnings.
fn delete_remote_branch(repo: &str, branch: &str) {
    let output = Command::new("gh")
        .args(["api", "-X", "DELETE"])
        .arg(format!("repos/{repo}/git/refs/heads/{branch}"))
        .output();
    match output {
        Ok(o) if o.status.success() => println!("Deleted remote branch {repo}:{branch}"),
        // The repo's "automatically delete head branches" setting may have beaten us to it.
        Ok(o)
            if [&o.stdout, &o.stderr]
                .iter()
                .any(|out| String::from_utf8_lossy(out).contains("Reference does not exist")) => {}
        Ok(o) => eprintln!(
            "warning: failed to delete remote branch {repo}:{branch}: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => eprintln!("warning: failed to run gh: {e}"),
    }
}

fn gh_installed() -> bool {
    Command::new("gh")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn gh_authenticated() -> bool {
    Command::new("gh")
        .args(["auth", "status"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn gh_graphql(query: &str, vars: &[(&str, &str)]) -> Result<String, String> {
    let mut cmd = Command::new("gh");
    cmd.args(["api", "graphql", "-f"])
        .arg(format!("query={query}"));
    for (name, value) in vars {
        cmd.arg("-f").arg(format!("{name}={value}"));
    }
    let output = cmd
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("failed to run gh: {e}"))?;
    if !output.status.success() {
        return Err("`gh api graphql` failed".to_string());
    }
    String::from_utf8(output.stdout).map_err(|e| format!("gh returned invalid UTF-8: {e}"))
}

/// `watched` is the substitution for QUERY's WATCHED placeholder, see `watched_search`.
fn list_open_prs(with_checks: bool, watched: &str) -> Result<String, String> {
    let contexts = if with_checks { CONTEXTS } else { "" };
    let query = QUERY
        .replace("CONTEXTS", contexts)
        .replace("WATCHED", watched);
    gh_graphql(&query, &[])
}

fn parse_prs(json: &str) -> Result<Vec<PullRequest>, String> {
    let response: Response =
        serde_json::from_str(json).map_err(|e| format!("failed to parse gh output: {e}"))?;
    let Data { search, watched } = response.data;
    let mut prs: Vec<PullRequest> = search
        .nodes
        .into_iter()
        .chain(watched.into_iter().flat_map(|w| w.nodes))
        .map(PullRequest::from)
        .collect();
    // updatedAt is ISO 8601 in UTC, so lexicographic order is chronological.
    prs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(prs)
}

/// e.g. "an hour ago", "3 months ago". Falls back to the raw string if it isn't RFC 3339.
fn relative_time(timestamp: &str, now: DateTime<Utc>) -> String {
    match DateTime::parse_from_rfc3339(timestamp) {
        Ok(t) => HumanTime::from(t.with_timezone(&Utc) - now).to_string(),
        Err(_) => timestamp.to_string(),
    }
}

fn is_hidden(pr: &PullRequest, now: DateTime<Utc>, show_drafts: bool) -> bool {
    if pr.is_draft {
        return !show_drafts;
    }
    // Release PRs are opened by bots and tend to linger; hide them once they're stale.
    let Ok(updated) = DateTime::parse_from_rfc3339(&pr.updated_at) else {
        return false;
    };
    pr.title.starts_with("chore: release")
        && now - updated.with_timezone(&Utc) > TimeDelta::days(30)
}

fn filter_prs(prs: Vec<PullRequest>, now: DateTime<Utc>, show_drafts: bool) -> Vec<PullRequest> {
    prs.into_iter()
        .filter(|pr| !is_hidden(pr, now, show_drafts))
        .collect()
}

fn format_table(prs: &[PullRequest], now: DateTime<Utc>, show_author: bool) -> String {
    let mut headers = vec!["REPO", "TITLE", "URL", "LAST UPDATED"];
    if show_author {
        headers.insert(1, "AUTHOR");
    }
    let updated: Vec<String> = prs
        .iter()
        .map(|pr| relative_time(&pr.updated_at, now))
        .collect();
    let titles: Vec<String> = prs
        .iter()
        .map(|pr| match pr.is_draft {
            true => format!("{} (draft)", pr.title),
            false => pr.title.clone(),
        })
        .collect();
    let rows: Vec<Vec<&str>> = prs
        .iter()
        .zip(&updated)
        .zip(&titles)
        .map(|((pr, updated), title)| {
            let mut row = vec![
                pr.repo.as_str(),
                title.as_str(),
                pr.url.as_str(),
                updated.as_str(),
            ];
            if show_author {
                row.insert(1, pr.author.as_str());
            }
            row
        })
        .collect();

    // Width in chars rather than bytes, so non-ASCII titles stay aligned.
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }

    // The CI column is always a single emoji (2 terminal columns, same as the "CI" header),
    // so it's kept out of the width calculation, which counts chars.
    let icons = std::iter::once("CI").chain(prs.iter().map(|pr| pr.ci.icon()));

    let mut out = String::new();
    for ((icon, row), checks) in icons
        .zip(std::iter::once(headers).chain(rows))
        .zip(std::iter::once(&[][..]).chain(prs.iter().map(|pr| &pr.checks[..])))
    {
        out.push_str(icon);
        for (cell, &w) in row.iter().zip(&widths) {
            out.push_str(&format!("  {cell:<w$}"));
        }
        // Last column is padded too; don't leave trailing spaces.
        out.truncate(out.trim_end().len());
        out.push('\n');

        // Indented under the REPO column.
        let name_width = checks.iter().map(|c| c.name.chars().count()).max();
        for check in checks {
            let w = name_width.unwrap_or(0);
            let line = format!("    {} {:<w$}  {}", check.ci.icon(), check.name, check.url);
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    out
}

fn main() -> ExitCode {
    if !gh_installed() {
        eprintln!("error: `gh` is not installed. See https://cli.github.com/");
        return ExitCode::FAILURE;
    }

    if !gh_authenticated() {
        eprintln!("error: `gh` is not authenticated. Run:\n\n    gh auth login\n");
        return ExitCode::FAILURE;
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [cmd, url] if cmd == "merge" => merge_pr(url),
        [cmd, url] if cmd == "rebase" => rebase_pr(url),
        [cmd, url] if cmd == "wait" => wait_pr(url),
        [cmd, url] if cmd == "rerun" => rerun(url),
        [cmd, branch] if cmd == "pr" => create_pr(branch),
        flags
            if flags
                .iter()
                .all(|f| ["--actions", "--draft", "--watched"].contains(&f.as_str())) =>
        {
            let has = |flag| flags.iter().any(|f| f == flag);
            list(has("--actions"), has("--draft"), has("--watched"))
        }
        _ => {
            eprintln!(
                "usage: gh_wrapper [--actions] [--draft] [--watched] | merge <pr-url> | rebase <pr-url> \
                 | wait <pr-url> | rerun <pr-run-or-job-url> | pr <branch>"
            );
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn list(with_checks: bool, show_drafts: bool, with_watched: bool) -> Result<(), String> {
    let now = Utc::now();
    let repos = if with_watched {
        watched_repos()?
    } else {
        vec![]
    };
    // With no `repo:` qualifier the search would cover all of GitHub.
    let watched = match repos.is_empty() {
        true => String::new(),
        false => watched_search(&repos, now),
    };
    let prs = parse_prs(&list_open_prs(with_checks, &watched)?)?;
    let prs = filter_prs(prs, now, show_drafts);
    print!("{}", format_table(&prs, now, with_watched));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr_node(title: &str, updated_at: &str, state: Option<&str>) -> String {
        let rollup = match state {
            Some(s) => format!(r#"{{"state":"{s}"}}"#),
            None => "null".to_string(),
        };
        format!(
            r#"{{"repository":{{"nameWithOwner":"o/r"}},"author":{{"login":"me"}},"title":"{title}","url":"https://a/{title}","updatedAt":"{updated_at}","isDraft":false,
                "commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{rollup}}}}}]}}}}"#
        )
    }

    fn response(nodes: &[String]) -> String {
        format!(
            r#"{{"data":{{"search":{{"nodes":[{}]}}}}}}"#,
            nodes.join(",")
        )
    }

    #[test]
    fn parse_prs_sorts_most_recent_first() {
        let json = response(&[
            pr_node("old", "2026-01-01T00:00:00Z", None),
            pr_node("new", "2026-09-24T20:16:53Z", None),
            pr_node("mid", "2026-07-31T10:37:06Z", None),
        ]);
        let titles: Vec<_> = parse_prs(&json)
            .unwrap()
            .into_iter()
            .map(|pr| pr.title)
            .collect();
        assert_eq!(titles, ["new", "mid", "old"]);
    }

    #[test]
    fn parse_prs_ci_status() {
        let json = response(&[
            pr_node("a", "2026-01-06T00:00:00Z", Some("SUCCESS")),
            pr_node("b", "2026-01-05T00:00:00Z", Some("FAILURE")),
            pr_node("c", "2026-01-04T00:00:00Z", Some("ERROR")),
            pr_node("d", "2026-01-03T00:00:00Z", Some("PENDING")),
            pr_node("e", "2026-01-02T00:00:00Z", Some("EXPECTED")),
            pr_node("f", "2026-01-01T00:00:00Z", None),
        ]);
        let statuses: Vec<_> = parse_prs(&json)
            .unwrap()
            .into_iter()
            .map(|pr| pr.ci)
            .collect();
        assert_eq!(
            statuses,
            [
                CiStatus::Success,
                CiStatus::Failure,
                CiStatus::Failure,
                CiStatus::Running,
                CiStatus::Running,
                CiStatus::None,
            ]
        );
    }

    #[test]
    fn parse_prs_empty() {
        assert!(parse_prs(&response(&[])).unwrap().is_empty());
    }

    #[test]
    fn parse_prs_invalid() {
        assert!(parse_prs("not json").is_err());
    }

    fn now() -> DateTime<Utc> {
        "2026-09-24T21:00:00Z".parse().unwrap()
    }

    fn pr(title: &str, updated_at: &str) -> PullRequest {
        PullRequest {
            repo: "o/r".into(),
            author: "me".into(),
            title: title.into(),
            url: "https://a/1".into(),
            updated_at: updated_at.into(),
            ci: CiStatus::None,
            is_draft: false,
            checks: vec![],
        }
    }

    #[test]
    fn filter_prs_hides_stale_release_prs() {
        let prs = vec![
            pr("chore: release v1", "2026-05-02T10:32:51Z"),
            pr("chore: release v2", "2026-09-20T00:00:00Z"),
            pr("feat: old but not a release", "2025-01-01T00:00:00Z"),
        ];
        let titles: Vec<_> = filter_prs(prs, now(), false)
            .into_iter()
            .map(|pr| pr.title)
            .collect();
        assert_eq!(titles, ["chore: release v2", "feat: old but not a release"]);
    }

    #[test]
    fn parse_pr_info_reads_head_and_ci() {
        let json = r#"{"data":{"resource":{"title":"t","repository":{"nameWithOwner":"o/r"},
            "headRefName":"feat/x","headRefOid":"abc123","headRepository":{"nameWithOwner":"o/r"},
            "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}}}}"#;
        assert_eq!(
            parse_pr_info(json),
            Ok(PrInfo {
                ci: CiStatus::Success,
                title: "t".into(),
                repo: "o/r".into(),
                head_branch: "feat/x".into(),
                head_oid: "abc123".into(),
                head_repo: Some("o/r".into()),
            })
        );

        let json = r#"{"data":{"resource":{"title":"t","repository":{"nameWithOwner":"o/r"},
            "headRefName":"feat/x","headRefOid":"abc123","headRepository":null,
            "commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}}}"#;
        let pr = parse_pr_info(json).unwrap();
        assert_eq!(pr.ci, CiStatus::None);
        assert_eq!(pr.head_repo, None);
    }

    #[test]
    fn gvariant_string_escapes_quotes() {
        assert_eq!(gvariant_string(r"it's a\b"), r"'it\'s a\\b'");
    }

    #[test]
    fn escape_markup_escapes_html() {
        assert_eq!(
            escape_markup("a < b && c > d"),
            "a &lt; b &amp;&amp; c &gt; d"
        );
    }

    #[test]
    fn parse_notification_id_reads_reply() {
        assert_eq!(parse_notification_id("(uint32 253,)\n"), Some(253));
        assert_eq!(parse_notification_id("()"), None);
    }

    #[test]
    fn parse_notification_signal_matches_id() {
        let prefix = "/org/freedesktop/Notifications: org.freedesktop.Notifications.";
        let parse = |line: &str| parse_notification_signal(&format!("{prefix}{line}"), 42);
        assert_eq!(
            parse("ActionInvoked (uint32 42, 'merge')"),
            Some(NotificationSignal::Action("merge".into()))
        );
        assert_eq!(
            parse("NotificationClosed (uint32 42, uint32 2)"),
            Some(NotificationSignal::Closed)
        );
        assert_eq!(parse("NotificationClosed (uint32 4242, uint32 1)"), None);
        assert_eq!(parse("ActivationToken (uint32 42, 'xyz')"), None);
        assert_eq!(
            parse_notification_signal(
                "The name org.freedesktop.Notifications is owned by :1.24",
                42
            ),
            None
        );
    }

    #[test]
    fn check_pr_branch_rejects_main_and_master() {
        assert!(check_pr_branch("main").is_err());
        assert!(check_pr_branch("master").is_err());
        assert!(check_pr_branch("feature").is_ok());
    }

    #[test]
    fn parse_pr_info_rejects_non_pr_urls() {
        assert!(parse_pr_info(r#"{"data":{"resource":null}}"#).is_err());
        assert!(parse_pr_info(r#"{"data":{"resource":{}}}"#).is_err());
    }

    #[test]
    fn parse_rerun_url_accepts_jobs_and_runs() {
        let target = |run_id: &str, job_id: Option<&str>| RerunTarget {
            repo: "github.com/o/r".into(),
            run_id: run_id.into(),
            job_id: job_id.map(Into::into),
        };
        let parse = |url| parse_rerun_url(url).unwrap();
        assert_eq!(
            parse("https://github.com/o/r/actions/runs/1/job/2"),
            target("1", Some("2"))
        );
        assert_eq!(
            parse("https://github.com/o/r/actions/runs/1/job/2?pr=3#step:4:5"),
            target("1", Some("2"))
        );
        assert_eq!(
            parse("https://github.com/o/r/actions/runs/1/"),
            target("1", None)
        );
        assert_eq!(
            parse("https://github.com/o/r/actions/runs/1/attempts/2"),
            target("1", None)
        );
    }

    #[test]
    fn parse_pr_url_accepts_prs_only() {
        let expected = Some((
            "github.com/o/r".to_string(),
            "https://github.com/o/r/pull/1".to_string(),
        ));
        assert_eq!(parse_pr_url("https://github.com/o/r/pull/1"), expected);
        assert_eq!(
            parse_pr_url("https://github.com/o/r/pull/1/checks"),
            expected
        );
        assert_eq!(parse_pr_url("https://github.com/o/r/pull/1/#top"), expected);
        for url in [
            "https://github.com/o/r/pull/x",
            "https://github.com/o/r/pull/",
            "https://github.com/o/r/issues/1",
            "https://github.com/o/r/actions/runs/1",
            "http://github.com/o/r/pull/1",
        ] {
            assert_eq!(parse_pr_url(url), None, "{url}");
        }
    }

    #[test]
    fn parse_failed_run_ids_includes_cancelled_and_dedups() {
        let job = |run: u64, status: &str, conclusion: &str| {
            format!(
                r#"{{"__typename":"CheckRun","name":"j","status":"{status}","conclusion":{conclusion},
                "detailsUrl":null,"checkSuite":{{"workflowRun":{{"databaseId":{run},"workflow":{{"name":"w"}}}}}}}}"#
            )
        };
        let contexts = [
            job(1, "COMPLETED", r#""SUCCESS""#),
            job(2, "COMPLETED", r#""FAILURE""#),
            job(3, "COMPLETED", r#""CANCELLED""#),
            job(2, "COMPLETED", r#""TIMED_OUT""#),
            job(4, "IN_PROGRESS", "null"),
            job(5, "COMPLETED", r#""SKIPPED""#),
            r#"{"__typename":"StatusContext","context":"c","state":"FAILURE","targetUrl":null}"#
                .to_string(),
        ]
        .join(",");
        let json = format!(
            r#"{{"data":{{"resource":{{"commits":{{"nodes":[{{"commit":{{"statusCheckRollup":
            {{"state":"FAILURE","contexts":{{"nodes":[{contexts}]}}}}}}}}]}}}}}}}}"#
        );
        assert_eq!(parse_failed_run_ids(&json), Ok(vec![2, 3]));
        assert!(parse_failed_run_ids(r#"{"data":{"resource":{}}}"#).is_err());
    }

    #[test]
    fn parse_rerun_url_rejects_other_urls() {
        for url in [
            "https://github.com/o/r/pull/1",
            "https://github.com/o/r/actions/runs/x",
            "https://github.com/o/r/actions/runs/1/job/",
            "https://github.com/o/r/actions/runs/1/job/2/extra",
            "http://github.com/o/r/actions/runs/1",
            "https://ci.example.com/build/1",
        ] {
            assert!(parse_rerun_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn filter_prs_hides_drafts() {
        let prs = vec![
            PullRequest {
                is_draft: true,
                ..pr("draft", "2026-09-24T20:00:00Z")
            },
            pr("ready", "2026-09-24T20:00:00Z"),
        ];
        let titles: Vec<_> = filter_prs(prs, now(), false)
            .into_iter()
            .map(|pr| pr.title)
            .collect();
        assert_eq!(titles, ["ready"]);
    }

    #[test]
    fn filter_prs_shows_drafts_when_asked() {
        let prs = vec![
            PullRequest {
                is_draft: true,
                ..pr("draft", "2026-09-24T20:00:00Z")
            },
            pr("ready", "2026-09-24T20:00:00Z"),
        ];
        let titles: Vec<_> = filter_prs(prs, now(), true)
            .into_iter()
            .map(|pr| pr.title)
            .collect();
        assert_eq!(titles, ["draft", "ready"]);
    }

    #[test]
    fn relative_time_rounds_to_largest_unit() {
        let cases = [
            ("2026-09-24T20:59:50Z", "now"),
            ("2026-09-24T20:16:53Z", "43 minutes ago"),
            ("2026-09-24T19:00:00Z", "2 hours ago"),
            ("2026-09-23T20:00:00Z", "a day ago"),
            ("2026-09-10T20:00:00Z", "2 weeks ago"),
            ("2026-07-31T10:37:06Z", "2 months ago"),
            ("2024-10-26T22:03:57Z", "2 years ago"),
            ("garbage", "garbage"),
        ];
        for (timestamp, expected) in cases {
            assert_eq!(relative_time(timestamp, now()), expected, "{timestamp}");
        }
    }

    #[test]
    fn format_table_aligns_columns() {
        let prs = [
            PullRequest {
                repo: "KDAB/KDDockWidgets".into(),
                title: "short".into(),
                url: "https://a/1".into(),
                updated_at: "2026-09-24T20:16:53Z".into(),
                ci: CiStatus::Success,
                is_draft: true,
                checks: vec![],
                ..pr("", "")
            },
            PullRequest {
                title: "a longer title".into(),
                url: "https://a/22".into(),
                updated_at: "2026-07-31T10:37:06Z".into(),
                ci: CiStatus::Running,
                checks: vec![
                    Check {
                        name: "CI / build".into(),
                        ci: CiStatus::Failure,
                        url: "https://a/job/1".into(),
                    },
                    Check {
                        name: "lint".into(),
                        ci: CiStatus::Running,
                        url: "".into(),
                    },
                ],
                ..pr("", "")
            },
        ];
        let expected = "\
CI  REPO                TITLE           URL           LAST UPDATED
🟢  KDAB/KDDockWidgets  short (draft)   https://a/1   43 minutes ago
🟡  o/r                 a longer title  https://a/22  2 months ago
    🔴 CI / build  https://a/job/1
    🟡 lint
";
        assert_eq!(format_table(&prs, now(), false), expected);

        let expected = "\
CI  REPO                AUTHOR  TITLE          URL          LAST UPDATED
🟢  KDAB/KDDockWidgets  me      short (draft)  https://a/1  43 minutes ago
";
        assert_eq!(format_table(&prs[..1], now(), true), expected);
    }

    #[test]
    fn parse_prs_merges_watched() {
        let node = |title: &str, author: &str, updated_at: &str| {
            format!(
                r#"{{"repository":{{"nameWithOwner":"o/r"}},"author":{{"login":"{author}"}},"title":"{title}",
                    "url":"https://a/{title}","updatedAt":"{updated_at}","isDraft":false,"commits":{{"nodes":[]}}}}"#
            )
        };
        let json = format!(
            r#"{{"data":{{"search":{{"nodes":[{}]}},"watched":{{"nodes":[{}]}}}}}}"#,
            node("mine", "me", "2026-09-01T00:00:00Z"),
            node("theirs", "bob", "2026-09-02T00:00:00Z"),
        );
        let prs: Vec<_> = parse_prs(&json)
            .unwrap()
            .into_iter()
            .map(|pr| (pr.title, pr.author))
            .collect();
        assert_eq!(
            prs,
            [
                ("theirs".into(), "bob".into()),
                ("mine".into(), "me".into())
            ]
        );
    }

    #[test]
    fn watched_search_lists_repos_and_cutoff() {
        let repos = ["o/a".to_string(), "o/b".to_string()];
        assert_eq!(
            watched_search(&repos, now()),
            "watched: search(query: \"is:pr is:open -author:@me created:>=2026-09-03 repo:o/a repo:o/b\", \
             type: ISSUE, first: 100) { nodes { ...Pr } }"
        );
    }

    #[test]
    fn parse_prs_keeps_only_failed_or_running_checks() {
        let json = r#"{"data":{"search":{"nodes":[{"repository":{"nameWithOwner":"o/r"},
            "author":null,"title":"t","url":"https://a/1","updatedAt":"2026-01-01T00:00:00Z","isDraft":false,
            "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"FAILURE","contexts":{"nodes":[
                {"__typename":"CheckRun","name":"ok","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"u1","checkSuite":null},
                {"__typename":"CheckRun","name":"skipped","status":"COMPLETED","conclusion":"SKIPPED","detailsUrl":"u2","checkSuite":null},
                {"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"TIMED_OUT","detailsUrl":"u3",
                    "checkSuite":{"workflowRun":{"databaseId":1,"workflow":{"name":"CI"}}}},
                {"__typename":"CheckRun","name":"cancelled","status":"COMPLETED","conclusion":"CANCELLED","detailsUrl":"u6","checkSuite":null},
                {"__typename":"CheckRun","name":"test","status":"QUEUED","conclusion":null,"detailsUrl":null,"checkSuite":null},
                {"__typename":"StatusContext","context":"ext/ci","state":"ERROR","targetUrl":"u5"},
                {"__typename":"StatusContext","context":"ext/ok","state":"SUCCESS","targetUrl":null}
            ]}}}}]}}]}}}"#;
        let prs = parse_prs(json).unwrap();
        assert_eq!(
            prs[0].checks,
            [
                Check {
                    name: "CI / build (timed out)".into(),
                    ci: CiStatus::Failure,
                    url: "u3".into(),
                },
                Check {
                    name: "ext/ci".into(),
                    ci: CiStatus::Failure,
                    url: "u5".into(),
                },
                Check {
                    name: "test".into(),
                    ci: CiStatus::Running,
                    url: "".into(),
                },
            ]
        );
    }
}
