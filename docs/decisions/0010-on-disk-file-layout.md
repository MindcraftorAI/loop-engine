# ADR-0010: On-disk file layout — files canonical, database as derived index

**Status:** Accepted
**Date:** 2026-05-11

## Context

LOOP's free tier is self-hosted, with skills + memory + lessons persisting on the user's disk. Two reasonable approaches exist for the canonical representation:

- **Database-canonical:** all state in SQLite; filesystem files (if any) are exports
- **Files-canonical:** state on disk as inspectable files; database is a derived index for fast retrieval

Claude Code chose files-canonical for its memory system. Hermes Agent leans database-canonical for its built-in memory backend. LOOP's stance affects observability, debuggability, manual editing, and backup/sync workflows.

## Decision

LOOP uses **files-canonical with the database as a derived index**.

### Top-level layout under `~/.loop/`

```
~/.loop/
├── config.yaml              # user configuration
├── db/                      # SQLite + vector index (derived, not authoritative)
│   ├── loop.sqlite
│   └── vectors.idx
├── skills/                  # canonical skill files
│   └── <name>/
│       ├── SKILL.md
│       ├── references/
│       └── templates/
├── memory/                  # canonical memory files; DB indexes them
│   └── <scope>/
│       └── <memory-id>.md
├── lessons/                 # provisional learnings, organized by status
│   ├── active/              # currently layered into inference
│   ├── pending/             # observed / hypothesized — not yet active
│   ├── promoted/            # archived after merge into skill/memory (audit trail)
│   └── discarded/           # kept briefly for debugging
├── bundles/                 # installed marketplace packs (unpacked)
│   └── <bundle-id>/
└── logs/                    # debug + audit logs
```

### Key design principles

- **Files are canonical.** Deleting `~/.loop/db/` rebuilds the index from files. Deleting files loses data.
- **DB is performance, files are trust.** SQLite + vectors.idx give fast queries; files give user-readable, user-editable, manually-debuggable state.
- **Status-as-directory for lessons** — not status-in-frontmatter. `ls ~/.loop/lessons/active/` shows in-flight learning at a glance. Lifecycle transitions move files between subdirs (`pending/` → `active/` → `promoted/` or `discarded/`).
- **`LOOP_HOME` env var overrides the default path.** For server deployments (RankLabs), the same layout rooted at `/var/lib/loop/` or wherever the operator chooses.

### File watching

LOOP watches `~/.loop/skills/`, `~/.loop/memory/`, and `~/.loop/lessons/` for manual edits. Detected changes trigger re-indexing in the SQLite store + vector index. This means:

- A user can hand-edit a skill file in their editor — LOOP picks up the change
- A user can `rm` a misbehaving lesson — LOOP removes it from the active context
- A user can `cp` skills between machines without going through any LOOP-specific tooling

### Hosted SaaS tier

The hosted tier (paid) abstracts the filesystem entirely — equivalent records live in Postgres + a managed vector DB. The data model is identical; the persistence backend differs. This is why the data model defines entities by their semantic shape, not their on-disk path.

## Consequences

**Pros:**
- User trust and observability — anyone can `ls` to see what LOOP holds
- Manual debugging — delete a file to fix bad state
- Standard tooling works (file sync via Dropbox / iCloud / Git, backups via Time Machine, etc.)
- Self-healing — corrupt DB? Delete it; LOOP rebuilds from files
- Matches Claude Code's mental model — users moving between the two systems aren't surprised
- Status-as-directory makes lesson lifecycle visible without opening files

**Cons:**
- Two storage layers must be kept in sync (files + DB index)
- File system watchers can be platform-quirky (especially on Windows / macOS Finder)
- Very large memory counts (10,000+) generate many small files — most filesystems handle this fine, but it's worth noting
- Concurrent writers (multiple LOOP processes on the same `~/.loop/`) need file-locking discipline

## Mitigation for the sync issue

- LOOP uses an append-friendly write order: write the file first, then update the DB index. If the process dies between, the next startup re-indexes.
- File-system watchers fall back to scan-on-startup if the watcher fails.
- File operations use atomic writes (write to temp, rename) to avoid half-written files mid-crash.

## Alternatives considered

- **Database-canonical:** rejected. Loses observability and manual debugging. Matches Hermes's pattern but Hermes also chose a runtime-shape product where users don't expect to inspect storage; LOOP is substrate-shape where transparency matters.
- **All-in-one file (single markdown per concept):** rejected. Doesn't scale and merge conflicts on file sync would be brutal.
- **Status-in-frontmatter for lessons (single flat dir):** rejected. Status changes would require frontmatter edits + index updates with no `ls`-level visibility. Status-as-directory is more honest.

## Related

- [ARCHITECTURE.md](../ARCHITECTURE.md) — On-disk Layout section
- [ADR-0002](0002-loop-complements-claude-code.md) — namespace isolation under `~/.loop/`
- [ADR-0003](0003-two-tier-free-self-hosted-paid-saas.md) — hosted tier abstracts the filesystem
