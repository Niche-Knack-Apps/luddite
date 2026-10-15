//! The few operations luddite delegates to the `git` binary: bundling, purging,
//! and anything that must honour the user's full git configuration (signing,
//! pushing). Everything else goes through libgit2.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// `<dir>/luddite-backup-<name>-<unix-ts>.bundle`
pub fn bundle_path(dir: &Path, name: &str) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let safe: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect();
    dir.join(format!("luddite-backup-{safe}-{ts}.bundle"))
}

/// `git bundle create <bundle> --all` — the external, complete backup.
pub fn bundle_all(repo: &Path, bundle: &Path) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("bundle")
        .arg("create")
        .arg(bundle)
        .arg("--all")
        .status()
        .context("failed to run `git bundle` (is git installed?)")?;
    if !status.success() {
        bail!("git bundle create failed — aborting before any rewrite");
    }
    Ok(())
}

/// Expire every reflog and prune every unreachable object.
pub fn purge(repo: &Path) -> Result<()> {
    git_ok(repo, &["reflog", "expire", "--expire=now", "--all"])?;
    git_ok(repo, &["gc", "--prune=now", "--quiet"])?;
    Ok(())
}

pub fn git_ok(repo: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .with_context(|| format!("failed to run `git {}`", args.join(" ")))?;
    if !status.success() {
        bail!("`git {}` failed", args.join(" "));
    }
    Ok(())
}
