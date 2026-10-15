//! Submodule awareness.
//!
//! A superproject records each submodule as a *gitlink* — a tree entry of type
//! commit holding the submodule's commit id. Rewriting the superproject never
//! alters trees, so gitlinks survive verbatim. Under `--recurse-submodules`
//! every initialized submodule is scrubbed as a repository of its own and the
//! superproject's trees are remapped so its gitlinks (and the index) point at
//! the rewritten submodule commits instead of ones that will be purged.

use anyhow::{Context, Result};
use colored::Colorize;
use git2::{ObjectType, Oid, Repository, TreeBuilder};
use std::collections::{HashMap, HashSet};

use crate::gitops;
use crate::rewriter::{self, Options};
use crate::verify;

/// One gitlink that changed (or would change) inside a commit's tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitlinkChange {
    pub path: String,
    pub old: Oid,
    pub new: Oid,
}

/// Submodule commit id → rewritten id. In a dry run only the *set* of ids
/// that would change is known.
#[derive(Default)]
pub struct GitlinkMap {
    map: HashMap<Oid, Oid>,
    dirty: HashSet<Oid>,
}

impl GitlinkMap {
    pub fn is_empty(&self) -> bool {
        self.map.is_empty() && self.dirty.is_empty()
    }

    /// The rewritten id for `oid`, if it changes. In a dry run the id maps to
    /// itself, which is enough to know the tree *would* change.
    pub fn hit(&self, oid: Oid) -> Option<Oid> {
        if let Some(&n) = self.map.get(&oid) {
            return Some(n);
        }
        if self.dirty.contains(&oid) {
            return Some(oid);
        }
        None
    }

    pub fn absorb(&mut self, report: &rewriter::Report) {
        self.map.extend(report.remap.iter().map(|(k, v)| (*k, *v)));
        self.dirty.extend(report.dirty.iter().copied());
    }

    pub fn raw_map(&self) -> &HashMap<Oid, Oid> {
        &self.map
    }
}

#[derive(Clone, Debug)]
pub enum SubStatus {
    /// Present in .gitmodules, left alone (no --recurse-submodules).
    Skipped,
    /// Present in .gitmodules but not cloned/initialized here.
    NotInitialized,
    /// Scrubbed: nothing to rewrite.
    Clean { commits: usize },
    /// Scrubbed and rewritten.
    Rewritten { commits: usize, rewritten: usize },
    /// Scrubbed under --guarantee but indicators survived.
    VerifyFailed { residues: usize },
}

#[derive(Clone, Debug)]
pub struct SubmoduleSummary {
    pub path: String,
    pub url: String,
    pub gitlink: Option<Oid>,
    pub status: SubStatus,
}

/// Scrub the submodules of `repo` (when asked to) and return what happened plus
/// the gitlink remap the superproject's rewrite must apply.
pub fn process(repo: &Repository, opts: &Options) -> Result<(Vec<SubmoduleSummary>, GitlinkMap)> {
    let mut summaries = Vec::new();
    let mut map = GitlinkMap::default();
    let pad = opts.indent();

    let subs = match repo.submodules() {
        Ok(s) => s,
        Err(_) => return Ok((summaries, map)),
    };

    for sm in subs {
        let path = sm.path().display().to_string();
        let url = sm.url().unwrap_or("").to_string();
        let gitlink = sm.head_id();

        if !opts.recurse_submodules {
            summaries.push(SubmoduleSummary {
                path,
                url,
                gitlink,
                status: SubStatus::Skipped,
            });
            continue;
        }

        let sub = match sm.open() {
            Ok(r) => r,
            Err(_) => {
                summaries.push(SubmoduleSummary {
                    path,
                    url,
                    gitlink,
                    status: SubStatus::NotInitialized,
                });
                continue;
            }
        };
        let workdir = sub
            .workdir()
            .map(|p| p.to_path_buf())
            .with_context(|| format!("submodule {path} has no work tree"))?;

        println!("{pad}{}  submodule {}", "↳".cyan(), path.bold());

        if opts.guarantee && !opts.dry_run {
            if let Some(dir) = opts.bundle_dir {
                let bundle = gitops::bundle_path(dir, &format!("submodule-{path}"));
                gitops::bundle_all(&workdir, &bundle)?;
                println!(
                    "{pad}   {}  backup bundle: {}",
                    "✓".green(),
                    bundle.display().to_string().dimmed()
                );
            }
        }

        let sub_opts = Options {
            depth: opts.depth + 1,
            ..*opts
        };
        let report = rewriter::run(&sub, &sub_opts)?;
        let commits = report.stats.total_commits;
        let rewritten = report.stats.rewritten_commits;

        let mut status = if rewritten == 0 {
            SubStatus::Clean { commits }
        } else {
            SubStatus::Rewritten { commits, rewritten }
        };

        if opts.guarantee && !opts.dry_run {
            gitops::purge(&workdir)?;
            let residues = verify::verify(&sub, opts.rules)?;
            if !residues.is_empty() {
                status = SubStatus::VerifyFailed {
                    residues: residues.len(),
                };
            }
        }

        println!(
            "{pad}   {}  {} commits scanned, {} rewritten",
            if rewritten > 0 {
                "~".yellow()
            } else {
                "✓".green()
            },
            commits,
            rewritten
        );

        map.absorb(&report);
        summaries.push(SubmoduleSummary {
            path,
            url,
            gitlink,
            status,
        });
    }

    Ok((summaries, map))
}

/// Memo per tree id: the rewritten tree (when written) and the gitlink changes
/// beneath it, with paths relative to that tree.
pub type TreeCache = HashMap<Oid, (Option<Oid>, Vec<GitlinkChange>)>;

/// Walk `tree_oid` and replace every gitlink found in `map`. Returns the new
/// tree id (only when `write` is set and something changed) and the list of
/// gitlinks that changed, paths relative to `tree_oid`.
pub fn remap_tree(
    repo: &Repository,
    tree_oid: Oid,
    map: &GitlinkMap,
    write: bool,
    cache: &mut TreeCache,
) -> Result<(Option<Oid>, Vec<GitlinkChange>)> {
    if let Some(hit) = cache.get(&tree_oid) {
        return Ok(hit.clone());
    }
    let tree = repo
        .find_tree(tree_oid)
        .with_context(|| format!("failed to read tree {tree_oid}"))?;
    let mut builder: Option<TreeBuilder> = None;
    let mut changes: Vec<GitlinkChange> = Vec::new();

    for entry in tree.iter() {
        let name = entry.name().unwrap_or("").to_string();
        match entry.kind() {
            Some(ObjectType::Commit) => {
                if let Some(new) = map.hit(entry.id()) {
                    changes.push(GitlinkChange {
                        path: name.clone(),
                        old: entry.id(),
                        new,
                    });
                    if write {
                        if builder.is_none() {
                            builder = Some(repo.treebuilder(Some(&tree))?);
                        }
                        if let Some(b) = builder.as_mut() {
                            b.insert(&name, new, entry.filemode())?;
                        }
                    }
                }
            }
            Some(ObjectType::Tree) => {
                let (sub_new, sub_changes) = remap_tree(repo, entry.id(), map, write, cache)?;
                if !sub_changes.is_empty() {
                    changes.extend(sub_changes.into_iter().map(|c| GitlinkChange {
                        path: format!("{name}/{}", c.path),
                        ..c
                    }));
                    if let (true, Some(sub_new)) = (write, sub_new) {
                        if builder.is_none() {
                            builder = Some(repo.treebuilder(Some(&tree))?);
                        }
                        if let Some(b) = builder.as_mut() {
                            b.insert(&name, sub_new, entry.filemode())?;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let new_oid = match builder {
        Some(b) => Some(b.write().context("failed to write remapped tree")?),
        None => None,
    };
    let result = (new_oid, changes);
    cache.insert(tree_oid, result.clone());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;

    fn temp_repo(tag: &str) -> Repository {
        let dir = std::env::temp_dir().join(format!(
            "luddite-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        Repository::init(&dir).expect("init temp repo")
    }

    #[test]
    fn remaps_nested_gitlink_and_leaves_other_entries() {
        let repo = temp_repo("remap");
        let blob = repo.blob(b"hello").unwrap();
        let old_sub = Oid::from_str("1111111111111111111111111111111111111111").unwrap();
        let new_sub = Oid::from_str("2222222222222222222222222222222222222222").unwrap();

        let mut inner = repo.treebuilder(None).unwrap();
        inner.insert("file.txt", blob, 0o100644).unwrap();
        inner.insert("vendor", old_sub, 0o160000).unwrap();
        let inner_oid = inner.write().unwrap();
        let mut outer = repo.treebuilder(None).unwrap();
        outer.insert("README", blob, 0o100644).unwrap();
        outer.insert("pkg", inner_oid, 0o040000).unwrap();
        let outer_oid = outer.write().unwrap();

        let mut map = GitlinkMap::default();
        map.map.insert(old_sub, new_sub);
        let mut cache = TreeCache::new();

        // Dry pass: reports the change, writes nothing.
        let (written, changes) = remap_tree(&repo, outer_oid, &map, false, &mut cache).unwrap();
        assert!(written.is_none());
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "pkg/vendor");
        assert_eq!(changes[0].new, new_sub);

        let mut cache = TreeCache::new();
        let (written, _) = remap_tree(&repo, outer_oid, &map, true, &mut cache).unwrap();
        let new_tree = repo.find_tree(written.expect("tree rewritten")).unwrap();
        assert_eq!(new_tree.get_name("README").unwrap().id(), blob);
        let pkg = repo
            .find_tree(new_tree.get_name("pkg").unwrap().id())
            .unwrap();
        assert_eq!(pkg.get_name("vendor").unwrap().id(), new_sub);
        assert_eq!(pkg.get_name("vendor").unwrap().filemode(), 0o160000);
        assert_eq!(pkg.get_name("file.txt").unwrap().id(), blob);

        // An unrelated tree is untouched and memoised.
        let sig = Signature::now("t", "t@example.com").unwrap();
        let _ = sig;
        let (again, _) = remap_tree(&repo, inner_oid, &map, true, &mut cache).unwrap();
        assert_eq!(again, cache.get(&inner_oid).unwrap().0);
    }

    #[test]
    fn untouched_tree_returns_none() {
        let repo = temp_repo("untouched");
        let blob = repo.blob(b"x").unwrap();
        let mut t = repo.treebuilder(None).unwrap();
        t.insert("a", blob, 0o100644).unwrap();
        let oid = t.write().unwrap();
        let map = GitlinkMap::default();
        let mut cache = TreeCache::new();
        let (written, changes) = remap_tree(&repo, oid, &map, true, &mut cache).unwrap();
        assert!(written.is_none());
        assert!(changes.is_empty());
    }
}
