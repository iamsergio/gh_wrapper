use std::io::{self, BufRead, BufReader};
use std::process::{Child, Command, ExitCode, ExitStatus, Output, Stdio};
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
    /// Tally of all checks; empty unless fetched with `--actions`.
    bar: CheckBar,
}

/// How many checks passed, failed or are still running (cancelled ones are left out, see
/// `Check::from`); drawn as a filled bar under the PR's row with `--actions`.
#[derive(Debug, Default, PartialEq, Clone, Copy)]
struct CheckBar {
    passed: usize,
    failed: usize,
    running: usize,
}

impl CheckBar {
    const WIDTH: usize = 30;

    fn tally(checks: &[Check]) -> Self {
        let mut bar = Self::default();
        for check in checks {
            match check.ci {
                CiStatus::Success => bar.passed += 1,
                CiStatus::Failure => bar.failed += 1,
                CiStatus::Running => bar.running += 1,
                CiStatus::None => {}
            }
        }
        bar
    }

    /// Cells for passed, failed and running: proportional, but every non-empty part gets at
    /// least one (running enough to fit its count), and together they fill `WIDTH`.
    fn cells(&self) -> [usize; 3] {
        let counts = [self.passed, self.failed, self.running];
        let total: usize = counts.iter().sum();
        if total == 0 {
            return [0; 3];
        }
        let min = [1, 1, self.running.to_string().len()];
        let mut cells = [0; 3];
        for i in 0..3 {
            if counts[i] > 0 {
                cells[i] = (counts[i] * Self::WIDTH / total).max(min[i]);
            }
        }
        // Rounding may over- or undershoot; the largest part absorbs the difference.
        let largest = (0..3).max_by_key(|&i| cells[i]).unwrap_or(0);
        let others: usize = cells.iter().sum::<usize>() - cells[largest];
        cells[largest] = Self::WIDTH.saturating_sub(others);
        cells
    }

    /// Green, red and yellow blocks, the yellow one showing how many checks are still
    /// running; None if there are no checks.
    fn render(&self) -> Option<String> {
        const BACKGROUNDS: [&str; 3] = ["42", "41", "43;30"];
        let cells = self.cells();
        if cells == [0; 3] {
            return None;
        }
        let mut out = String::new();
        for (i, (n, bg)) in cells.into_iter().zip(BACKGROUNDS).enumerate() {
            if n == 0 {
                continue;
            }
            let text = match i {
                2 => format!("{:^n$}", self.running),
                _ => " ".repeat(n),
            };
            out.push_str(&format!("\x1b[{bg}m{text}\x1b[0m"));
        }
        Some(out)
    }
}

/// An Actions run with failed or cancelled jobs.
#[derive(Debug, PartialEq)]
struct FailedRun {
    id: u64,
    /// Job ids, excluding cancelled jobs.
    failed_jobs: Vec<u64>,
    has_cancelled: bool,
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
        /// For Actions, the job id.
        database_id: u64,
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
                ..
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

impl PrInfo {
    /// Errors if the PR is no longer open, since there's no CI to wait for or merge.
    fn ensure_open(&self) -> Result<(), String> {
        match self.state.as_str() {
            "OPEN" => Ok(()),
            state => Err(format!("PR is already {}", state.to_lowercase())),
        }
    }
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

    /// The tally of all checks, and the failed or running ones.
    fn tally_and_checks(self) -> (CheckBar, Vec<Check>) {
        let all: Vec<Check> = self.into_contexts().into_iter().map(Check::from).collect();
        let bar = CheckBar::tally(&all);
        let mut checks: Vec<Check> = all
            .into_iter()
            .filter(|c| matches!(c.ci, CiStatus::Failure | CiStatus::Running))
            .collect();
        // Stable sort: failures first, otherwise keep GitHub's order.
        checks.sort_by_key(|c| c.ci != CiStatus::Failure);
        (bar, checks)
    }

    /// Actions runs with a failed or cancelled job, in GitHub's order.
    fn failed_runs(self) -> Vec<FailedRun> {
        let mut runs: Vec<FailedRun> = Vec::new();
        for context in self.into_contexts() {
            if let CheckContext::CheckRun {
                database_id,
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
            {
                let index = runs
                    .iter()
                    .position(|r| r.id == run.database_id)
                    .unwrap_or_else(|| {
                        runs.push(FailedRun {
                            id: run.database_id,
                            failed_jobs: Vec::new(),
                            has_cancelled: false,
                        });
                        runs.len() - 1
                    });
                let failed_run = &mut runs[index];
                if conclusion.as_deref() == Some("CANCELLED") {
                    failed_run.has_cancelled = true;
                } else {
                    failed_run.failed_jobs.push(database_id);
                }
            }
        }
        runs
    }
}

impl From<PrNode> for PullRequest {
    fn from(node: PrNode) -> Self {
        let ci = node.commits.ci_status();
        let (bar, checks) = node.commits.tally_and_checks();
        Self {
            ci,
            repo: node.repository.name_with_owner,
            author: node.author.map_or_else(|| "ghost".to_string(), |a| a.login),
            title: node.title,
            url: node.url,
            updated_at: node.updated_at,
            is_draft: node.is_draft,
            checks,
            bar,
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
        .traced_output()
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
      databaseId name status conclusion detailsUrl
      checkSuite { workflowRun { databaseId workflow { name } } }
    }
    ... on StatusContext { context state targetUrl }
  }
}";

const PR_INFO_QUERY: &str = "query($url: URI!) {
  resource(url: $url) {
    ... on PullRequest {
      title
      state
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
    state: Option<String>,
    repository: Option<Repository>,
    head_ref_name: Option<String>,
    head_ref_oid: Option<String>,
    head_repository: Option<Repository>,
    commits: Option<Commits>,
}

#[derive(Debug, PartialEq)]
struct PrInfo {
    ci: CiStatus,
    /// OPEN, CLOSED or MERGED.
    state: String,
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
        state: resource.state.ok_or_else(not_a_pr)?,
        repo: resource.repository.ok_or_else(not_a_pr)?.name_with_owner,
        head_branch: resource.head_ref_name.ok_or_else(not_a_pr)?,
        head_oid: resource.head_ref_oid.ok_or_else(not_a_pr)?,
        head_repo: resource.head_repository.map(|r| r.name_with_owner),
    })
}

fn parse_failed_runs(json: &str) -> Result<Vec<FailedRun>, String> {
    let response: ResourceResponse =
        serde_json::from_str(json).map_err(|e| format!("failed to parse gh output: {e}"))?;
    let not_a_pr = || "not a pull request URL".to_string();
    let commits = response
        .data
        .resource
        .and_then(|r| r.commits)
        .ok_or_else(not_a_pr)?;
    Ok(commits.failed_runs())
}

fn fetch_failed_runs(url: &str) -> Result<Vec<FailedRun>, String> {
    let query = PR_RUNS_QUERY.replace("CONTEXTS", CONTEXTS);
    parse_failed_runs(&gh_graphql(&query, &[("url", url)])?)
}

fn fetch_pr_info(url: &str) -> Result<PrInfo, String> {
    parse_pr_info(&gh_graphql(PR_INFO_QUERY, &[("url", url)])?)
}

/// Waits for CI to pass, then merges; bails out if it fails. If it had to wait, the outcome is
/// also shown as a desktop notification. With `force`, merges right away without looking at CI.
fn merge_pr(url: &str, force: bool) -> Result<(), String> {
    if force {
        let pr = fetch_pr_info(url)?;
        pr.ensure_open()?;
        return merge_head(url, &pr);
    }
    let (pr, waited) = wait_for_ci(url)?;
    let result = match pr.ci {
        CiStatus::Success => merge_head(url, &pr),
        CiStatus::None => Err("PR has no CI checks, not merging".to_string()),
        CiStatus::Failure | CiStatus::Running => Err("CI failed, not merging".to_string()),
    };
    if waited {
        let (summary, icon) = match &result {
            Ok(()) => ("Merged".to_string(), "emblem-success"),
            Err(e) => (format!("Not merged: {e}"), "dialog-error"),
        };
        // The merge result matters more than a failure to open the PR.
        if let Err(e) = notify_pr(url, &pr, &summary, icon, false) {
            eprintln!("warning: {e}");
        }
    }
    result
}

/// Merges the PR at the head commit `pr` was fetched at.
fn merge_head(url: &str, pr: &PrInfo) -> Result<(), String> {
    // Passing a merge method makes gh non-interactive. --delete-branch isn't used because it
    // also deletes the local branch; the remote one is deleted below instead.
    // --match-head-commit refuses the merge if someone pushed after CI was checked.
    let status = Command::new("gh")
        .args(["pr", "merge", url, "--rebase", "--match-head-commit"])
        .arg(&pr.head_oid)
        .traced_status()
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
        .traced_status()
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
    let runs = fetch_failed_runs(url)?;
    if runs.is_empty() {
        return Err("no failed or cancelled Actions jobs to rerun".to_string());
    }
    let reruns: Vec<_> = runs
        .iter()
        .map(|run| vec![run.id.to_string(), "--failed".to_string()])
        .collect();
    rerun_each(repo, &reruns)
}

/// Like `rerun_pr`, but leaves cancelled jobs alone, as they're usually fail-fast siblings of
/// a failed job. `--failed` would rerun them too, so runs that have any are rerun job by job.
fn run_failed(url: &str) -> Result<(), String> {
    let (repo, url) = parse_pr_url(url).ok_or_else(|| format!("not a pull request URL: {url}"))?;
    let mut reruns = Vec::new();
    for run in fetch_failed_runs(&url)? {
        if !run.has_cancelled {
            reruns.push(vec![run.id.to_string(), "--failed".to_string()]);
            continue;
        }
        for job in run.failed_jobs {
            reruns.push(vec!["--job".to_string(), job.to_string()]);
        }
    }
    if reruns.is_empty() {
        return Err("no failed Actions jobs to rerun".to_string());
    }
    rerun_each(&repo, &reruns)
}

/// Runs `gh run rerun <args> -R <repo>` for each of `reruns`. Keeps going, so that one that
/// can't be rerun (e.g. still in progress) doesn't block the others.
fn rerun_each(repo: &str, reruns: &[Vec<String>]) -> Result<(), String> {
    let mut failures = 0;
    for args in reruns {
        let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
        args.splice(0..0, ["run", "rerun"]);
        args.extend(["-R", repo]);
        if let Err(e) = run("gh", &args) {
            eprintln!("error: {e}");
            failures += 1;
        }
    }
    if failures > 0 {
        return Err(format!("{failures} of {} reruns failed", reruns.len()));
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

/// Polls the PR until CI succeeds or fails, or has had no checks for `NO_CHECKS_GRACE`
/// (returned as `CiStatus::None`). Each poll looks at the current head commit, so force pushes
/// are followed. Also returns whether CI was still unfinished at the first poll.
fn wait_for_ci(url: &str) -> Result<(PrInfo, bool), String> {
    // Fail fast on a bad URL; later failures are likely network hiccups, so they're retried.
    let mut pr = fetch_pr_info(url)?;
    pr.ensure_open()?;
    let waited = !matches!(pr.ci, CiStatus::Success | CiStatus::Failure);
    let mut head_since = Instant::now();
    if waited {
        println!("Waiting for CI of {}: {}", pr.repo, pr.title);
    }
    loop {
        match pr.ci {
            CiStatus::Success | CiStatus::Failure => break,
            CiStatus::None if head_since.elapsed() > NO_CHECKS_GRACE => break,
            CiStatus::Running | CiStatus::None => {}
        }
        thread::sleep(POLL_INTERVAL);
        match fetch_pr_info(url) {
            Ok(new) => {
                if new.head_oid != pr.head_oid {
                    head_since = Instant::now();
                }
                pr = new;
                // Merged or closed while waiting.
                pr.ensure_open()?;
            }
            Err(e) => eprintln!("warning: {e}, retrying"),
        }
    }
    match pr.ci {
        CiStatus::Success => println!("{} CI passed", pr.ci.icon()),
        CiStatus::Failure => println!("{} CI failed", pr.ci.icon()),
        CiStatus::Running | CiStatus::None => {}
    }
    Ok((pr, waited))
}

/// Waits for CI, then shows a desktop notification whose buttons merge or open the PR.
/// With `notify` false, only the exit status and the terminal output report the result.
fn wait_pr(url: &str, notify: bool) -> Result<(), String> {
    let (pr, _) = wait_for_ci(url)?;
    let (summary, icon) = match pr.ci {
        CiStatus::Success => ("CI passed", "emblem-success"),
        CiStatus::Failure | CiStatus::Running => ("CI failed", "dialog-error"),
        CiStatus::None => return Err("PR has no CI checks".to_string()),
    };
    let passed = pr.ci == CiStatus::Success;
    if notify {
        notify_pr(url, &pr, summary, icon, passed)?;
    }
    match passed {
        true => Ok(()),
        false => Err("CI failed".to_string()),
    }
}

/// Shows a desktop notification about the PR with an Open button, plus Merge if `can_merge`,
/// and blocks until it's closed. Failing to show it is only a warning.
fn notify_pr(
    url: &str,
    pr: &PrInfo,
    summary: &str,
    icon: &str,
    can_merge: bool,
) -> Result<(), String> {
    let actions = [("merge", "Merge"), ("open", "Open")];
    let actions = if can_merge {
        &actions[..]
    } else {
        &actions[1..]
    };
    let body = format!("{}: {}", pr.repo, pr.title);
    match notify(summary, &body, icon, actions) {
        Ok(Some(action)) if action == "merge" => merge_head(url, pr)?,
        Ok(Some(action)) if action == "open" => run(
            if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            },
            &[url],
        )?,
        Ok(_) => {}
        // e.g. no desktop session; the result is also printed to the terminal.
        Err(e) => eprintln!("warning: failed to show notification: {e}"),
    }
    Ok(())
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
    if cfg!(target_os = "macos") {
        return notify_macos(summary, body, actions);
    }
    let mut monitor = Command::new("gdbus")
        .args(["monitor", "--session"])
        .args(NOTIFICATIONS)
        .stdout(Stdio::piped())
        .traced_spawn()
        .map_err(|e| format!("failed to run gdbus: {e}"))?;
    let result = show_notification(&mut monitor, summary, body, icon, actions);
    let _ = monitor.kill();
    let _ = monitor.wait();
    result
}

const DISMISS_LABEL: &str = "Dismiss";

/// macOS has no D-Bus, so this shows an `osascript` alert instead (it supports at most three
/// buttons: the actions plus Dismiss). Blocks until a button is clicked, returning the key of
/// the clicked action, if any. The text is passed as arguments so it needs no escaping.
fn notify_macos(
    summary: &str,
    body: &str,
    actions: &[(&str, &str)],
) -> Result<Option<String>, String> {
    let script = "on run argv\n\
        set btns to rest of rest of argv\n\
        set r to display alert (item 1 of argv) message (item 2 of argv) buttons btns as critical\n\
        return button returned of r\n\
        end run";
    let labels = actions
        .iter()
        .map(|(_, label)| *label)
        .chain([DISMISS_LABEL]);
    let output = Command::new("osascript")
        .args(["-e", script, "--", summary, body])
        .args(labels)
        .traced_output()
        .map_err(|e| format!("failed to run osascript: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let clicked = String::from_utf8_lossy(&output.stdout);
    let clicked = clicked.trim();
    Ok(actions
        .iter()
        .find(|(_, label)| *label == clicked)
        .map(|(key, _)| key.to_string()))
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
        .traced_output()
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

/// Env var that, when set, makes every subprocess be logged to stderr with its duration.
const DEBUG_ENV: &str = "GHW_DEBUG";

/// `status`/`output`/`spawn` that log the command line and how long it took if `DEBUG_ENV` is
/// set. The line is printed before running, so a hanging command is visible.
trait Traced {
    fn traced_status(&mut self) -> io::Result<ExitStatus>;
    fn traced_output(&mut self) -> io::Result<Output>;
    fn traced_spawn(&mut self) -> io::Result<Child>;
}

impl Traced for Command {
    fn traced_status(&mut self) -> io::Result<ExitStatus> {
        trace(self, Command::status)
    }

    fn traced_output(&mut self) -> io::Result<Output> {
        trace(self, Command::output)
    }

    fn traced_spawn(&mut self) -> io::Result<Child> {
        trace(self, Command::spawn)
    }
}

fn debug_enabled() -> bool {
    std::env::var_os(DEBUG_ENV).is_some()
}

fn debug(message: &str) {
    if debug_enabled() {
        eprintln!("[{DEBUG_ENV}] {message}");
    }
}

fn trace<T>(cmd: &mut Command, f: impl FnOnce(&mut Command) -> io::Result<T>) -> io::Result<T> {
    if !debug_enabled() {
        return f(cmd);
    }
    debug(&format!("$ {}", command_line(cmd)));
    let start = Instant::now();
    let result = f(cmd);
    let elapsed = start.elapsed().as_secs_f64();
    match &result {
        Ok(_) => debug(&format!("  took {elapsed:.2}s")),
        Err(e) => debug(&format!("  failed after {elapsed:.2}s: {e}")),
    }
    result
}

/// The command line on one line; long arguments such as GraphQL queries are truncated.
fn command_line(cmd: &Command) -> String {
    const MAX_ARG: usize = 60;
    let mut line = cmd.get_program().to_string_lossy().into_owned();
    for arg in cmd.get_args() {
        let arg = arg
            .to_string_lossy()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        line.push(' ');
        match arg.char_indices().nth(MAX_ARG) {
            Some((i, _)) => line.push_str(&format!("{}…", &arg[..i])),
            None => line.push_str(&arg),
        }
    }
    line
}

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .traced_status()
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
        .traced_output();
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
        .traced_status()
        .is_ok_and(|s| s.success())
}

fn gh_authenticated() -> bool {
    Command::new("gh")
        .args(["auth", "status"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .traced_status()
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
        .traced_output()
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
    let no_checks: &[Check] = &[];
    let details = std::iter::once((no_checks, None))
        .chain(prs.iter().map(|pr| (&pr.checks[..], pr.bar.render())));
    let url_column = headers.iter().position(|&h| h == "URL");
    for (line, ((icon, row), (checks, bar))) in icons
        .zip(std::iter::once(headers).chain(rows))
        .zip(details)
        .enumerate()
    {
        out.push_str(icon);
        for (column, (cell, &w)) in row.iter().zip(&widths).enumerate() {
            // Blue PR URLs (not the header); the padding stays outside the color.
            if line > 0 && Some(column) == url_column {
                let pad = " ".repeat(w - cell.chars().count());
                out.push_str(&format!("  \x1b[34m{cell}\x1b[0m{pad}"));
            } else {
                out.push_str(&format!("  {cell:<w$}"));
            }
        }
        // Last column is padded too; don't leave trailing spaces.
        out.truncate(out.trim_end().len());
        out.push('\n');

        // Indented under the REPO column.
        if let Some(bar) = bar {
            out.push_str(&format!("    {bar}\n"));
        }
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

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("merge" | "wait"))
        && args[1..].iter().all(|a| a.starts_with("--"))
    {
        match sole_pr_url() {
            Ok(url) => args.push(url),
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    let result = match args.as_slice() {
        [cmd, url] if cmd == "merge" => merge_pr(&normalize_url(url), false),
        [cmd, a, b] if cmd == "merge" && (a == "--force" || b == "--force") => {
            merge_pr(&normalize_url(if a == "--force" { b } else { a }), true)
        }
        [cmd, url] if cmd == "rebase" => rebase_pr(&normalize_url(url)),
        [cmd, url] if cmd == "wait" => wait_pr(&normalize_url(url), true),
        [cmd, a, b] if cmd == "wait" && (a == "--no-notify" || b == "--no-notify") => wait_pr(
            &normalize_url(if a == "--no-notify" { b } else { a }),
            false,
        ),
        [cmd, url] if cmd == "rerun" => rerun(&normalize_url(url)),
        [cmd, url] if cmd == "run_failed" => run_failed(&normalize_url(url)),
        flags
            if flags
                .iter()
                .all(|f| ["--actions", "--draft", "--watched"].contains(&f.as_str())) =>
        {
            let has = |flag| flags.iter().any(|f| f == flag);
            list(has("--actions"), has("--draft"), has("--watched")).map(|_| ())
        }
        _ => {
            eprintln!(
                "usage: gh_wrapper [--actions] [--draft] [--watched] | merge [--force] [<pr-url>] | rebase <pr-url> \
                 | wait [--no-notify] [<pr-url>] | rerun <pr-run-or-job-url> | run_failed <pr-url>"
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

/// Prepends `https:` to a scheme-less `//host/...` URL: on macOS, double-clicking a URL doesn't
/// select its `https:` part, so pasted URLs often lack it.
fn normalize_url(url: &str) -> String {
    if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url.to_string()
    }
}

/// Prints the user's open PRs and returns the URL of the only one, for `merge`/`wait` without a
/// URL argument. More than one (or none) is an error.
fn sole_pr_url() -> Result<String, String> {
    let prs = list(false, false, false)?;
    match prs.as_slice() {
        [pr] => Ok(pr.url.clone()),
        [] => Err("no open PRs to pick from, pass a PR URL".into()),
        _ => Err(format!("{} open PRs, pass a PR URL to pick one", prs.len())),
    }
}

fn list(
    with_checks: bool,
    show_drafts: bool,
    with_watched: bool,
) -> Result<Vec<PullRequest>, String> {
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
    Ok(prs)
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
            bar: CheckBar::default(),
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
        let json = r#"{"data":{"resource":{"title":"t","state":"OPEN","repository":{"nameWithOwner":"o/r"},
            "headRefName":"feat/x","headRefOid":"abc123","headRepository":{"nameWithOwner":"o/r"},
            "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}}}}"#;
        assert_eq!(
            parse_pr_info(json),
            Ok(PrInfo {
                ci: CiStatus::Success,
                state: "OPEN".into(),
                title: "t".into(),
                repo: "o/r".into(),
                head_branch: "feat/x".into(),
                head_oid: "abc123".into(),
                head_repo: Some("o/r".into()),
            })
        );

        let json = r#"{"data":{"resource":{"title":"t","state":"OPEN","repository":{"nameWithOwner":"o/r"},
            "headRefName":"feat/x","headRefOid":"abc123","headRepository":null,
            "commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}}}"#;
        let pr = parse_pr_info(json).unwrap();
        assert_eq!(pr.ci, CiStatus::None);
        assert_eq!(pr.head_repo, None);
    }

    #[test]
    fn check_bar_cells_fill_the_width() {
        let bar = |passed, failed, running| CheckBar {
            passed,
            failed,
            running,
        };
        assert_eq!(bar(0, 0, 0).cells(), [0; 3]);
        assert_eq!(bar(5, 0, 0).cells(), [30, 0, 0]);
        assert_eq!(bar(1, 1, 1).cells(), [10; 3]);
        // Small parts stay visible.
        let [passed, failed, running] = bar(100, 1, 1).cells();
        assert!(failed >= 1 && running >= 1);
        assert_eq!(passed + failed + running, 30);
        assert!(bar(0, 0, 0).render().is_none());
        assert!(bar(1, 0, 7).render().unwrap().contains(" 7 "));
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
    fn normalize_url_prepends_https_to_scheme_relative_urls() {
        assert_eq!(
            normalize_url("//github.com/o/r/pull/1"),
            "https://github.com/o/r/pull/1"
        );
        assert_eq!(
            normalize_url("https://github.com/o/r/pull/1"),
            "https://github.com/o/r/pull/1"
        );
        assert_eq!(normalize_url("github.com/o/r"), "github.com/o/r");
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
    fn parse_failed_runs_groups_jobs_by_run() {
        let job = |run: u64, job: u64, status: &str, conclusion: &str| {
            format!(
                r#"{{"__typename":"CheckRun","databaseId":{job},"name":"j","status":"{status}","conclusion":{conclusion},
                "detailsUrl":null,"checkSuite":{{"workflowRun":{{"databaseId":{run},"workflow":{{"name":"w"}}}}}}}}"#
            )
        };
        let contexts = [
            job(1, 10, "COMPLETED", r#""SUCCESS""#),
            job(2, 20, "COMPLETED", r#""FAILURE""#),
            job(3, 30, "COMPLETED", r#""CANCELLED""#),
            job(2, 21, "COMPLETED", r#""TIMED_OUT""#),
            job(4, 40, "IN_PROGRESS", "null"),
            job(5, 50, "COMPLETED", r#""SKIPPED""#),
            job(2, 22, "COMPLETED", r#""CANCELLED""#),
            r#"{"__typename":"StatusContext","context":"c","state":"FAILURE","targetUrl":null}"#
                .to_string(),
        ]
        .join(",");
        let json = format!(
            r#"{{"data":{{"resource":{{"commits":{{"nodes":[{{"commit":{{"statusCheckRollup":
            {{"state":"FAILURE","contexts":{{"nodes":[{contexts}]}}}}}}}}]}}}}}}}}"#
        );
        let failed_run = |id, failed_jobs, has_cancelled| FailedRun {
            id,
            failed_jobs,
            has_cancelled,
        };
        assert_eq!(
            parse_failed_runs(&json),
            Ok(vec![
                failed_run(2, vec![20, 21], true),
                failed_run(3, vec![], true)
            ])
        );
        assert!(parse_failed_runs(r#"{"data":{"resource":{}}}"#).is_err());
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
                bar: CheckBar::default(),
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
🟢  KDAB/KDDockWidgets  short (draft)   \x1b[34mhttps://a/1\x1b[0m   43 minutes ago
🟡  o/r                 a longer title  \x1b[34mhttps://a/22\x1b[0m  2 months ago
    🔴 CI / build  https://a/job/1
    🟡 lint
";
        assert_eq!(format_table(&prs, now(), false), expected);

        let expected = "\
CI  REPO                AUTHOR  TITLE          URL          LAST UPDATED
🟢  KDAB/KDDockWidgets  me      short (draft)  \x1b[34mhttps://a/1\x1b[0m  43 minutes ago
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
                {"__typename":"CheckRun","databaseId":1,"name":"ok","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"u1","checkSuite":null},
                {"__typename":"CheckRun","databaseId":1,"name":"skipped","status":"COMPLETED","conclusion":"SKIPPED","detailsUrl":"u2","checkSuite":null},
                {"__typename":"CheckRun","databaseId":1,"name":"build","status":"COMPLETED","conclusion":"TIMED_OUT","detailsUrl":"u3",
                    "checkSuite":{"workflowRun":{"databaseId":1,"workflow":{"name":"CI"}}}},
                {"__typename":"CheckRun","databaseId":1,"name":"cancelled","status":"COMPLETED","conclusion":"CANCELLED","detailsUrl":"u6","checkSuite":null},
                {"__typename":"CheckRun","databaseId":1,"name":"test","status":"QUEUED","conclusion":null,"detailsUrl":null,"checkSuite":null},
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
