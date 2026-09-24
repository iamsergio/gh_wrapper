use std::process::{Command, ExitCode, Stdio};

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

fn list_open_prs() -> std::io::Result<bool> {
    // `gh search` works across all repos, unlike `gh pr list` which needs a repo context.
    let status = Command::new("gh")
        .args(["search", "prs", "--author", "@me", "--state", "open"])
        .status()?;
    Ok(status.success())
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

    match list_open_prs() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: failed to run gh: {e}");
            ExitCode::FAILURE
        }
    }
}
