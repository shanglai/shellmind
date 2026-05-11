//! CLI dispatch — routes `sm <subcommand>` to the right handler.
//!
//! Commands:
//!   sm hook                        Print shell hook for rc file
//!   sm resolve <input...>          Resolve input (called by shell hook)
//!   sm wrap last <N> as <verb>     Wrap last N successful commands
//!   sm confirm <verb>              Promote staging → curated
//!   sm demote <verb>               Curated → staging
//!   sm list [--staging]            List registry entries
//!   sm reindex                     Rebuild embedding store
//!   sm promote                     Run auto-promotion pass
//!   sm init                        First-time setup
//!   sm __exec <verb> [args...]     Internal: executor for procedures

use anyhow::{Context, Result};

use crate::embedder::{EmbeddingStore, embed_text_bow};
use crate::model_client::{ModelClient, ModelConfig};
use crate::platform::{Platform, executor::Executor, paths::ShellmindPaths, shell::ShellEnv};
use crate::registry::{CommandEntry, EntryKind, Registry};
use crate::registry::promotion::PromotionPolicy;
use crate::resolver::{Resolver, Resolution, EMBED_DIM};
use crate::session::{SessionBuffer, build_wrap_preview, finalize_wrap};

pub async fn dispatch(args: Vec<String>, paths: ShellmindPaths, shell_env: ShellEnv) -> Result<()> {
    let subcmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match subcmd {
        // ── Setup ────────────────────────────────────────────────────────────
        "init" => cmd_init(&paths, &shell_env),

        "hook" => {
            println!("{}", shell_env.hook_snippet());
            Ok(())
        }

        // ── Core: resolve ────────────────────────────────────────────────────
        "resolve" => {
            let input = args[2..].join(" ");
            if input.is_empty() { anyhow::bail!("Usage: sm resolve <input>"); }
            cmd_resolve(&input, &paths).await
        }

        // ── Internal: execute a procedure by verb ────────────────────────────
        "__exec" => {
            let verb = args.get(2).context("__exec requires verb")?;
            let slots: Vec<String> = args[3..].to_vec();
            cmd_exec(verb, &slots, &paths).await
        }

        // ── Wrap ─────────────────────────────────────────────────────────────
        "add" => cmd_add(&args[2..], &paths),
        "wrap" => cmd_wrap(&args[2..], &paths),

        // ── Registry management ───────────────────────────────────────────────
        "confirm" => {
            let verb = args.get(2).context("Usage: sm confirm <verb>")?;
            cmd_confirm(verb, &paths)
        }
        "demote" => {
            let verb = args.get(2).context("Usage: sm demote <verb>")?;
            cmd_demote(verb, &paths)
        }
        "list" => {
            let staging = args.get(2).map(|a| a == "--staging").unwrap_or(false);
            cmd_list(staging, &paths)
        }
        "reindex" => cmd_reindex(&paths),
        "promote" => cmd_promote(&paths),

        "help" | "--help" | "-h" => { print_help(); Ok(()) }
        _ => {
            // Unknown subcommand — try resolving it as a direct command
            let input = args[1..].join(" ");
            cmd_resolve(&input, &paths).await
        }
    }
}

// ── Subcommand handlers ───────────────────────────────────────────────────────

fn cmd_init(paths: &ShellmindPaths, shell_env: &ShellEnv) -> Result<()> {
    paths.ensure_dirs()?;
    println!("✓ shellmind initialized at {}", paths.config_dir.display());
    println!("\nAdd this to your shell rc file:\n");
    println!("{}", shell_env.hook_snippet());
    println!("Then reload your shell or run: source ~/.bashrc  (or equivalent)");
    Ok(())
}

async fn cmd_resolve(input: &str, paths: &ShellmindPaths) -> Result<()> {
    // Load session first — disambiguator needs recent history for context
    let mut session = SessionBuffer::load_or_create(&paths.session_history)?;
    let recent: Vec<&crate::session::SessionEntry> = session.last_successful(10);

    let mut resolver = build_resolver(paths)?;
    let resolution = resolver.resolve(input, &recent).await?;

    match &resolution {
        Resolution::Exact { entry, expanded } => {
            tracing::info!("[exact] {} → {}", entry.verb, expanded);
            println!("{}", expanded);
            let _ = resolver.record_usage(&entry.verb);
        }
        Resolution::Similar { entry, expanded, score } => {
            tracing::info!("[similar:{:.2}] {} → {}", score, entry.verb, expanded);
            println!("{}", expanded);
            let _ = resolver.record_usage(&entry.verb);
        }
        Resolution::Disambiguated { entry, expanded, score, from_n } => {
            tracing::info!("[disambig:{:.2} from {}] {} → {}", score, from_n, entry.verb, expanded);
            println!("{}", expanded);
            let _ = resolver.record_usage(&entry.verb);
        }
        Resolution::Inferred { expanded, confidence, stored_as } => {
            eprintln!("~[inferred:{:.0}%] stored as '{}' in staging — run `sm confirm {}` to promote",
                confidence * 100.0, stored_as, stored_as);
            println!("{}", expanded);
        }
        Resolution::Passthrough { raw } => {
            println!("{}", raw);
        }
    }

    session.record(input, resolution.as_shell_str(), 0, None);
    let _ = session.save();

    Ok(())
}

async fn cmd_exec(verb: &str, slots: &[String], paths: &ShellmindPaths) -> Result<()> {
    let registry = build_registry(paths);
    let mut all = registry.load_all_from_dir(&registry.curated_dir)?;
    all.extend(registry.load_all_from_dir(&registry.staging_dir)?);

    let entry = all.iter()
        .find(|e| e.verb == verb)
        .with_context(|| format!("No entry found for verb '{}'", verb))?;

    match &entry.kind {
        EntryKind::Procedure { steps, .. } => {
            let step_kinds: Vec<_> = steps.iter().map(|s| s.step.clone()).collect();
            let mut exec = Executor::new(Platform::current());
            let results = exec.run_steps(&step_kinds, slots, false).await?;
            let failed = results.iter().filter(|r| !r.success).count();
            if failed > 0 {
                eprintln!("{}/{} steps failed", failed, results.len());
            }
        }
        EntryKind::Alias { expansion, arg_names } => {
            // Shouldn't reach here normally, but handle gracefully
            let mut cmd = expansion.clone();
            for (i, name) in arg_names.iter().enumerate() {
                if let Some(val) = slots.get(i) {
                    cmd = cmd.replace(&format!("{{{}}}", name), val);
                }
            }
            let status = std::process::Command::new("sh")
                .arg("-c").arg(&cmd).status()?;
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
    }
    Ok(())
}

fn cmd_wrap(rest: &[String], paths: &ShellmindPaths) -> Result<()> {
    // Syntax: wrap last <N> as <verb> [label1 label2 ...]
    // Example: wrap last 4 as same_as_yesterday file api_url
    if rest.len() < 4 || rest[0] != "last" || rest[2] != "as" {
        anyhow::bail!("Usage: sm wrap last <N> as <verb> [arg_labels...]");
    }
    let n: usize = rest[1].parse().context("N must be a number")?;
    let verb = &rest[3];
    let user_labels: Vec<String> = rest[4..].to_vec();

    let session = SessionBuffer::load_or_create(&paths.session_history)?;
    let candidates = session.last_successful(n);

    if candidates.is_empty() {
        anyhow::bail!("No successful commands in history to wrap");
    }
    if candidates.len() < n {
        eprintln!("Warning: only {} successful commands in history (requested {})", candidates.len(), n);
    }

    let preview = build_wrap_preview(&candidates);

    // Show preview
    println!("\n  Wrapping {} steps as '{}':\n", candidates.len(), verb);
    for step in &preview.steps {
        println!("  [{}] {}", step.index + 1, step.description);
        println!("       > {}", step.raw);
    }

    if !preview.slot_candidates.is_empty() {
        println!("\n  Detected slot candidates:");
        for sc in &preview.slot_candidates {
            let steps_str: Vec<String> = sc.appears_in_steps.iter().map(|i| (i+1).to_string()).collect();
            println!("  {} {} → used in step(s) {}", sc.slot_label, sc.token, steps_str.join(","));
        }
    }

    // Build slot assignments: user_labels override auto-detected
    let slot_assignments: Vec<(String, String)> = if !user_labels.is_empty() {
        preview.slot_candidates.iter().zip(user_labels.iter())
            .map(|(sc, label)| (sc.token.clone(), label.clone()))
            .collect()
    } else {
        preview.slot_candidates.iter()
            .map(|sc| (sc.token.clone(), sc.slot_label.clone()))
            .collect()
    };

    print!("\n  Save as '{}'? [y/N] ", verb);
    use std::io::{BufRead, Write};
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;

    if !line.trim().eq_ignore_ascii_case("y") {
        println!("  Cancelled.");
        return Ok(());
    }

    let entry = finalize_wrap(verb, &candidates, &slot_assignments);
    let registry = build_registry(paths);
    registry.write_toml(&entry)?;

    // Add to embedding store
    let embed_text = format!("{} {}", verb, slot_assignments.iter()
        .map(|(_, l)| l.as_str()).collect::<Vec<_>>().join(" "));
    let vec = embed_text_bow(&embed_text, EMBED_DIM);
    let mut store = EmbeddingStore::load_or_create(&paths.embeddings_bin, EMBED_DIM)?;
    store.upsert(verb, crate::embedder::EmbeddingSource::Wrapped, vec);
    store.save()?;

    println!("  ✓ Saved '{}' to staging. Run `sm confirm {}` to promote to curated.", verb, verb);
    Ok(())
}

fn cmd_confirm(verb: &str, paths: &ShellmindPaths) -> Result<()> {
    let registry = build_registry(paths);
    if registry.confirm(verb)? {
        println!("✓ '{}' promoted to curated.", verb);
    } else {
        println!("'{}' not found in staging.", verb);
    }
    Ok(())
}

fn cmd_demote(verb: &str, paths: &ShellmindPaths) -> Result<()> {
    let registry = build_registry(paths);
    if registry.demote(verb)? {
        println!("✓ '{}' moved back to staging.", verb);
    } else {
        println!("'{}' not found in curated.", verb);
    }
    Ok(())
}

fn cmd_list(staging: bool, paths: &ShellmindPaths) -> Result<()> {
    let registry = build_registry(paths);
    let dir = if staging { &registry.staging_dir } else { &registry.curated_dir };
    let entries = registry.load_all_from_dir(dir)?;

    if entries.is_empty() {
        println!("No entries in {} registry.", if staging { "staging" } else { "curated" });
        return Ok(());
    }

    let label = if staging { "STAGING" } else { "CURATED" };
    println!("\n  {} ({} entries)\n", label, entries.len());
    println!("  {:<24} {:<12} {:<8} {}", "VERB", "KIND", "USES", "CONFIDENCE");
    println!("  {}", "-".repeat(60));

    for e in &entries {
        let kind = match &e.kind {
            EntryKind::Alias { .. } => "alias",
            EntryKind::Procedure { .. } => if e.is_wrapped() { "proc:wr" } else { "proc" },
        };
        println!("  {:<24} {:<12} {:<8} {:.0}%",
            e.verb, kind, e.usage_count, e.confidence * 100.0);
    }
    println!();
    Ok(())
}

fn cmd_reindex(paths: &ShellmindPaths) -> Result<()> {
    let mut resolver = build_resolver(paths)?;
    resolver.refresh()?;
    let count = resolver.reindex()?;
    println!("✓ Reindexed {} entries.", count);
    Ok(())
}

fn cmd_promote(paths: &ShellmindPaths) -> Result<()> {
    let registry = build_registry(paths);
    let policy = PromotionPolicy::default();
    let results = registry.run_promotion_pass(&policy)?;

    let promoted = results.iter().filter(|r| matches!(r.action, crate::registry::promotion::PromotionAction::Promoted)).count();
    let decayed = results.iter().filter(|r| matches!(r.action, crate::registry::promotion::PromotionAction::Decayed { .. })).count();
    let flagged = results.iter().filter(|r| matches!(r.action, crate::registry::promotion::PromotionAction::FlaggedForReview { .. })).count();

    println!("Promotion pass complete:");
    println!("  Promoted : {}", promoted);
    println!("  Decayed  : {}", decayed);
    println!("  Flagged  : {}", flagged);
    Ok(())
}

// ── Builders ─────────────────────────────────────────────────────────────────

fn build_registry(paths: &ShellmindPaths) -> Registry {
    Registry::new(
        paths.curated_toml_dir.clone(),
        paths.staging_toml_dir.clone(),
    )
}

fn build_resolver(paths: &ShellmindPaths) -> Result<Resolver> {
    let registry = build_registry(paths);
    let embeddings = EmbeddingStore::load_or_create(&paths.embeddings_bin, EMBED_DIM)?;
    let model_config = load_model_config(paths);
    let model = ModelClient::new(model_config);
    Ok(Resolver::new(registry, embeddings, model))
}

fn load_model_config(paths: &ShellmindPaths) -> ModelConfig {
    // Try to load from shellmind.toml [model] section
    // Fall back to defaults — key comes from env var anyway
    let config_path = paths.user_config.clone();
    if config_path.exists() {
        if let Ok(raw) = std::fs::read_to_string(&config_path) {
            // Minimal parse: look for model_name = "..." line
            let mut config = ModelConfig::default();
            for line in raw.lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("model_name = ") {
                    config.model_name = rest.trim_matches('"').to_string();
                } else if let Some(rest) = line.strip_prefix("api_base = ") {
                    config.api_base = rest.trim_matches('"').to_string();
                } else if let Some(rest) = line.strip_prefix("api_key_env = ") {
                    config.api_key_env = rest.trim_matches('"').to_string();
                }
            }
            return config;
        }
    }
    ModelConfig::default()
}

// ── Help ──────────────────────────────────────────────────────────────────────

fn print_help() {
    println!(r#"
shellmind (sm) — cross-platform personal automation kernel

USAGE
  sm <command> [args]

COMMANDS
  init                     First-time setup, prints shell hook
  hook                     Print shell hook snippet for your rc file

  resolve <input>          Resolve a command (called by shell hook)
                           4-stage: exact → prefix → similarity → model

  add <verb> "<expansion>" [args...]
                           Add an alias directly to curated
  add proc <verb>          Interactive procedure builder (step by step)
  add <verb>               Show existing entry or start alias wizard

  wrap last <N> as <verb>  Wrap last N successful commands into a procedure
    [label1 label2 ...]    Optional: label the detected argument slots

  confirm <verb>           Promote a staging entry to curated
  demote  <verb>           Move a curated entry back to staging
  list [--staging]         List curated (default) or staging entries
  reindex                  Rebuild embedding store from registry
  promote                  Run automatic promotion/decay pass

REGISTRY FILES
  Curated  ~/.config/shellmind/registry/curated/   (TOML, hand-editable)
  Staging  ~/.config/shellmind/registry/staging/   (TOML, auto-managed)

CONFIG
  ~/.config/shellmind/shellmind.toml
  
  model_name  = "gpt-4o-mini"
  api_base    = "https://api.openai.com/v1"
  api_key_env = "OPENAI_API_KEY"

ENVIRONMENT
  SHELLMIND_LOG    Log level: error|warn|info|debug|trace
  OPENAI_API_KEY   (or whatever api_key_env is set to)

RESOLUTION PIPELINE
  1. Exact verb match      (curated priority)
  2. Prefix / Levenshtein  (typo tolerance, abbreviations)
  3. Embedding similarity  (BOW cosine, :wr: boosted)
  4. Model inference       (few-shot, result → staging)
"#);
}

// ── sm add ────────────────────────────────────────────────────────────────────
//
// Syntax:
//   sm add <verb> "<expansion>" [arg1 arg2 ...]   → alias, goes to curated
//   sm add proc <verb>                              → interactive procedure builder
//   sm add <verb>                                   → show existing or start wizard
//
// All new entries land in curated directly (user is explicitly authoring them).
// Embedding is built immediately. No staging step needed for declarative authoring.

fn cmd_add(rest: &[String], paths: &ShellmindPaths) -> Result<()> {
    use std::io::{BufRead, Write};

    // ── Mode: procedure builder ───────────────────────────────────────────────
    if rest.first().map(|s| s.as_str()) == Some("proc") {
        let verb = rest.get(1)
            .context("Usage: sm add proc <verb>")?
            .clone();
        return add_procedure_interactive(&verb, paths);
    }

    // ── Need at least a verb ──────────────────────────────────────────────────
    let verb = rest.first()
        .context("Usage: sm add <verb> \"<expansion>\" [arg1 arg2 ...]\n       sm add proc <verb>")?
        .clone();

    let registry = build_registry(paths);

    // ── Collision check ───────────────────────────────────────────────────────
    let curated_path = registry.curated_dir.join(format!("{}.toml", verb));
    let staging_path = registry.staging_dir.join(format!("{}.toml", verb));
    let existing_path = if curated_path.exists() { Some((&curated_path, "curated")) }
                        else if staging_path.exists() { Some((&staging_path, "staging")) }
                        else { None };

    if let Some((path, store)) = existing_path {
        // Show what exists
        let existing = registry.load_all_from_dir(
            if store == "curated" { &registry.curated_dir } else { &registry.staging_dir }
        )?;
        if let Some(e) = existing.iter().find(|e| e.verb == verb) {
            println!("\n  \x1b[33m!\x1b[0m '{}' already exists in {}:", verb, store);
            print_entry_detail(e);
        }

        // If no expansion given, just show and exit
        if rest.len() < 2 {
            println!("\n  Run `sm add {} \"<new expansion>\" [args...]` to replace.", verb);
            println!("  Run `sm edit {}` to open in editor.", verb);
            return Ok(());
        }

        // Replacement prompt
        print!("\n  Replace? [y/N] ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if !line.trim().eq_ignore_ascii_case("y") {
            println!("  Cancelled.");
            return Ok(());
        }
    } else if rest.len() < 2 {
        // No expansion and no existing entry — start alias wizard
        return add_alias_interactive(&verb, paths);
    }

    // ── Parse expansion + arg names from args ─────────────────────────────────
    let expansion = rest.get(1)
        .context("Provide an expansion string")?
        .clone();
    let arg_names: Vec<String> = rest[2..].to_vec();

    // Validate: check that all named slots in expansion have a corresponding arg_name
    let missing = find_missing_args(&expansion, &arg_names);
    if !missing.is_empty() {
        eprintln!("\n  \x1b[33mWarning:\x1b[0m expansion contains slots not listed as args: {}",
            missing.join(", "));
        eprintln!("  Add them after the expansion string, or they won't be bound.");
        print!("  Continue anyway? [y/N] ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if !line.trim().eq_ignore_ascii_case("y") {
            println!("  Cancelled.");
            return Ok(());
        }
    }

    // ── Build and store entry ─────────────────────────────────────────────────
    let entry = build_alias_entry(&verb, &expansion, arg_names);
    write_and_index(&entry, paths)?;

    println!("\n  \x1b[32m✓\x1b[0m Added '{}' to curated.", verb);
    print_entry_detail(&entry);
    Ok(())
}

// ── Interactive alias wizard (no expansion given) ─────────────────────────────

fn add_alias_interactive(verb: &str, paths: &ShellmindPaths) -> Result<()> {
    use std::io::{BufRead, Write};
    let stdout = std::io::stdout();
    let stdin = std::io::stdin();

    println!("\n  Adding alias '{}'\n", verb);

    // Expansion
    print!("  Expansion (shell command, use {{arg}} for slots): ");
    stdout.lock().flush()?;
    let mut expansion = String::new();
    stdin.lock().read_line(&mut expansion)?;
    let expansion = expansion.trim().to_string();
    if expansion.is_empty() { println!("  Cancelled."); return Ok(()); }

    // Detect slots from expansion, ask to label them
    let detected = detect_slots_in_expansion(&expansion);
    let arg_names: Vec<String> = if detected.is_empty() {
        vec![]
    } else {
        println!("  Detected slots: {}", detected.join(", "));
        print!("  Arg names in order (Enter to keep as-is): ");
        stdout.lock().flush()?;
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            detected
        } else {
            trimmed.split_whitespace().map(|s| s.to_string()).collect()
        }
    };

    let entry = build_alias_entry(verb, &expansion, arg_names);
    write_and_index(&entry, paths)?;
    println!("\n  \x1b[32m✓\x1b[0m Added '{}' to curated.", verb);
    print_entry_detail(&entry);
    Ok(())
}

// ── Interactive procedure builder ─────────────────────────────────────────────

fn add_procedure_interactive(verb: &str, paths: &ShellmindPaths) -> Result<()> {
    use std::io::{BufRead, Write};
    use crate::platform::{Arg, StepKind};
    use crate::platform::HttpMethod;
    use crate::registry::{ArgBind, ProcStep};

    let stdout = std::io::stdout();
    let stdin  = std::io::stdin();

    println!("\n  Building procedure '{}'\n", verb);
    println!("  Step types:");
    println!("    [1] shell    — raw shell command");
    println!("    [2] copy     — copy file/dir");
    println!("    [3] move     — move file/dir");
    println!("    [4] mkdir    — create directory");
    println!("    [5] sample   — sample rows from CSV");
    println!("    [6] join     — join two CSV files on a key");
    println!("    [7] http     — HTTP call (GET/POST/…)");
    println!("    [8] echo     — print a message");
    println!("    [done]       — finish\n");

    let mut steps: Vec<ProcStep> = vec![];
    let mut all_slots: Vec<String> = vec![];  // collect slot labels across steps

    loop {
        let step_n = steps.len() + 1;
        print!("  Step {} type [1-8 or done]: ", step_n);
        stdout.lock().flush()?;
        let mut choice = String::new();
        stdin.lock().read_line(&mut choice)?;
        let choice = choice.trim().to_lowercase();

        if choice == "done" || choice == "d" || choice.is_empty() {
            if steps.is_empty() { println!("  No steps added — cancelled."); return Ok(()); }
            break;
        }

        let kind_opt: Option<StepKind> = match choice.as_str() {
            "1" | "shell" => {
                let cmd = prompt_field("  Command", true)?;
                Some(StepKind::ShellRaw { cmd, platform: crate::platform::Platform::current() })
            }
            "2" | "copy" => {
                let src = prompt_field("  Source path/slot", true)?;
                let dst = prompt_field("  Dest path/slot", true)?;
                Some(StepKind::FileCopy { src: parse_arg(&src, &all_slots), dst: parse_arg(&dst, &all_slots) })
            }
            "3" | "move" => {
                let src = prompt_field("  Source path/slot", true)?;
                let dst = prompt_field("  Dest path/slot", true)?;
                Some(StepKind::FileMove { src: parse_arg(&src, &all_slots), dst: parse_arg(&dst, &all_slots) })
            }
            "4" | "mkdir" => {
                let path = prompt_field("  Directory path/slot", true)?;
                Some(StepKind::MkDir { path: parse_arg(&path, &all_slots) })
            }
            "5" | "sample" => {
                let file    = prompt_field("  Input file/slot", true)?;
                let pct_str = prompt_field("  Sample % (e.g. 10)", true)?;
                let pct: f32 = pct_str.trim_end_matches('%').parse().unwrap_or(10.0) / 100.0;
                let strat   = prompt_field("  Stratify column (Enter to skip)", false)?;
                let output  = prompt_field("  Output file/slot", true)?;
                let stratified = !strat.is_empty();
                Some(StepKind::DataSample {
                    file:       parse_arg(&file, &all_slots),
                    pct,
                    stratified,
                    strat_col:  if stratified { Some(strat) } else { None },
                    output:     parse_arg(&output, &all_slots),
                })
            }
            "6" | "join" => {
                let left   = prompt_field("  Left file/slot", true)?;
                let right  = prompt_field("  Right file/slot", true)?;
                let on     = prompt_field("  Join key column", true)?;
                let output = prompt_field("  Output file/slot", true)?;
                Some(StepKind::DataJoin {
                    left:   parse_arg(&left, &all_slots),
                    right:  parse_arg(&right, &all_slots),
                    on,
                    output: parse_arg(&output, &all_slots),
                })
            }
            "7" | "http" => {
                let url    = prompt_field("  URL/slot", true)?;
                let method = prompt_field("  Method [GET/POST/PUT/PATCH/DELETE]", true)?;
                let body   = prompt_field("  Body file/slot (Enter to skip)", false)?;
                let output = prompt_field("  Save response to file/slot (Enter to skip)", false)?;
                let method = match method.to_uppercase().as_str() {
                    "POST"   => HttpMethod::Post,
                    "PUT"    => HttpMethod::Put,
                    "PATCH"  => HttpMethod::Patch,
                    "DELETE" => HttpMethod::Delete,
                    _        => HttpMethod::Get,
                };
                Some(StepKind::HttpCall {
                    url:     parse_arg(&url, &all_slots),
                    method,
                    body:    if body.is_empty() { None } else { Some(parse_arg(&body, &all_slots)) },
                    headers: vec![],
                    output:  if output.is_empty() { None } else { Some(parse_arg(&output, &all_slots)) },
                })
            }
            "8" | "echo" => {
                let msg = prompt_field("  Message/slot", true)?;
                Some(StepKind::Echo { message: parse_arg(&msg, &all_slots) })
            }
            _ => { println!("  Unknown choice — try 1-8 or 'done'"); None }
        };

        let Some(kind) = kind_opt else { continue; };

        // Collect new slot labels introduced by this step
        let new_slots = extract_slots_from_step(&kind);
        for s in &new_slots {
            if !all_slots.contains(s) { all_slots.push(s.clone()); }
        }

        let desc = prompt_field("  Step description (optional)", false)?;

        steps.push(ProcStep {
            index: steps.len(),
            step: kind,
            description: if desc.is_empty() { None } else { Some(desc) },
            depends_on: if steps.is_empty() { vec![] } else { vec![steps.len() - 1] },
        });

        println!("  \x1b[32m✓\x1b[0m Step {} added. ({} total)\n", step_n, steps.len());
    }

    // Build ArgBind list from collected slots
    let arg_bindings: Vec<ArgBind> = all_slots.iter().enumerate().map(|(i, label)| {
        ArgBind {
            call_position: i,
            label: label.clone(),
            step_index: 0,
            placeholder: format!("{{{}}}", label),
        }
    }).collect();

    let description = prompt_field("  Procedure description (optional)", false)?;

    let entry = CommandEntry {
        id: uuid::Uuid::new_v4(),
        verb: verb.to_string(),
        kind: crate::registry::EntryKind::Procedure {
            steps,
            arg_bindings,
            description: if description.is_empty() { None } else { Some(description) },
        },
        tags: vec![],
        source: crate::registry::EntrySource::Declarative,
        usage_count: 0,
        created_at: chrono::Utc::now(),
        last_used: None,
        confidence: 1.0,
        embedding: None,
    };

    write_and_index(&entry, paths)?;
    println!("\n  \x1b[32m✓\x1b[0m Procedure '{}' added to curated ({} steps).", verb, entry.is_procedure() as u8);
    print_entry_detail(&entry);
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn prompt_field(label: &str, required: bool) -> Result<String> {
    use std::io::{BufRead, Write};
    loop {
        print!("{}: ", label);
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        let val = line.trim().to_string();
        if val.is_empty() && required {
            println!("  (required)");
        } else {
            return Ok(val);
        }
    }
}

/// Parse a field value as Slot if it matches $N or {label}, otherwise Literal.
fn parse_arg(s: &str, known_slots: &[String]) -> crate::platform::Arg {
    let s = s.trim();
    // $1, $2, … positional
    if let Some(rest) = s.strip_prefix('$') {
        if let Ok(n) = rest.parse::<usize>() {
            return crate::platform::Arg::slot(n.saturating_sub(1));
        }
    }
    // {label} named slot
    if s.starts_with('{') && s.ends_with('}') {
        let label = &s[1..s.len()-1];
        let idx = known_slots.iter().position(|sl| sl == label).unwrap_or(known_slots.len());
        return crate::platform::Arg::slot(idx);
    }
    crate::platform::Arg::literal(s)
}

fn detect_slots_in_expansion(expansion: &str) -> Vec<String> {
    let mut slots = vec![];
    let mut chars = expansion.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            let mut name = String::new();
            for c2 in chars.by_ref() {
                if c2 == '}' { break; }
                name.push(c2);
            }
            if !name.is_empty() && !slots.contains(&name) {
                slots.push(name);
            }
        }
    }
    slots
}

fn find_missing_args(expansion: &str, declared: &[String]) -> Vec<String> {
    detect_slots_in_expansion(expansion)
        .into_iter()
        .filter(|s| !declared.contains(s))
        .collect()
}

fn extract_slots_from_step(step: &crate::platform::StepKind) -> Vec<String> {
    // Collect Slot-type Arg labels from a step for display/binding purposes
    // We use a naming convention: slots are returned as their index as string
    // Real label recovery requires the full ArgBind machinery; this is a best-effort
    vec![] // populated during interactive builder via prompt flow
}

fn build_alias_entry(verb: &str, expansion: &str, arg_names: Vec<String>) -> CommandEntry {
    CommandEntry {
        id: uuid::Uuid::new_v4(),
        verb: verb.to_string(),
        kind: EntryKind::Alias { expansion: expansion.to_string(), arg_names },
        tags: vec![],
        source: crate::registry::EntrySource::Declarative,
        usage_count: 0,
        created_at: chrono::Utc::now(),
        last_used: None,
        confidence: 1.0,
        embedding: None,
    }
}

fn write_and_index(entry: &CommandEntry, paths: &ShellmindPaths) -> Result<()> {
    let registry = build_registry(paths);
    registry.write_toml(entry)?;

    // Build embedding immediately
    let text = match &entry.kind {
        EntryKind::Alias { expansion, arg_names } =>
            format!("{} {} {}", entry.verb, arg_names.join(" "), expansion),
        EntryKind::Procedure { steps, description, .. } =>
            format!("{} {}", entry.verb, description.as_deref().unwrap_or("")),
    };
    let vec = embed_text_bow(&text, EMBED_DIM);
    let source = if entry.is_wrapped() {
        crate::embedder::EmbeddingSource::Wrapped
    } else {
        crate::embedder::EmbeddingSource::Curated
    };
    let mut store = EmbeddingStore::load_or_create(&paths.embeddings_bin, EMBED_DIM)?;
    store.upsert(&entry.verb, source, vec);
    store.save()?;
    Ok(())
}

fn print_entry_detail(entry: &CommandEntry) {
    println!();
    match &entry.kind {
        EntryKind::Alias { expansion, arg_names } => {
            println!("  \x1b[90mverb    :\x1b[0m {}", entry.verb);
            println!("  \x1b[90mexpands :\x1b[0m {}", expansion);
            if !arg_names.is_empty() {
                println!("  \x1b[90margs    :\x1b[0m {}", arg_names.join(", "));
            }
        }
        EntryKind::Procedure { steps, arg_bindings, description } => {
            println!("  \x1b[90mverb    :\x1b[0m {}", entry.verb);
            if let Some(d) = description {
                println!("  \x1b[90mdesc    :\x1b[0m {}", d);
            }
            println!("  \x1b[90msteps   :\x1b[0m {}", steps.len());
            for s in steps {
                let label = s.description.as_deref().unwrap_or("(step)");
                println!("    [{}] {}", s.index + 1, label);
            }
            if !arg_bindings.is_empty() {
                let labels: Vec<&str> = arg_bindings.iter().map(|b| b.label.as_str()).collect();
                println!("  \x1b[90margs    :\x1b[0m {}", labels.join(", "));
            }
        }
    }
    println!("  \x1b[90msource  :\x1b[0m {:?}", entry.source);
    println!("  \x1b[90mconfid. :\x1b[0m {:.0}%", entry.confidence * 100.0);
    println!();
}
