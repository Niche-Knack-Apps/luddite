# luddite — architecture

A single Rust binary. libgit2 (`git2`, no network transports) reads and writes
objects and refs; the `git` binary is used only where the user's full git
configuration must apply (`bundle`, `reflog expire`/`gc`, signing, `ls-remote`,
`push`).

## Modules

| Module | Role |
|---|---|
| `cli` | clap definition, `SignMode` |
| `patterns` | built-in TOML rule set (`luddite_patterns.toml`, compiled in) + discovered/explicit config files → `RuleSet` |
| `cleaner` | pure functions: strip matching message lines, normalise blank lines, identity matching |
| `rewriter` | the rewrite proper: collects refs, walks every reachable commit (topological, parents first), creates immutable backups only when a mutation is needed, writes cleaned commits, moves refs, recreates tags, keeps the index's gitlinks in step, and writes the maps. It can also reconstruct a prior `Report` from maps or legacy backups for `--ui` replay |
| `signing` | signature detection (`gpgsig` header / tag message block), `git commit-tree -S` and `git tag -s` wrappers with author/committer/tagger identity and date passed through the environment |
| `submodules` | `.gitmodules` discovery, recursive scrub of initialized submodules, memoised tree remapping of gitlinks |
| `verify` | the `--guarantee` re-scan of every reachable commit across every live ref |
| `push` | `ls-remote` parsing, plan rows (rewritten / diverged / new), the atomic `--force-with-lease=<ref>:<value>` command, streamed execution |
| `tui` | the review screen: `graph` (lane layout), `diff` (LCS line diff), `app` (state, keys, rendering) |
| `gitops` | bundle / purge helpers |

## Data flow

```
cli ─► patterns::load ─► rewriter::run(repo, Options)
                            ├─ submodules::process  (recursion: rewriter::run per submodule) ─► GitlinkMap
                            ├─ collect_refs ─► create_backup_refs
                            ├─ revwalk ─► per commit: clean message · sanitize identity · remap tree · (sign) · write
                            ├─ move refs · recreate tags · delete notes · detached HEAD · index gitlinks · maps
                            └─ Report
main ─► print stats ─► (--guarantee: purge + verify) ─► tui::run | push_now | guidance
```

## Invariants

- Trees are never changed except to remap gitlinks under `--recurse-submodules`; commit order, author/committer timestamps and human identities are preserved.
- Refs are only moved after every commit has been written, so an error mid-walk leaves nothing but dangling objects.
- `refs/backup/` and `refs/original/` are never read as sources nor rewritten.
- A clean repeat run does not change refs or maps, and an existing backup ref is never replaced.
- Pushing never relies on local remote-tracking refs for leases; every lease value is observed from the remote immediately before the push, and the push is `--atomic`.
