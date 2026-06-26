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

use crate::embedder::{EmbeddingStore, embed_text};
use crate::model_client::{ModelClient, ModelConfig};
use crate::platform::{Platform, executor::Executor, paths::ShellmindPaths, shell::ShellEnv};
use crate::registry::{CommandEntry, EntryKind, Registry};
use crate::registry::promotion::PromotionPolicy;
use crate::registry::store::RegistryStore;
use crate::embedder::EMBED_DIM;
use crate::resolver::{Resolver, Resolution};
use crate::scheduler::{Schedule, ScheduleStore};
use crate::session::{SessionBuffer, build_wrap_preview, finalize_wrap};

pub async fn dispatch(args: Vec<String>, paths: ShellmindPaths, shell_env: ShellEnv) -> Result<()> {
    let subcmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match subcmd {
        // ── Setup ────────────────────────────────────────────────────────────
        "init" => cmd_init(&args[2..], &paths, &shell_env),

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
        "list" => cmd_list(&args[2..], &paths),
        "reindex" => cmd_reindex(&paths),
        "promote" => cmd_promote(&paths),
        "remove" => {
            let verb = args.get(2).context("Usage: sm remove <verb>")?;
            cmd_remove(verb, &paths)
        }
        "rename" => {
            let old = args.get(2).context("Usage: sm rename <old> <new>")?;
            let new = args.get(3).context("Usage: sm rename <old> <new>")?;
            cmd_rename(old, new, &paths)
        }
        "edit" => {
            let verb = args.get(2).context("Usage: sm edit <verb>")?;
            cmd_edit(verb, &paths)
        }
        "run" => {
            let verb = args.get(2).context("Usage: sm run <verb> [args...]")?;
            let slots: Vec<String> = args[3..].to_vec();
            cmd_run(verb, &slots, &paths).await
        }
        "schedule" => cmd_schedule(&args[2..], &paths).await,
        "a.n" | "agennect" => crate::agennect::dispatch(&args[2..], &paths).await,
        "__record" => {
            // Called by the shell PROMPT_COMMAND hook to capture native commands
            let cmd = args[2..].join(" ");
            if !cmd.is_empty() {
                let mut session = SessionBuffer::load_or_create(&paths.session_history)?;
                // Skip if identical to last recorded entry (guards against double PROMPT_COMMAND)
                let is_dup = session.entries.back()
                    .map(|e| e.raw_input == cmd)
                    .unwrap_or(false);
                if !is_dup {
                    session.record(&cmd, &cmd, 0, None);
                    let _ = session.save();
                }
            }
            Ok(())
        }

        "help" | "--help" | "-h" => { print_help(); Ok(()) }
        _ => {
            // Unknown subcommand — try resolving it as a direct command
            let input = args[1..].join(" ");
            cmd_resolve(&input, &paths).await
        }
    }
}

// ── Subcommand handlers ───────────────────────────────────────────────────────

fn cmd_init(rest: &[String], paths: &ShellmindPaths, shell_env: &ShellEnv) -> Result<()> {
    use crate::platform::shell::ShellKind;

    paths.ensure_dirs()?;
    eprintln!("\x1b[32m✓\x1b[0m shellmind initialized at \x1b[90m{}\x1b[0m", paths.config_dir.display());

    let write_flag = rest.iter().any(|a| a == "--write");
    let snippet = shell_env.hook_snippet();

    // Locate rc file: shell-derived default, with stdin fallback if shell is unknown
    let rc_path = match default_rc_path(&shell_env.shell_kind) {
        Some(p) => p,
        None => {
            eprintln!("\x1b[33m!\x1b[0m Could not auto-detect your shell rc file.");
            eprintln!("  Detected shell: {:?}", shell_env.shell_kind);
            eprint!("  Enter rc file path (or blank to skip): ");
            std::io::Write::flush(&mut std::io::stderr()).ok();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).ok();
            let trimmed = line.trim();
            if trimmed.is_empty() {
                eprintln!("\nAdd this to your shell rc file manually:\n");
                eprintln!("{}", snippet);
                return Ok(());
            }
            std::path::PathBuf::from(shellexpand_tilde(trimmed))
        }
    };

    // Check whether hook already installed
    let existing = std::fs::read_to_string(&rc_path).unwrap_or_default();
    if existing.contains("shellmind hook") || existing.contains("sm resolve") {
        eprintln!("\x1b[32m✓\x1b[0m Hook already installed in \x1b[90m{}\x1b[0m", rc_path.display());
        return Ok(());
    }

    if !write_flag {
        eprintln!("\nDetected rc file: \x1b[90m{}\x1b[0m", rc_path.display());
        eprintln!("Add this snippet to it (or rerun with \x1b[33m--write\x1b[0m to append automatically):\n");
        eprintln!("{}", snippet);
        return Ok(());
    }

    // Append with a comment header for traceability
    if let Some(parent) = rc_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create parent directory: {}", parent.display()))?;
    }
    let header = format!(
        "\n# ── shellmind hook (added {}) ──\n",
        chrono::Utc::now().format("%Y-%m-%d")
    );
    let mut to_append = String::with_capacity(header.len() + snippet.len() + 1);
    to_append.push_str(&header);
    to_append.push_str(&snippet);
    if !to_append.ends_with('\n') { to_append.push('\n'); }

    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&rc_path)
        .with_context(|| format!("Failed to open rc file for append: {}", rc_path.display()))?;
    file.write_all(to_append.as_bytes())
        .with_context(|| format!("Failed to write to rc file: {}", rc_path.display()))?;

    eprintln!("\x1b[32m✓\x1b[0m Hook appended to \x1b[90m{}\x1b[0m", rc_path.display());
    let reload_hint = match shell_env.shell_kind {
        ShellKind::Bash => format!("source {}", rc_path.display()),
        ShellKind::Zsh  => format!("source {}", rc_path.display()),
        ShellKind::Fish => format!("source {}", rc_path.display()),
        ShellKind::PowerShell => format!(". \"{}\"", rc_path.display()),
        _ => format!("reload your shell (or source {})", rc_path.display()),
    };
    eprintln!("  Reload with: \x1b[33m{}\x1b[0m", reload_hint);
    Ok(())
}

/// Default rc file path per shell. Returns None when the shell is unrecognised
/// or when we cannot resolve $HOME / $PROFILE — caller falls back to prompting.
fn default_rc_path(shell: &crate::platform::shell::ShellKind) -> Option<std::path::PathBuf> {
    use crate::platform::shell::ShellKind;
    let home = dirs::home_dir()?;
    match shell {
        ShellKind::Bash => Some(home.join(".bashrc")),
        ShellKind::Zsh  => Some(home.join(".zshrc")),
        ShellKind::Fish => Some(home.join(".config").join("fish").join("config.fish")),
        ShellKind::PowerShell => powershell_profile_path().or_else(|| {
            // Conventional fallback: prefer PowerShell 7+ location if the
            // directory exists; otherwise the Windows PowerShell 5.x path
            let ps7 = home.join("Documents").join("PowerShell").join("Microsoft.PowerShell_profile.ps1");
            if ps7.parent().map(|p| p.exists()).unwrap_or(false) {
                Some(ps7)
            } else {
                Some(home.join("Documents").join("WindowsPowerShell").join("Microsoft.PowerShell_profile.ps1"))
            }
        }),
        ShellKind::Cmd | ShellKind::Unknown(_) => None,
    }
}

/// Query the actual `$PROFILE` path from PowerShell. Tries `pwsh.exe`
/// (PowerShell 7+) first and falls back to `powershell.exe` (Windows
/// PowerShell 5.x). Returns None if neither is on PATH or both queries fail.
fn powershell_profile_path() -> Option<std::path::PathBuf> {
    for exe in ["pwsh.exe", "powershell.exe"] {
        let Ok(output) = std::process::Command::new(exe)
            .args(["-NoProfile", "-Command", "$PROFILE"])
            .output()
        else { continue };
        if !output.status.success() { continue; }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(std::path::PathBuf::from(path));
        }
    }
    None
}

/// Minimal tilde expansion for the rc-path prompt — handles only a leading "~/".
fn shellexpand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().to_string();
        }
    }
    s.to_string()
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
            let mut exec = Executor::new(Platform::current()).interactive();
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
        println!("  [{}] {} \x1b[90m[{}]\x1b[0m", step.index + 1, step.description, step.inferred_kind);
        println!("       \x1b[90m>\x1b[0m {}", step.raw);
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
    let embed_str = format!("{} {}", verb, slot_assignments.iter()
        .map(|(_, l)| l.as_str()).collect::<Vec<_>>().join(" "));
    let vec = embed_text(&embed_str, EMBED_DIM);
    let mut emb_store = EmbeddingStore::load_or_create(&paths.embeddings_bin, EMBED_DIM)?;
    emb_store.upsert(verb, crate::embedder::EmbeddingSource::Wrapped, vec);
    emb_store.save()?;

    // Sync to redb staging store so sm resolve can find it immediately
    if let Some(db) = open_store(&paths.staging_db) {
        let _ = db.put(&entry);
    }

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

fn cmd_list(rest: &[String], paths: &ShellmindPaths) -> Result<()> {
    // Parse flags and optional pattern
    let staging   = rest.iter().any(|a| a == "--staging");
    let detail    = rest.iter().any(|a| a == "--detail");
    let by_usage  = rest.iter().any(|a| a == "--by-usage");
    let by_date   = rest.iter().any(|a| a == "--by-date");
    let pattern   = rest.iter().find(|a| !a.starts_with("--")).map(|s| s.to_lowercase());

    let registry = build_registry(paths);
    let dir = if staging { &registry.staging_dir } else { &registry.curated_dir };
    let mut entries = registry.load_all_from_dir(dir)?;

    // Always load the other store for the footer count
    let other_count = if staging {
        registry.load_all_from_dir(&registry.curated_dir).map(|e| e.len()).unwrap_or(0)
    } else {
        registry.load_all_from_dir(&registry.staging_dir).map(|e| e.len()).unwrap_or(0)
    };

    // Filter by pattern
    if let Some(ref pat) = pattern {
        entries.retain(|e| e.verb.to_lowercase().contains(pat.as_str()));
    }

    if entries.is_empty() {
        let msg = if let Some(ref pat) = pattern {
            format!("No entries matching '{}'.", pat)
        } else {
            format!("No entries in {} registry.", if staging { "staging" } else { "curated" })
        };
        println!("{}", msg);
        // Still show footer
        if !staging && other_count > 0 {
            println!("  \x1b[90m({} in staging)\x1b[0m", other_count);
        }
        return Ok(());
    }

    // Sort
    if by_usage {
        entries.sort_by(|a, b| b.usage_count.cmp(&a.usage_count));
    } else if by_date {
        entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    } else {
        entries.sort_by(|a, b| a.verb.cmp(&b.verb));
    }

    let label = if staging { "STAGING" } else { "CURATED" };
    let filter_note = pattern.as_deref().map(|p| format!(" matching '{}'", p)).unwrap_or_default();
    println!("\n  \x1b[1m{}\x1b[0m  \x1b[90m{} entries{}\x1b[0m\n", label, entries.len(), filter_note);

    if detail {
        println!("  {:<24} {:<10} {:<6} {:<6}  {}", "VERB", "KIND", "USES", "CONF", "DETAIL");
        println!("  {}", "-".repeat(72));
        for e in &entries {
            let (kind, detail_str) = match &e.kind {
                EntryKind::Alias { expansion, .. } => {
                    let truncated = if expansion.chars().count() > 36 {
                        let head: String = expansion.chars().take(35).collect();
                        format!("{}…", head)
                    } else {
                        expansion.clone()
                    };
                    ("alias", truncated)
                }
                EntryKind::Procedure { steps, .. } => {
                    let kind = if e.is_wrapped() { "proc:wr" } else { "proc" };
                    (kind, format!("{} steps", steps.len()))
                }
            };
            println!("  {:<24} {:<10} {:<6} {:<5}%  \x1b[90m{}\x1b[0m",
                e.verb, kind, e.usage_count, (e.confidence * 100.0) as u32, detail_str);
        }
    } else {
        println!("  {:<24} {:<10} {:<6} {}", "VERB", "KIND", "USES", "CONF");
        println!("  {}", "-".repeat(52));
        for e in &entries {
            let kind = match &e.kind {
                EntryKind::Alias { .. } => "alias",
                EntryKind::Procedure { .. } => if e.is_wrapped() { "proc:wr" } else { "proc" },
            };
            println!("  {:<24} {:<10} {:<6} {:.0}%",
                e.verb, kind, e.usage_count, e.confidence * 100.0);
        }
    }

    // Footer: show count from the other store
    if !staging && other_count > 0 {
        println!("\n  \x1b[90m+{} in staging  (sm list --staging)\x1b[0m", other_count);
    } else if staging && other_count > 0 {
        println!("\n  \x1b[90m+{} in curated  (sm list)\x1b[0m", other_count);
    }
    println!();
    Ok(())
}

fn cmd_reindex(paths: &ShellmindPaths) -> Result<()> {
    let registry = build_registry(paths);

    // Rebuild redb stores from TOML source of truth
    let curated_count = match RegistryStore::open(&paths.curated_db) {
        Ok(store) => store.rebuild_from_toml_dir(&registry.curated_dir, &registry)?,
        Err(e) => { eprintln!("  \x1b[33m!\x1b[0m curated store unavailable: {}", e); 0 }
    };
    let staging_count = match RegistryStore::open(&paths.staging_db) {
        Ok(store) => store.rebuild_from_toml_dir(&registry.staging_dir, &registry)?,
        Err(e) => { eprintln!("  \x1b[33m!\x1b[0m staging store unavailable: {}", e); 0 }
    };

    // Rebuild embeddings (uses redb now that stores exist)
    let mut resolver = build_resolver(paths)?;
    resolver.refresh()?;
    let embed_count = resolver.reindex()?;

    println!("✓ Reindexed {} curated + {} staging entries, {} embeddings updated.",
        curated_count, staging_count, embed_count);
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

fn cmd_remove(verb: &str, paths: &ShellmindPaths) -> Result<()> {
    use std::io::{BufRead, Write};

    let registry = build_registry(paths);

    // Find which store the verb lives in
    let curated_path = registry.curated_dir.join(format!("{}.toml", verb));
    let staging_path = registry.staging_dir.join(format!("{}.toml", verb));
    let (toml_path, store_name) = if curated_path.exists() {
        (curated_path, "curated")
    } else if staging_path.exists() {
        (staging_path, "staging")
    } else {
        anyhow::bail!("'{}' not found in curated or staging.", verb);
    };

    // Load and preview the entry
    let dir = if store_name == "curated" { &registry.curated_dir } else { &registry.staging_dir };
    let entries = registry.load_all_from_dir(dir)?;
    let entry = entries.iter()
        .find(|e| e.verb == verb)
        .with_context(|| format!("Could not load entry for '{}'", verb))?;

    eprintln!("  Found '{}' in {}.", verb, store_name);
    print_entry_detail(entry);

    // Confirm
    print!("  Remove? [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if !line.trim().eq_ignore_ascii_case("y") {
        println!("  Cancelled.");
        return Ok(());
    }

    // Delete TOML file
    std::fs::remove_file(&toml_path)
        .with_context(|| format!("Failed to delete {}", toml_path.display()))?;

    // Remove from redb store
    let db_path = if store_name == "curated" { &paths.curated_db } else { &paths.staging_db };
    if let Some(db) = open_store(db_path) {
        let _ = db.delete(verb);
    }

    // Remove from embedding store
    let mut emb_store = EmbeddingStore::load_or_create(&paths.embeddings_bin, EMBED_DIM)?;
    emb_store.remove(verb);
    emb_store.save()?;

    println!("  \x1b[32m✓\x1b[0m Removed '{}'.", verb);
    Ok(())
}

async fn cmd_run(verb: &str, slots: &[String], paths: &ShellmindPaths) -> Result<()> {
    let entry = load_entry(verb, paths)?;
    let outcome = run_entry(&entry, slots).await?;
    if !outcome.success {
        std::process::exit(1);
    }
    Ok(())
}

/// Result of running a single `CommandEntry`. Shared by `cmd_run` (CLI) and
/// the scheduler. The scheduler needs to *not* exit on failure so it can
/// continue running other due schedules, hence the explicit struct return.
struct RunOutcome {
    success: bool,
    #[allow(dead_code)] succeeded: usize,
    #[allow(dead_code)] total: usize,
}

fn load_entry(verb: &str, paths: &ShellmindPaths) -> Result<CommandEntry> {
    let registry = build_registry(paths);
    let mut all = registry.load_all_from_dir(&registry.curated_dir)?;
    all.extend(registry.load_all_from_dir(&registry.staging_dir)?);
    all.into_iter()
        .find(|e| e.verb == verb)
        .with_context(|| format!("'{}' not found in registry.", verb))
}

async fn run_entry(entry: &CommandEntry, slots: &[String]) -> Result<RunOutcome> {
    match &entry.kind {
        EntryKind::Alias { expansion, arg_names } => {
            let mut cmd = expansion.clone();
            for (i, name) in arg_names.iter().enumerate() {
                if let Some(val) = slots.get(i) {
                    cmd = cmd.replace(&format!("{{{}}}", name), val);
                }
            }
            eprintln!("  \x1b[90m→\x1b[0m {}", cmd);
            let (program, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };
            let status = tokio::process::Command::new(program)
                .arg(flag).arg(&cmd)
                .status().await?;
            let success = status.success();
            Ok(RunOutcome { success, succeeded: if success { 1 } else { 0 }, total: 1 })
        }
        EntryKind::Procedure { steps, .. } => {
            if steps.is_empty() {
                anyhow::bail!("'{}' has no steps to run.", entry.verb);
            }
            let total = steps.len();
            let step_kinds: Vec<_> = steps.iter().map(|s| s.step.clone()).collect();
            let descriptions: Vec<String> = steps.iter().map(|s| {
                s.description.clone().unwrap_or_else(|| format!("{:?}", s.step).split_whitespace().next().unwrap_or("step").to_string())
            }).collect();

            let mut exec = Executor::new(Platform::current());
            let mut succeeded = 0usize;

            for (i, step) in step_kinds.iter().enumerate() {
                eprint!("  [{}/{}] {} … ", i + 1, total, descriptions[i]);
                let result = exec.run_steps(std::slice::from_ref(step), slots, true).await?;
                let r = &result[0];
                if r.success {
                    eprintln!("\x1b[32m✓\x1b[0m");
                    succeeded += 1;
                } else {
                    eprintln!("\x1b[31m✗\x1b[0m");
                }
            }

            let color = if succeeded == total { "\x1b[32m" } else { "\x1b[33m" };
            eprintln!("  {}Done. ({}/{} steps succeeded)\x1b[0m", color, succeeded, total);
            Ok(RunOutcome { success: succeeded == total, succeeded, total })
        }
    }
}

// ── sm schedule ──────────────────────────────────────────────────────────────

async fn cmd_schedule(rest: &[String], paths: &ShellmindPaths) -> Result<()> {
    let action = rest.first().map(|s| s.as_str()).unwrap_or("list");
    match action {
        "add"     => cmd_schedule_add(&rest[1..], paths),
        "list"    => cmd_schedule_list(paths),
        "remove" | "rm" => {
            let name = rest.get(1).context("Usage: sm schedule remove <name>")?;
            cmd_schedule_remove(name, paths)
        }
        "enable"  => {
            let name = rest.get(1).context("Usage: sm schedule enable <name>")?;
            cmd_schedule_set_enabled(name, true, paths)
        }
        "disable" => {
            let name = rest.get(1).context("Usage: sm schedule disable <name>")?;
            cmd_schedule_set_enabled(name, false, paths)
        }
        "next"    => cmd_schedule_next(paths),
        "run"     => cmd_schedule_run(paths).await,
        _ => anyhow::bail!("Usage: sm schedule <add|list|remove|enable|disable|next|run> ..."),
    }
}

fn cmd_schedule_add(rest: &[String], paths: &ShellmindPaths) -> Result<()> {
    if rest.len() < 3 {
        anyhow::bail!("Usage: sm schedule add <name> \"<cron>\" <verb> [args...]");
    }
    let name = &rest[0];
    let cron = &rest[1];
    let verb = &rest[2];
    let args: Vec<String> = rest[3..].to_vec();

    // Validate verb exists
    let _ = load_entry(verb, paths)
        .with_context(|| format!("Cannot schedule unknown verb '{}'", verb))?;

    let store = ScheduleStore::new(&paths.schedules_dir);
    if store.get(name)?.is_some() {
        anyhow::bail!("Schedule '{}' already exists. Remove it first or pick another name.", name);
    }

    let schedule = Schedule::new(name, verb, args, cron)?;
    store.put(&schedule)?;

    println!("  \x1b[32m✓\x1b[0m Scheduled '{}' to run '{}' on cron '{}'.", name, verb, cron);
    if let Some(nr) = schedule.next_run {
        println!("    Next run: \x1b[33m{}\x1b[0m", nr.format("%Y-%m-%d %H:%M UTC"));
    }
    Ok(())
}

fn cmd_schedule_list(paths: &ShellmindPaths) -> Result<()> {
    let store = ScheduleStore::new(&paths.schedules_dir);
    let schedules = store.load_all()?;

    if schedules.is_empty() {
        println!("No schedules. Add one with `sm schedule add <name> \"<cron>\" <verb> [args...]`.");
        return Ok(());
    }

    println!("\n  \x1b[1mSCHEDULES\x1b[0m  \x1b[90m{} entries\x1b[0m\n", schedules.len());
    println!("  {:<20} {:<14} {:<16} {:<8} {}", "NAME", "CRON", "VERB", "ENABLED", "NEXT RUN");
    println!("  {}", "-".repeat(80));
    for s in &schedules {
        let next = if !s.enabled {
            "\x1b[90m(disabled)\x1b[0m".to_string()
        } else if let Some(nr) = s.next_run {
            nr.format("%Y-%m-%d %H:%M UTC").to_string()
        } else {
            "—".to_string()
        };
        println!("  {:<20} {:<14} {:<16} {:<8} {}",
            s.name, s.cron, s.verb,
            if s.enabled { "yes" } else { "no" },
            next);
    }
    println!();
    Ok(())
}

fn cmd_schedule_remove(name: &str, paths: &ShellmindPaths) -> Result<()> {
    let store = ScheduleStore::new(&paths.schedules_dir);
    if !store.delete(name)? {
        anyhow::bail!("Schedule '{}' not found.", name);
    }
    println!("  \x1b[32m✓\x1b[0m Removed schedule '{}'.", name);
    Ok(())
}

fn cmd_schedule_set_enabled(name: &str, enabled: bool, paths: &ShellmindPaths) -> Result<()> {
    let store = ScheduleStore::new(&paths.schedules_dir);
    let mut sched = store.get(name)?
        .with_context(|| format!("Schedule '{}' not found", name))?;
    sched.enabled = enabled;
    // When re-enabling, recompute next_run so we don't immediately fire on a
    // stale timestamp from when it was disabled.
    if enabled {
        sched.next_run = sched.compute_next_after(chrono::Utc::now())?;
    }
    store.put(&sched)?;
    println!("  \x1b[32m✓\x1b[0m Schedule '{}' is now \x1b[33m{}\x1b[0m.", name,
        if enabled { "enabled" } else { "disabled" });
    Ok(())
}

fn cmd_schedule_next(paths: &ShellmindPaths) -> Result<()> {
    let store = ScheduleStore::new(&paths.schedules_dir);
    let mut schedules: Vec<Schedule> = store.load_all()?
        .into_iter()
        .filter(|s| s.enabled && s.next_run.is_some())
        .collect();
    schedules.sort_by_key(|s| s.next_run.unwrap());

    if schedules.is_empty() {
        println!("No upcoming schedule runs.");
        return Ok(());
    }

    println!("\n  Upcoming:");
    for s in &schedules {
        let args = if s.args.is_empty() { String::new() } else { format!(" {}", s.args.join(" ")) };
        println!("    \x1b[33m{}\x1b[0m  {:<20} \x1b[90m{}{}\x1b[0m",
            s.next_run.unwrap().format("%Y-%m-%d %H:%M UTC"),
            s.name, s.verb, args);
    }
    println!();
    Ok(())
}

async fn cmd_schedule_run(paths: &ShellmindPaths) -> Result<()> {
    let store = ScheduleStore::new(&paths.schedules_dir);
    let schedules = store.load_all()?;
    let now = chrono::Utc::now();

    let due: Vec<Schedule> = schedules.into_iter().filter(|s| s.is_due(now)).collect();

    if due.is_empty() {
        eprintln!("No schedules due as of {}.", now.format("%Y-%m-%d %H:%M:%S UTC"));
        return Ok(());
    }

    eprintln!("\n  Found {} due schedule(s).\n", due.len());

    let mut ran = 0usize;
    let mut succeeded = 0usize;

    for mut sched in due {
        let args_str = if sched.args.is_empty() { String::new() } else { format!(" {}", sched.args.join(" ")) };
        eprintln!("  \x1b[1m▶\x1b[0m '{}' \x1b[90m({}{})\x1b[0m",
            sched.name, sched.verb, args_str);

        let entry = match load_entry(&sched.verb, paths) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("    \x1b[31m✗\x1b[0m verb missing: {}", e);
                ran += 1;
                // Still update next_run so we don't loop on a broken schedule
                sched.last_run = Some(now);
                sched.next_run = sched.compute_next_after(now).ok().flatten();
                let _ = store.put(&sched);
                continue;
            }
        };

        let outcome = run_entry(&entry, &sched.args).await;
        ran += 1;
        match outcome {
            Ok(o) if o.success => succeeded += 1,
            Ok(_) => {}
            Err(e) => eprintln!("    \x1b[31m✗\x1b[0m run failed: {}", e),
        }

        sched.last_run = Some(now);
        sched.next_run = sched.compute_next_after(now).ok().flatten();
        if let Err(e) = store.put(&sched) {
            tracing::warn!("Could not persist schedule '{}': {}", sched.name, e);
        }
    }

    let color = if succeeded == ran { "\x1b[32m" } else { "\x1b[33m" };
    eprintln!("\n  {}✓\x1b[0m Ran {} schedule(s), {} succeeded.\n", color, ran, succeeded);
    Ok(())
}

fn cmd_edit(verb: &str, paths: &ShellmindPaths) -> Result<()> {
    let registry = build_registry(paths);

    let curated_path = registry.curated_dir.join(format!("{}.toml", verb));
    let staging_path = registry.staging_dir.join(format!("{}.toml", verb));
    let toml_path = if curated_path.exists() {
        curated_path
    } else if staging_path.exists() {
        staging_path
    } else {
        anyhow::bail!("'{}' not found in curated or staging.", verb);
    };

    crate::editor::open_in_editor(&toml_path)?;
    crate::editor::reload_and_reindex(verb, paths)?;

    eprintln!("  \x1b[32m✓\x1b[0m '{}' saved and re-indexed.", verb);
    Ok(())
}

fn cmd_rename(old_verb: &str, new_verb: &str, paths: &ShellmindPaths) -> Result<()> {
    use std::io::{BufRead, Write};

    let registry = build_registry(paths);

    // Locate the source entry
    let curated_old = registry.curated_dir.join(format!("{}.toml", old_verb));
    let staging_old = registry.staging_dir.join(format!("{}.toml", old_verb));
    let (old_toml, store_dir, store_name) = if curated_old.exists() {
        (curated_old, &registry.curated_dir, "curated")
    } else if staging_old.exists() {
        (staging_old, &registry.staging_dir, "staging")
    } else {
        anyhow::bail!("'{}' not found in curated or staging.", old_verb);
    };

    // Collision check
    let curated_new = registry.curated_dir.join(format!("{}.toml", new_verb));
    let staging_new = registry.staging_dir.join(format!("{}.toml", new_verb));
    if curated_new.exists() || staging_new.exists() {
        let existing_store = if curated_new.exists() { "curated" } else { "staging" };
        eprintln!("  \x1b[33m!\x1b[0m '{}' already exists in {}.", new_verb, existing_store);
        print!("  Overwrite? [y/N] ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if !line.trim().eq_ignore_ascii_case("y") {
            println!("  Cancelled.");
            return Ok(());
        }
        // Remove the colliding file so write_toml won't conflict
        let collision_path = if curated_new.exists() { &curated_new } else { &staging_new };
        std::fs::remove_file(collision_path)?;
    }

    // Load the entry, update verb, write new TOML, delete old
    let entries = registry.load_all_from_dir(store_dir)?;
    let mut entry = entries.into_iter()
        .find(|e| e.verb == old_verb)
        .with_context(|| format!("Could not load entry for '{}'", old_verb))?;

    entry.verb = new_verb.to_string();

    // Write to the same store the original lived in
    let new_toml_path = store_dir.join(format!("{}.toml", new_verb));
    let serialized = entry.to_toml_file()?;
    std::fs::write(&new_toml_path, serialized)
        .with_context(|| format!("Failed to write {}", new_toml_path.display()))?;

    std::fs::remove_file(&old_toml)
        .with_context(|| format!("Failed to delete {}", old_toml.display()))?;

    // Update redb store: remove old, insert new
    let db_path = if store_name == "curated" { &paths.curated_db } else { &paths.staging_db };
    if let Some(db) = open_store(db_path) {
        let _ = db.delete(old_verb);
        let _ = db.put(&entry);
    }

    // Update embedding store: remove old key, upsert new key with fresh vector
    let embed_str = match &entry.kind {
        EntryKind::Alias { expansion, arg_names } =>
            format!("{} {} {}", new_verb, arg_names.join(" "), expansion),
        EntryKind::Procedure { description, .. } =>
            format!("{} {}", new_verb, description.as_deref().unwrap_or("")),
    };
    let source = if entry.is_wrapped() {
        crate::embedder::EmbeddingSource::Wrapped
    } else if store_name == "staging" {
        crate::embedder::EmbeddingSource::Staging
    } else {
        crate::embedder::EmbeddingSource::Curated
    };
    let vec = embed_text(&embed_str, EMBED_DIM);
    let mut store = EmbeddingStore::load_or_create(&paths.embeddings_bin, EMBED_DIM)?;
    store.remove(old_verb);
    store.upsert(new_verb, source, vec);
    store.save()?;

    println!("  \x1b[32m✓\x1b[0m Renamed '{}' → '{}'.", old_verb, new_verb);
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
    Ok(Resolver::new(registry, embeddings, model)
        .with_store(paths.curated_db.clone(), paths.staging_db.clone()))
}

fn open_store(path: &std::path::Path) -> Option<RegistryStore> {
    RegistryStore::open(path).ok()
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
  init [--write]           First-time setup. Prints shell hook (default)
                           or appends it to your rc file (with --write).
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
  remove  <verb>           Delete a verb from the registry and embeddings
  rename  <old> <new>      Rename a verb, preserving its definition and embedding
  edit    <verb>           Open a verb's definition in $EDITOR, re-index on save

  run     <verb> [args]   Execute a verb directly (no shell hook needed)
                           Prints step-by-step progress for procedures
  list [--staging] [--detail] [--by-usage|--by-date] [pattern]
                           List registry entries. Filter by pattern substring,
                           sort by usage or date (default: alphabetical).
  reindex                  Rebuild embedding store from registry
  promote                  Run automatic promotion/decay pass

  schedule add <name> "<cron>" <verb> [args...]
                           Schedule a verb to run on a 5-field cron expression
  schedule list            List configured schedules
  schedule remove <name>   Delete a schedule
  schedule enable|disable <name>
                           Toggle a schedule without removing it
  schedule next            Show upcoming runs sorted by time
  schedule run             Run all due schedules now (wire into cron/Task Scheduler)

  a.n lister <goal>        Search Agennect marketplace via the Lister agent
  a.n <index> <args>       Run an agent by index from the last Lister listing
  a.n <agent-name> <args>  Run a named agent on Agennect
  a.n list                 Show the last cached Lister listing
  a.n refresh              Re-fetch the Lister card

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

        // If no expansion given, offer edit or cancel
        if rest.len() < 2 {
            print!("\n  [e]dit in $EDITOR, or any other key to cancel: ");
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            if line.trim().eq_ignore_ascii_case("e") {
                return cmd_edit(&verb, paths);
            }
            println!("  Cancelled.");
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

    // Sync to redb store
    let db_path = if entry.is_curated() { &paths.curated_db } else { &paths.staging_db };
    if let Some(store) = open_store(db_path) {
        let _ = store.put(entry);
    }

    // Build embedding immediately
    let text = match &entry.kind {
        EntryKind::Alias { expansion, arg_names } =>
            format!("{} {} {}", entry.verb, arg_names.join(" "), expansion),
        EntryKind::Procedure { steps: _, description, .. } =>
            format!("{} {}", entry.verb, description.as_deref().unwrap_or("")),
    };
    let vec = embed_text(&text, EMBED_DIM);
    let source = if entry.is_wrapped() {
        crate::embedder::EmbeddingSource::Wrapped
    } else {
        crate::embedder::EmbeddingSource::Curated
    };
    let mut emb_store = EmbeddingStore::load_or_create(&paths.embeddings_bin, EMBED_DIM)?;
    emb_store.upsert(&entry.verb, source, vec);
    emb_store.save()?;
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
