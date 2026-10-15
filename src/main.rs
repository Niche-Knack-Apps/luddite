mod cleaner;
mod cli;
mod gitops;
mod patterns;
mod push;
mod rewriter;
mod signing;
mod submodules;
mod tui;
mod verify;

use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use git2::Repository;
use rewriter::{Identity, Options, RefKind, Report};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use submodules::{SubStatus, SubmoduleSummary};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{} {:#}", "error:".red().bold(), e);
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = cli::Cli::parse();

    let repo = Repository::open(&cli.repo)
        .map_err(|e| anyhow::anyhow!("{} is not a git repository: {}", cli.repo.display(), e))?;
    if repo.is_bare() {
        bail!("bare repositories are not supported");
    }
    let workdir = repo
        .workdir()
        .map(Path::to_path_buf)
        .context("repository has no work tree")?;

    if cli.ui {
        if cli.dry_run
            || cli.guarantee
            || cli.force
            || cli.replace_identity.is_some()
            || cli.sign.is_some()
            || cli.recurse_submodules
            || cli.no_ui
            || cli.push.is_some()
            || cli.keep_stash
            || cli.patterns.is_some()
            || cli.list_patterns
        {
            bail!("--ui replays the previous run and cannot be combined with rewrite options");
        }
        if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
            bail!("--ui requires an interactive stdin and stdout terminal");
        }
        eprintln!("  loading previous luddite review…");
        let report = rewriter::load_report(&repo)?;
        let outcome = tui::run(&repo, &report, None)?;
        println!();
        if let (true, Some(remote)) = (outcome.pushed, outcome.remote) {
            println!(
                "  {}  pushed {} refs to {} (atomic, --force-with-lease per ref)",
                "✓".green().bold(),
                outcome.refs,
                remote
            );
        } else {
            println!("  {}", "Review closed without pushing.".dimmed());
        }
        return Ok(ExitCode::SUCCESS);
    }

    // ── Load rule set: built-ins + discovered configs + explicit --patterns ──
    let mut extra: Vec<PathBuf> = Vec::new();
    if !cli.no_config {
        extra.extend(patterns::discover_configs(Some(&workdir)));
    }
    if let Some(p) = &cli.patterns {
        extra.push(p.clone());
    }
    let rules = patterns::load(&extra)?;

    if cli.list_patterns {
        print_rules(&rules, &extra);
        return Ok(ExitCode::SUCCESS);
    }

    let replacement = resolve_identity(&repo, cli.replace_identity.as_deref())?;
    if cli.push.is_some() && cli.dry_run {
        bail!("--push cannot be combined with --dry-run (nothing is rewritten)");
    }

    // ── Header ───────────────────────────────────────────────────────────────
    println!();
    println!(
        "  {}  {} rules  ({} message · {} identity · {} notes)",
        "luddite".bold(),
        rules.total().to_string().cyan(),
        rules.messages.len(),
        rules.identities.len(),
        rules.delete_notes.len(),
    );
    if cli.dry_run {
        println!("  {}  dry run — no changes will be made", "~".yellow());
    } else if cli.guarantee {
        println!(
            "  {}  guarantee mode — external bundle, purge, verify",
            "⛨".cyan()
        );
    } else if cli.force {
        println!("  {}  --force: backup refs will NOT be created", "!".red());
    }
    println!(
        "  {}  AI identities → {} <{}>",
        "→".dimmed(),
        replacement.name.dimmed(),
        replacement.email.dimmed()
    );
    if let Some(mode) = cli.sign {
        println!(
            "  {}  re-signing {} (repository signing config)",
            "✎".cyan(),
            match mode {
                cli::SignMode::Signed => "commits and tags that were signed before",
                cli::SignMode::All => "every rewritten commit and tag",
            }
        );
    }
    if cli.recurse_submodules {
        println!("  {}  recursing into initialized submodules", "↳".cyan());
    }
    println!();

    // ── Guarantee: bundle a full backup OUTSIDE the repo before touching it ──
    let bundle_dir = workdir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let repo_name = workdir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .to_string();
    if cli.guarantee && !cli.dry_run {
        let bundle = gitops::bundle_path(&bundle_dir, &repo_name);
        gitops::bundle_all(&workdir, &bundle)?;
        println!(
            "  {}  backup bundle: {}",
            "✓".green(),
            bundle.display().to_string().dimmed()
        );
    }

    let opts = Options {
        rules: &rules,
        replacement: &replacement,
        dry_run: cli.dry_run,
        guarantee: cli.guarantee,
        force: cli.force,
        keep_stash: cli.keep_stash,
        verbose: cli.verbose,
        sign: cli.sign,
        recurse_submodules: cli.recurse_submodules,
        bundle_dir: Some(&bundle_dir),
        depth: 0,
    };
    let report = rewriter::run(&repo, &opts)?;

    print_stats(&report.stats);
    print_submodules(&report.submodules, cli.recurse_submodules);

    // ── Guarantee: purge old copies, then verify ─────────────────────────────
    if cli.guarantee && !cli.dry_run {
        gitops::purge(&workdir)?;
        println!(
            "  {}  reflogs expired, unreachable objects pruned",
            "✓".green()
        );

        let residues = verify::verify(&repo, &rules)?;
        let sub_failed: Vec<&SubmoduleSummary> = report
            .submodules
            .iter()
            .filter(|s| matches!(s.status, SubStatus::VerifyFailed { .. }))
            .collect();
        println!();
        if residues.is_empty() && sub_failed.is_empty() {
            println!(
                "  {}  VERIFIED CLEAN — zero AI indicators across every ref",
                "✓".green().bold()
            );
        } else {
            println!(
                "  {}  VERIFICATION FAILED — {} indicator(s) survived:",
                "✗".red().bold(),
                residues.len()
            );
            for r in residues.iter().take(40) {
                println!(
                    "    {} {} [{}:{}] {}",
                    "•".red(),
                    r.oid.to_string()[..8.min(r.oid.to_string().len())].dimmed(),
                    r.kind,
                    r.rule,
                    r.summary.dimmed()
                );
            }
            if residues.len() > 40 {
                println!("    {} … and {} more", "•".red(), residues.len() - 40);
            }
            for s in sub_failed {
                println!(
                    "    {} submodule {} still carries indicators",
                    "•".red(),
                    s.path
                );
            }
            return Ok(ExitCode::FAILURE);
        }
    }

    if report.stats.rewritten_commits == 0 {
        println!();
        println!("  {}", "No AI indicators found.".dimmed());
        return Ok(ExitCode::SUCCESS);
    }

    // ── Review / push ────────────────────────────────────────────────────────
    let mut pushed_to: Option<(String, usize)> = None;
    let mut failed = false;
    if let Some(remote) = &cli.push {
        match push_now(&repo, &workdir, &report, remote)? {
            Some(n) => pushed_to = Some((remote.clone(), n)),
            None => failed = true,
        }
    } else if !cli.no_ui && std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
        let outcome = tui::run(&repo, &report, None)?;
        if outcome.pushed {
            pushed_to = outcome.remote.map(|r| (r, outcome.refs));
        }
    }

    println!();
    if let Some((remote, n)) = &pushed_to {
        println!(
            "  {}  pushed {} refs to {} (atomic, --force-with-lease per ref)",
            "✓".green().bold(),
            n,
            remote
        );
    } else if cli.dry_run {
        println!(
            "  {}",
            "Dry run — re-run without --dry-run to rewrite, then push from the review screen."
                .dimmed()
        );
    } else {
        print_push_guidance(&repo, &report);
    }
    if !cli.dry_run {
        if !cli.force && !cli.guarantee {
            println!(
                "  {}",
                "Restore any branch: git checkout -b restored refs/backup/luddite/heads/<branch>"
                    .dimmed()
            );
            println!(
                "  {}",
                "Old→new maps: .git/luddite/commit-map and .git/luddite/ref-map".dimmed()
            );
        }
        if !cli.guarantee {
            warn_unpurged(&repo);
        }
    }

    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `--push <remote>`: the same plan the screen shows, without the screen.
/// Returns the number of refs pushed, or None on failure.
fn push_now(
    repo: &Repository,
    workdir: &Path,
    report: &Report,
    remote: &str,
) -> Result<Option<usize>> {
    println!();
    println!("  {}  contacting {} (git ls-remote)…", "→".dimmed(), remote);
    let state = push::ls_remote(workdir, remote)?;
    let rows = push::build_plan(repo, report, remote, &state)?;
    let selected: Vec<&push::PlanRow> = rows.iter().filter(|r| r.selected).collect();
    if selected.is_empty() {
        println!(
            "  {}",
            "Every rewritten ref already matches the remote — nothing to push.".dimmed()
        );
        return Ok(Some(0));
    }
    for r in &selected {
        println!(
            "     {}  {}  {} → {}",
            "↑".yellow(),
            r.dst,
            r.remote
                .map(|o| o.to_string()[..7].to_string())
                .unwrap_or_else(|| "(absent)".into()),
            &r.src.to_string()[..7]
        );
    }
    let skipped = rows.len() - selected.len();
    if skipped > 0 {
        println!(
            "     {}  {} other ref(s) differ from the remote for reasons unrelated to this run — left alone (use the review screen to include them)",
            "·".dimmed(),
            skipped
        );
    }
    println!();
    let ok = push::run_blocking(workdir, remote, &rows)?;
    Ok(if ok { Some(selected.len()) } else { None })
}

// ── Output ──────────────────────────────────────────────────────────────────

fn print_rules(rules: &patterns::RuleSet, extra: &[PathBuf]) {
    println!("{} rules loaded", rules.total());
    if !extra.is_empty() {
        println!("  (incl. {} config file(s))", extra.len());
    }
    println!("\n  {}", "message patterns".bold());
    for r in &rules.messages {
        println!("    {}  {}", r.name.cyan(), r.description.dimmed());
    }
    println!("\n  {}", "identities".bold());
    for r in &rules.identities {
        println!("    {}  {}", r.name.cyan(), r.description.dimmed());
    }
    println!("\n  {}", "notes deletion".bold());
    for r in &rules.delete_notes {
        println!("    {}  {}", r.name.cyan(), r.description.dimmed());
    }
}

fn print_stats(s: &rewriter::Stats) {
    println!();
    println!(
        "  {:>6}  commits scanned",
        s.total_commits.to_string().cyan()
    );
    println!(
        "  {:>6}  commits rewritten",
        s.rewritten_commits.to_string().green().bold()
    );
    println!(
        "  {:>6}  messages cleaned",
        s.messages_cleaned.to_string().green()
    );
    if s.identities_sanitized > 0 {
        println!(
            "  {:>6}  identities sanitized",
            s.identities_sanitized.to_string().green()
        );
    }
    if s.trees_remapped > 0 {
        println!(
            "  {:>6}  commits with gitlinks remapped",
            s.trees_remapped.to_string().green()
        );
    }
    if s.commits_signed > 0 {
        println!(
            "  {:>6}  commits re-signed",
            s.commits_signed.to_string().green()
        );
    }
    println!(
        "  {:>6}  of {} branches updated",
        s.branches_updated.to_string().green(),
        s.branches_scanned
    );
    if s.remotes_updated > 0 {
        println!(
            "  {:>6}  remote tracking refs updated",
            s.remotes_updated.to_string().green()
        );
    }
    if s.tags_updated > 0 {
        println!("  {:>6}  tags updated", s.tags_updated.to_string().green());
    }
    if s.tags_signed > 0 {
        println!("  {:>6}  tags re-signed", s.tags_signed.to_string().green());
    }
    if s.generic_refs_updated > 0 {
        println!(
            "  {:>6}  other refs updated",
            s.generic_refs_updated.to_string().green()
        );
    }
    if s.notes_deleted > 0 {
        println!(
            "  {:>6}  AI notes refs deleted",
            s.notes_deleted.to_string().green()
        );
    }
    if s.index_gitlinks_updated > 0 {
        println!(
            "  {:>6}  index gitlinks updated",
            s.index_gitlinks_updated.to_string().green()
        );
    }
    if s.stash_dropped {
        println!("  {:>6}  stash dropped", "✓".green());
    }
}

fn print_submodules(subs: &[SubmoduleSummary], recursing: bool) {
    if subs.is_empty() {
        return;
    }
    println!();
    if !recursing {
        println!(
            "  {}  {} submodule(s) — gitlinks preserved verbatim; add --recurse-submodules to scrub them too",
            "↳".dimmed(),
            subs.len()
        );
        for s in subs {
            println!(
                "     {}  {}  {}",
                "·".dimmed(),
                s.path,
                s.gitlink
                    .map(|o| o.to_string()[..7].to_string())
                    .unwrap_or_default()
                    .dimmed()
            );
        }
        return;
    }
    println!("  {}  submodules", "↳".cyan());
    for s in subs {
        let status = match &s.status {
            SubStatus::Skipped => "skipped".dimmed().to_string(),
            SubStatus::NotInitialized => "not initialized — clone it (git submodule update --init) to scrub it".yellow().to_string(),
            SubStatus::Clean { commits } => format!("{commits} commits, nothing to rewrite").green().to_string(),
            SubStatus::Rewritten { commits, rewritten } => format!(
                "{rewritten} of {commits} commits rewritten — push it from inside the submodule (luddite --push …)"
            )
            .yellow()
            .to_string(),
            SubStatus::VerifyFailed { residues } => format!("{residues} indicator(s) survived").red().to_string(),
        };
        println!(
            "     {}  {}  {}  {}",
            "·".dimmed(),
            s.path,
            status,
            s.url.dimmed()
        );
    }
}

/// Exact, lease-safe commands for the manual path. A bare
/// `git push --force-with-lease` would be rejected: the remote-tracking refs
/// were rewritten too, so their values no longer match the remote.
fn print_push_guidance(repo: &Repository, report: &Report) {
    let remote = match push::default_remote(repo, None) {
        Some(r) => r,
        None => {
            println!(
                "  {}",
                "History rewritten. This repository has no remote to update.".yellow()
            );
            return;
        }
    };
    let branches: Vec<String> = report
        .ref_updates
        .iter()
        .filter(|u| u.kind == RefKind::Branch)
        .map(|u| u.name.trim_start_matches("refs/heads/").to_string())
        .collect();
    let tags: Vec<String> = report
        .ref_updates
        .iter()
        .filter(|u| u.kind == RefKind::Tag)
        .map(|u| u.name.clone())
        .collect();

    println!(
        "  {}",
        "History rewritten locally. Update the remote with:".yellow()
    );
    println!("     luddite {} --push {}", ".".dimmed(), remote);
    println!(
        "  {}",
        "or by hand — fetch first, because the remote-tracking refs were rewritten too:".dimmed()
    );
    println!("     git fetch {remote}");
    if !branches.is_empty() {
        println!(
            "     git push --force-with-lease {remote} {}",
            branches.join(" ")
        );
    }
    if !tags.is_empty() {
        let shown: Vec<&str> = tags.iter().take(3).map(String::as_str).collect();
        let more = if tags.len() > 3 {
            format!(
                " … ({} tags in total — see .git/luddite/ref-map)",
                tags.len()
            )
        } else {
            String::new()
        };
        println!(
            "     git push --force {remote} {}{}",
            shown.join(" "),
            more.dimmed()
        );
    }
}

fn warn_unpurged(repo: &Repository) {
    let has_stash = repo.find_reference("refs/stash").is_ok();
    if has_stash {
        println!();
        println!(
            "  {}  refs/stash still references pre-rewrite history.",
            "!".yellow()
        );
        println!(
            "  {}",
            "     Drop it (git stash clear) or use --guarantee for a full purge.".dimmed()
        );
    }
    println!(
        "  {}",
        "     Backup refs + reflog still hold old commits. Use --guarantee to erase them.".dimmed()
    );
}

// ── Identity resolution ───────────────────────────────────────────────────────

fn resolve_identity(repo: &Repository, flag: Option<&str>) -> Result<Identity> {
    if let Some(s) = flag {
        return parse_identity(s);
    }
    let cfg = repo.config().context("failed to read git config")?;
    let name = cfg.get_string("user.name").ok();
    let email = cfg.get_string("user.email").ok();
    match (name, email) {
        (Some(n), Some(e)) => Ok(Identity { name: n, email: e }),
        _ => bail!(
            "no replacement identity: set git config user.name/user.email, or pass \
             --replace-identity \"Name <email>\""
        ),
    }
}

fn parse_identity(s: &str) -> Result<Identity> {
    // Expect: Name <email>
    if let (Some(lt), Some(gt)) = (s.find('<'), s.rfind('>')) {
        if gt > lt {
            let name = s[..lt].trim().to_string();
            let email = s[lt + 1..gt].trim().to_string();
            if !name.is_empty() && !email.is_empty() {
                return Ok(Identity { name, email });
            }
        }
    }
    bail!("--replace-identity must be of the form \"Name <email>\" (got: {s:?})")
}
