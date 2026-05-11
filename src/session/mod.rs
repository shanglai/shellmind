use std::collections::VecDeque;
use std::path::Path;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::platform::StepKind;
use crate::registry::{ArgBind, CommandEntry, EntryKind, EntrySource, ProcStep};

pub const MAX_HISTORY: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub index: usize,
    pub raw_input: String,
    pub resolved_command: String,
    pub timestamp: DateTime<Utc>,
    pub exit_code: i32,
    pub cwd: String,
    /// Steps if this resolved to a procedure
    pub steps: Option<Vec<StepKind>>,
}

pub struct SessionBuffer {
    pub entries: VecDeque<SessionEntry>,
    next_index: usize,
    persist_path: std::path::PathBuf,
}

impl SessionBuffer {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)?;
            let entries: VecDeque<SessionEntry> = serde_json::from_str(&raw).unwrap_or_default();
            let next_index = entries.back().map(|e| e.index + 1).unwrap_or(0);
            return Ok(Self { entries, next_index, persist_path: path.to_path_buf() });
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self { entries: VecDeque::new(), next_index: 0, persist_path: path.to_path_buf() })
    }

    pub fn push(&mut self, entry: SessionEntry) {
        if self.entries.len() >= MAX_HISTORY {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
        self.next_index += 1;
    }

    pub fn record(
        &mut self,
        raw_input: &str,
        resolved: &str,
        exit_code: i32,
        steps: Option<Vec<StepKind>>,
    ) {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        self.push(SessionEntry {
            index: self.next_index,
            raw_input: raw_input.to_string(),
            resolved_command: resolved.to_string(),
            timestamp: Utc::now(),
            exit_code,
            cwd,
            steps,
        });
    }

    /// Last N successful entries (exit_code == 0).
    pub fn last_successful(&self, n: usize) -> Vec<&SessionEntry> {
        self.entries.iter()
            .filter(|e| e.exit_code == 0)
            .rev()
            .take(n)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    pub fn save(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.entries)?;
        std::fs::write(&self.persist_path, json)?;
        Ok(())
    }
}

// ── Wrap handler ─────────────────────────────────────────────────────────────

/// Preview shown to user before confirming a wrap.
#[derive(Debug)]
pub struct WrapPreview {
    pub steps: Vec<WrapPreviewStep>,
    pub slot_candidates: Vec<SlotCandidate>,
}

#[derive(Debug)]
pub struct WrapPreviewStep {
    pub index: usize,
    pub description: String,
    pub raw: String,
}

#[derive(Debug)]
pub struct SlotCandidate {
    pub token: String,
    pub slot_label: String,  // "$1", "$2" etc.
    pub appears_in_steps: Vec<usize>,
}

/// Build a wrap preview from N successful session entries.
pub fn build_wrap_preview(entries: &[&SessionEntry]) -> WrapPreview {
    let steps: Vec<WrapPreviewStep> = entries.iter().enumerate().map(|(i, e)| {
        WrapPreviewStep {
            index: i,
            description: summarize_command(&e.resolved_command),
            raw: e.raw_input.clone(),
        }
    }).collect();

    // Heuristic slot detection: tokens that look like file paths or URLs
    let slot_candidates = detect_slot_candidates(entries);

    WrapPreview { steps, slot_candidates }
}

/// Convert a confirmed wrap into a CommandEntry for the registry.
pub fn finalize_wrap(
    verb: &str,
    entries: &[&SessionEntry],
    slot_assignments: &[(String, String)], // (token, label) pairs
) -> CommandEntry {
    let session_indices: Vec<usize> = entries.iter().map(|e| e.index).collect();

    let proc_steps: Vec<ProcStep> = entries.iter().enumerate().map(|(i, e)| {
        let mut template = e.raw_input.clone();
        for (token, label) in slot_assignments {
            template = template.replace(token.as_str(), &format!("{{{}}}", label));
        }
        ProcStep {
            index: i,
            step: StepKind::ShellRaw {
                cmd: template,
                platform: crate::platform::Platform::current(),
            },
            description: Some(summarize_command(&e.resolved_command)),
            depends_on: if i > 0 { vec![i - 1] } else { vec![] },
        }
    }).collect();

    let arg_bindings: Vec<ArgBind> = slot_assignments.iter().enumerate().map(|(i, (_, label))| {
        ArgBind {
            call_position: i,
            label: label.clone(),
            step_index: 0, // simplified — full resolution happens at executor
            placeholder: format!("{{{}}}", label),
        }
    }).collect();

    CommandEntry {
        id: uuid::Uuid::new_v4(),
        verb: verb.to_string(),
        kind: EntryKind::Procedure {
            steps: proc_steps,
            arg_bindings,
            description: Some(format!("Wrapped from {} session steps", entries.len())),
        },
        tags: vec![":wr:".to_string()],
        source: EntrySource::Wrapped { session_indices },
        usage_count: 0,
        created_at: Utc::now(),
        last_used: None,
        confidence: 0.9,
        embedding: None,
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn summarize_command(cmd: &str) -> String {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    match tokens.len() {
        0 => "(empty)".to_string(),
        1 => tokens[0].to_string(),
        _ => format!("{} {} …", tokens[0], tokens[1]),
    }
}

fn detect_slot_candidates(entries: &[&SessionEntry]) -> Vec<SlotCandidate> {
    let mut candidates = vec![];
    let mut slot_counter = 1;

    // Tokens that look like paths or URLs and appear in at least one step
    for entry in entries {
        for token in entry.raw_input.split_whitespace() {
            let looks_like_slot = token.contains('/')
                || token.contains('\\')
                || token.contains('.')
                || token.starts_with("http");

            if looks_like_slot && !candidates.iter().any(|c: &SlotCandidate| c.token == token) {
                candidates.push(SlotCandidate {
                    token: token.to_string(),
                    slot_label: format!("${}", slot_counter),
                    appears_in_steps: entries.iter().enumerate()
                        .filter(|(_, e)| e.raw_input.contains(token))
                        .map(|(i, _)| i)
                        .collect(),
                });
                slot_counter += 1;
            }
        }
    }

    candidates
}
