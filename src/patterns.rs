use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct RuleDef {
    pub name: String,
    pub regex: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Default, Deserialize)]
struct RuleFile {
    #[serde(default)]
    patterns: Vec<RuleDef>,
    #[serde(default)]
    identities: Vec<RuleDef>,
    #[serde(default)]
    delete_notes: Vec<RuleDef>,
}

pub struct CompiledRule {
    pub name: String,
    pub description: String,
    pub regex: Regex,
}

/// The full compiled rule set: message-line patterns, identity matchers, and
/// notes-ref deletion matchers.
pub struct RuleSet {
    pub messages: Vec<CompiledRule>,
    pub identities: Vec<CompiledRule>,
    pub delete_notes: Vec<CompiledRule>,
}

impl RuleSet {
    pub fn total(&self) -> usize {
        self.messages.len() + self.identities.len() + self.delete_notes.len()
    }
}

const BUILTIN: &str = include_str!("../luddite_patterns.toml");

/// Discover default config files in priority order (low → high):
///   ~/.config/luddite/patterns.toml, then <repo>/.luddite.toml.
/// Built-ins always load first; explicit --patterns loads last.
pub fn discover_configs(repo_workdir: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(cfg) = dirs::config_dir() {
        let p = cfg.join("luddite").join("patterns.toml");
        if p.is_file() {
            out.push(p);
        }
    }
    if let Some(wd) = repo_workdir {
        let p = wd.join(".luddite.toml");
        if p.is_file() {
            out.push(p);
        }
    }
    out
}

/// Load and compile built-ins + every extra file, in the order given.
pub fn load(extra_files: &[PathBuf]) -> Result<RuleSet> {
    let mut acc: RuleFile = toml::from_str(BUILTIN).context("failed to parse built-in patterns")?;

    for path in extra_files {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read patterns file: {}", path.display()))?;
        let extra: RuleFile = toml::from_str(&content)
            .with_context(|| format!("failed to parse patterns file: {}", path.display()))?;
        acc.patterns.extend(extra.patterns);
        acc.identities.extend(extra.identities);
        acc.delete_notes.extend(extra.delete_notes);
    }

    Ok(RuleSet {
        messages: compile(acc.patterns, "pattern")?,
        identities: compile(acc.identities, "identity")?,
        delete_notes: compile(acc.delete_notes, "delete_notes")?,
    })
}

fn compile(defs: Vec<RuleDef>, kind: &str) -> Result<Vec<CompiledRule>> {
    defs.into_iter()
        .map(|d| {
            let regex = Regex::new(&d.regex)
                .with_context(|| format!("invalid {} regex in '{}': {}", kind, d.name, d.regex))?;
            Ok(CompiledRule {
                name: d.name,
                description: d.description,
                regex,
            })
        })
        .collect()
}
