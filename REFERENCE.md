# Shellmind — Architecture Reference
> Last updated: v0.2.0 — Tasks 1–8 + __record hook landed in 36fec8e

---

## What it is

Shellmind (`sm`) is a cross-platform personal automation kernel with semantic memory.
It is **not** a smarter shell. It is a resolution layer that sits in front of your shell,
translating natural/custom command vocabulary into real commands — and learning from use.

Core idea: you build a personal registry of named operations (aliases and multi-step
procedures). When you type something, shellmind resolves it through a 4-stage pipeline.
Unknown inputs are either inferred by a small model or passed through unchanged.

---

## 1. Module Map

```
src/
├── main.rs                    Entry point, tokio runtime, module declarations
│
├── platform/                  OS abstraction — the foundation everything else builds on
│   ├── mod.rs                 StepKind (12 variants), Arg, Platform, HttpMethod
│   ├── paths.rs               ShellmindPaths — cross-platform config dir resolution
│   ├── shell.rs               ShellEnv, ShellKind, hook snippet generation
│   └── executor.rs            Executor — runs StepKind sequences, pure Rust (no awk/python)
│
├── registry/                  Command storage — dual-store (curated + staging)
│   ├── mod.rs                 CommandEntry, EntryKind, Registry, TOML/JSON serialization
│   ├── promotion.rs           PromotionPolicy, confidence decay, confirm/demote
│   └── store.rs               RegistryStore — redb O(1) verb lookup, cache over TOML SoT
│
├── embedder/                  Semantic memory — flat binary embedding store
│   └── mod.rs                 EmbeddingStore, EmbeddingSource, embed_text(), BoW + optional fastembed
│
├── session/                   Runtime history — ring buffer, wrap machinery
│   └── mod.rs                 SessionBuffer, WrapPreview, build_wrap_preview(), finalize_wrap()
│
├── disambiguator/             Disambiguation layer — sits between Stage 2/3 and Resolution
│   └── mod.rs                 evaluate(), prompt(), ScoredCandidate, re-ranking passes
│
├── resolver/                  4-stage resolution pipeline
│   └── mod.rs                 Resolver, Resolution (5 variants), split_verb_args(), expand_entry()
│
├── model_client/              Small model inference — OpenAI-compatible
│   └── mod.rs                 ModelClient, ModelConfig, curl-based HTTP fallback
│
├── executor/                  Re-export shim
│   └── mod.rs                 pub use crate::platform::executor::*
│
├── editor/                    $EDITOR / $VISUAL integration, reload + re-embed on save
│   └── mod.rs                 open_in_editor(), reload_and_reindex()
│
└── cli/                       User-facing dispatch + all command handlers (~1300 lines)
    └── mod.rs                 dispatch(), cmd_add(), cmd_wrap(), cmd_resolve(), cmd_run(), cmd_edit(), cmd_remove(), cmd_rename(), …
```

---

## 2. Complete Public API

### `platform/mod.rs`

```rust
Platform { Linux, Mac, Windows }
  ::current() → Platform

StepKind (enum, 12 variants — all cross-platform, no shell dependencies)
  FileCopy    { src: Arg, dst: Arg }
  FileMove    { src: Arg, dst: Arg }
  FileDelete  { target: Arg, recursive: bool }
  MkDir       { path: Arg }
  ListDir     { path: Arg, pattern: Option<String> }
  ReadFile    { path: Arg }
  WriteFile   { path: Arg, content: Arg, append: bool }
  DataSample  { file, pct: f32, stratified: bool, strat_col, output }
  DataJoin    { left, right, on: String, output }
  HttpCall    { url, method: HttpMethod, body, headers, output }
  SetEnv      { key: String, value: Arg }
  ShellRaw    { cmd: String, platform: Platform }   ← tagged escape hatch
  Echo        { message: Arg }

Arg (enum)
  Literal     { value: String }
  Slot        { index: usize }           ← $1, $2 from call site
  StepOutput  { step_index: usize }      ← output of prior step
  ::literal(s), ::slot(i), ::step_output(i)
  .resolve(slots: &[String], outputs: &[Option<String>]) → Result<String>

HttpMethod { Get, Post, Put, Patch, Delete }
```

### `platform/paths.rs`

```rust
ShellmindPaths
  .config_dir          ~/.config/shellmind  (Linux/Mac) | %APPDATA%\shellmind (Win)
  .curated_toml_dir    config_dir/registry/curated/
  .staging_toml_dir    config_dir/registry/staging/
  .curated_db          config_dir/db/curated.redb       ← future redb
  .staging_db          config_dir/db/staging.redb
  .embeddings_bin      config_dir/db/embeddings.bin
  .session_history     config_dir/session/history.bin
  .user_config         config_dir/shellmind.toml
  ::resolve() → Result<Self>
  .ensure_dirs()
```

### `platform/shell.rs`

```rust
ShellEnv
  .platform: Platform
  .shell_kind: ShellKind    { Bash, Zsh, Fish, PowerShell, Cmd, Unknown }
  .eval_wrapper: EvalWrapper { UnixEval, PowerShellIex, Unsupported }
  ::detect() → ShellEnv     ← reads $SHELL / $PSModulePath
  .hook_snippet() → String  ← bash/zsh/fish/PowerShell rc hook
```

### `platform/executor.rs`

```rust
Executor
  ::new(platform: Platform) → Self
  .run_steps(steps: &[StepKind], slots: &[String], continue_on_error: bool)
    → Result<Vec<StepResult>>

StepResult { success: bool, output: Option<String>, exit_code: Option<i32> }
```

### `registry/mod.rs`

```rust
CommandEntry
  .id: Uuid
  .verb: String
  .kind: EntryKind
  .tags: Vec<String>          ← [":wr:"] for wrapped procedures
  .source: EntrySource
  .usage_count: u32
  .confidence: f32            ← 0.0–1.0; declarative/confirmed = 1.0
  .is_procedure(), .is_wrapped(), .is_curated()
  .to_toml_file() → Result<String>   ← serializes as JSON (files named .toml)
  .toml_filename() → String

EntryKind
  Alias     { expansion: String, arg_names: Vec<String> }
  Procedure { steps: Vec<ProcStep>, arg_bindings: Vec<ArgBind>,
              description: Option<String> }

ProcStep  { index, step: StepKind, description, depends_on: Vec<usize> }
ArgBind   { call_position, label, step_index, placeholder }

EntrySource
  Declarative
  Inferred   { model: String, confidence: f32 }
  Confirmed  { original_confidence: f32 }
  Wrapped    { session_indices: Vec<usize> }

Registry
  ::new(curated_dir, staging_dir) → Self
  .write_toml(entry)              → Result<()>
  .load_all_from_dir(dir)         → Result<Vec<CommandEntry>>
```

### `registry/promotion.rs`

```rust
PromotionPolicy { auto_promote_uses: u32=5, decay_days: i64=30, min_confidence: f32=0.70 }
PromotionAction { Promoted, FlaggedForReview{reason}, Decayed{days}, Skipped }

impl Registry (extended):
  .run_promotion_pass(policy) → Result<Vec<PromotionResult>>
  .confirm(verb)              → Result<bool>   ← staging → curated
  .demote(verb)               → Result<bool>   ← curated → staging
```

### `embedder/mod.rs`

```rust
EMBED_DIM = 384 (semantic-embeddings feature) | 512 (default bow-embeddings)

EmbeddingSource { Curated(1.0x), Staging(0.85x), Wrapped(1.15x) }

EmbeddingStore
  ::load_or_create(path, dim: usize) → Result<Self>
  .len(), .has_verb(verb)
  .upsert(verb, source, vector)
  .remove(verb)
  .search(query: &[f32], top_k, threshold) → Vec<(String, f32)>  ← boosted scores desc
  .save()

embed_text(text: &str, dim: usize) → Vec<f32>
  ← unified entry point; tries fastembed (MiniLM-L6-v2) under semantic-embeddings,
    falls back to embed_text_bow on init/embed failure

embed_text_bow(text: &str, dim: usize) → Vec<f32>
  ← bag-of-words hash, L2 normalized, dim=512
```

### `registry/store.rs`

```rust
RegistryStore                                   ← redb cache layer over TOML SoT
  ::open(path: &Path) → Result<Self>
  .get(verb)               → Result<Option<CommandEntry>>
  .put(entry)              → Result<()>
  .delete(verb)            → Result<()>
  .all_entries()           → Result<Vec<CommandEntry>>
  .all_verbs()             → Result<Vec<String>>
  .rebuild_from_toml_dir(dir, registry) → Result<usize>   ← wipes and reloads
```

### `editor/mod.rs`

```rust
open_in_editor(path: &Path) → Result<()>
  ← $EDITOR → $VISUAL → first available of {nano, vi, notepad} → "vi"
  ← blocks until editor exits, returns Err on non-zero exit
reload_and_reindex(verb: &str, paths: &ShellmindPaths) → Result<()>
  ← re-reads the entry from disk and updates its embedding vector
  ← returns Err with hint if the verb was changed inside the editor
```

### `session/mod.rs`

```rust
SessionEntry { index, raw_input, resolved_command, timestamp, exit_code, cwd, steps }

SessionBuffer  (ring buffer, max 200)
  ::load_or_create(path) → Result<Self>
  .record(raw, resolved, exit_code, steps)
  .last_successful(n) → Vec<&SessionEntry>   ← exit_code == 0 only
  .save()

build_wrap_preview(entries: &[&SessionEntry]) → WrapPreview
finalize_wrap(verb, entries, slot_assignments) → CommandEntry
```

### `disambiguator/mod.rs`

```rust
// Constants
AUTOSELECT_GAP      = 0.12   ← score gap above which top candidate wins silently
AUTOSELECT_CEILING  = 0.94   ← score above which always auto-select
PROMPT_FLOOR        = 0.70   ← minimum score to appear in prompt
MAX_PROMPT_CANDIDATES = 4

ScoredCandidate { entry, raw_score, final_score, match_reason: MatchReason }
MatchReason { Exact, Prefix, Similarity{cosine}, SimilarityBoosted{cosine, boost} }

DisambiguationResult
  Clear(ScoredCandidate)         ← auto-selected, no prompt
  Ambiguous(Vec<ScoredCandidate>) ← caller must call prompt()
  Passthrough                    ← non-TTY or all below floor

evaluate(candidates, args: &[String], session: &[&SessionEntry]) → DisambiguationResult
  ← runs 3 re-ranking passes: arg_compatibility, session_recency, wrapped_preference

prompt(candidates, raw_input) → Option<usize>
  ← renders to stderr (safe for shell eval capture), reads stdin with 15s timeout

stdin_is_tty() → bool   ← isatty(0) on Unix, env-var heuristic on Windows

// Re-ranking details:
// arg_compatibility: URL args boost HttpCall steps (+10-12%), penalise FileCopy (-15-18%)
//                   path args boost file steps (+8-10%)
//                   arg count match boosts (+8%), mismatch penalises (-8-12%)
// session_recency:  +8% for verbs in last 10 successful commands
// wrapped_preference: +5% for :wr: entries when any wrapped candidate exists
```

### `resolver/mod.rs`

```rust
EMBED_DIM           = 512
SIMILARITY_THRESHOLD = 0.72   ← slightly lower than before; disambiguator handles FPs
TOP_K               = 5

Resolution (enum, 5 variants)
  Exact         { entry, expanded }
  Similar       { entry, expanded, score }
  Disambiguated { entry, expanded, score, from_n }   ← user selected from prompt
  Inferred      { expanded, confidence, stored_as }  ← model result, stored in staging
  Passthrough   { raw }
  .as_shell_str() → &str
  .is_passthrough() → bool
  .matched_verb() → Option<&str>

Resolver
  ::new(registry, embeddings, model) → Self
  .refresh()
  .resolve(raw_input, session: &[&SessionEntry]) → Result<Resolution>
  .record_usage(verb)
  .reindex() → Result<usize>

split_verb_args(input) → (String, Vec<String>)
expand_entry(entry, args) → Result<String>
```

### `model_client/mod.rs`

```rust
ModelConfig
  .model_name: "gpt-4o-mini"      ← any OpenAI-compatible model
  .api_base: "https://api.openai.com/v1"
  .api_key_env: "OPENAI_API_KEY"  ← never hardcoded
  .max_tokens: 256
  .temperature: 0.1               ← deterministic expansions
  .timeout_secs: 8
  ::default()

ModelClient
  ::new(config) → Self
  .is_configured() → bool
  .infer(raw_input, few_shot: &[(&str, &str)]) → Result<(String, f32)>
  ← uses curl subprocess as HTTP fallback (no reqwest dep currently)
  ← few_shot: top-k similar entries as context
```

### `cli/mod.rs` — all commands

```rust
dispatch(args, paths, shell_env) → Result<()>

// User commands:
cmd_init(rest, paths, shell_env)        sm init [--write]
cmd_resolve(input, paths)               sm resolve <input>       ← called by shell hook
cmd_exec(verb, slots, paths)            sm __exec <verb> [args]  ← internal, called by hook
cmd_add(rest, paths)                    sm add …                 ← see below
cmd_wrap(rest, paths)                   sm wrap last <N> as <verb>
cmd_confirm(verb, paths)                sm confirm <verb>
cmd_demote(verb, paths)                 sm demote <verb>
cmd_list(rest, paths)                   sm list [--staging|--detail|--by-usage|--by-date] [pattern]
cmd_reindex(paths)                      sm reindex               ← rebuilds redb + embeddings
cmd_promote(paths)                      sm promote

// Task 2–7 additions:
cmd_remove(verb, paths)                 sm remove <verb>
cmd_rename(old, new, paths)             sm rename <old> <new>
cmd_edit(verb, paths)                   sm edit <verb>
cmd_run(verb, slots, paths)             sm run <verb> [args]     ← bypasses shell hook
__record                                sm __record <cmd>        ← internal, called by shell hook

// sm add modes:
// sm add <verb> "<expansion>" [arg1 arg2 ...]  → one-liner alias, immediate curated
// sm add <verb>                                → show existing or interactive alias wizard
// sm add proc <verb>                           → interactive procedure builder (8 step types)
//
// sm add internals:
add_alias_interactive(verb, paths)      ← wizard when no expansion given
add_procedure_interactive(verb, paths)  ← step-by-step builder
write_and_index(entry, paths)           ← write TOML + build embedding immediately
print_entry_detail(entry)               ← coloured detail display
prompt_field(label, required) → Result<String>
parse_arg(s, known_slots) → Arg         ← parses $N, {label}, or Literal
detect_slots_in_expansion(expansion) → Vec<String>
find_missing_args(expansion, declared) → Vec<String>
build_alias_entry(verb, expansion, arg_names) → CommandEntry
```

---

## 3. Resolution Pipeline (full trace)

```
sm <input>
│
└─ shell hook: eval $(sm resolve <input>)
   │
   └─ cmd_resolve(input, paths)
      ├─ SessionBuffer::load_or_create()   ← for disambiguator context
      ├─ Resolver::refresh()               ← load_all_from_dir(curated) + (staging)
      ├─ split_verb_args(input)
      │
      ├─ Stage 1: exact_match(verb)
      │    curated-first lookup by verb string
      │    → Resolution::Exact             SHORT-CIRCUIT, no disambiguation
      │
      ├─ Stage 2: prefix_candidates(verb)
      │    starts_with / ends_with / levenshtein(≤2)
      │    → Vec<ScoredCandidate>
      │    → disambiguator::evaluate(candidates, args, session)
      │         rerank_by_args()           ← URL/path/count signals
      │         rerank_by_session()        ← +8% for recent verbs
      │         rerank_wrapped_preference() ← +5% for :wr:
      │         gap >= 0.12 or top >= 0.94 → DisambiguationResult::Clear
      │         tight gap + TTY            → DisambiguationResult::Ambiguous
      │                                        disambiguator::prompt() → stderr
      │         non-TTY or all < 0.70      → DisambiguationResult::Passthrough
      │    → Resolution::Similar or Resolution::Disambiguated
      │
      ├─ Stage 3: embedding similarity
      │    embed_text_bow(raw_input, 512)
      │    EmbeddingStore::search(query, TOP_K=5, threshold=0.72)
      │    → Vec<ScoredCandidate> (Wrapped entries boosted 1.15x)
      │    → disambiguator::evaluate()     (same as Stage 2)
      │    → Resolution::Similar or Resolution::Disambiguated
      │
      └─ Stage 4: model inference
           ModelClient::infer(raw_input, few_shot_from_top_k)
           → (expanded, confidence)
           → store in staging + embed
           → Resolution::Inferred
           (if model not configured → Resolution::Passthrough)
```

---

## 4. Registry File Format

Files live in `~/.config/shellmind/registry/curated/` and `.../staging/`.
Named `<verb>.toml` but contain JSON (for reliable parsing).

```json
{
  "id": "uuid-v4",
  "verb": "post_ingest",
  "confidence": 1.0,
  "source": "declarative",
  "created_at": "2026-05-10T00:00:00Z",
  "usage_count": 3,
  "tags": [],
  "expansion": "curl -s -X POST {url} -H 'Content-Type: text/csv' --data-binary @{file}",
  "arg_names": ["url", "file"]
}
```

Procedure files include `steps` and `arg_bindings` arrays instead of `expansion`.
The `source` field encodes trust level:
- `"declarative"` — user-authored, always confidence 1.0
- `"inferred:gpt-4o-mini:0.82"` — model-generated, pending confirmation
- `"confirmed:0.82"` — previously inferred, user confirmed
- `"wrapped"` — created by `sm wrap`, tagged `:wr:`

---

## 5. Embedding Store Binary Format

`~/.config/shellmind/db/embeddings.bin`

```
[u32 MAGIC=0x534D454D][u32 VERSION=1][u32 dim][u32 count]
per entry:
  [u8 source_tag: 0=Curated 1=Staging 2=Wrapped]
  [u32 verb_len][verb utf8 bytes]
  [f32 * dim]   ← L2 normalized vector
```

Single file shared across curated and staging — search spans both with source-based
score boosting. Rebuilt via `sm reindex`.

---

## 6. Shell Hook (how it all connects)

```bash
# ~/.bashrc
function sm() {
    local result
    result=$(command sm resolve "$@" 2>/tmp/sm_err)
    if [ $? -eq 0 ] && [ -n "$result" ]; then
        eval "$result"
    else
        command sm "$@"
    fi
}
```

`sm resolve` outputs to **stdout** (gets eval'd by the hook).
All user-facing output (disambiguation prompt, warnings, inferred notices) goes to **stderr**.
This split is load-bearing — do not merge them.

The `sm __exec <verb> [slots...]` pattern is what procedures resolve to.
The shell hook eval's it, which calls back into `sm` as `cmd_exec`, which runs `Executor::run_steps`.

---

## 7. Known Gaps / Next Development Priorities

Completed in 36fec8e: `sm init --write`, redb integration, optional fastembed (MiniLM-L6-v2), `sm edit`, `sm remove`, `sm rename`. Plus bonus `sm run` and `__record` session capture.

| Priority | Feature | Module | Notes |
|---|---|---|---|
| 1 | Native `StepKind` inference in `wrap` | `session`, `cli` | Currently wraps `ShellRaw` only; detect curl/wget→`HttpCall`, cp→`FileCopy`, mv→`FileMove`, mkdir→`MkDir`, rm→`FileDelete`, export→`SetEnv`, echo→`Echo` |
| 2 | Windows ConPTY testing | `platform/shell.rs`, `cli` | PowerShell hook end-to-end on real Windows shell; verify `__exec`, `__record` paths |
| 3 | Scheduled procedures | `cli`, new `scheduler/mod.rs` | Cron-like execution of registered procedures |
| 4 | Config file parser | `cli/load_model_config` | Currently line-by-line; use `serde_json` / `toml` |
| 5 | Drop `=` version pins | `Cargo.toml` | Inherited from Rust 1.75 sandbox; modern toolchain doesn't need them |

---

## 8. Build Notes

**Toolchain:** Tested on Rust 1.89 (Windows MSVC). Minimum is Rust ≥ 1.80 — required by `redb` and `fastembed` transitive deps. Most `Cargo.toml` entries still carry inherited `=` pins from a Rust 1.75 sandbox; they build fine on modern toolchains but are noise to clean up.

**Features:**
- `bow-embeddings` (default) — bag-of-words hash, 512-dim, no model download, fast compile
- `semantic-embeddings` — pulls `fastembed`, downloads MiniLM-L6-v2 on first run, 384-dim. Falls back to BoW on init failure.

**Crates intentionally still stubbed** (upgrade when convenient):
- `reqwest` — replace `curl` subprocess in `model_client/mod.rs`
- `shellexpand` — replace `expand_env()` in `platform/mod.rs`
- `rustyline` — replace `std::io::stdin` readline in `cli/mod.rs`

**Build:**
```bash
cargo build --release                                              # default BoW
cargo build --release --no-default-features --features semantic-embeddings   # MiniLM
cp target/release/sm ~/.local/bin/   # Linux/Mac
sm init --write   # detect rc file and append hook
```
