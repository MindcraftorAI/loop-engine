# Day 11 post-research — what we learned

**Distinct from audit** (which asked "is THIS correct?"). Post-research
asks "what did we LEARN from building this that we didn't know going in?
What should the NEXT day's pre-research focus on?"

## New knowledge surfaced during the build

1. **The TS yaml library is MORE permissive than YAML 1.2 says.** It
   emits values plain that the spec arguably says should be quoted.
   Empirically verifying TS output (vs reading the spec) is the only
   reliable way to match byte output. Audit A4 caught this for
   `yes/no/on/off` lowercase + embedded quotes/tabs.

2. **YAML extra-numerics aren't covered by Rust's f64::from_str.**
   `.inf`, `.nan`, `0x10`, `0o7`, `+42` parse fine in YAML but Rust
   parsers reject them. Means our `needs_quoting` check has to be
   smarter than "would Rust parse this as a number." Audit A3.

3. **Control-char escapes have a named-form table.** TS yaml emits
   `\0`, `\a`, `\b`, `\t`, `\n`, `\v`, `\f`, `\r`, `\e` for those
   specific bytes, and `\xNN` only for unmapped controls. Default Rust
   `\xNN`-for-everything strategy doesn't match.

4. **Field order divergence between TS capture-path and load-path is
   real** and was undocumented elsewhere. The load-path order matters
   because it's the order lessons stabilize in after any read-modify-write.

5. **`combine_frontmatter` accumulates leading newlines per cycle** —
   a real bug we inherit from TS. Day 12 must normalize body whitespace
   before recombining or lessons grow unboundedly.

## What this means for Day 12 (lesson loader + signal writer)

- Body normalization is load-bearing. Don't ship Day 12 without
  `trim_start_matches('\n')` on body before `combine_frontmatter`.
- The lesson loader can use `serde_yml` (parse-direction) safely.
- Writer reuse: Day 12's signal writes route through
  `serialize_lesson_frontmatter` → byte-stable output for free.

## What the Day 12 pre-research should explicitly cover

- `fd-lock` semantics on macOS vs Linux: per-OFD vs per-inode flock.
  Open question: does the lock-on-data-file pattern work when atomic
  rename replaces the inode?
- Empirical: what happens to a held flock when the underlying inode is
  unlinked? Does the lock survive on the orphan? Does a fresh open of
  the path (now a new inode) take the lock immediately?
- TS-side reference: does `core/src/lessons/signals.ts` take any
  file-level lock? (Spoiler from the build: no — uses `async-mutex`
  for in-process only.)

(In practice the Day 12 audit caught the lock-then-rename race; pre-
research would have surfaced it before code landed.)
