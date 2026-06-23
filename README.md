# shellmind (`sm`)

A cross-platform personal automation kernel with semantic memory. `sm` sits in front of your shell as a resolution layer — translating natural and custom command vocabulary into real commands, and learning from use.

It is **not** a smarter shell. It is a layer that resolves what you mean into what your shell should run.

---

## How it works

When you type `sm <anything>`, the shell hook captures it and passes it through a 4-stage resolution pipeline:

1. **Exact match** — looks up your verb directly in the registry (curated-first)
2. **Prefix / fuzzy match** — starts-with, ends-with, or Levenshtein distance ≤ 2
3. **Semantic similarity** — bag-of-words embedding search (cosine similarity ≥ 0.72)
4. **Model inference** — calls an OpenAI-compatible LLM to infer intent; result is stored in staging for review

When multiple candidates are close, a disambiguation prompt is shown on stderr. When the winner is clear (score gap ≥ 0.12 or score ≥ 0.94), it auto-selects silently.

---

## Features

- **Alias registry** — store named one-liner expansions with positional or named argument slots (`$1`, `{label}`)
- **Procedures** — multi-step workflows built from typed, cross-platform `StepKind`s (file ops, HTTP calls, env vars, shell raw, etc.)
- **Session wrapping** — `sm wrap last <N> as <verb>` captures recent successful commands into a reusable procedure
- **Semantic memory** — flat binary embedding store; verbs are embedded on write and searched at resolve time
- **Two-tier registry** — curated (trusted) and staging (inferred/unconfirmed); automatic promotion after 5 uses with confidence ≥ 0.70
- **Confidence decay** — staging entries not used in 30 days are flagged for review
- **Shell-agnostic hook** — works with Bash, Zsh, Fish, PowerShell, and Cmd; `sm init` prints the correct snippet for your shell
- **Model-free by default** — all four resolution stages degrade gracefully; model inference is opt-in via environment variable
- **Cross-platform executor** — 12 `StepKind` variants implemented in pure Rust with no `awk`/`python`/shell dependencies

---

## Commands

| Command | Description |
|---|---|
| `sm init` | Print the shell hook snippet to paste into your rc file |
| `sm add <verb> "<expansion>" [args...]` | Add a one-liner alias immediately to curated |
| `sm add <verb>` | Interactive alias wizard |
| `sm add proc <verb>` | Interactive procedure builder (8 step types) |
| `sm wrap last <N> as <verb>` | Wrap the last N successful commands into a procedure |
| `sm resolve <input>` | Resolve input and print the shell command (called by the hook) |
| `sm confirm <verb>` | Promote a staging entry to curated |
| `sm demote <verb>` | Move a curated entry back to staging |
| `sm list` | List curated entries |
| `sm list --staging` | List staging entries |
| `sm reindex` | Rebuild the embedding store from the current registry |
| `sm promote` | Run the promotion pass (decay, auto-promote, flag for review) |

---

## Setup

### Prerequisites

- Rust ≥ 1.80 (for modern dependency resolution; see [Build Notes](#build-notes) for older toolchains)
- A supported shell: Bash, Zsh, Fish, PowerShell, or Cmd

### Build and install

```bash
cargo build --release
cp target/release/sm ~/.local/bin/   # Linux/Mac
# Windows: copy target\release\sm.exe to a directory on your PATH
```

### Wire up the shell hook

```bash
sm init
```

Paste the printed snippet into your shell's rc file. For Bash/Zsh it looks like:

```bash
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

### Optional: model inference (Stage 4)

Set `OPENAI_API_KEY` (or equivalent for any OpenAI-compatible endpoint). The default model is `gpt-4o-mini`. No key = graceful passthrough; unknown inputs are passed to the shell unchanged.

### Logging

Set `SHELLMIND_LOG=debug` (or `info`, `warn`) to enable tracing output.

---

## Registry file format

Entries live in `~/.config/shellmind/registry/curated/` and `.../staging/` (Linux/Mac) or `%APPDATA%\shellmind\registry\` (Windows). Files are named `<verb>.toml` and contain JSON.

**Alias:**
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

Source trust levels: `declarative` → `inferred:<model>:<score>` → `confirmed:<score>` → `wrapped`.

---

## TODOs / Roadmap

| Priority | Feature | Notes |
|---|---|---|
| 1 | `sm init --write` | Auto-detect rc file and append hook, instead of printing |
| 2 | `redb` integration | Replace per-file JSON scan with O(1) key lookup |
| 3 | Real embeddings | Swap `embed_text_bow` (BoW hash) for `fastembed` (ONNX transformer) |
| 4 | `sm edit <verb>` | Open procedure in `$EDITOR` for in-place editing |
| 5 | Native StepKinds in `wrap` | Currently `wrap` only captures `ShellRaw`; should infer typed steps |
| 6 | Windows ConPTY testing | PowerShell hook path not yet end-to-end tested |
| 7 | `sm remove <verb>` | Delete entry from registry and embedding store |
| 8 | `sm rename <old> <new>` | Rename verb while preserving usage history |
| 9 | Scheduled procedures | Cron-like execution of registered procedures |
| 10 | Config file parser | Replace line-by-line model config parsing with `serde_json` |

---

## Architecture

```
src/
├── main.rs              Entry point, tokio runtime
├── platform/            OS abstraction (StepKind executor, path resolution, shell detection)
├── registry/            Dual-store command registry (curated + staging), promotion policy
├── embedder/            Flat binary embedding store, BoW hash embedder
├── session/             Ring buffer of resolved commands (last 200)
├── disambiguator/       Re-ranking and interactive disambiguation prompt
├── resolver/            4-stage resolution pipeline
├── model_client/        OpenAI-compatible inference client (curl subprocess)
├── editor/              Stub — $EDITOR integration (not yet implemented)
└── cli/                 All user-facing command handlers
```

See [REFERENCE.md](REFERENCE.md) for the full public API, resolution pipeline trace, binary format spec, and build notes.

---

## Build notes

**Pinned dependencies:** `Cargo.toml` uses exact `=` version pins targeting Rust 1.75. On Rust ≥ 1.80, remove the `=` prefixes and let Cargo resolve normally.

**Crates stubbed out for sandbox constraints** (add back on a modern toolchain):
- `reqwest` — replace the `curl` subprocess in `model_client`
- `shellexpand` — replace the manual `expand_env()` in `platform/mod.rs`
- `rustyline` — replace `std::io::stdin` readline in `cli/mod.rs` for history/completion
- `fastembed` — replace `embed_text_bow()` for production-quality semantic search
- `redb` — implement `registry/store.rs` for O(1) registry lookup

---

## License

MIT
