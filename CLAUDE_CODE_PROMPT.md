# Shellmind — Claude Code Continuation Prompt

You are continuing development of **shellmind** (`sm`), a cross-platform personal
automation kernel written in Rust. The codebase is complete and compiling at v0.1.0.
Read `REFERENCE.md` fully before touching any code — it is the authoritative map of
every module, type, and function.

---

## Project summary

`sm` is a CLI tool that wraps your shell with a semantic resolution layer:

```
user types: sm same_as_yesterday jan.csv https://api.io
                │
                ▼
         4-stage pipeline:
         1. exact verb match
         2. prefix / levenshtein
         3. embedding similarity  ←─ disambiguator (re-rank by args + session + :wr:)
         4. model inference
                │
                ▼
         eval'd by shell hook → sm __exec → Executor::run_steps
```

The registry is dual-store (curated/staging JSON files). Embeddings are a flat binary
store (`embeddings.bin`). The disambiguator shows an interactive prompt on ambiguous
hits, routes to passthrough on non-TTY. All user output goes to stderr; only the
resolved command goes to stdout (shell hook safety).

---

## Current state (what works)

All of the following compile and are tested:

- `sm add <verb> "<expansion>" [args]` — one-liner alias, immediate curated + embedded
- `sm add proc <verb>` — interactive 8-step-type procedure builder
- `sm add <verb>` — show existing or start wizard
- `sm wrap last <N> as <verb>` — wrap session history into a procedure
- `sm confirm / demote` — staging ↔ curated promotion
- `sm list [--staging]` — coloured registry table
- `sm reindex` — rebuild embeddings from registry
- `sm promote` — auto-promotion pass with confidence decay
- `sm resolve` — full 4-stage pipeline with disambiguation
- `sm __exec` — procedure executor
- `sm init / hook` — shell hook generation (bash/zsh/fish/PowerShell)

---

## Your task: implement in priority order

Work through these in order. Complete each fully (including compile check) before
moving to the next. After each feature, run `cargo build` and fix all errors before
proceeding.

---

### Task 1 — `sm init --write`

**File:** `src/cli/mod.rs` → `cmd_init()`

Current `sm init` only prints the hook snippet. Extend it to:

1. Detect the user's rc file:
   - Linux/Mac: check `$SHELL`, map to `~/.bashrc` / `~/.zshrc` / `~/.config/fish/config.fish`
   - Windows: `$PROFILE` (PowerShell)
   - Fallback: ask the user to enter a path
2. Check if the hook is already present (grep for `shellmind hook` or `sm resolve`)
3. If not present and `--write` flag passed: append the hook snippet with a comment header
4. If already present: print "Hook already installed in <path>" and exit cleanly
5. Print the rc file path and tell user to `source` it

Signature change:
```rust
fn cmd_init(rest: &[String], paths: &ShellmindPaths, shell_env: &ShellEnv) -> Result<()>
```

Wire `--write` from `rest`. Update dispatch accordingly.

---

### Task 2 — `sm remove <verb>`

**File:** `src/cli/mod.rs`

New command. Removes a verb from whichever store it lives in (curated or staging),
removes its embedding from `embeddings.bin`, and deletes the TOML file.

```
sm remove post_ingest
  Found 'post_ingest' in curated.
  verb    : post_ingest
  expands : curl -s -X POST {url} …
  Remove? [y/N]
  ✓ Removed.
```

Add to dispatch, add to help text. Use `print_entry_detail()` for the preview.
Use `EmbeddingStore::remove()` + `store.save()` for the embedding cleanup.

---

### Task 3 — `sm rename <old> <new>`

**File:** `src/cli/mod.rs`

Rename a verb: load entry, change `.verb`, rewrite TOML with new filename, delete old
TOML file, update embedding store (remove old key, upsert new key with same vector).

```
sm rename sampl sample_csv
  Renamed 'sampl' → 'sample_csv'.
```

Collision check: if `<new>` already exists, prompt to confirm overwrite.

---

### Task 4 — `sm edit <verb>`

**File:** `src/editor/mod.rs` (currently a stub — implement it fully)

Opens the entry's TOML file in `$EDITOR` (fallback to `$VISUAL`, then `nano`, then `vi`).
After the editor exits, reload the file and re-embed.

```rust
pub fn open_in_editor(path: &Path) -> Result<()>
pub fn reload_and_reindex(verb: &str, paths: &ShellmindPaths) -> Result<()>
```

Wire into `cli/mod.rs` as `sm edit <verb>`.

The collision mentioned in `sm add <verb>` (bare verb, existing entry) should also
offer `[e]dit` as an option alongside replace/cancel.

---

### Task 5 — `redb` integration

**File:** `src/registry/store.rs` (currently a stub)

Replace the `load_all_from_dir` + `write_toml` file-scan approach with redb for
O(1) verb lookup. Keep TOML files as the human-readable source of truth — redb is
a cache/index.

Add `redb = "2"` to `Cargo.toml` (check edition2024 compatibility first with
`cargo add redb` and inspect what resolves).

Design:
```rust
pub struct RegistryStore {
    db: redb::Database,
}

impl RegistryStore {
    pub fn open(path: &Path) -> Result<Self>
    pub fn get(&self, verb: &str) -> Result<Option<CommandEntry>>
    pub fn put(&self, entry: &CommandEntry) -> Result<()>
    pub fn delete(&self, verb: &str) -> Result<()>
    pub fn all_verbs(&self) -> Result<Vec<String>>
    pub fn rebuild_from_toml_dir(&self, dir: &Path) -> Result<usize>
}
```

The `rebuild_from_toml_dir` method is what `sm reindex` calls to warm the cache.
Wire `RegistryStore` into `Registry` as an optional fast path:
- if redb file exists → use it for lookups
- if not → fall back to file scan and offer to build it

---

### Task 6 — Real embeddings with `fastembed`

**File:** `src/embedder/mod.rs`

Replace `embed_text_bow()` with `fastembed` using the `all-MiniLM-L6-v2` model.

```rust
pub fn embed_text(text: &str) -> Result<Vec<f32>>
```

Feature-flag it so the BOW fallback still works when `fastembed` isn't available:

```toml
[features]
default = ["bow-embeddings"]
semantic-embeddings = ["fastembed"]
bow-embeddings = []
```

```rust
pub fn embed_text(text: &str, dim: usize) -> Vec<f32> {
    #[cfg(feature = "semantic-embeddings")]
    { fastembed_embed(text) }
    #[cfg(feature = "bow-embeddings")]
    { embed_text_bow(text, dim) }
}
```

Update `EMBED_DIM` to 384 (MiniLM output dimension) when semantic embeddings are active.
Note: existing `embeddings.bin` will need to be rebuilt with `sm reindex` after switching.

---

### Task 7 — `sm run <verb> [args]` (explicit execution without shell hook)

**File:** `src/cli/mod.rs`

Currently procedures only run via the shell hook eval path (`sm __exec`). Add a direct
`sm run <verb> [args]` that bypasses the hook and executes immediately, printing step
progress.

```
sm run linko_daily_push sales.csv https://api.io/ingest
  [1/3] Sample 10% stratified by category … ✓
  [2/3] Move to processed folder … ✓
  [3/3] POST file to API … ✓
  Done. (3/3 steps succeeded)
```

Useful for: testing procedures, running from scripts, CI contexts.

---

### Task 8 — `sm list` improvements

**File:** `src/cli/mod.rs` → `cmd_list()`

Add:
- `sm list --detail` — show expansion/step count per entry
- `sm list <pattern>` — filter by verb prefix or substring
- Sort options: `--by-usage`, `--by-date` (default: alphabetical)
- Show staging count even in curated view: "7 curated, 2 in staging"

---

## Code conventions to follow

- All user-facing text to **stderr** except the resolved command from `sm resolve`
- ANSI colour codes: `\x1b[32m` green (success), `\x1b[33m` yellow (warning),
  `\x1b[31m` red (error), `\x1b[90m` dim (labels), `\x1b[0m` reset
- All new commands: add to `dispatch()` match, add to `print_help()`, add to `REFERENCE.md`
- New entries always go through `write_and_index()` to keep embeddings in sync
- `build_registry(paths)` and `build_resolver(paths)` are the standard builder fns —
  use them, don't construct Registry/Resolver directly in handlers
- Keep `cmd_resolve` stdout-only; all annotation goes to stderr
- `prompt_field(label, required)` is available for all interactive input
- Run `cargo build` after each task. Fix all warnings in new code (existing warnings ok).

---

## Cargo.toml note

Crates currently pinned to pre-edition2024 versions due to Rust 1.75 sandbox.
On a modern toolchain (≥ 1.80), remove `=` pins and add:

```toml
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
shellexpand = "3"
rustyline = "14"
redb = "2"
```

Replace in code:
- `model_client/mod.rs`: curl subprocess → reqwest async client
- `platform/mod.rs` `expand_env()` → `shellexpand::full()`
- `cli/mod.rs` stdin readline → `rustyline::Editor`

---

## Testing each task

After each task, test with:

```bash
cargo build

# Task 1
sm init --write
cat ~/.bashrc | tail -20   # verify hook appended

# Task 2
sm add test_remove "echo test"
sm remove test_remove

# Task 3
sm add old_name "echo old"
sm rename old_name new_name
sm list

# Task 4
sm edit post_ingest   # opens editor

# Task 5
sm reindex   # should use redb now
SHELLMIND_LOG=debug sm resolve "sample" 2>&1 | grep redb

# Task 6
cargo build --features semantic-embeddings
sm reindex
sm resolve "sample csv file"   # should show better similarity

# Task 7
sm run linko_daily_push test.csv https://httpbin.org/post

# Task 8
sm list --detail
sm list sample
sm list --by-usage
```

---

## File structure reminder

```
src/
├── main.rs              declare all modules here
├── platform/{mod,paths,shell,executor}.rs
├── registry/{mod,promotion,store}.rs
├── embedder/mod.rs
├── session/mod.rs
├── disambiguator/mod.rs
├── resolver/mod.rs
├── model_client/mod.rs
├── executor/mod.rs      (re-export shim only)
├── editor/mod.rs        (stub → implement in Task 4)
└── cli/mod.rs           (all command handlers)
```

Good luck. Read REFERENCE.md first. Build after every task.
