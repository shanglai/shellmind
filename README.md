# shellmind (`sm`)

A cross-platform personal automation kernel with semantic memory. `sm` sits in front of your shell as a resolution layer — translating natural and custom command vocabulary into real commands, and learning from use.

It is **not** a smarter shell. It is a layer that resolves what you mean into what your shell should run.

---

## How it works

When you type `sm <anything>`, the shell hook captures it and passes it through a 4-stage resolution pipeline:

1. **Exact match** — looks up your verb directly in the registry (curated-first)
2. **Prefix / fuzzy match** — starts-with, ends-with, or Levenshtein distance ≤ 2
3. **Semantic similarity** — embedding cosine search (≥ 0.72); BoW hash by default, MiniLM-L6-v2 via the optional `semantic-embeddings` feature
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
- **Shell-agnostic hook** — works with Bash, Zsh, Fish, PowerShell, and Cmd; `sm init [--write]` prints the correct snippet for your shell or appends it to your rc file directly
- **Model-free by default** — all four resolution stages degrade gracefully; model inference is opt-in via environment variable
- **Cross-platform executor** — 12 `StepKind` variants implemented in pure Rust with no `awk`/`python`/shell dependencies

---

## Commands

| Command | Description |
|---|---|
| `sm init` | Print the shell hook snippet to paste into your rc file |
| `sm init --write` | Detect your rc file and append the hook (idempotent) |
| `sm hook` | Print the hook snippet only |
| `sm add <verb> "<expansion>" [args...]` | Add a one-liner alias immediately to curated |
| `sm add <verb>` | Interactive alias wizard (offers `[e]dit` on collision) |
| `sm add proc <verb>` | Interactive procedure builder (12 step types) |
| `sm wrap last <N> as <verb>` | Wrap the last N successful commands into a procedure |
| `sm resolve <input>` | Resolve input and print the shell command (called by the hook) |
| `sm run <verb> [args...]` | Execute a verb directly, bypassing the shell hook |
| `sm edit <verb>` | Open the verb's definition in `$EDITOR`, re-index on save |
| `sm rename <old> <new>` | Rename a verb, preserving its definition, embedding, and history |
| `sm remove <verb>` | Delete a verb from the registry and embedding store |
| `sm confirm <verb>` | Promote a staging entry to curated |
| `sm demote <verb>` | Move a curated entry back to staging |
| `sm list [--detail] [--by-usage|--by-date] [pattern]` | List curated entries, with optional filter and sort |
| `sm list --staging` | List staging entries |
| `sm reindex` | Rebuild the redb cache and embedding store from TOML registry |
| `sm promote` | Run the promotion pass (decay, auto-promote, flag for review) |

---

## Setup

### Prerequisites

- Rust ≥ 1.80 (required by `redb` and `fastembed`; tested on 1.89)
- A supported shell: Bash, Zsh, Fish, PowerShell, or Cmd

### Build and install

```bash
cargo build --release
cp target/release/sm ~/.local/bin/   # Linux/Mac
# Windows: copy target\release\sm.exe to a directory on your PATH
```

### Wire up the shell hook

```bash
sm init           # prints the snippet for review
sm init --write   # detects your rc file and appends it (idempotent)
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

Completed in earlier passes: `sm init --write`, `redb` integration, optional `fastembed` (MiniLM-L6-v2) embeddings, `sm edit`, `sm remove`, `sm rename`, plus bonus `sm run` and shell-hook `__record` capture.

| Priority | Feature | Notes |
|---|---|---|
| 1 | Native `StepKind` inference in `wrap` | Currently `wrap` only captures `ShellRaw`; should detect curl/wget→`HttpCall`, cp→`FileCopy`, mv→`FileMove`, mkdir→`MkDir`, rm→`FileDelete`, export→`SetEnv`, echo→`Echo` |
| 2 | Windows ConPTY testing | PowerShell hook path not yet end-to-end tested on a real Windows shell |
| 3 | Scheduled procedures | Cron-like execution of registered procedures (new module) |
| 4 | Config file parser | Replace line-by-line model config parsing with `serde_json` / `toml` |
| 5 | Drop the rest of the version pins | `Cargo.toml` still has `=` pins from the Rust 1.75 sandbox era; modern toolchain doesn't need them |

---

## Architecture

```
src/
├── main.rs              Entry point, tokio runtime
├── platform/            OS abstraction (StepKind executor, path resolution, shell detection)
├── registry/            Dual-store command registry (curated + staging), promotion policy, redb cache
├── embedder/            Flat binary embedding store; BoW default, fastembed (MiniLM) optional
├── session/             Ring buffer of resolved + native commands (last 200)
├── disambiguator/       Re-ranking and interactive disambiguation prompt
├── resolver/            4-stage resolution pipeline (redb fast path when warm)
├── model_client/        OpenAI-compatible inference client (curl subprocess)
├── editor/              $EDITOR / $VISUAL integration, reload + re-embed on save
└── cli/                 All user-facing command handlers
```

See [REFERENCE.md](REFERENCE.md) for the full public API, resolution pipeline trace, binary format spec, and build notes.

---

## Build notes

**Pinned dependencies:** Most `Cargo.toml` entries still use exact `=` pins inherited from the Rust 1.75 sandbox era. They build cleanly on modern toolchains but the pins are noise — strip them at your leisure. `redb` and `fastembed` are already unpinned.

**Build modes:**
- `cargo build` — default; BoW embeddings (no ONNX download, fast compile)
- `cargo build --no-default-features --features semantic-embeddings` — pulls in `fastembed` and downloads MiniLM-L6-v2 on first run

**Crates still stubbed out** (add back when you want to upgrade those paths):
- `reqwest` — replace the `curl` subprocess in `model_client`
- `shellexpand` — replace the manual `expand_env()` in `platform/mod.rs`
- `rustyline` — replace `std::io::stdin` readline in `cli/mod.rs` for history/completion

---

## License

MIT
