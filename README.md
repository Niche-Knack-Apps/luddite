# luddite

[![ci](https://github.com/Niche-Knack-Apps/luddite/actions/workflows/ci.yml/badge.svg)](https://github.com/Niche-Knack-Apps/luddite/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/Niche-Knack-Apps/luddite)](https://github.com/Niche-Knack-Apps/luddite/releases/latest)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

> Deterministically erase all evidence of AI/agent involvement from git history.

Made by [Niche-Knack](https://nk.tools) — Smart Tools. Sharp Solutions.

`luddite` rewrites a repository's commit history to remove every trace that an AI
coding agent was involved — footers, co-author trailers, bot author/committer
identities, and AI-attribution git notes — while preserving commit order, human
authorship, timestamps, submodule gitlinks, and legitimate human-written content
(including innocent mentions like a `CLAUDE.md` filename). After the run a review
screen shows the commit graph with every rewritten commit highlighted, the
before/after of each one, and a confirm-and-push step with correct leases.

It is not a best-effort scrubber. In `--guarantee` mode it ends with a
**verification pass** that re-scans every reachable commit across every ref and
exits non-zero if a single indicator survives.

## What counts as "evidence"

AI involvement leaks onto several surfaces. `luddite` covers all of them:

| Surface | Example | Handling |
|---|---|---|
| Message trailer | `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` (any Claude model), `Co-authored-by: Codex <noreply@openai.com>` | line removed |
| Session trailer | `Claude-Session: https://claude.ai/code/session_…`, bare session URL lines left by squash-merged PR bodies | line removed |
| Message footer | `🤖 Generated with [Claude Code]`, `Generated with [Codex](https://openai.com/codex/).` | line removed |
| Author / committer identity | `Codex <noreply@openai.com>`, `chatgpt-codex-connector[bot]`, `Devin <bot@devin.ai>`, `github-copilot[bot]`, `Cursor Agent <…>` | rewritten to your identity |
| Git notes | `refs/notes/ai/*` (engineered to survive `filter-repo`) | ref deleted |
| Stash / all refs | `refs/stash`, remote-tracking, tags, packed-refs | rewritten or dropped |
| Old reachable copies | backup refs, reflog, unpruned objects | purged under `--guarantee` |
| GPG/SSH signature | agent signing key | dropped — or re-created with your key under `--sign` |
| Submodules | gitlinks in the superproject | preserved verbatim; `--recurse-submodules` scrubs them and remaps the gitlinks |

Built-in rules are verified against real histories, not guessed: the Claude rule is
one regex that matches every model name Claude Code has ever written (Sonnet/Opus/
Haiku/Fable/Mythos, any version, with or without `(1M context)`), and the Codex
rules were taken from the strings inside the Codex CLI 0.152.0 binary. A human
co-author with a real `@openai.com` or `@anthropic.com` address is never touched —
only the `noreply@` bot addresses match. Run `luddite --list-patterns` for the
full set.

## Install

Download the binary for your platform from the
[latest release](https://github.com/Niche-Knack-Apps/luddite/releases/latest) —
`luddite_linux_amd64`, `luddite_linux_arm64` (both static), `luddite_darwin_arm64` or
`luddite_darwin_amd64` — check it against `SHA256SUMS`, and put it on your PATH:

```sh
curl -LO https://github.com/Niche-Knack-Apps/luddite/releases/latest/download/luddite_linux_amd64
curl -LO https://github.com/Niche-Knack-Apps/luddite/releases/latest/download/SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS
install -m 0755 luddite_linux_amd64 ~/.local/bin/luddite
```

Or build it with Cargo: `cargo install --locked --git https://github.com/Niche-Knack-Apps/luddite --tag v0.3.0`.
`git` must be on `PATH` (see [Build & install](#build--install)).

## Usage

```sh
luddite [REPO]                 # safe mode: rewrite + backups + review screen
luddite [REPO] --dry-run       # show what would change, touch nothing
luddite [REPO] --guarantee     # full deterministic erase (see below)
luddite [REPO] --sign          # re-sign what was signed before (--sign=all: every rewrite)
luddite [REPO] --recurse-submodules   # scrub initialized submodules, remap gitlinks
luddite [REPO] --ui            # reopen the most recent review without rewriting
luddite [REPO] --push origin   # rewrite, then push with correct leases, no screen
luddite [REPO] --no-ui         # never open the review screen (scripts, CI)
luddite --list-patterns        # show every loaded rule
```

### Safe mode (default)

Rewrites all branches, remotes, and tags, and sanitizes identities. Every ref it
may touch is mirrored first under `refs/backup/luddite/` — `heads/<branch>`,
`tags/<tag>` (the tag object for annotated tags), `remotes/<remote>/<branch>`,
`stash`, `other/<ref>` — and the old→new maps are written to
`.git/luddite/commit-map` and `.git/luddite/ref-map`:

```sh
git checkout -b restored refs/backup/luddite/heads/main
git update-ref refs/tags/v1.2.0 refs/backup/luddite/tags/v1.2.0
```

Old commits remain reachable via the backups and reflog — this mode does **not**
guarantee erasure, it guarantees *recoverability*.

Repeat runs are idempotent. Once a repository is clean, another normal run does
not move refs, replace the original backup refs, or rewrite the maps. If new
history later needs cleaning, existing backup refs remain the immutable restore
points from the first run.

### The review screen

Unless `--no-ui` is given (or stdin/stdout are not a terminal) the run ends in a
full-screen review:

- **left**: the commit graph, newest first, lanes coloured, refs shown as
  `(main, origin/main, tag: v1)`. Rewritten commits are highlighted — `~` when
  its own message/identity/gitlinks were cleaned, `·` when only its id changed
  because a parent was rewritten. `n`/`N` jump between them.
- **right**: separate **Before** and **After** views for the selected commit,
  with changed ids, identities, signatures, parents, gitlinks, and message lines
  highlighted red/green. `h`/`l`, left/right, or Tab cycle
  Graph → Before → After → Graph.
- `j`/`k` or up/down move or scroll the focused view; the mouse wheel scrolls
  whichever pane is under it, and clicking selects a commit or detail side.
  `g`/`G` jump to the ends, `?` lists every key, and `q` quits.
- `p` **push**: pick a remote, luddite runs `git ls-remote` and lists every branch
  and tag that differs from it — rewritten refs pre-selected, refs that diverged
  for unrelated reasons unselected — then pushes the selection atomically, each
  ref with `--force-with-lease=<ref>:<value the remote had a moment ago>`.

If the original run did not have an interactive terminal, reopen its review with
`luddite /path/to/repo --ui`. This reads `.git/luddite/{commit-map,ref-map}` and
does not scan or mutate the repository. It can also reconstruct rewrites made by
luddite 0.2 from legacy `refs/backup/luddite/<branch>` refs. Replay requires the
old commits to remain available; after `--guarantee` prunes them, use the review
shown during that run or restore its external bundle.

Why not plain `git push --force-with-lease`? The bare form compares the remote
against your *remote-tracking* refs — which luddite has just rewritten, so the
lease is stale and the push is rejected. If you push by hand, `git fetch` first.

### `--sign`

Rewriting changes the signed bytes, so every rewritten commit and recreated
annotated tag loses its signature. `--sign` re-signs the ones that were signed
before; `--sign=all` signs every rewritten commit and tag. Signing goes through
`git commit-tree -S` / `git tag -s`, so `gpg.format` (openpgp/ssh/x509),
`user.signingkey` and `gpg.program` apply exactly as they do to `git commit`.

### Submodules

Gitlinks (the submodule commit ids recorded in the superproject's trees) are
never touched by a plain run and are listed in the report. With
`--recurse-submodules` each *initialized* submodule is scrubbed as a repository
of its own (bundled and verified under `--guarantee`), then the superproject's
trees are remapped across its whole history so every gitlink points at the
rewritten submodule commit, and the index is updated so `git status` stays
clean. Push each submodule from inside it (`luddite --push …` there).

### `--guarantee` mode

The deterministic erase. In order:

1. **Bundle** a full backup *outside* the repo (`../luddite-backup-<repo>-<ts>.bundle`,
   plus one per scrubbed submodule) so nothing is lost but nothing remains inside
   the object graph.
2. Rewrite every ref + sanitize identities + delete AI notes refs.
3. Drop the stash (`--keep-stash` to retain).
4. Expire all reflogs and `git gc --prune=now`.
5. **Verify**: re-scan every reachable commit across every ref. Print
   `VERIFIED CLEAN` and exit 0, or list survivors and exit 1.

Restore from the bundle if needed:

```sh
git clone ../luddite-backup-<repo>-<ts>.bundle restored
```

### Identity replacement

When an agent committed *as itself*, `luddite` rewrites the author/committer to
your identity (from `git config user.name` / `user.email`), preserving the
original timestamp. Override with:

```sh
luddite --replace-identity "Jane Doe <jane@example.com>"
```

## Extending the patterns

Adding a new indicator is a one-block copy-paste. Rules come from three sources,
merged in priority order (built-ins → discovered configs → `--patterns`):

```
<repo>/.luddite.toml                 # per-repo overrides
~/.config/luddite/patterns.toml      # personal global rules
--patterns <file.toml>               # explicit, loaded last
```

Three rule kinds (see `luddite_patterns.toml` for the full built-in set):

```toml
# Remove matching LINES from a commit message
[[patterns]]
name = "my-agent-footer"
regex = '(?m)^Generated by MyAgent[^\n]*\r?\n?'
description = "MyAgent footer"

# Rewrite a commit's author/committer when name/email matches
[[identities]]
name = "my-agent-bot"
regex = 'bot@myagent\.ai'
description = "MyAgent bot identity"

# Delete an entire git-notes ref
[[delete_notes]]
name = "my-agent-notes"
regex = '^refs/notes/myagent($|/)'
description = "MyAgent attribution notes"
```

Regexes use Rust `regex` crate syntax; `(?m)` enables per-line `^`/`$`.

## Safety notes

- Human co-author trailers are **never** removed — only AI ones.
- After rewriting, push from the review screen or with `luddite --push <remote>`;
  by hand, `git fetch <remote>` first, then `git push --force-with-lease`.
- GitHub caches the contributor graph separately from git history; a co-author
  may linger in the UI after a force-push until GitHub recomputes it (contact
  GitHub Support to refresh).

## Build & install

```sh
make install               # release build (static musl when musl-gcc exists) → ~/.local/bin/luddite
make verify                # clippy + tests + fmt check
make smoke                 # two fixture repos (safe mode + bare origin + --push;
                           #   --guarantee --sign --recurse-submodules), re-checked
                           #   with plain git → SMOKE PASS
```

Other targets: `make build` (→ `dist/`), `make release` (→ `../_shared/releases/luddite/<platform>/`),
`make dev` (debug build + `--list-patterns`), `make uninstall`, `make help`.
The Makefile honours `CARGO_TARGET_DIR`. `luddite` shells out to `git` for
`bundle`, `reflog expire`, `gc`, signing, `ls-remote` and `push`, so git must be
on `PATH`. Design notes: [`docs/architecture.md`](docs/architecture.md); history:
[`CHANGELOG.md`](CHANGELOG.md).

### Trying it on a real repo

Always start with a **copy** — `--guarantee` rewrites history and purges the old objects:

```sh
git clone --mirror /path/to/repo scratch.git && git clone scratch.git scratch && cd scratch
luddite --dry-run          # counts only, touches nothing
luddite --guarantee        # rewrite + purge + VERIFIED CLEAN (or exit 1 with the survivors listed)
git log --all --format='%B%an <%ae>' | grep -iE 'claude|codex|anthropic|openai'   # independent re-check
```

## Releases

Every merge to `main` runs formatting, clippy, the tests and `make smoke`. When the
merge carries a new version in `VERSION` (kept equal to `Cargo.toml`), the release
workflow builds amd64 and arm64 binaries for Linux (static musl) and macOS, tags `vX.Y.Z`, and
publishes them with `SHA256SUMS` as a GitHub release. A pull request that does not
bump `VERSION` fails its check, so every merge ships.

## Support

luddite is free and open. Niche-Knack also makes small, sharp, offline-first apps —
see them at [nk.tools](https://nk.tools).

Really believe in this project's potential and want to contribute to its
development? [Buy Me a Coffee](https://buymeacoffee.com/nicheknack).

## License

[Apache License 2.0](LICENSE). See [NOTICE](NOTICE).
