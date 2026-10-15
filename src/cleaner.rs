use crate::patterns::CompiledRule;

/// Render a name/email pair the way it appears in a signature: `Name <email>`.
pub fn render_identity(name: &str, email: &str) -> String {
    format!("{} <{}>", name, email)
}

/// True if the rendered `Name <email>` identity matches any AI identity rule.
pub fn identity_matches(rendered: &str, identities: &[CompiledRule]) -> bool {
    identities.iter().any(|r| r.regex.is_match(rendered))
}

/// Strip every matching message-pattern line, then normalize whitespace.
pub fn clean_message(message: &str, patterns: &[CompiledRule]) -> String {
    let mut result = message.to_string();

    for pattern in patterns {
        result = pattern.regex.replace_all(&result, "").to_string();
    }

    normalize(&result)
}

fn normalize(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();

    // Remove trailing blank lines, then collapse consecutive blank lines into one.
    let mut trimmed: Vec<&str> = lines.to_vec();

    while trimmed.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        trimmed.pop();
    }

    let mut result: Vec<&str> = Vec::with_capacity(trimmed.len());
    let mut prev_blank = false;
    for line in &trimmed {
        let blank = line.trim().is_empty();
        if blank && prev_blank {
            continue;
        }
        result.push(line);
        prev_blank = blank;
    }

    if result.is_empty() {
        return String::new();
    }

    // Git commit messages always end with a newline.
    result.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns;

    fn rules() -> patterns::RuleSet {
        patterns::load(&[]).unwrap()
    }

    fn clean(msg: &str) -> String {
        clean_message(msg, &rules().messages)
    }

    #[test]
    fn strips_claude_coauthor() {
        let msg = "fix: resolve null pointer\n\nCo-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>\n";
        assert_eq!(clean(msg), "fix: resolve null pointer\n");
    }

    #[test]
    fn strips_claude_code_footer() {
        let msg = "feat: add thing\n\n🤖 Generated with [Claude Code](https://claude.ai/code)\n";
        assert_eq!(clean(msg), "feat: add thing\n");
    }

    #[test]
    fn strips_copilot() {
        let msg = "chore: tidy up\n\nCo-authored-by: GitHub Copilot <copilot@github.com>\n";
        assert_eq!(clean(msg), "chore: tidy up\n");
    }

    #[test]
    fn preserves_human_coauthors() {
        let msg = "feat: pair prog\n\nCo-Authored-By: Jane Doe <jane@example.com>\n";
        assert_eq!(clean(msg), msg);
    }

    #[test]
    fn strips_ai_preserves_human_in_mixed_trailer() {
        let msg = "fix: thing\n\nCo-Authored-By: Jane Doe <jane@example.com>\nCo-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>\n";
        let expected = "fix: thing\n\nCo-Authored-By: Jane Doe <jane@example.com>\n";
        assert_eq!(clean(msg), expected);
    }

    #[test]
    fn collapses_blank_lines_left_by_stripping() {
        let msg = "feat: thing\n\n\nCo-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>\n";
        assert_eq!(clean(msg), "feat: thing\n");
    }

    #[test]
    fn subject_only_message_unchanged() {
        let msg = "chore: bump version\n";
        assert_eq!(clean(msg), msg);
    }

    #[test]
    fn full_message_with_body_and_footer() {
        let msg = "feat: add search\n\nAdds full-text search across all documents.\nIndexed on first launch.\n\nCo-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>\n";
        let expected = "feat: add search\n\nAdds full-text search across all documents.\nIndexed on first launch.\n";
        assert_eq!(clean(msg), expected);
    }

    #[test]
    fn empty_message_stays_empty() {
        assert_eq!(clean(""), "");
    }

    // ── Claude 5 family + newer Claude Code surfaces ────────────────────────

    #[test]
    fn strips_claude_5_family_coauthors() {
        for model in [
            "Claude",
            "Claude Opus 4.8 (1M context)",
            "Claude Opus 5",
            "Claude Sonnet 5",
            "Claude Haiku 4.5",
            "Claude Fable 5",
            "Claude Fable 5.1",
            "Claude Mythos 5.1",
        ] {
            let msg = format!("fix: thing\n\nCo-Authored-By: {model} <noreply@anthropic.com>\n");
            assert_eq!(clean(&msg), "fix: thing\n", "model variant: {model}");
        }
    }

    #[test]
    fn strips_modern_claude_code_trailer_block() {
        // The full block Claude Code writes since 2026-08: co-author + session trailer.
        let msg = "feat: add thing\n\nBody text.\n\nCo-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>\nClaude-Session: https://claude.ai/code/session_01TQhbs9D6hQAWkDj4E9Fywc\n";
        assert_eq!(clean(msg), "feat: add thing\n\nBody text.\n");
    }

    #[test]
    fn strips_squash_merged_pr_body_footer() {
        // PR bodies carry the emoji footer plus a bare session URL into squash merges.
        let msg = "Add search (#42)\n\nAdds full-text search.\n\n🤖 Generated with [Claude Code](https://claude.com/claude-code)\n\nhttps://claude.ai/code/session_01TQhbs9D6hQAWkDj4E9Fywc\n";
        assert_eq!(clean(msg), "Add search (#42)\n\nAdds full-text search.\n");
    }

    #[test]
    fn preserves_non_session_claude_urls_in_body() {
        // Only the session URL line is a fingerprint; a docs link in prose is content.
        let msg = "docs: link the CLI docs\n\nSee https://claude.ai/code for the tool itself.\n";
        assert_eq!(clean(msg), msg);
    }

    // ── OpenAI Codex / ChatGPT ──────────────────────────────────────────────

    #[test]
    fn strips_codex_cli_trailer() {
        // Exact trailer Codex CLI 0.152.0 appends.
        let msg = "fix: thing\n\nCo-authored-by: Codex <noreply@openai.com>\n";
        assert_eq!(clean(msg), "fix: thing\n");
    }

    #[test]
    fn strips_codex_footers() {
        for footer in [
            "Generated with [Codex](https://openai.com/codex/).",
            "Generated with Codex.",
            "Generated by Codex",
        ] {
            let msg = format!("feat: thing\n\nBody.\n\n{footer}\n");
            assert_eq!(clean(&msg), "feat: thing\n\nBody.\n", "footer: {footer}");
        }
    }

    #[test]
    fn strips_chatgpt_and_gpt_model_coauthors() {
        for name in [
            "ChatGPT",
            "OpenAI Codex",
            "GPT-5.6",
            "GPT-5.3-Codex",
            "GPT-4o",
        ] {
            let msg = format!("fix: thing\n\nCo-authored-by: {name} <noreply@openai.com>\n");
            assert_eq!(clean(&msg), "fix: thing\n", "name: {name}");
        }
    }

    #[test]
    fn strips_codex_connector_bot_coauthor() {
        let msg = "Merge PR (#7)\n\nCo-authored-by: chatgpt-codex-connector[bot] <199175422+chatgpt-codex-connector[bot]@users.noreply.github.com>\n";
        assert_eq!(clean(msg), "Merge PR (#7)\n");
    }

    #[test]
    fn preserves_human_openai_employee_coauthor() {
        // A real person at OpenAI is a human co-author, not a fingerprint.
        let msg = "feat: pair prog\n\nCo-Authored-By: Jane Doe <jane@openai.com>\n";
        assert_eq!(clean(msg), msg);
    }

    // ── Identity matching ───────────────────────────────────────────────────

    #[test]
    fn identity_matches_anthropic_noreply() {
        let r = rules();
        let id = render_identity("Claude", "noreply@anthropic.com");
        assert!(identity_matches(&id, &r.identities));
    }

    #[test]
    fn identity_matches_devin_bot() {
        let r = rules();
        let id = render_identity("Devin", "bot@devin.ai");
        assert!(identity_matches(&id, &r.identities));
    }

    #[test]
    fn identity_matches_copilot_bot() {
        let r = rules();
        let id = render_identity(
            "github-copilot[bot]",
            "198982749+Copilot@users.noreply.github.com",
        );
        assert!(identity_matches(&id, &r.identities));
    }

    #[test]
    fn identity_matches_codex_noreply() {
        let r = rules();
        let id = render_identity("Codex", "noreply@openai.com");
        assert!(identity_matches(&id, &r.identities));
    }

    #[test]
    fn identity_matches_codex_connector_bot() {
        let r = rules();
        let id = render_identity(
            "chatgpt-codex-connector[bot]",
            "199175422+chatgpt-codex-connector[bot]@users.noreply.github.com",
        );
        assert!(identity_matches(&id, &r.identities));
    }

    #[test]
    fn identity_ignores_human_openai_employee() {
        let r = rules();
        let id = render_identity("Jane Doe", "jane@openai.com");
        assert!(!identity_matches(&id, &r.identities));
    }

    #[test]
    fn identity_ignores_human() {
        let r = rules();
        let id = render_identity("Jonathan Batteas", "jonathan@example.com");
        assert!(!identity_matches(&id, &r.identities));
    }
}
