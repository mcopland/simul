use anyhow::{Context, Result};

/// The checked-out branch in `cwd`, or `None` outside a repo or on a detached HEAD.
pub async fn current_branch(cwd: &str) -> Result<Option<String>> {
    let out = tokio::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("failed to run git rev-parse in {cwd}"))?;
    if !out.status.success() {
        return Ok(None);
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok((!branch.is_empty() && branch != "HEAD").then_some(branch))
}
