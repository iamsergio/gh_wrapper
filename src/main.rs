use std::process::{Command, ExitCode, Stdio};

use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PullRequest {
    title: String,
    url: String,
    updated_at: String,
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

fn list_open_prs() -> Result<String, String> {
    // `gh search` works across all repos, unlike `gh pr list` which needs a repo context.
    let output = Command::new("gh")
        .args(["search", "prs", "--author", "@me", "--state", "open"])
        .args(["--json", "title,url,updatedAt"])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("failed to run gh: {e}"))?;
    if !output.status.success() {
        return Err("`gh search prs` failed".to_string());
    }
    String::from_utf8(output.stdout).map_err(|e| format!("gh returned invalid UTF-8: {e}"))
}

fn parse_prs(json: &str) -> Result<Vec<PullRequest>, String> {
    let mut prs: Vec<PullRequest> =
        serde_json::from_str(json).map_err(|e| format!("failed to parse gh output: {e}"))?;
    // updatedAt is ISO 8601 in UTC, so lexicographic order is chronological.
    prs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(prs)
}

fn format_table(prs: &[PullRequest]) -> String {
    let headers = ["TITLE", "URL", "LAST UPDATED"];
    let rows: Vec<[&str; 3]> = prs
        .iter()
        .map(|pr| [pr.title.as_str(), pr.url.as_str(), pr.updated_at.as_str()])
        .collect();

    // Width in chars rather than bytes, so non-ASCII titles stay aligned.
    let mut widths = headers.map(|h| h.chars().count());
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }

    let mut out = String::new();
    for row in std::iter::once(headers).chain(rows) {
        let line = format!(
            "{:<w0$}  {:<w1$}  {}",
            row[0],
            row[1],
            row[2],
            w0 = widths[0],
            w1 = widths[1]
        );
        out.push_str(&line);
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

    match list_open_prs().and_then(|json| parse_prs(&json)) {
        Ok(prs) => {
            print!("{}", format_table(&prs));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_prs_sorts_most_recent_first() {
        let json = r#"[
            {"title":"old","updatedAt":"2026-01-01T00:00:00Z","url":"https://a/1"},
            {"title":"new","updatedAt":"2026-09-24T20:16:53Z","url":"https://a/2"},
            {"title":"mid","updatedAt":"2026-07-31T10:37:06Z","url":"https://a/3"}
        ]"#;
        let titles: Vec<_> = parse_prs(json)
            .unwrap()
            .into_iter()
            .map(|pr| pr.title)
            .collect();
        assert_eq!(titles, ["new", "mid", "old"]);
    }

    #[test]
    fn parse_prs_empty() {
        assert!(parse_prs("[]").unwrap().is_empty());
    }

    #[test]
    fn parse_prs_invalid() {
        assert!(parse_prs("not json").is_err());
    }

    #[test]
    fn format_table_aligns_columns() {
        let prs = [
            PullRequest {
                title: "short".into(),
                url: "https://a/1".into(),
                updated_at: "2026-09-24T20:16:53Z".into(),
            },
            PullRequest {
                title: "a longer title".into(),
                url: "https://a/22".into(),
                updated_at: "2026-07-31T10:37:06Z".into(),
            },
        ];
        let expected = "\
TITLE           URL           LAST UPDATED
short           https://a/1   2026-09-24T20:16:53Z
a longer title  https://a/22  2026-07-31T10:37:06Z
";
        assert_eq!(format_table(&prs), expected);
    }
}
