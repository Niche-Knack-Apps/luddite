//! Pushing rewritten refs with correct leases.
//!
//! A bare `git push --force-with-lease` compares the remote against the local
//! remote-tracking ref — which luddite has just rewritten, so the lease is
//! stale and the push is rejected. Instead every ref is pushed with an
//! explicit `--force-with-lease=<ref>:<value>` whose value comes from a live
//! `git ls-remote`, and the whole update is `--atomic`.

use anyhow::{bail, Context, Result};
use git2::{BranchType, Oid, Repository};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

use crate::rewriter::{RefKind, Report};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// luddite rewrote this ref.
    Rewritten,
    /// Local and remote differ for reasons unrelated to this run.
    Diverged,
    /// The ref does not exist on the remote yet.
    New,
}

impl RowKind {
    pub fn label(self) -> &'static str {
        match self {
            RowKind::Rewritten => "rewritten",
            RowKind::Diverged => "diverged before this run",
            RowKind::New => "not on remote",
        }
    }
}

/// One ref the push screen offers to update.
#[derive(Clone, Debug)]
pub struct PlanRow {
    /// Destination ref on the remote (refs/heads/… or refs/tags/…).
    pub dst: String,
    /// What to push there.
    pub src: Oid,
    /// The remote's current value (the lease), if it has the ref.
    pub remote: Option<Oid>,
    pub kind: RowKind,
    pub selected: bool,
    /// Extra context, e.g. "remote-only" for a rewritten tracking ref with no local branch.
    pub note: &'static str,
}

pub fn remotes(repo: &Repository) -> Vec<String> {
    repo.remotes()
        .map(|arr| arr.iter().flatten().map(str::to_string).collect())
        .unwrap_or_default()
}

/// The remote to offer first: an explicit hint, then HEAD's upstream remote,
/// then `origin`, then the only/first remote.
pub fn default_remote(repo: &Repository, hint: Option<&str>) -> Option<String> {
    let all = remotes(repo);
    if let Some(h) = hint {
        return Some(h.to_string());
    }
    if let Ok(head) = repo.head() {
        if let Some(branch) = head.shorthand() {
            if let Ok(cfg) = repo.config() {
                if let Ok(r) = cfg.get_string(&format!("branch.{branch}.remote")) {
                    if all.contains(&r) {
                        return Some(r);
                    }
                }
            }
        }
    }
    if all.iter().any(|r| r == "origin") {
        return Some("origin".to_string());
    }
    all.first().cloned()
}

/// `git ls-remote --refs --heads --tags <remote>` → ref name → id.
pub fn ls_remote(workdir: &Path, remote: &str) -> Result<HashMap<String, Oid>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["ls-remote", "--refs", "--heads", "--tags", remote])
        .output()
        .context("failed to run `git ls-remote` (is git installed?)")?;
    if !out.status.success() {
        bail!(
            "git ls-remote {remote} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut map = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split('\t');
        if let (Some(sha), Some(name)) = (it.next(), it.next()) {
            if let Ok(oid) = Oid::from_str(sha.trim()) {
                map.insert(name.trim().to_string(), oid);
            }
        }
    }
    Ok(map)
}

/// Compare local refs (and rewritten remote-tracking refs) against the
/// remote's state and produce the rows to offer. Rewritten refs are
/// pre-selected; anything that merely differs is shown unselected.
pub fn build_plan(
    repo: &Repository,
    report: &Report,
    remote: &str,
    state: &HashMap<String, Oid>,
) -> Result<Vec<PlanRow>> {
    let rewritten: HashMap<&str, &crate::rewriter::RefUpdate> = report
        .ref_updates
        .iter()
        .map(|u| (u.name.as_str(), u))
        .collect();

    let mut rows = Vec::new();

    // Local branches.
    let mut local_branches: Vec<String> = Vec::new();
    for b in repo.branches(Some(BranchType::Local))? {
        let (branch, _) = b?;
        let name = match branch.name()? {
            Some(n) => n.to_string(),
            None => continue,
        };
        let oid = match branch.get().target() {
            Some(o) => o,
            None => continue,
        };
        local_branches.push(name.clone());
        let dst = format!("refs/heads/{name}");
        push_row(
            &mut rows,
            dst,
            oid,
            state,
            rewritten
                .get(&*format!("refs/heads/{name}"))
                .is_some_and(|update| update.new == oid),
            "",
        );
    }

    // Tags.
    for name in repo.tag_names(None)?.iter().flatten() {
        let dst = format!("refs/tags/{name}");
        let oid = match repo.find_reference(&dst).ok().and_then(|r| r.target()) {
            Some(o) => o,
            None => continue,
        };
        push_row(
            &mut rows,
            dst.clone(),
            oid,
            state,
            rewritten
                .get(dst.as_str())
                .is_some_and(|update| update.new == oid),
            "",
        );
    }

    // Rewritten remote-tracking refs of this remote with no local branch of
    // the same name: the cleaned copy of a branch that exists only remotely.
    let prefix = format!("refs/remotes/{remote}/");
    for u in &report.ref_updates {
        if u.kind != RefKind::Remote || !u.name.starts_with(&prefix) {
            continue;
        }
        let branch = &u.name[prefix.len()..];
        if branch == "HEAD" || local_branches.iter().any(|b| b == branch) {
            continue;
        }
        push_row(
            &mut rows,
            format!("refs/heads/{branch}"),
            u.new,
            state,
            true,
            "remote-only",
        );
    }

    rows.sort_by_key(|r| {
        (
            match r.kind {
                RowKind::Rewritten => 0,
                RowKind::Diverged => 1,
                RowKind::New => 2,
            },
            !r.dst.starts_with("refs/heads/"),
            r.dst.clone(),
        )
    });
    Ok(rows)
}

fn push_row(
    rows: &mut Vec<PlanRow>,
    dst: String,
    local: Oid,
    state: &HashMap<String, Oid>,
    rewritten: bool,
    note: &'static str,
) {
    let remote = state.get(&dst).copied();
    if remote == Some(local) {
        return; // in sync — nothing to do
    }
    let kind = if rewritten {
        RowKind::Rewritten
    } else if remote.is_none() {
        RowKind::New
    } else {
        RowKind::Diverged
    };
    rows.push(PlanRow {
        dst,
        src: local,
        remote,
        kind,
        selected: rewritten,
        note,
    });
}

/// The exact `git push` invocation for the selected rows.
pub fn command(workdir: &Path, remote: &str, rows: &[PlanRow]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "git".into(),
        "-C".into(),
        workdir.display().to_string(),
        "push".into(),
        "--atomic".into(),
        "--progress".into(),
        remote.into(),
    ];
    for r in rows.iter().filter(|r| r.selected) {
        let lease = r.remote.map(|o| o.to_string()).unwrap_or_default();
        args.push(format!("--force-with-lease={}:{}", r.dst, lease));
    }
    for r in rows.iter().filter(|r| r.selected) {
        args.push(format!("{}:{}", r.src, r.dst));
    }
    args
}

pub enum PushEvent {
    Line(String),
    Done { success: bool, code: Option<i32> },
}

/// Run the push on a worker thread, streaming git's output back as events.
pub fn spawn(workdir: PathBuf, remote: String, rows: Vec<PlanRow>, tx: Sender<PushEvent>) {
    thread::spawn(move || {
        let args = command(&workdir, &remote, &rows);
        let mut child = match Command::new(&args[0])
            .args(&args[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(PushEvent::Line(format!("failed to start git: {e}")));
                let _ = tx.send(PushEvent::Done {
                    success: false,
                    code: None,
                });
                return;
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let tx_out = tx.clone();
        let reader = thread::spawn(move || {
            if let Some(out) = stdout {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let _ = tx_out.send(PushEvent::Line(line));
                }
            }
        });
        if let Some(err) = stderr {
            // git's progress uses \r to redraw; split on both so the log stays readable.
            let mut buf = Vec::new();
            let mut reader_err = BufReader::new(err);
            loop {
                let mut chunk = Vec::new();
                match reader_err.read_until(b'\n', &mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        buf.extend_from_slice(&chunk);
                        let text = String::from_utf8_lossy(&buf).to_string();
                        for piece in text.split(['\r', '\n']) {
                            if !piece.trim().is_empty() {
                                let _ = tx.send(PushEvent::Line(piece.to_string()));
                            }
                        }
                        buf.clear();
                    }
                }
            }
        }
        let _ = reader.join();
        let status = child.wait();
        let (success, code) = match status {
            Ok(s) => (s.success(), s.code()),
            Err(_) => (false, None),
        };
        let _ = tx.send(PushEvent::Done { success, code });
    });
}

/// Non-interactive push (`--push <remote>`): print the plan, run it, echo git.
pub fn run_blocking(workdir: &Path, remote: &str, rows: &[PlanRow]) -> Result<bool> {
    let args = command(workdir, remote, rows);
    let status = Command::new(&args[0])
        .args(&args[1..])
        .status()
        .context("failed to run git push")?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_uses_explicit_leases_and_atomic() {
        let a = Oid::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let b = Oid::from_str("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let rows = vec![
            PlanRow {
                dst: "refs/heads/main".into(),
                src: b,
                remote: Some(a),
                kind: RowKind::Rewritten,
                selected: true,
                note: "",
            },
            PlanRow {
                dst: "refs/tags/v1".into(),
                src: b,
                remote: None,
                kind: RowKind::New,
                selected: true,
                note: "",
            },
            PlanRow {
                dst: "refs/heads/skip".into(),
                src: b,
                remote: Some(a),
                kind: RowKind::Diverged,
                selected: false,
                note: "",
            },
        ];
        let cmd = command(Path::new("/r"), "origin", &rows);
        assert_eq!(
            &cmd[..7],
            &[
                "git",
                "-C",
                "/r",
                "push",
                "--atomic",
                "--progress",
                "origin"
            ]
        );
        assert!(cmd.contains(&format!("--force-with-lease=refs/heads/main:{a}")));
        assert!(cmd.contains(&"--force-with-lease=refs/tags/v1:".to_string()));
        assert!(cmd.contains(&format!("{b}:refs/heads/main")));
        assert!(!cmd.iter().any(|s| s.contains("skip")));
    }

    #[test]
    fn rows_classify_against_remote_state() {
        let a = Oid::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let b = Oid::from_str("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let mut state = HashMap::new();
        state.insert("refs/heads/same".to_string(), a);
        state.insert("refs/heads/rew".to_string(), a);
        state.insert("refs/heads/div".to_string(), a);
        let mut rows = Vec::new();
        push_row(&mut rows, "refs/heads/same".into(), a, &state, false, "");
        push_row(&mut rows, "refs/heads/rew".into(), b, &state, true, "");
        push_row(&mut rows, "refs/heads/div".into(), b, &state, false, "");
        push_row(&mut rows, "refs/heads/new".into(), b, &state, false, "");
        let kinds: Vec<(String, RowKind, bool)> = rows
            .iter()
            .map(|r| (r.dst.clone(), r.kind, r.selected))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("refs/heads/rew".to_string(), RowKind::Rewritten, true),
                ("refs/heads/div".to_string(), RowKind::Diverged, false),
                ("refs/heads/new".to_string(), RowKind::New, false),
            ]
        );
    }
}
