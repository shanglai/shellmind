//! Disambiguation layer — sits between Stage 2/3 hits and Resolution.
//!
//! Decision flow:
//!
//!   hits (scored candidates)
//!     │
//!     ├─ rerank_by_args()         re-score using arg type compatibility
//!     ├─ rerank_by_session()      boost recently-used verbs
//!     │
//!     ├─ single candidate?        → Clear (no prompt needed)
//!     ├─ gap >= AUTOSELECT_GAP?   → Clear (top is clearly better)
//!     ├─ top score >= CEILING?    → Clear (high confidence)
//!     ├─ non-TTY stdin?           → Passthrough (can't prompt safely)
//!     └─ otherwise                → Prompt (interactive selection)

use crate::registry::{CommandEntry, EntryKind};
use crate::session::SessionEntry;

/// Score gap above which we auto-select without prompting.
pub const AUTOSELECT_GAP: f32 = 0.12;

/// Score above which we always auto-select regardless of gap.
pub const AUTOSELECT_CEILING: f32 = 0.94;

/// Minimum score for any candidate to appear in the prompt.
pub const PROMPT_FLOOR: f32 = 0.70;

/// Maximum candidates shown in the disambiguation prompt.
pub const MAX_PROMPT_CANDIDATES: usize = 4;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    pub entry: CommandEntry,
    /// Raw similarity score from embedding search or prefix match
    pub raw_score: f32,
    /// Final score after all re-ranking passes
    pub final_score: f32,
    /// Human-readable reason shown in the prompt
    pub match_reason: MatchReason,
}

#[derive(Debug, Clone)]
pub enum MatchReason {
    Exact,
    Prefix,
    Similarity { cosine: f32 },
    SimilarityBoosted { cosine: f32, boost: &'static str },
}

impl MatchReason {
    pub fn display(&self) -> String {
        match self {
            MatchReason::Exact                          => "exact match".to_string(),
            MatchReason::Prefix                         => "prefix match".to_string(),
            MatchReason::Similarity { cosine }          => format!("{:.0}% similar", cosine * 100.0),
            MatchReason::SimilarityBoosted { cosine, boost } =>
                format!("{:.0}% similar ({})", cosine * 100.0, boost),
        }
    }
}

pub enum DisambiguationResult {
    /// Clear winner — proceed without prompting.
    Clear(ScoredCandidate),
    /// Ambiguous — caller should call `prompt()` then re-enter with the selection.
    Ambiguous(Vec<ScoredCandidate>),
    /// Cannot disambiguate (non-TTY or all candidates below floor).
    Passthrough,
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Evaluate a set of scored candidates and decide whether to auto-select or prompt.
///
/// `args`    — the arguments the user provided (used for type-compatibility scoring)
/// `session` — recent history (used for recency boost)
pub fn evaluate(
    mut candidates: Vec<ScoredCandidate>,
    args: &[String],
    session: &[&SessionEntry],
) -> DisambiguationResult {
    if candidates.is_empty() {
        return DisambiguationResult::Passthrough;
    }

    // ── Re-ranking passes ─────────────────────────────────────────────────────
    rerank_by_args(&mut candidates, args);
    rerank_by_session(&mut candidates, session);
    rerank_wrapped_preference(&mut candidates);

    // Sort descending by final_score
    candidates.sort_by(|a, b| b.final_score.partial_cmp(&a.final_score).unwrap_or(std::cmp::Ordering::Equal));

    // Drop below-floor candidates
    candidates.retain(|c| c.final_score >= PROMPT_FLOOR);
    candidates.truncate(MAX_PROMPT_CANDIDATES);

    if candidates.is_empty() {
        return DisambiguationResult::Passthrough;
    }

    // Single candidate — always clear
    if candidates.len() == 1 {
        return DisambiguationResult::Clear(candidates.remove(0));
    }

    let top = candidates[0].final_score;
    let second = candidates[1].final_score;
    let gap = top - second;

    // High-confidence single pick
    if top >= AUTOSELECT_CEILING {
        tracing::debug!("Auto-select: score {:.3} >= ceiling {:.3}", top, AUTOSELECT_CEILING);
        return DisambiguationResult::Clear(candidates.remove(0));
    }

    // Clear gap between first and second
    if gap >= AUTOSELECT_GAP {
        tracing::debug!("Auto-select: gap {:.3} >= threshold {:.3}", gap, AUTOSELECT_GAP);
        return DisambiguationResult::Clear(candidates.remove(0));
    }

    // Can we prompt? Only if stdin is an interactive TTY
    if !stdin_is_tty() {
        tracing::debug!("Non-TTY stdin — passthrough instead of prompt");
        return DisambiguationResult::Passthrough;
    }

    DisambiguationResult::Ambiguous(candidates)
}

// ── Re-ranking passes ─────────────────────────────────────────────────────────

/// Re-score candidates based on how well their expected argument profile
/// matches the actual arguments provided.
///
/// Signals used:
///   - URL args → boost HttpCall steps, penalise FileCopy
///   - Path args → boost file-oriented steps
///   - Arg count match → boost entries whose arg_names count matches
///   - No args provided → slight boost to zero-arg entries
fn rerank_by_args(candidates: &mut Vec<ScoredCandidate>, args: &[String]) {
    for c in candidates.iter_mut() {
        let compat = arg_compatibility(&c.entry, args);
        c.final_score *= compat;
        if (compat - 1.0).abs() > 0.01 {
            tracing::debug!(
                "  arg compat for '{}': {:.2}x → {:.3}",
                c.entry.verb, compat, c.final_score
            );
        }
    }
}

/// Boost entries that appear in recent successful session history.
fn rerank_by_session(candidates: &mut Vec<ScoredCandidate>, session: &[&SessionEntry]) {
    let recent_verbs: std::collections::HashSet<&str> = session.iter()
        .filter_map(|e| e.raw_input.split_whitespace().next())
        .collect();

    for c in candidates.iter_mut() {
        if recent_verbs.contains(c.entry.verb.as_str()) {
            c.final_score *= 1.08;
            tracing::debug!("  recency boost for '{}'", c.entry.verb);
        }
    }
}

/// Prefer wrapped procedures over plain aliases when scores are otherwise close.
/// Wrapped entries represent confirmed multi-step intent — they are more specific.
fn rerank_wrapped_preference(candidates: &mut Vec<ScoredCandidate>) {
    let any_wrapped = candidates.iter().any(|c| c.entry.is_wrapped());
    if !any_wrapped { return; }

    for c in candidates.iter_mut() {
        if c.entry.is_wrapped() {
            c.final_score *= 1.05;
        }
    }
}

// ── Arg compatibility scoring ─────────────────────────────────────────────────

fn arg_compatibility(entry: &CommandEntry, args: &[String]) -> f32 {
    let mut score = 1.0f32;

    match &entry.kind {
        EntryKind::Alias { arg_names, expansion } => {
            // Arg count match
            score *= count_compat(arg_names.len(), args.len());

            // Type signals from args vs expansion content
            for arg in args {
                if looks_like_url(arg) {
                    if expansion.contains("http") || expansion.contains("curl")
                        || expansion.contains("api") || expansion.contains("post") {
                        score *= 1.10;
                    } else {
                        score *= 0.85;
                    }
                } else if looks_like_path(arg) {
                    if expansion.contains("file") || expansion.contains("csv")
                        || expansion.contains("cp") || expansion.contains("mv") {
                        score *= 1.08;
                    }
                }
            }
        }

        EntryKind::Procedure { steps, arg_bindings, .. } => {
            // Arg count match against bindings
            score *= count_compat(arg_bindings.len(), args.len());

            // Check step kinds against arg types
            for arg in args {
                let step_is_http = steps.iter().any(|s| {
                    matches!(s.step, crate::platform::StepKind::HttpCall { .. })
                });
                let step_is_file = steps.iter().any(|s| {
                    matches!(
                        s.step,
                        crate::platform::StepKind::FileCopy { .. }
                        | crate::platform::StepKind::FileMove { .. }
                        | crate::platform::StepKind::DataSample { .. }
                        | crate::platform::StepKind::DataJoin { .. }
                    )
                });

                if looks_like_url(arg) {
                    score *= if step_is_http { 1.12 } else { 0.82 };
                } else if looks_like_path(arg) {
                    score *= if step_is_file { 1.10 } else { 0.90 };
                }
            }
        }
    }

    score.clamp(0.5, 1.5)
}

fn count_compat(expected: usize, provided: usize) -> f32 {
    match (expected, provided) {
        (0, 0) => 1.10, // both expect no args — good signal
        (e, p) if e == p => 1.08,
        (e, p) if p > e => 0.92, // too many args provided
        (_, 0) => 0.88,           // expects args, none provided
        _ => 0.95,
    }
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://") || s.contains(".io/") || s.contains(".com/")
}

fn looks_like_path(s: &str) -> bool {
    s.contains('/') || s.contains('\\') || s.contains('.') && !s.contains("://")
}

// ── Interactive prompt ────────────────────────────────────────────────────────

/// Render the disambiguation prompt to stderr and read the user's selection.
/// Returns the chosen ScoredCandidate, or None if cancelled/timeout.
///
/// Prompt goes to stderr so the shell hook's stdout capture stays clean.
/// Selection confirmation is echoed to stderr too — only the final
/// expanded command reaches stdout.
pub fn prompt(candidates: &[ScoredCandidate], raw_input: &str) -> Option<usize> {
    use std::io::Write;
    let stderr = std::io::stderr();
    let mut err = stderr.lock();

    writeln!(err, "").ok();
    writeln!(err, "  \x1b[33m~\x1b[0m ambiguous: '{}' matches {} commands", raw_input, candidates.len()).ok();
    writeln!(err, "").ok();

    for (i, c) in candidates.iter().enumerate() {
        let tag = if c.entry.is_wrapped() { " \x1b[36m:wr:\x1b[0m" } else { "" };
        let kind_label = match &c.entry.kind {
            EntryKind::Alias { expansion, .. } => {
                // Show first 48 chars of expansion
                let preview: String = expansion.chars().take(48).collect();
                format!("→ {}", preview)
            }
            EntryKind::Procedure { description, steps, .. } => {
                let desc = description.as_deref()
                    .unwrap_or("(procedure)");
                format!("→ {} ({} steps)", desc, steps.len())
            }
        };

        writeln!(
            err,
            "  \x1b[1m[{}]\x1b[0m {}{} \x1b[90m({})\x1b[0m",
            i + 1,
            c.entry.verb,
            tag,
            c.match_reason.display()
        ).ok();
        writeln!(err, "      \x1b[90m{}\x1b[0m", kind_label).ok();
    }

    writeln!(err, "").ok();
    write!(err, "  Select [1–{}], or Enter to cancel: ", candidates.len()).ok();
    err.flush().ok();

    // Read one line with a timeout via a thread
    let line = read_line_with_timeout(std::time::Duration::from_secs(15));

    match line {
        None => {
            writeln!(err, "\n  \x1b[90mTimed out — passing through\x1b[0m").ok();
            None
        }
        Some(input) => {
            let trimmed = input.trim();
            if trimmed.is_empty() {
                writeln!(err, "  \x1b[90mCancelled\x1b[0m").ok();
                return None;
            }
            match trimmed.parse::<usize>() {
                Ok(n) if n >= 1 && n <= candidates.len() => {
                    writeln!(
                        err,
                        "  \x1b[32m✓\x1b[0m Selected: {}",
                        candidates[n - 1].entry.verb
                    ).ok();
                    Some(n - 1)
                }
                _ => {
                    writeln!(err, "  \x1b[90mInvalid selection — passing through\x1b[0m").ok();
                    None
                }
            }
        }
    }
}

// ── TTY detection ─────────────────────────────────────────────────────────────

/// Returns true only if stdin is connected to an interactive terminal.
/// Piped input, CI environments, and subshells all return false.
pub fn stdin_is_tty() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: isatty is safe to call with fd 0
        unsafe { libc_isatty(0) }
    }
    #[cfg(not(unix))]
    {
        // Windows: check if stdin handle is a console
        windows_stdin_is_console()
    }
}

#[cfg(unix)]
fn libc_isatty(fd: i32) -> bool {
    extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    unsafe { isatty(fd) != 0 }
}

#[cfg(windows)]
fn windows_stdin_is_console() -> bool {
    // Conservative: assume interactive on Windows unless we can prove otherwise
    std::env::var("CI").is_err() && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
}

#[cfg(not(any(unix, windows)))]
fn stdin_is_tty_fallback() -> bool { false }

// ── Line reading with timeout ─────────────────────────────────────────────────

fn read_line_with_timeout(timeout: std::time::Duration) -> Option<String> {
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() {
            let _ = tx.send(line);
        }
    });

    rx.recv_timeout(timeout).ok()
}

// ── Constructor helpers (used by resolver) ────────────────────────────────────

impl ScoredCandidate {
    pub fn from_similarity(entry: CommandEntry, score: f32) -> Self {
        Self {
            final_score: score,
            raw_score: score,
            match_reason: MatchReason::Similarity { cosine: score },
            entry,
        }
    }

    pub fn from_prefix(entry: CommandEntry, score: f32) -> Self {
        Self {
            final_score: score,
            raw_score: score,
            match_reason: MatchReason::Prefix,
            entry,
        }
    }
}
