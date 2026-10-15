use anyhow::{bail, Context, Result};
use colored::Colorize;
use git2::{ObjectType, Oid, Repository, Signature, Sort};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use crate::cleaner;
use crate::cli::SignMode;
use crate::patterns::RuleSet;
use crate::signing;
use crate::submodules::{self, GitlinkChange, SubmoduleSummary, TreeCache};

#[derive(Default, Clone)]
pub struct Stats {
    pub total_commits: usize,
    pub rewritten_commits: usize,
    pub messages_cleaned: usize,
    pub identities_sanitized: usize,
    pub trees_remapped: usize,
    pub commits_signed: usize,
    pub tags_signed: usize,
    pub branches_scanned: usize,
    pub branches_updated: usize,
    pub remotes_updated: usize,
    pub tags_updated: usize,
    pub generic_refs_updated: usize,
    pub notes_deleted: usize,
    pub stash_dropped: bool,
    pub refs_backed_up: usize,
    pub index_gitlinks_updated: usize,
}

/// Replacement identity for AI authors/committers.
pub struct Identity {
    pub name: String,
    pub email: String,
}

#[derive(Clone, Copy)]
pub struct Options<'a> {
    pub rules: &'a RuleSet,
    pub replacement: &'a Identity,
    pub dry_run: bool,
    pub guarantee: bool,
    pub force: bool,
    pub keep_stash: bool,
    pub verbose: bool,
    pub sign: Option<SignMode>,
    pub recurse_submodules: bool,
    /// Where --guarantee bundles are written (next to the root repository).
    pub bundle_dir: Option<&'a Path>,
    /// 0 for the root repository, 1 for its submodules, and so on.
    pub depth: usize,
}

impl Options<'_> {
    pub fn indent(&self) -> String {
        "  ".repeat(self.depth + 1)
    }
}

/// How a commit was affected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Unchanged,
    /// Its own message, identity or gitlinks were cleaned.
    Cleaned,
    /// Only its id changed, because a parent was rewritten.
    Cascade,
}

/// Everything the review screen needs to show a commit before and after.
#[derive(Clone, Debug)]
pub struct CommitRecord {
    pub old: Oid,
    /// The rewritten id. `None` when unchanged or in a dry run.
    pub new: Option<Oid>,
    pub parents: Vec<Oid>,
    pub summary: String,
    pub old_message: String,
    pub new_message: String,
    pub old_author: String,
    pub new_author: String,
    pub old_committer: String,
    pub new_committer: String,
    pub signed_before: bool,
    pub signed_after: bool,
    pub msg_changed: bool,
    pub identity_changed: bool,
    pub parents_changed: bool,
    pub gitlinks: Vec<GitlinkChange>,
}

impl CommitRecord {
    pub fn change(&self) -> Change {
        if self.msg_changed || self.identity_changed || !self.gitlinks.is_empty() {
            Change::Cleaned
        } else if self.parents_changed {
            Change::Cascade
        } else {
            Change::Unchanged
        }
    }

    /// The id the commit has now (the rewritten one when there is one).
    pub fn display_id(&self) -> Oid {
        self.new.unwrap_or(self.old)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    Branch,
    Remote,
    Tag,
    Stash,
    Other,
}

#[derive(Clone, Debug)]
pub struct RefUpdate {
    /// Full ref name.
    pub name: String,
    pub old: Oid,
    pub new: Oid,
    pub kind: RefKind,
}

pub struct Report {
    pub stats: Stats,
    pub dry_run: bool,
    /// Every commit walked, parents before children.
    pub records: Vec<CommitRecord>,
    pub remap: HashMap<Oid, Oid>,
    /// Dry run: the commits that would be rewritten.
    pub dirty: HashSet<Oid>,
    pub ref_updates: Vec<RefUpdate>,
    pub submodules: Vec<SubmoduleSummary>,
}

impl Report {
    pub fn mapped(&self, oid: Oid) -> Oid {
        self.remap.get(&oid).copied().unwrap_or(oid)
    }
}

pub fn run(repo: &Repository, opts: &Options) -> Result<Report> {
    let mut stats = Stats::default();
    let pad = opts.indent();

    // ── Submodules first: their rewritten ids feed the gitlink remap below ───
    let (sub_summaries, gitlink_map) = submodules::process(repo, opts)?;

    // ── Categorize every ref in the repo ────────────────────────────────────
    let refs = collect_refs(repo, opts.rules)?;
    stats.branches_scanned = refs.branches.len();

    // ── Walk every commit reachable from refs we intend to keep ──────────────
    let mut walk = repo.revwalk()?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
    for (_, oid) in refs
        .branches
        .iter()
        .chain(refs.remotes.iter())
        .chain(refs.generic.iter())
        .chain(refs.stash.iter())
    {
        walk.push(*oid)?;
    }
    for tag in &refs.tags {
        walk.push(tag.commit_oid)?;
    }

    let oids: Vec<Oid> = walk
        .collect::<Result<Vec<_>, _>>()
        .context("failed to walk commit graph")?;
    stats.total_commits = oids.len();

    let pb = ProgressBar::new(oids.len() as u64);
    if let Ok(style) = ProgressStyle::default_bar()
        .template("  {spinner:.cyan} [{elapsed_precise}] [{bar:38.cyan/blue}] {pos}/{len} ({eta})")
    {
        pb.set_style(style.progress_chars("=>-"));
    }

    let workdir = repo.workdir().map(Path::to_path_buf);

    // old OID → new OID for commits actually rewritten.
    let mut remap: HashMap<Oid, Oid> = HashMap::new();
    // dry-run: commits that WOULD be rewritten.
    let mut dirty: HashSet<Oid> = HashSet::new();
    let mut records: Vec<CommitRecord> = Vec::with_capacity(oids.len());
    let mut tree_cache = TreeCache::new();

    for &oid in &oids {
        let commit = repo.find_commit(oid).context("failed to find commit")?;

        let original_msg = commit.message_raw().unwrap_or("").to_string();
        let cleaned_msg = cleaner::clean_message(&original_msg, &opts.rules.messages);
        let msg_changed = cleaned_msg != original_msg;

        let (new_author, author_changed) =
            sanitize_sig(&commit.author(), opts.replacement, opts.rules)?;
        let (new_committer, committer_changed) =
            sanitize_sig(&commit.committer(), opts.replacement, opts.rules)?;
        let identity_changed = author_changed || committer_changed;

        let parents: Vec<Oid> = commit.parent_ids().collect();
        let parents_changed = if opts.dry_run {
            parents.iter().any(|p| dirty.contains(p))
        } else {
            parents.iter().any(|p| remap.contains_key(p))
        };

        let (new_tree, gitlinks) = if gitlink_map.is_empty() {
            (None, Vec::new())
        } else {
            submodules::remap_tree(
                repo,
                commit.tree_id(),
                &gitlink_map,
                !opts.dry_run,
                &mut tree_cache,
            )?
        };
        let tree_changed = !gitlinks.is_empty();

        let signed_before = signing::commit_is_signed(&commit);
        let mut signed_after = signed_before;
        let mut new_oid = None;

        if msg_changed || identity_changed || parents_changed || tree_changed {
            if opts.verbose && (msg_changed || identity_changed || tree_changed) {
                pb.println(format!(
                    "{pad}  {} {}  {}",
                    "~".yellow(),
                    oid.to_string()[..8].dimmed(),
                    commit.summary().unwrap_or("(no subject)"),
                ));
            }

            let sign_it = signing::wanted(opts.sign, signed_before);
            if opts.dry_run {
                dirty.insert(oid);
                signed_after = sign_it;
            } else {
                let new_parents: Vec<Oid> = parents
                    .iter()
                    .map(|p| remap.get(p).copied().unwrap_or(*p))
                    .collect();
                let tree_oid = new_tree.unwrap_or_else(|| commit.tree_id());

                let id = if sign_it {
                    let wd = workdir
                        .as_deref()
                        .context("--sign needs a work tree (bare repositories are unsupported)")?;
                    stats.commits_signed += 1;
                    signing::commit_tree_signed(
                        wd,
                        tree_oid,
                        &new_parents,
                        &new_author,
                        &new_committer,
                        &cleaned_msg,
                    )
                    .with_context(|| format!("failed to re-sign commit {oid}"))?
                } else {
                    let tree = repo
                        .find_tree(tree_oid)
                        .context("failed to read commit tree")?;
                    let parent_commits: Vec<git2::Commit> = new_parents
                        .iter()
                        .map(|&p| repo.find_commit(p))
                        .collect::<Result<Vec<_>, _>>()
                        .context("failed to find parent commit")?;
                    let parent_refs: Vec<&git2::Commit> = parent_commits.iter().collect();
                    repo.commit(
                        None,
                        &new_author,
                        &new_committer,
                        &cleaned_msg,
                        &tree,
                        &parent_refs,
                    )
                    .context("failed to write rewritten commit")?
                };
                signed_after = sign_it;
                remap.insert(oid, id);
                new_oid = Some(id);
            }

            stats.rewritten_commits += 1;
            if msg_changed {
                stats.messages_cleaned += 1;
            }
            if identity_changed {
                stats.identities_sanitized += 1;
            }
            if tree_changed {
                stats.trees_remapped += 1;
            }
        }

        records.push(CommitRecord {
            old: oid,
            new: new_oid,
            parents,
            summary: commit.summary().unwrap_or("(no subject)").to_string(),
            old_message: original_msg,
            new_message: cleaned_msg,
            old_author: render(&commit.author()),
            new_author: render(&new_author),
            old_committer: render(&commit.committer()),
            new_committer: render(&new_committer),
            signed_before,
            signed_after,
            msg_changed,
            identity_changed,
            parents_changed,
            gitlinks,
        });

        pb.inc(1);
    }
    pb.finish_and_clear();

    let in_set = |oid: &Oid| -> bool {
        if opts.dry_run {
            dirty.contains(oid)
        } else {
            remap.contains_key(oid)
        }
    };

    // Only create recovery refs once we know this run will mutate something.
    // Existing recovery refs are deliberately immutable: a later invocation
    // must never replace the original pre-luddite restore points.
    let will_delete_notes = !refs.notes_to_delete.is_empty();
    let will_drop_stash = opts.guarantee && !opts.keep_stash && !refs.stash.is_empty();
    if !opts.dry_run
        && !opts.guarantee
        && !opts.force
        && (!remap.is_empty() || will_delete_notes || will_drop_stash)
    {
        stats.refs_backed_up = create_backup_refs(repo, &refs)?;
        if stats.refs_backed_up > 0 {
            println!(
                "{pad}{}  {} refs backed up under refs/backup/luddite/",
                "✓".green(),
                stats.refs_backed_up
            );
        } else {
            println!(
                "{pad}{}  existing refs/backup/luddite/ restore points retained",
                "✓".green()
            );
        }
    }

    // ── Update refs to point at rewritten tips ───────────────────────────────
    let mut ref_updates: Vec<RefUpdate> = Vec::new();
    for (name, old) in &refs.branches {
        if in_set(old) {
            let full = format!("refs/heads/{name}");
            let new = remap.get(old).copied().unwrap_or(*old);
            if !opts.dry_run {
                set_ref(repo, &full, new)?;
            }
            ref_updates.push(RefUpdate {
                name: full,
                old: *old,
                new,
                kind: RefKind::Branch,
            });
            stats.branches_updated += 1;
        }
    }
    for (name, old) in &refs.remotes {
        if in_set(old) {
            let new = remap.get(old).copied().unwrap_or(*old);
            if !opts.dry_run {
                set_ref(repo, name, new)?;
            }
            ref_updates.push(RefUpdate {
                name: name.clone(),
                old: *old,
                new,
                kind: RefKind::Remote,
            });
            stats.remotes_updated += 1;
        }
    }
    for (name, old) in &refs.generic {
        if in_set(old) {
            let new = remap.get(old).copied().unwrap_or(*old);
            if !opts.dry_run {
                set_ref(repo, name, new)?;
            }
            ref_updates.push(RefUpdate {
                name: name.clone(),
                old: *old,
                new,
                kind: RefKind::Other,
            });
            stats.generic_refs_updated += 1;
        }
    }
    for (name, old) in &refs.stash {
        if in_set(old) {
            let new = remap.get(old).copied().unwrap_or(*old);
            if !opts.dry_run {
                set_ref(repo, name, new)?;
            }
            ref_updates.push(RefUpdate {
                name: name.clone(),
                old: *old,
                new,
                kind: RefKind::Stash,
            });
        }
    }
    update_tags(
        repo,
        workdir.as_deref(),
        &refs.tags,
        &remap,
        opts,
        &in_set,
        &mut stats,
        &mut ref_updates,
    )?;

    // ── Delete AI notes refs ─────────────────────────────────────────────────
    for name in &refs.notes_to_delete {
        stats.notes_deleted += 1;
        if !opts.dry_run {
            if let Ok(mut r) = repo.find_reference(name) {
                r.delete()
                    .with_context(|| format!("failed to delete notes ref {}", name))?;
            }
        }
    }

    // ── Drop the stash under --guarantee (it anchors pre-rewrite history) ─────
    if opts.guarantee && !opts.keep_stash && !refs.stash.is_empty() {
        stats.stash_dropped = true;
        if !opts.dry_run {
            if let Ok(mut r) = repo.find_reference("refs/stash") {
                r.delete().context("failed to drop stash ref")?;
            }
        }
    }

    if !opts.dry_run {
        // ── Update detached HEAD if it points at a rewritten commit ──────────
        if let Ok(head) = repo.head() {
            if !head.is_branch() {
                if let Some(old) = head.target() {
                    if let Some(&new) = remap.get(&old) {
                        repo.set_head_detached(new)
                            .context("failed to update detached HEAD")?;
                    }
                }
            }
        }

        // ── Keep the index's gitlinks in step with the remapped trees ────────
        if !gitlink_map.is_empty() {
            stats.index_gitlinks_updated = update_index_gitlinks(repo, gitlink_map.raw_map())?;
        }

        // ── Persist the maps (filter-repo style) ─────────────────────────────
        if !remap.is_empty() {
            write_maps(repo, &remap, &ref_updates)?;
        }
    }

    Ok(Report {
        stats,
        dry_run: opts.dry_run,
        records,
        remap,
        dirty,
        ref_updates,
        submodules: sub_summaries,
    })
}

/// Reconstruct the most recent before/after report without rewriting anything.
/// Newer versions use the persisted maps; repositories rewritten by 0.2 can be
/// recovered from its legacy `refs/backup/luddite/<branch>` restore points.
pub fn load_report(repo: &Repository) -> Result<Report> {
    let map_dir = repo.path().join("luddite");
    let map_path = map_dir.join("commit-map");
    let ref_path = map_dir.join("ref-map");

    let (remap, ref_updates) = if map_path.is_file() {
        let remap = read_commit_map(&map_path)?;
        let refs = if ref_path.is_file() {
            read_ref_map(&ref_path)?
        } else {
            Vec::new()
        };
        (remap, refs)
    } else {
        infer_legacy_maps(repo)?
    };

    if remap.is_empty() {
        bail!(
            "no previous luddite rewrite found; run luddite normally first (or keep its refs/backup/luddite restore refs)"
        );
    }

    let missing: Vec<Oid> = remap
        .keys()
        .copied()
        .filter(|oid| repo.find_commit(*oid).is_err())
        .collect();
    if !missing.is_empty() {
        bail!(
            "cannot replay before/after: {} original commit(s) were pruned (likely by --guarantee); restore the backup bundle or review during the guarantee run",
            missing.len()
        );
    }

    let inverse: HashMap<Oid, Oid> = remap.iter().map(|(old, new)| (*new, *old)).collect();
    let oids = current_commits(repo)?;
    let mut records = Vec::with_capacity(oids.len());

    for new_oid in oids {
        let old_oid = inverse.get(&new_oid).copied().unwrap_or(new_oid);
        let new = repo
            .find_commit(new_oid)
            .with_context(|| format!("failed to load current commit {new_oid}"))?;
        if old_oid == new_oid {
            let message = new.message_raw().unwrap_or("").to_string();
            let author = render(&new.author());
            let committer = render(&new.committer());
            let signed = signing::commit_is_signed(&new);
            records.push(CommitRecord {
                old: new_oid,
                new: None,
                parents: new.parent_ids().collect(),
                summary: new.summary().unwrap_or("(no subject)").to_string(),
                old_message: message.clone(),
                new_message: message,
                old_author: author.clone(),
                new_author: author,
                old_committer: committer.clone(),
                new_committer: committer,
                signed_before: signed,
                signed_after: signed,
                msg_changed: false,
                identity_changed: false,
                parents_changed: false,
                gitlinks: Vec::new(),
            });
            continue;
        }
        let old = repo
            .find_commit(old_oid)
            .with_context(|| format!("failed to load original commit {old_oid}"))?;
        let old_parents: Vec<Oid> = old.parent_ids().collect();
        let new_parents: Vec<Oid> = new.parent_ids().collect();
        let parents_changed = old_parents != new_parents;
        let old_message = old.message_raw().unwrap_or("").to_string();
        let new_message = new.message_raw().unwrap_or("").to_string();
        let old_author = render(&old.author());
        let new_author = render(&new.author());
        let old_committer = render(&old.committer());
        let new_committer = render(&new.committer());
        let gitlinks = changed_gitlinks(repo, old.tree_id(), new.tree_id())?;

        records.push(CommitRecord {
            old: old_oid,
            new: Some(new_oid),
            parents: old_parents,
            summary: old.summary().unwrap_or("(no subject)").to_string(),
            msg_changed: old_message != new_message,
            identity_changed: old_author != new_author || old_committer != new_committer,
            parents_changed,
            signed_before: signing::commit_is_signed(&old),
            signed_after: signing::commit_is_signed(&new),
            old_message,
            new_message,
            old_author,
            new_author,
            old_committer,
            new_committer,
            gitlinks,
        });
    }

    let mut stats = Stats {
        total_commits: records.len(),
        rewritten_commits: records.iter().filter(|r| r.new.is_some()).count(),
        messages_cleaned: records.iter().filter(|r| r.msg_changed).count(),
        identities_sanitized: records.iter().filter(|r| r.identity_changed).count(),
        trees_remapped: records.iter().filter(|r| !r.gitlinks.is_empty()).count(),
        commits_signed: records
            .iter()
            .filter(|r| r.new.is_some() && r.signed_after)
            .count(),
        branches_scanned: repo
            .branches(Some(git2::BranchType::Local))
            .map(|b| b.filter_map(|item| item.ok()).count())
            .unwrap_or(0),
        ..Stats::default()
    };
    for update in &ref_updates {
        match update.kind {
            RefKind::Branch => stats.branches_updated += 1,
            RefKind::Remote => stats.remotes_updated += 1,
            RefKind::Tag => {
                stats.tags_updated += 1;
                if repo
                    .find_tag(update.new)
                    .ok()
                    .and_then(|tag| {
                        signing::split_tag_signature(tag.message().unwrap_or(""))
                            .1
                            .map(str::to_string)
                    })
                    .is_some()
                {
                    stats.tags_signed += 1;
                }
            }
            RefKind::Other => stats.generic_refs_updated += 1,
            RefKind::Stash => {}
        }
    }

    Ok(Report {
        stats,
        dry_run: false,
        records,
        remap,
        dirty: HashSet::new(),
        ref_updates,
        submodules: Vec::new(),
    })
}

fn read_commit_map(path: &Path) -> Result<HashMap<Oid, Oid>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut map = HashMap::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with("old ") {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 2 {
            bail!("malformed {} line {}", path.display(), i + 1);
        }
        let old = Oid::from_str(fields[0])
            .with_context(|| format!("invalid old id in {} line {}", path.display(), i + 1))?;
        let new = Oid::from_str(fields[1])
            .with_context(|| format!("invalid new id in {} line {}", path.display(), i + 1))?;
        map.insert(old, new);
    }
    Ok(map)
}

fn read_ref_map(path: &Path) -> Result<Vec<RefUpdate>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut refs = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line == "ref old new" {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 3 {
            bail!("malformed {} line {}", path.display(), i + 1);
        }
        refs.push(RefUpdate {
            name: fields[0].to_string(),
            old: Oid::from_str(fields[1])
                .with_context(|| format!("invalid old id in {} line {}", path.display(), i + 1))?,
            new: Oid::from_str(fields[2])
                .with_context(|| format!("invalid new id in {} line {}", path.display(), i + 1))?,
            kind: ref_kind(fields[0]),
        });
    }
    Ok(refs)
}

fn ref_kind(name: &str) -> RefKind {
    if name.starts_with("refs/heads/") {
        RefKind::Branch
    } else if name.starts_with("refs/remotes/") {
        RefKind::Remote
    } else if name.starts_with("refs/tags/") {
        RefKind::Tag
    } else if name == "refs/stash" {
        RefKind::Stash
    } else {
        RefKind::Other
    }
}

/// Infer a 0.2-era map by aligning current branch history with immutable
/// `refs/backup/luddite/<branch>` tips. Trees and timestamps were preserved by
/// that release, which gives us a strong match even when the message changed.
fn infer_legacy_maps(repo: &Repository) -> Result<(HashMap<Oid, Oid>, Vec<RefUpdate>)> {
    let mut remap = HashMap::new();
    let mut updates = Vec::new();

    for branch in repo.branches(Some(git2::BranchType::Local))? {
        let (branch, _) = branch?;
        let Some(name) = branch.name()?.map(str::to_string) else {
            continue;
        };
        let Some(current_tip) = branch.get().target() else {
            continue;
        };
        let candidates = [
            format!("refs/backup/luddite/heads/{name}"),
            format!("refs/backup/luddite/{name}"),
        ];
        for candidate in candidates {
            let Some(old_tip) = repo
                .find_reference(&candidate)
                .ok()
                .and_then(|reference| reference.target())
            else {
                continue;
            };
            // A buggy clean rerun in early 0.3 builds may have created the new
            // heads/ backup namespace from already-clean refs. Keep looking for
            // the older legacy ref instead of accepting an identity mapping.
            if old_tip == current_tip {
                continue;
            }
            let Some(rewritten_tip) = find_matching_commit(repo, old_tip, current_tip)? else {
                continue;
            };
            if old_tip == rewritten_tip {
                continue;
            }
            let mut branch_map = remap.clone();
            if pair_history(repo, old_tip, rewritten_tip, &mut branch_map).is_err() {
                continue;
            }
            remap = branch_map;
            updates.push(RefUpdate {
                name: format!("refs/heads/{name}"),
                old: old_tip,
                new: rewritten_tip,
                kind: RefKind::Branch,
            });
            break;
        }
    }

    if remap.is_empty() {
        bail!(
            "no .git/luddite/commit-map or compatible refs/backup/luddite branch history was found"
        );
    }
    Ok((remap, updates))
}

fn find_matching_commit(repo: &Repository, old: Oid, current_tip: Oid) -> Result<Option<Oid>> {
    let old_commit = repo.find_commit(old)?;
    let mut walk = repo.revwalk()?;
    walk.push(current_tip)?;
    for oid in walk {
        let oid = oid?;
        let candidate = repo.find_commit(oid)?;
        if commits_align(&old_commit, &candidate) {
            return Ok(Some(oid));
        }
    }
    Ok(None)
}

fn commits_align(old: &git2::Commit, new: &git2::Commit) -> bool {
    old.tree_id() == new.tree_id()
        && old.parent_count() == new.parent_count()
        && same_time(old.author().when(), new.author().when())
        && same_time(old.committer().when(), new.committer().when())
}

fn same_time(a: git2::Time, b: git2::Time) -> bool {
    a.seconds() == b.seconds() && a.offset_minutes() == b.offset_minutes()
}

fn pair_history(
    repo: &Repository,
    old_tip: Oid,
    new_tip: Oid,
    map: &mut HashMap<Oid, Oid>,
) -> Result<()> {
    let mut pending = vec![(old_tip, new_tip)];
    while let Some((old_oid, new_oid)) = pending.pop() {
        if old_oid == new_oid {
            continue;
        }
        if let Some(existing) = map.get(&old_oid) {
            if *existing != new_oid {
                bail!("legacy backup histories map {old_oid} inconsistently");
            }
            continue;
        }
        let old = repo.find_commit(old_oid)?;
        let new = repo.find_commit(new_oid)?;
        if !commits_align(&old, &new) {
            bail!("legacy backup history does not align at {old_oid} → {new_oid}");
        }
        map.insert(old_oid, new_oid);
        pending.extend(old.parent_ids().zip(new.parent_ids()));
    }
    Ok(())
}

fn current_commits(repo: &Repository) -> Result<Vec<Oid>> {
    let mut walk = repo.revwalk()?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
    let mut pushed = false;
    for reference in repo.references()? {
        let reference = reference?;
        let Some(name) = reference.name() else {
            continue;
        };
        if name.starts_with("refs/backup/")
            || name.starts_with("refs/original/")
            || name.starts_with("refs/notes/")
        {
            continue;
        }
        if let Ok(commit) = reference.peel_to_commit() {
            walk.push(commit.id())?;
            pushed = true;
        }
    }
    if !pushed {
        bail!("repository has no commits to review");
    }
    walk.collect::<Result<Vec<_>, _>>()
        .context("failed to walk current commit graph")
}

fn changed_gitlinks(repo: &Repository, old_tree: Oid, new_tree: Oid) -> Result<Vec<GitlinkChange>> {
    if old_tree == new_tree {
        return Ok(Vec::new());
    }
    let mut old = HashMap::new();
    let mut new = HashMap::new();
    collect_gitlinks(repo, old_tree, "", &mut old)?;
    collect_gitlinks(repo, new_tree, "", &mut new)?;
    let mut changes: Vec<GitlinkChange> = old
        .into_iter()
        .filter_map(|(path, old)| {
            new.get(&path)
                .copied()
                .filter(|new| *new != old)
                .map(|new| GitlinkChange { path, old, new })
        })
        .collect();
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}

fn collect_gitlinks(
    repo: &Repository,
    tree_oid: Oid,
    prefix: &str,
    out: &mut HashMap<String, Oid>,
) -> Result<()> {
    let tree = repo.find_tree(tree_oid)?;
    for entry in tree.iter() {
        let Some(name) = entry.name() else {
            continue;
        };
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        match entry.kind() {
            Some(ObjectType::Commit) => {
                out.insert(path, entry.id());
            }
            Some(ObjectType::Tree) => collect_gitlinks(repo, entry.id(), &path, out)?,
            _ => {}
        }
    }
    Ok(())
}

// ── Ref collection ──────────────────────────────────────────────────────────

enum TagKind {
    Lightweight,
    Annotated { tag_oid: Oid },
}

struct TagEntry {
    ref_name: String,
    commit_oid: Oid,
    kind: TagKind,
}

#[derive(Default)]
struct Refs {
    branches: Vec<(String, Oid)>, // short name → tip
    remotes: Vec<(String, Oid)>,  // full ref name → tip
    generic: Vec<(String, Oid)>,  // full ref name → tip (refs/* not otherwise handled)
    stash: Vec<(String, Oid)>,    // refs/stash, if present
    tags: Vec<TagEntry>,
    notes_to_delete: Vec<String>, // full notes ref names matching delete rules
}

fn collect_refs(repo: &Repository, rules: &RuleSet) -> Result<Refs> {
    let mut refs = Refs::default();

    for r in repo.references()? {
        let r = r?;
        let name = match r.name() {
            Some(n) => n.to_string(),
            None => continue,
        };

        // Never touch our own backups or filter-branch originals.
        if name.starts_with("refs/backup/") || name.starts_with("refs/original/") {
            continue;
        }
        // Symbolic refs (e.g. refs/remotes/origin/HEAD) carry no commit target.
        let target = match r.target() {
            Some(t) => t,
            None => continue,
        };

        if name.starts_with("refs/heads/") {
            let short = name.trim_start_matches("refs/heads/").to_string();
            refs.branches.push((short, target));
        } else if name.starts_with("refs/remotes/") {
            refs.remotes.push((name, target));
        } else if name.starts_with("refs/tags/") {
            // handled via tag_foreach below for proper peeling
        } else if name == "refs/stash" {
            refs.stash.push((name, target));
        } else if name.starts_with("refs/notes/") {
            if rules
                .delete_notes
                .iter()
                .any(|rule| rule.regex.is_match(&name))
            {
                refs.notes_to_delete.push(name);
            }
            // non-AI notes: left alone (they may reference rewritten shas; warned in main)
        } else {
            refs.generic.push((name, target));
        }
    }

    refs.tags = collect_tag_refs(repo)?;
    Ok(refs)
}

fn collect_tag_refs(repo: &Repository) -> Result<Vec<TagEntry>> {
    let mut result = Vec::new();
    repo.tag_foreach(|oid, name_bytes| {
        let ref_name = match std::str::from_utf8(name_bytes) {
            Ok(s) => s.to_string(),
            Err(_) => return true,
        };
        let commit_oid = match repo
            .find_object(oid, None)
            .and_then(|o| o.peel(ObjectType::Commit))
        {
            Ok(obj) => obj.id(),
            Err(_) => return true,
        };
        let kind = match repo.find_tag(oid) {
            Ok(_) => TagKind::Annotated { tag_oid: oid },
            Err(_) => TagKind::Lightweight,
        };
        result.push(TagEntry {
            ref_name,
            commit_oid,
            kind,
        });
        true
    })?;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn update_tags(
    repo: &Repository,
    workdir: Option<&Path>,
    tags: &[TagEntry],
    remap: &HashMap<Oid, Oid>,
    opts: &Options,
    in_set: &dyn Fn(&Oid) -> bool,
    stats: &mut Stats,
    ref_updates: &mut Vec<RefUpdate>,
) -> Result<()> {
    for tag in tags {
        if !in_set(&tag.commit_oid) {
            continue;
        }
        stats.tags_updated += 1;
        let short = tag.ref_name.trim_start_matches("refs/tags/");
        let new_commit_oid = remap
            .get(&tag.commit_oid)
            .copied()
            .unwrap_or(tag.commit_oid);

        match &tag.kind {
            TagKind::Lightweight => {
                if !opts.dry_run {
                    set_ref(repo, &tag.ref_name, new_commit_oid)?;
                }
                ref_updates.push(RefUpdate {
                    name: tag.ref_name.clone(),
                    old: tag.commit_oid,
                    new: new_commit_oid,
                    kind: RefKind::Tag,
                });
            }
            TagKind::Annotated { tag_oid } => {
                let existing = repo
                    .find_tag(*tag_oid)
                    .context("failed to read annotated tag object")?;
                let (message, old_sig) =
                    signing::split_tag_signature(existing.message().unwrap_or(""));
                let sign_it = signing::wanted(opts.sign, old_sig.is_some());
                if opts.dry_run {
                    if sign_it {
                        stats.tags_signed += 1;
                    }
                    ref_updates.push(RefUpdate {
                        name: tag.ref_name.clone(),
                        old: *tag_oid,
                        new: *tag_oid,
                        kind: RefKind::Tag,
                    });
                    continue;
                }
                let name = existing.name().unwrap_or(short);
                let tagger = match existing.tagger().map(|s| clone_signature(&s)).transpose()? {
                    Some(s) => s,
                    None => Signature::now("luddite", "luddite@localhost")?,
                };
                let new_tag_oid = if sign_it {
                    let wd = workdir.context("--sign needs a work tree")?;
                    stats.tags_signed += 1;
                    signing::tag_signed(wd, name, new_commit_oid, &tagger, message)
                        .with_context(|| format!("failed to re-sign tag {name}"))?
                } else {
                    let new_commit = repo
                        .find_object(new_commit_oid, Some(ObjectType::Commit))
                        .context("failed to find rewritten commit for tag")?;
                    repo.tag(name, &new_commit, &tagger, message, true)
                        .with_context(|| format!("failed to recreate annotated tag {name}"))?
                };
                ref_updates.push(RefUpdate {
                    name: tag.ref_name.clone(),
                    old: *tag_oid,
                    new: new_tag_oid,
                    kind: RefKind::Tag,
                });
            }
        }
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn set_ref(repo: &Repository, name: &str, target: Oid) -> Result<()> {
    repo.find_reference(name)
        .with_context(|| format!("failed to find ref {}", name))?
        .set_target(target, "luddite: erased AI indicators")
        .with_context(|| format!("failed to update ref {}", name))?;
    Ok(())
}

fn render(sig: &Signature) -> String {
    cleaner::render_identity(sig.name().unwrap_or(""), sig.email().unwrap_or(""))
}

/// Replace an AI author/committer with the configured identity, preserving the
/// original timestamp. Returns (signature, changed).
fn sanitize_sig(
    sig: &Signature,
    repl: &Identity,
    rules: &RuleSet,
) -> Result<(Signature<'static>, bool)> {
    let rendered = render(sig);
    if cleaner::identity_matches(&rendered, &rules.identities) {
        let new = Signature::new(&repl.name, &repl.email, &sig.when())
            .context("failed to build replacement signature")?;
        Ok((new, true))
    } else {
        Ok((clone_signature(sig)?, false))
    }
}

/// Mirror every ref we may touch under refs/backup/luddite/ (heads/, tags/,
/// remotes/, stash, other/). Existing backups are immutable so a repeat run
/// cannot destroy the original restore points. Returns how many were created.
fn create_backup_refs(repo: &Repository, refs: &Refs) -> Result<usize> {
    let mut n = 0;
    let mut put = |name: String, oid: Oid| -> Result<()> {
        if repo.find_reference(&name).is_ok() {
            return Ok(());
        }
        repo.reference(&name, oid, false, "luddite backup")
            .with_context(|| format!("failed to create backup ref {}", name))?;
        n += 1;
        Ok(())
    };
    for (name, oid) in &refs.branches {
        put(format!("refs/backup/luddite/heads/{name}"), *oid)?;
    }
    for (name, oid) in &refs.remotes {
        let rest = name.trim_start_matches("refs/remotes/");
        put(format!("refs/backup/luddite/remotes/{rest}"), *oid)?;
    }
    for tag in &refs.tags {
        let target = match tag.kind {
            TagKind::Annotated { tag_oid } => tag_oid,
            TagKind::Lightweight => tag.commit_oid,
        };
        let rest = tag.ref_name.trim_start_matches("refs/tags/");
        put(format!("refs/backup/luddite/tags/{rest}"), target)?;
    }
    for (_, oid) in &refs.stash {
        put("refs/backup/luddite/stash".to_string(), *oid)?;
    }
    for (name, oid) in &refs.generic {
        let rest = name.trim_start_matches("refs/");
        put(format!("refs/backup/luddite/other/{rest}"), *oid)?;
    }
    for name in &refs.notes_to_delete {
        if let Some(oid) = repo.find_reference(name).ok().and_then(|r| r.target()) {
            let rest = name.trim_start_matches("refs/");
            put(format!("refs/backup/luddite/other/{rest}"), oid)?;
        }
    }
    Ok(n)
}

/// Point every gitlink entry in the index at its rewritten submodule commit.
fn update_index_gitlinks(repo: &Repository, map: &HashMap<Oid, Oid>) -> Result<usize> {
    let mut index = repo.index().context("failed to open the index")?;
    let updated: Vec<git2::IndexEntry> = index
        .iter()
        .filter(|e| e.mode == 0o160000)
        .filter_map(|e| map.get(&e.id).map(|&new| git2::IndexEntry { id: new, ..e }))
        .collect();
    for entry in &updated {
        index
            .add(entry)
            .context("failed to update a gitlink in the index")?;
    }
    if !updated.is_empty() {
        index.write().context("failed to write the index")?;
    }
    Ok(updated.len())
}

/// `.git/luddite/commit-map` (old new) and `.git/luddite/ref-map` (ref old new).
fn write_maps(repo: &Repository, remap: &HashMap<Oid, Oid>, refs: &[RefUpdate]) -> Result<()> {
    let dir = repo.path().join("luddite");
    std::fs::create_dir_all(&dir).context("failed to create .git/luddite")?;

    let mut pairs: Vec<(&Oid, &Oid)> = remap.iter().collect();
    pairs.sort();
    let mut f = std::fs::File::create(dir.join("commit-map"))?;
    writeln!(f, "old                                      new")?;
    for (old, new) in pairs {
        writeln!(f, "{old} {new}")?;
    }

    let mut f = std::fs::File::create(dir.join("ref-map"))?;
    writeln!(f, "ref old new")?;
    for r in refs {
        writeln!(f, "{} {} {}", r.name, r.old, r.new)?;
    }
    Ok(())
}

fn clone_signature(sig: &Signature) -> Result<Signature<'static>> {
    Signature::new(
        sig.name().unwrap_or(""),
        sig.email().unwrap_or(""),
        &sig.when(),
    )
    .context("failed to clone git signature")
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Time;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture() -> (Repository, std::path::PathBuf, Oid, Oid) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("luddite-rewriter-{nonce}"));
        let repo = Repository::init(&path).unwrap();
        let tree_oid = repo.treebuilder(None).unwrap().write().unwrap();
        let when = Time::new(1_700_000_000, -240);
        let sig = Signature::new("Human", "human@example.com", &when).unwrap();
        let (old, new) = {
            let tree = repo.find_tree(tree_oid).unwrap();
            let old = repo
                .commit(
                    None,
                    &sig,
                    &sig,
                    "dirty\n\nCo-authored-by: Codex <noreply@openai.com>\n",
                    &tree,
                    &[],
                )
                .unwrap();
            let new = repo
                .commit(None, &sig, &sig, "dirty\n", &tree, &[])
                .unwrap();
            (old, new)
        };
        repo.reference("refs/heads/main", new, true, "fixture")
            .unwrap();
        repo.set_head("refs/heads/main").unwrap();
        (repo, path, old, new)
    }

    #[test]
    fn legacy_replay_skips_newer_identity_backup() {
        let (repo, path, old, new) = fixture();
        repo.reference(
            "refs/backup/luddite/heads/main",
            new,
            true,
            "bad clean-rerun backup",
        )
        .unwrap();
        repo.reference("refs/backup/luddite/main", old, true, "real legacy backup")
            .unwrap();

        let report = load_report(&repo).unwrap();
        assert_eq!(report.remap.get(&old), Some(&new));
        assert_eq!(report.stats.rewritten_commits, 1);
        assert_eq!(report.stats.messages_cleaned, 1);

        drop(repo);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn existing_backup_is_never_replaced() {
        let (repo, path, old, new) = fixture();
        repo.reference("refs/backup/luddite/heads/main", old, true, "original")
            .unwrap();
        let refs = Refs {
            branches: vec![("main".into(), new)],
            ..Refs::default()
        };

        assert_eq!(create_backup_refs(&repo, &refs).unwrap(), 0);
        assert_eq!(
            repo.find_reference("refs/backup/luddite/heads/main")
                .unwrap()
                .target(),
            Some(old)
        );

        drop(repo);
        std::fs::remove_dir_all(path).unwrap();
    }
}
