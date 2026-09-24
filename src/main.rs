use std::process::{Command, ExitCode, Stdio};

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
    title: String,
    url: String,
    updated_at: String,
    ci: CiStatus,
    is_draft: bool,
}

// Mirrors the shape of the GraphQL response in `list_open_prs`.
#[derive(Deserialize)]
struct Response {
    data: Data,
}

#[derive(Deserialize)]
struct Data {
    search: Search,
}

#[derive(Deserialize)]
struct Search {
    nodes: Vec<PrNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrNode {
    repository: Repository,
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
}

impl From<PrNode> for PullRequest {
    fn from(node: PrNode) -> Self {
        Self {
            ci: node.commits.ci_status(),
            repo: node.repository.name_with_owner,
            title: node.title,
            url: node.url,
            updated_at: node.updated_at,
            is_draft: node.is_draft,
        }
    }
}

// GraphQL rather than `gh search prs`, whose --json can't return CI status.
// Search works across all repos, unlike `gh pr list` which needs a repo context.
const QUERY: &str = "query {
  search(query: \"is:pr is:open author:@me\", type: ISSUE, first: 100) {
    nodes {
      ... on PullRequest {
        repository { nameWithOwner }
        title
        url
        updatedAt
        isDraft
        commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      }
    }
  }
}";

const PR_INFO_QUERY: &str = "query($url: URI!) {
  resource(url: $url) {
    ... on PullRequest {
      headRefName
      headRefOid
      headRepository { nameWithOwner }
      commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
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
    head_ref_name: Option<String>,
    head_ref_oid: Option<String>,
    head_repository: Option<Repository>,
    commits: Option<Commits>,
}

#[derive(Debug, PartialEq)]
struct PrInfo {
    ci: CiStatus,
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
        head_branch: resource.head_ref_name.ok_or_else(not_a_pr)?,
        head_oid: resource.head_ref_oid.ok_or_else(not_a_pr)?,
        head_repo: resource.head_repository.map(|r| r.name_with_owner),
    })
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

fn list_open_prs() -> Result<String, String> {
    gh_graphql(QUERY, &[])
}

fn parse_prs(json: &str) -> Result<Vec<PullRequest>, String> {
    let response: Response =
        serde_json::from_str(json).map_err(|e| format!("failed to parse gh output: {e}"))?;
    let mut prs: Vec<PullRequest> = response
        .data
        .search
        .nodes
        .into_iter()
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

fn is_hidden(pr: &PullRequest, now: DateTime<Utc>) -> bool {
    if pr.is_draft {
        return true;
    }
    // Release PRs are opened by bots and tend to linger; hide them once they're stale.
    let Ok(updated) = DateTime::parse_from_rfc3339(&pr.updated_at) else {
        return false;
    };
    pr.title.starts_with("chore: release")
        && now - updated.with_timezone(&Utc) > TimeDelta::days(30)
}

fn filter_prs(prs: Vec<PullRequest>, now: DateTime<Utc>) -> Vec<PullRequest> {
    prs.into_iter().filter(|pr| !is_hidden(pr, now)).collect()
}

fn format_table(prs: &[PullRequest], now: DateTime<Utc>) -> String {
    let headers = ["REPO", "TITLE", "URL", "LAST UPDATED"];
    let updated: Vec<String> = prs
        .iter()
        .map(|pr| relative_time(&pr.updated_at, now))
        .collect();
    let rows: Vec<[&str; 4]> = prs
        .iter()
        .zip(&updated)
        .map(|(pr, updated)| {
            [
                pr.repo.as_str(),
                pr.title.as_str(),
                pr.url.as_str(),
                updated.as_str(),
            ]
        })
        .collect();

    // Width in chars rather than bytes, so non-ASCII titles stay aligned.
    let mut widths = headers.map(|h| h.chars().count());
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }

    // The CI column is always a single emoji (2 terminal columns, same as the "CI" header),
    // so it's kept out of the width calculation, which counts chars.
    let icons = std::iter::once("CI").chain(prs.iter().map(|pr| pr.ci.icon()));

    let mut out = String::new();
    for (icon, row) in icons.zip(std::iter::once(headers).chain(rows)) {
        out.push_str(icon);
        for (cell, w) in row.iter().zip(widths) {
            out.push_str(&format!("  {cell:<w$}"));
        }
        // Last column is padded too; don't leave trailing spaces.
        out.truncate(out.trim_end().len());
        out.push('\n');
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
        [] => list_open_prs()
            .and_then(|json| parse_prs(&json))
            .map(|prs| {
                let now = Utc::now();
                let prs = filter_prs(prs, now);
                print!("{}", format_table(&prs, now));
            }),
        [cmd, url] if cmd == "merge" => merge_pr(url),
        _ => {
            eprintln!("usage: gh_wrapper [merge <pr-url>]");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pr_node(title: &str, updated_at: &str, state: Option<&str>) -> String {
        let rollup = match state {
            Some(s) => format!(r#"{{"state":"{s}"}}"#),
            None => "null".to_string(),
        };
        format!(
            r#"{{"repository":{{"nameWithOwner":"o/r"}},"title":"{title}","url":"https://a/{title}","updatedAt":"{updated_at}","isDraft":false,
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
            title: title.into(),
            url: "https://a/1".into(),
            updated_at: updated_at.into(),
            ci: CiStatus::None,
            is_draft: false,
        }
    }

    #[test]
    fn filter_prs_hides_stale_release_prs() {
        let prs = vec![
            pr("chore: release v1", "2026-05-02T10:32:51Z"),
            pr("chore: release v2", "2026-09-20T00:00:00Z"),
            pr("feat: old but not a release", "2025-01-01T00:00:00Z"),
        ];
        let titles: Vec<_> = filter_prs(prs, now())
            .into_iter()
            .map(|pr| pr.title)
            .collect();
        assert_eq!(titles, ["chore: release v2", "feat: old but not a release"]);
    }

    #[test]
    fn parse_pr_info_reads_head_and_ci() {
        let json = r#"{"data":{"resource":{"headRefName":"feat/x","headRefOid":"abc123",
            "headRepository":{"nameWithOwner":"o/r"},
            "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}}}}"#;
        assert_eq!(
            parse_pr_info(json),
            Ok(PrInfo {
                ci: CiStatus::Success,
                head_branch: "feat/x".into(),
                head_oid: "abc123".into(),
                head_repo: Some("o/r".into()),
            })
        );

        let json = r#"{"data":{"resource":{"headRefName":"feat/x","headRefOid":"abc123",
            "headRepository":null,
            "commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}}}"#;
        let pr = parse_pr_info(json).unwrap();
        assert_eq!(pr.ci, CiStatus::None);
        assert_eq!(pr.head_repo, None);
    }

    #[test]
    fn parse_pr_info_rejects_non_pr_urls() {
        assert!(parse_pr_info(r#"{"data":{"resource":null}}"#).is_err());
        assert!(parse_pr_info(r#"{"data":{"resource":{}}}"#).is_err());
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
        let titles: Vec<_> = filter_prs(prs, now())
            .into_iter()
            .map(|pr| pr.title)
            .collect();
        assert_eq!(titles, ["ready"]);
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
                is_draft: false,
            },
            PullRequest {
                repo: "o/r".into(),
                title: "a longer title".into(),
                url: "https://a/22".into(),
                updated_at: "2026-07-31T10:37:06Z".into(),
                ci: CiStatus::Running,
                is_draft: false,
            },
        ];
        let expected = "\
CI  REPO                TITLE           URL           LAST UPDATED
🟢  KDAB/KDDockWidgets  short           https://a/1   43 minutes ago
🟡  o/r                 a longer title  https://a/22  2 months ago
";
        assert_eq!(format_table(&prs, now()), expected);
    }
}
