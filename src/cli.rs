use clap::{Parser, ValueEnum};
use std::path::PathBuf;

/// Which rewritten objects get a fresh signature under `--sign`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SignMode {
    /// Re-sign only the commits and annotated tags that were signed before the rewrite
    Signed,
    /// Sign every rewritten commit and every recreated annotated tag
    All,
}

#[derive(Parser)]
#[command(
    name = "luddite",
    version,
    about = "Deterministically erase all evidence of AI/agent involvement from git history",
    long_about = "Rewrites every branch, tag, remote-tracking ref, and stash to remove AI \
                  agent footers, co-author trailers, and bot author/committer identities \
                  (Claude Code, Copilot, Cursor, Devin, Aider, …) while preserving commit \
                  order, human authorship, timestamps, and submodule gitlinks.\n\n\
                  Default mode backs up every ref it touches under refs/backup/luddite/ \
                  (heads/, tags/, remotes/, stash, other/) and writes the old→new maps to \
                  .git/luddite/{commit-map,ref-map}. After the run an interactive screen \
                  shows the commit graph with every rewritten commit highlighted, a \
                  before/after pane, and a confirm-and-push step whose --force-with-lease \
                  values come from a live ls-remote. Reopen it later with --ui.\n\n\
                  --guarantee runs the full deterministic erase: bundles a backup OUTSIDE \
                  the repo, rewrites everything, deletes AI notes, drops the stash, expires \
                  reflogs, prunes unreachable objects, then VERIFIES that zero indicators \
                  survive across every ref — exiting non-zero if any remain."
)]
pub struct Cli {
    /// Path to the git repository (defaults to current directory)
    #[arg(default_value = ".")]
    pub repo: PathBuf,

    /// Show what would change without modifying anything
    #[arg(long, short = 'n')]
    pub dry_run: bool,

    /// Full deterministic erase: bundle backup, purge old copies, verify clean
    #[arg(long)]
    pub guarantee: bool,

    /// Skip creating backup refs before rewriting (ignored under --guarantee)
    #[arg(long)]
    pub force: bool,

    /// Identity to replace AI authors/committers with (default: repo user.name/user.email)
    #[arg(long, value_name = "NAME <EMAIL>")]
    pub replace_identity: Option<String>,

    /// Re-sign rewritten commits and recreated annotated tags with the repository's
    /// signing config (gpg.format, user.signingkey, gpg.program). Without a value only
    /// objects that were signed before are re-signed; `--sign=all` signs every rewrite.
    #[arg(long, value_name = "MODE", value_enum, num_args = 0..=1, default_missing_value = "signed")]
    pub sign: Option<SignMode>,

    /// Also scrub every initialized submodule (recursively) and remap the superproject's
    /// gitlinks and index to the rewritten submodule commits
    #[arg(long)]
    pub recurse_submodules: bool,

    /// Skip the interactive review/push screen after the run (also skipped when
    /// stdin/stdout are not a terminal)
    #[arg(long)]
    pub no_ui: bool,

    /// Open the review/push screen for the most recent rewrite without scanning
    /// or changing the repository. Loads .git/luddite maps, with a fallback for
    /// legacy refs/backup/luddite/<branch> backups created by luddite 0.2.
    #[arg(long)]
    pub ui: bool,

    /// Push the rewritten refs to REMOTE without the interactive screen. Every ref is
    /// pushed with --force-with-lease=<ref>:<value observed by ls-remote>, atomically.
    #[arg(long, value_name = "REMOTE")]
    pub push: Option<String>,

    /// Keep the stash under --guarantee (by default it is dropped — it anchors old history)
    #[arg(long)]
    pub keep_stash: bool,

    /// Additional TOML patterns file to load (merged last, after discovered configs)
    #[arg(long, value_name = "FILE")]
    pub patterns: Option<PathBuf>,

    /// Ignore discovered config files (.luddite.toml, ~/.config/luddite/patterns.toml)
    #[arg(long)]
    pub no_config: bool,

    /// Print each modified commit as it is processed
    #[arg(long, short)]
    pub verbose: bool,

    /// List all loaded rules and exit
    #[arg(long)]
    pub list_patterns: bool,
}
