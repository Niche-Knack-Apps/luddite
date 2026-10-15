use anyhow::{Context, Result};
use git2::{ObjectType, Oid, Repository, Sort};
use std::collections::HashSet;

use crate::cleaner;
use crate::patterns::RuleSet;

/// A surviving AI indicator found after rewriting.
pub struct Residue {
    pub oid: Oid,
    pub summary: String,
    pub kind: String,
    pub rule: String,
}

/// Re-scan every reachable commit across every live ref (excluding our own
/// backups) plus the notes refs, and report any surviving indicator. An empty
/// result is the deterministic guarantee that the history is clean.
pub fn verify(repo: &Repository, rules: &RuleSet) -> Result<Vec<Residue>> {
    let mut residues = Vec::new();

    // Notes refs that should have been deleted but still exist.
    for r in repo.references()? {
        let r = r?;
        if let Some(name) = r.name() {
            if rules
                .delete_notes
                .iter()
                .any(|rule| rule.regex.is_match(name))
            {
                residues.push(Residue {
                    oid: r.target().unwrap_or_else(Oid::zero),
                    summary: name.to_string(),
                    kind: "notes-ref".into(),
                    rule: "delete_notes".into(),
                });
            }
        }
    }

    // Collect commit tips from every live ref except backups/originals.
    let mut tips: Vec<Oid> = Vec::new();
    for r in repo.references()? {
        let r = r?;
        let name = match r.name() {
            Some(n) => n,
            None => continue,
        };
        if name.starts_with("refs/backup/")
            || name.starts_with("refs/original/")
            || name.starts_with("refs/notes/")
        {
            continue;
        }
        // Peel anything (incl. annotated tags) down to a commit.
        if let Ok(obj) = r.peel(ObjectType::Commit) {
            tips.push(obj.id());
        } else if let Some(t) = r.target() {
            tips.push(t);
        }
    }

    let mut walk = repo.revwalk()?;
    walk.set_sorting(Sort::TOPOLOGICAL)?;
    let mut pushed = HashSet::new();
    for oid in tips {
        if pushed.insert(oid) {
            let _ = walk.push(oid);
        }
    }

    for oid in walk {
        let oid = oid.context("walk error during verification")?;
        let commit = repo.find_commit(oid).context("failed to find commit")?;
        let summary = commit.summary().unwrap_or("(no subject)").to_string();

        let msg = commit.message_raw().unwrap_or("");
        for rule in &rules.messages {
            if rule.regex.is_match(msg) {
                residues.push(Residue {
                    oid,
                    summary: summary.clone(),
                    kind: "message".into(),
                    rule: rule.name.clone(),
                });
            }
        }

        for (role, sig) in [
            ("author", commit.author()),
            ("committer", commit.committer()),
        ] {
            let rendered =
                cleaner::render_identity(sig.name().unwrap_or(""), sig.email().unwrap_or(""));
            for rule in &rules.identities {
                if rule.regex.is_match(&rendered) {
                    residues.push(Residue {
                        oid,
                        summary: summary.clone(),
                        kind: role.into(),
                        rule: rule.name.clone(),
                    });
                }
            }
        }
    }

    Ok(residues)
}
