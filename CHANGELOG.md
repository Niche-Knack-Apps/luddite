# Changelog

All notable changes to luddite. Format: [Keep a Changelog](https://keepachangelog.com/), versions per SemVer.

## [0.3.0] - 2026-09-04

Driven by the first run against a real 736-commit repository, which was rewritten correctly but exposed three gaps in the tooling around the rewrite.

### Added
- **Review screen** after every run (ratatui): a lazygit-style commit graph with lanes, refs and every rewritten commit highlighted (`~` cleaned, `·` rewritten only because a parent changed); distinct red/green Before and After views for ids, identities, signature status, gitlinks and message changes; Graph → Before → After focus cycling with arrows, `hjkl`, or Tab; pane-aware mouse scrolling and click selection; `n`/`N` jumps between rewritten commits; `p` opens the push step. Skipped with `--no-ui` or when stdin/stdout are not a terminal.
- **`--ui` replay mode**: reopen the most recent review without scanning or rewriting, using `.git/luddite` maps or legacy 0.2 branch backups.
- **Confirm-and-push**: picks a remote, runs `git ls-remote`, lists every branch and tag that differs from the remote (rewritten refs pre-selected, pre-existing divergence unselected), then pushes atomically with an explicit `--force-with-lease=<ref>:<observed value>` per ref. `--push <remote>` does the same non-interactively.
- **`--sign[=signed|all]`**: re-sign rewritten commits and recreated annotated tags through `git commit-tree -S` / `git tag -s`, so `gpg.format`, `user.signingkey` and `gpg.program` apply exactly as for `git commit`. Default re-signs only what was signed before; `all` signs every rewrite.
- **Submodules**: gitlinks are reported and always preserved verbatim. `--recurse-submodules` scrubs each initialized submodule as its own repository (bundled and verified under `--guarantee`), then remaps the superproject's gitlinks across its whole history and updates the index so `git status` stays clean.
- **Backups for every ref kind**: `refs/backup/luddite/heads/<branch>`, `tags/<tag>` (the tag object for annotated tags), `remotes/<remote>/<branch>`, `stash`, `other/<ref>`.
- **Maps**: `.git/luddite/commit-map` (old new) and `.git/luddite/ref-map` (ref old new), filter-repo style.
- Post-run guidance prints exact, lease-safe commands and explains why a bare `git push --force-with-lease` is rejected after a run (the remote-tracking refs were rewritten too, so `git fetch` first).

### Fixed
- Clean repeat runs no longer touch backup refs or map files; existing backup refs are immutable and retain the original pre-luddite restore points.
- Recreated annotated tags carried the original signature block inside the message (git appends it there), which made `git tag -v` report a bad signature. The block is now stripped, and re-created under `--sign`.
- Annotated tags without a readable name were recreated as `refs/tags/refs/tags/<name>`.

### Changed
- Branch backups moved from `refs/backup/luddite/<branch>` to `refs/backup/luddite/heads/<branch>`; old backups from 0.2.0 are left untouched (luddite never touches `refs/backup/`).
- `make smoke` now runs two scenarios: safe mode with a bare origin (backups, maps, lease-safe `--push`, an unrelated diverged branch left alone, non-tty run) and `--guarantee --sign --recurse-submodules` (submodule + signed commits/tag when gpg is available).

## [0.2.0] - 2026-09-03
- Claude 5-family and Codex/ChatGPT fingerprints, `Claude-Session` trailer, session-URL lines.
- Makefile with dev/verify/build/release/install targets, static musl build, `make smoke`.

## [0.1.0]
- Initial release: message patterns, identity rewrite, notes deletion, `--guarantee`.
