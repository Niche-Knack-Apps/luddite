//! Signature handling for rewritten objects.
//!
//! A rewrite necessarily invalidates the original signature (the signed bytes
//! changed), so luddite strips it and — under `--sign` — produces a fresh one.
//! Signing is delegated to `git commit-tree -S` / `git tag -s` so that every
//! `gpg.*`, `user.signingkey` and `gpg.format` (openpgp / ssh / x509) setting
//! applies exactly as it does to `git commit`.

use anyhow::{bail, Context, Result};
use git2::{Commit, Oid, Signature, Time};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::cli::SignMode;

const TAG_SIG_MARKERS: [&str; 3] = [
    "-----BEGIN PGP SIGNATURE-----",
    "-----BEGIN SSH SIGNATURE-----",
    "-----BEGIN SIGNED MESSAGE-----",
];

/// True if the commit carries a `gpgsig` header (git stores OpenPGP, SSH and
/// X.509 signatures there alike).
pub fn commit_is_signed(commit: &Commit) -> bool {
    commit
        .raw_header_bytes()
        .split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"gpgsig "))
}

/// Split an annotated tag's raw message into (message, signature block). Git
/// appends the armored signature to the tag message itself, so recreating a
/// tag from `message()` verbatim would carry a stale, invalid block along.
pub fn split_tag_signature(message: &str) -> (&str, Option<&str>) {
    for marker in TAG_SIG_MARKERS {
        if let Some(i) = message.find(marker) {
            return (&message[..i], Some(&message[i..]));
        }
    }
    (message, None)
}

/// Does `mode` ask for a signature on an object whose original was `was_signed`?
pub fn wanted(mode: Option<SignMode>, was_signed: bool) -> bool {
    match mode {
        None => false,
        Some(SignMode::All) => true,
        Some(SignMode::Signed) => was_signed,
    }
}

/// Write a signed commit through `git commit-tree -S`, preserving author and
/// committer identities and timestamps. Returns the new commit id; no ref is
/// touched.
pub fn commit_tree_signed(
    workdir: &Path,
    tree: Oid,
    parents: &[Oid],
    author: &Signature,
    committer: &Signature,
    message: &str,
) -> Result<Oid> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(workdir)
        .arg("commit-tree")
        .arg(tree.to_string())
        .arg("-S")
        .arg("-F")
        .arg("-");
    for p in parents {
        cmd.arg("-p").arg(p.to_string());
    }
    identity_env(&mut cmd, "GIT_AUTHOR", author);
    identity_env(&mut cmd, "GIT_COMMITTER", committer);
    let out = run_with_stdin(cmd, message).context("git commit-tree -S")?;
    Oid::from_str(out.trim()).context("git commit-tree returned an unexpected id")
}

/// Recreate an annotated tag signed, through `git tag -s`. The tagger identity
/// and date are preserved via the committer environment. Git writes the ref;
/// the new tag object id is returned.
pub fn tag_signed(
    workdir: &Path,
    name: &str,
    target: Oid,
    tagger: &Signature,
    message: &str,
) -> Result<Oid> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(workdir)
        .args(["tag", "-s", "-f", "--cleanup=verbatim", "-F", "-"])
        .arg(name)
        .arg(target.to_string());
    identity_env(&mut cmd, "GIT_COMMITTER", tagger);
    run_with_stdin(cmd, message).context("git tag -s")?;
    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["rev-parse", &format!("refs/tags/{name}")])
        .output()
        .context("git rev-parse")?;
    if !out.status.success() {
        bail!("signed tag {name} was not written");
    }
    Oid::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .context("git rev-parse returned an unexpected id")
}

fn identity_env(cmd: &mut Command, prefix: &str, sig: &Signature) {
    cmd.env(format!("{prefix}_NAME"), sig.name().unwrap_or(""))
        .env(format!("{prefix}_EMAIL"), sig.email().unwrap_or(""))
        .env(format!("{prefix}_DATE"), raw_date(&sig.when()));
}

/// Git's internal date format: `@<unix-seconds> <±HHMM>`.
pub fn raw_date(t: &Time) -> String {
    let off = t.offset_minutes();
    let sign = if off < 0 { '-' } else { '+' };
    let off = off.abs();
    format!("@{} {}{:02}{:02}", t.seconds(), sign, off / 60, off % 60)
}

fn run_with_stdin(mut cmd: Command, stdin: &str) -> Result<String> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .context("failed to run git (is git installed?)")?;
    if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(stdin.as_bytes())
            .context("failed to feed the message to git")?;
    }
    let out = child.wait_with_output().context("git did not exit")?;
    if !out.status.success() {
        bail!(
            "git exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_pgp_block_off_tag_message() {
        let msg = "release v1\n-----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----\n";
        let (m, sig) = split_tag_signature(msg);
        assert_eq!(m, "release v1\n");
        assert!(sig.unwrap().starts_with("-----BEGIN PGP SIGNATURE-----"));
    }

    #[test]
    fn unsigned_tag_message_passes_through() {
        let (m, sig) = split_tag_signature("release v1\n");
        assert_eq!(m, "release v1\n");
        assert!(sig.is_none());
    }

    #[test]
    fn raw_date_keeps_offset_sign_and_padding() {
        assert_eq!(raw_date(&Time::new(1787945816, -240)), "@1787945816 -0400");
        assert_eq!(raw_date(&Time::new(0, 330)), "@0 +0530");
    }

    #[test]
    fn sign_mode_semantics() {
        assert!(!wanted(None, true));
        assert!(wanted(Some(SignMode::All), false));
        assert!(wanted(Some(SignMode::Signed), true));
        assert!(!wanted(Some(SignMode::Signed), false));
    }
}
