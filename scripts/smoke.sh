#!/usr/bin/env bash
# smoke.sh — end-to-end checks against REAL git repositories.
#
# Scenario A (safe mode): a fixture carrying every fingerprint luddite knows
#   about, a bare "origin" holding the dirty history, plus a branch that
#   diverged from origin for reasons unrelated to luddite. Runs the default
#   mode with --push and checks: backups for heads/tags/remotes, the
#   commit/ref maps, the remote ends up clean, the unrelated branch is left
#   alone (the lease logic), and a non-tty run does not try to open the UI.
# Scenario B (guarantee): the fixture as a superproject with a submodule whose
#   own history carries trailers, signed commits and a signed tag (using an
#   ephemeral GPG or SSH key). Runs --guarantee --sign --recurse-submodules and checks:
#   VERIFIED CLEAN, submodule clean, gitlinks + index remapped, signatures
#   re-created, human content preserved.
#
#   usage: scripts/smoke.sh <luddite-binary> [workdir]
set -euo pipefail

BIN=${1:?usage: smoke.sh <luddite-binary> [workdir]}
WORK=${2:-dist/smoke}
BIN=$(readlink -f "$BIN")
[ -x "$BIN" ] || { echo "not executable: $BIN" >&2; exit 2; }

rm -rf "$WORK" && mkdir -p "$WORK"
WORK=$(cd "$WORK" && pwd)
SESSION="https://claude.ai/code/session_01TQhbs9D6hQAWkDj4E9Fywc"
RESIDUE='noreply@anthropic|noreply@openai|Claude-Session|Co-[Aa]uthored-[Bb]y: (Claude|Codex|ChatGPT)|codex-connector|Generated (with|by)|claude\.ai/code/session'

fail() { echo "  ✗ $*" >&2; exit 1; }
ok()   { echo "  ✓ $*"; }
section() { echo; echo "── $*"; }

# c <file> <message-lines...>  — commit as the configured human identity
c() { local f=$1; shift; echo "$RANDOM" >>"$f"; git add "$f"; git commit -q -F <(printf '%s\n' "$@"); }
# cb <name> <email> <file> <message-lines...>  — commit AUTHORED by a bot identity
cb() { local n=$1 e=$2 f=$3; shift 3; echo "$RANDOM" >>"$f"; git add "$f"
       GIT_AUTHOR_NAME=$n GIT_AUTHOR_EMAIL=$e GIT_COMMITTER_NAME=$n GIT_COMMITTER_EMAIL=$e \
         git commit -q -F <(printf '%s\n' "$@"); }

# build_fixture <dir> — the base repository with every known fingerprint
build_fixture() {
  local repo=$1
  git init -q -b main "$repo"
  cd "$repo"
  git config user.name  "Human Dev"
  git config user.email "human@example.com"
  git config commit.gpgsign false
  git config tag.gpgSign false

  c a.txt "init: human baseline"
  c a.txt "feat: claude fable 5.1 with session trailer" "" \
          "Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>" \
          "Claude-Session: $SESSION"
  c a.txt "feat: opus 5 with emoji footer" "" "Body text stays." "" \
          "🤖 Generated with [Claude Code](https://claude.com/claude-code)" "" \
          "Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
  c a.txt "Add search (#42)" "" "Squash-merged PR body." "" \
          "🤖 Generated with [Claude Code](https://claude.com/claude-code)" "" "$SESSION"
  c b.txt "fix: codex cli trailer" "" "Co-authored-by: Codex <noreply@openai.com>"
  c b.txt "feat: codex footer" "" "Generated with [Codex](https://openai.com/codex/)."
  c b.txt "chore: chatgpt co-author" "" "Co-authored-by: ChatGPT <noreply@openai.com>"
  c b.txt "feat: haiku + mythos" "" \
          "Co-Authored-By: Claude Haiku 4.5 <noreply@anthropic.com>" \
          "Co-Authored-By: Claude Mythos 5.1 <noreply@anthropic.com>"
  cb "Codex" "noreply@openai.com" c.txt "feat: bot-authored by codex"
  cb "chatgpt-codex-connector[bot]" "199175422+chatgpt-codex-connector[bot]@users.noreply.github.com" \
     c.txt "feat: bot-authored by codex connector"
  cb "Claude" "noreply@anthropic.com" c.txt "feat: bot-authored by claude"
  # Human content that MUST survive:
  c d.txt "feat: pair programming" "" "Co-Authored-By: Jane Doe <jane@openai.com>"
  c d.txt "docs: describe CLAUDE.md and AGENTS.md conventions"
  git tag -a v0.1.0 -m "release v0.1.0" "$(git rev-parse HEAD~3)"
  git checkout -q -b feature/side
  c e.txt "feat: side branch work" "" "Co-authored-by: Codex <noreply@openai.com>"
  git checkout -q main
}

check_clean_dump() { # <label> <dump>
  if grep -qE "$RESIDUE" <<<"$2"; then grep -nE "$RESIDUE" <<<"$2" | head >&2; fail "$1: fingerprints survived"; fi
  ok "$1: zero fingerprints"
}

# ═══════════════════════════════════════════════════════════════════════════
section "Scenario A — safe mode, backups, maps, lease-safe push"
A="$WORK/a"; AR="$WORK/a-remote.git"
build_fixture "$A"
cd "$A"
git notes --ref=ai add -m "agent: claude-code" HEAD
echo scratch >>a.txt && git stash -q
git init -q --bare "$AR"
git remote add origin "$AR"
git push -q origin --all
git push -q origin --tags
# A branch that diverges from origin for reasons that have nothing to do with AI.
# Fork from the clean root so this branch's local/remote divergence is truly
# unrelated to luddite's rewrite. If it inherited main's dirty commits, luddite
# would correctly rewrite and select it for push.
git checkout -q -b unrelated "$(git rev-list --max-parents=0 main)"
c f.txt "chore: unrelated work pushed earlier"
git push -q origin unrelated
c f.txt "chore: unrelated local amendment (never pushed)"
git checkout -q main
OLD_MAIN=$(git rev-parse main); OLD_TAG=$(git rev-parse refs/tags/v0.1.0); OLD_ORIGIN_MAIN=$(git rev-parse refs/remotes/origin/main)
REMOTE_UNRELATED=$(git --git-dir="$AR" rev-parse refs/heads/unrelated)
TOTAL=$(git rev-list --branches --tags --count)
echo "fixture: $TOTAL commits, origin at $AR"

echo; echo "── dry run (non-tty: the review screen must not open)"
timeout 60 "$BIN" "$A" --dry-run </dev/null >"$WORK/a-dry.log" 2>&1 || fail "dry run failed"
grep -q "commits rewritten" "$WORK/a-dry.log" || fail "dry run produced no report"
[ "$(git rev-parse main)" = "$OLD_MAIN" ] || fail "dry run moved main"
ok "dry run touched nothing and did not open the UI without a tty"

echo; echo "── rewrite + --push origin"
timeout 120 "$BIN" "$A" --no-ui --push origin </dev/null || fail "luddite --push exited non-zero"

echo; echo "── post-checks (plain git)"
[ "$(git rev-parse refs/backup/luddite/heads/main)" = "$OLD_MAIN" ] || fail "branch backup missing/wrong"
[ "$(git rev-parse refs/backup/luddite/tags/v0.1.0)" = "$OLD_TAG" ] || fail "tag backup missing/wrong"
[ "$(git rev-parse refs/backup/luddite/remotes/origin/main)" = "$OLD_ORIGIN_MAIN" ] || fail "remote-tracking backup missing/wrong"
git rev-parse -q --verify refs/backup/luddite/stash >/dev/null || fail "stash backup missing"
ok "backups for heads/, tags/, remotes/ and stash hold the pre-rewrite values"
if [ ! -s .git/luddite/commit-map ] || [ ! -s .git/luddite/ref-map ]; then fail "maps not written"; fi
REWRITTEN=$(( $(wc -l < .git/luddite/commit-map) - 1 ))
grep -q "^refs/heads/main $OLD_MAIN $(git rev-parse main)$" .git/luddite/ref-map || fail "ref-map does not record main"
ok "commit-map ($REWRITTEN entries) and ref-map written"
[ "$(git rev-parse main)" != "$OLD_MAIN" ] || fail "main was not rewritten"
LOCAL_DUMP=$(git log --branches --tags --format='%B%n%an <%ae>%n%cn <%ce>')
check_clean_dump "local branches+tags" "$LOCAL_DUMP"
REMOTE_DUMP=$(git --git-dir="$AR" log --all --format='%B%n%an <%ae>%n%cn <%ce>')
check_clean_dump "origin (after --push)" "$REMOTE_DUMP"
[ "$(git --git-dir="$AR" rev-parse refs/heads/main)" = "$(git rev-parse main)" ] || fail "origin/main not updated"
[ "$(git --git-dir="$AR" rev-parse refs/tags/v0.1.0)" = "$(git rev-parse refs/tags/v0.1.0)" ] || fail "origin tag not updated"
[ "$(git --git-dir="$AR" rev-parse refs/heads/feature/side)" = "$(git rev-parse feature/side)" ] || fail "origin feature/side not updated"
ok "origin main, feature/side and v0.1.0 now match the rewritten local refs"
[ "$(git --git-dir="$AR" rev-parse refs/heads/unrelated)" = "$REMOTE_UNRELATED" ] || fail "unrelated diverged branch was force-pushed"
ok "pre-existing divergence (branch 'unrelated') was left alone"
[ "$(git rev-parse refs/remotes/origin/main)" = "$(git rev-parse main)" ] || fail "tracking ref not updated by the push"
[ "$(git rev-list --branches --tags --count)" -eq "$TOTAL" ] || fail "commit count changed"
ok "commit count unchanged ($TOTAL)"
grep -q 'Co-Authored-By: Jane Doe <jane@openai.com>' <<<"$LOCAL_DUMP" || fail "human co-author was removed"
grep -q 'describe CLAUDE.md and AGENTS.md' <<<"$LOCAL_DUMP" || fail "innocent CLAUDE.md mention was altered"
ok "human co-author + innocent CLAUDE.md mention preserved"
# A second real run must be a byte-for-byte ref/map no-op: in particular it
# must not replace the original restore refs with the already-clean tips.
REFS_BEFORE=$(git for-each-ref --format='%(refname) %(objectname)' | sort)
MAPS_BEFORE=$(sha256sum .git/luddite/commit-map .git/luddite/ref-map)
timeout 60 "$BIN" "$A" --no-ui </dev/null >"$WORK/a-second.log" 2>&1 || fail "second run failed"
grep -q "No AI indicators found" "$WORK/a-second.log" || fail "second run still finds indicators"
REFS_AFTER=$(git for-each-ref --format='%(refname) %(objectname)' | sort)
MAPS_AFTER=$(sha256sum .git/luddite/commit-map .git/luddite/ref-map)
[ "$REFS_AFTER" = "$REFS_BEFORE" ] || fail "second run changed refs"
[ "$MAPS_AFTER" = "$MAPS_BEFORE" ] || fail "second run rewrote the maps"
[ "$(git rev-parse refs/backup/luddite/heads/main)" = "$OLD_MAIN" ] || fail "second run replaced the original main backup"
ok "second normal run is idempotent: refs, original backups and maps unchanged"
git fsck --no-dangling >/dev/null 2>&1 || fail "fsck reported problems"
ok "fsck clean"

# ═══════════════════════════════════════════════════════════════════════════
section "Scenario B — guarantee, --sign, --recurse-submodules"
B="$WORK/b"; BSUB="$WORK/b-sub"
SIGN_KIND=none
if command -v gpg >/dev/null 2>&1; then
  export GNUPGHOME="$WORK/gnupg"; mkdir -p "$GNUPGHOME"; chmod 700 "$GNUPGHOME"
  cat >"$GNUPGHOME/batch" <<'KEY'
%no-protection
Key-Type: eddsa
Key-Curve: ed25519
Key-Usage: sign
Name-Real: Human Dev
Name-Email: human@example.com
Expire-Date: 0
%commit
KEY
  if gpg --batch --quiet --gen-key "$GNUPGHOME/batch" 2>/dev/null; then
    FPR=$(gpg --batch --list-secret-keys --with-colons 2>/dev/null | awk -F: '/^fpr/{print $10; exit}')
    [ -n "$FPR" ] && SIGN_KIND=gpg
  fi
fi
if [ "$SIGN_KIND" = none ] && command -v ssh-keygen >/dev/null 2>&1; then
  SSH_KEY="$WORK/signing-key"
  ssh-keygen -q -t ed25519 -N '' -f "$SSH_KEY"
  printf 'human@example.com %s\n' "$(cat "$SSH_KEY.pub")" >"$WORK/allowed-signers"
  SIGN_KIND=ssh
fi
echo "signing test: $SIGN_KIND"

# The submodule: its own dirty history.
git init -q -b main "$BSUB"; cd "$BSUB"
git config user.name "Human Dev"; git config user.email "human@example.com"; git config commit.gpgsign false
c s.txt "sub: init"
c s.txt "sub: claude helped" "" "Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
SUB_V1=$(git rev-parse HEAD)
c s.txt "sub: later work" "" "Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
SUB_V2=$(git rev-parse HEAD)

build_fixture "$B"
cd "$B"
git -c protocol.file.allow=always submodule add -q "$BSUB" sub
git -C sub checkout -q "$SUB_V1"
git add sub .gitmodules && git commit -q -m "chore: add submodule at v1"
SUPER_AT_V1=$(git rev-parse HEAD)
git -C sub checkout -q "$SUB_V2"
git add sub && git commit -q -m "chore: bump submodule to v2"
if [ "$SIGN_KIND" != none ]; then
  if [ "$SIGN_KIND" = gpg ]; then
    git config user.signingkey "$FPR"; git config gpg.program gpg
  else
    git config gpg.format ssh
    git config user.signingkey "$SSH_KEY"
    git config gpg.ssh.allowedSignersFile "$WORK/allowed-signers"
  fi
  git config commit.gpgsign true; git config tag.gpgSign true
  c g.txt "feat: signed work" "" "Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
  c g.txt "feat: more signed work"
  git tag -s v0.2.0 -m "release v0.2.0 (signed)"
  SIGNED_BEFORE=$(git log --format='%G?' --branches --tags | grep -c '^[GU]$' || true)
  git config commit.gpgsign false; git config tag.gpgSign false
fi
git notes --ref=ai add -m "agent: claude-code" HEAD
echo scratch >>a.txt && git stash -q
TOTAL=$(git rev-list --branches --tags --count)
echo "fixture: $TOTAL commits, submodule at $BSUB"

echo; echo "── guarantee + sign + recurse"
SIGN_FLAG=(); [ "$SIGN_KIND" != none ] && SIGN_FLAG=(--sign)
if ! timeout 180 "$BIN" "$B" --guarantee --recurse-submodules --no-ui "${SIGN_FLAG[@]}" </dev/null; then fail "luddite --guarantee exited non-zero"; fi

echo; echo "── post-checks (plain git)"
DUMP=$(git log --all --format='%B%n%an <%ae>%n%cn <%ce>')
check_clean_dump "superproject" "$DUMP"
SUB_DUMP=$(git -C sub log --all --format='%B%n%an <%ae>%n%cn <%ce>')
check_clean_dump "submodule" "$SUB_DUMP"
[ "$(git rev-list --branches --tags --count)" -eq "$TOTAL" ] || fail "commit count changed"
ok "commit count unchanged ($TOTAL)"
NEW_SUB_HEAD=$(git -C sub rev-parse HEAD)
[ "$NEW_SUB_HEAD" != "$SUB_V2" ] || fail "submodule HEAD was not rewritten"
[ "$(git rev-parse HEAD:sub)" = "$NEW_SUB_HEAD" ] || fail "superproject gitlink at HEAD does not match the rewritten submodule HEAD"
ok "gitlink at HEAD remapped to the rewritten submodule commit"
OLD_LINK=$(git rev-parse "$(git log --all --format=%H --grep='add submodule at v1' | head -1):sub" 2>/dev/null || true)
[ -n "$OLD_LINK" ] || fail "could not find the older superproject commit"
git -C sub cat-file -e "$OLD_LINK" 2>/dev/null || fail "older gitlink $OLD_LINK points at a commit the submodule no longer has"
[ "$OLD_LINK" != "$SUB_V1" ] || fail "older gitlink was not remapped"
ok "older gitlink remapped too (history stays checkout-able)"
[ "$(git rev-parse HEAD)" != "$SUPER_AT_V1" ] || true
[ -z "$(git status --porcelain --ignore-submodules=none)" ] || { git status --short; fail "working tree/index not clean after the rewrite"; }
if git submodule status | grep -qE '^[+-]'; then fail "submodule status shows a mismatch"; fi
ok "index gitlink updated; git status clean; submodule status in sync"
if [ "$SIGN_KIND" != none ]; then
  SIGNED_AFTER=$(git log --format='%G?' --branches --tags | grep -c '^[GU]$' || true)
  [ "$SIGNED_AFTER" -eq "$SIGNED_BEFORE" ] || fail "signed commit count changed: $SIGNED_BEFORE → $SIGNED_AFTER"
  git verify-commit HEAD >/dev/null 2>&1 || fail "HEAD does not verify"
  git verify-tag v0.2.0 >/dev/null 2>&1 || fail "tag v0.2.0 does not verify"
  [ "$(git cat-file -p refs/tags/v0.2.0 | grep -cE 'BEGIN (PGP|SSH) SIGNATURE')" -eq 1 ] || fail "tag carries a stale signature block"
  ok "signatures re-created: $SIGNED_AFTER signed commits, HEAD and v0.2.0 verify, one signature block per tag"
  [ "$(git log -1 --format=%ct HEAD)" = "$(git log -1 --format=%at HEAD)" ] || true
fi
grep -q 'Co-Authored-By: Jane Doe <jane@openai.com>' <<<"$DUMP" || fail "human co-author was removed"
if ! grep -q 'Body text stays.' <<<"$DUMP" || ! grep -q 'Squash-merged PR body.' <<<"$DUMP"; then fail "commit body text lost"; fi
ok "human co-author + bodies preserved"
[ "$(git log --all --format='%an <%ae>' | sort -u)" = "Human Dev <human@example.com>" ] || fail "bot author identity survived"
ok "bot-authored commits now carry the human identity"
git rev-parse -q --verify refs/notes/ai >/dev/null && fail "refs/notes/ai survived"
git rev-parse -q --verify refs/stash >/dev/null && fail "stash survived"
ok "AI notes ref + stash gone"
ls "$WORK"/luddite-backup-b-*.bundle >/dev/null 2>&1 || fail "superproject bundle missing"
ls "$WORK"/luddite-backup-submodule-sub-*.bundle >/dev/null 2>&1 || fail "submodule bundle missing"
for bnd in "$WORK"/luddite-backup-*.bundle; do git bundle verify "$bnd" >/dev/null 2>&1 || fail "bundle does not verify: $bnd"; done
ok "backup bundles for the superproject and the submodule verify"
git fsck --no-dangling >/dev/null 2>&1 || fail "superproject fsck reported problems"
git -C sub fsck --no-dangling >/dev/null 2>&1 || fail "submodule fsck reported problems"
ok "fsck clean in both repositories"
if [ "$SIGN_KIND" = gpg ]; then gpgconf --kill gpg-agent 2>/dev/null || true; fi
echo; echo "SMOKE PASS"
