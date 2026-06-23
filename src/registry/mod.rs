use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::platform::StepKind;

pub mod promotion;
pub mod store;

// ── Core types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandEntry {
    pub id: Uuid,
    pub verb: String,
    pub kind: EntryKind,
    pub tags: Vec<String>,         // includes ":wr:" for procedures
    pub source: EntrySource,
    pub usage_count: u32,
    pub created_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
    pub confidence: f32,           // 0.0–1.0; curated always 1.0
    /// Embedding stored separately in embeddings.bin; None until first embed pass
    #[serde(skip)]
    pub embedding: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryKind {
    /// Single-command alias with a template string
    Alias {
        expansion: String,
        /// Ordered arg names for documentation ("file", "api_url", …)
        arg_names: Vec<String>,
    },
    /// Multi-step procedure (tagged :wr: if created via wrap)
    Procedure {
        steps: Vec<ProcStep>,
        arg_bindings: Vec<ArgBind>,
        description: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcStep {
    pub index: usize,
    pub step: StepKind,
    pub description: Option<String>,
    /// Step indices this step must wait for
    pub depends_on: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArgBind {
    /// Position at call site: `same_as_yesterday $1 $2` → 0-indexed
    pub call_position: usize,
    pub label: String,             // human name: "file", "api_url"
    pub step_index: usize,
    pub placeholder: String,       // "{file}" inside template
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EntrySource {
    /// User-authored declarative
    Declarative,
    /// Model inferred, not yet confirmed
    Inferred { model: String, confidence: f32 },
    /// Inferred and confirmed by user → promoted to curated
    Confirmed { original_confidence: f32 },
    /// Created by `wrap last N`
    Wrapped { session_indices: Vec<usize> },
}

impl CommandEntry {
    pub fn is_procedure(&self) -> bool {
        matches!(self.kind, EntryKind::Procedure { .. })
    }

    pub fn is_wrapped(&self) -> bool {
        self.tags.iter().any(|t| t == ":wr:")
    }

    pub fn is_curated(&self) -> bool {
        self.confidence >= 1.0 || matches!(self.source, EntrySource::Declarative | EntrySource::Confirmed { .. })
    }
}

// ── TOML on-disk format ──────────────────────────────────────────────────────
// This is what users edit directly in registry/curated/ or registry/staging/

#[derive(Debug, Serialize, Deserialize)]
pub struct EntryToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub verb: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub confidence: f32,
    pub source: String,            // "declarative" | "inferred" | "confirmed" | "wrapped"
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used: Option<String>,
    pub usage_count: u32,

    // One of these is present:
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expansion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arg_names: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<serde_json::Value>>,
}

impl CommandEntry {
    pub fn to_toml_file(&self) -> Result<String> {
        // Registry files are stored as JSON (named .toml for discoverability but JSON inside)
        use std::collections::HashMap;
        let source_str = match &self.source {
            EntrySource::Declarative => "declarative".to_string(),
            EntrySource::Inferred { model, confidence } => format!("inferred:{}:{:.2}", model, confidence),
            EntrySource::Confirmed { original_confidence } => format!("confirmed:{:.2}", original_confidence),
            EntrySource::Wrapped { .. } => "wrapped".to_string(),
        };

        let mut map: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        map.insert("id".into(), serde_json::Value::String(self.id.to_string()));
        map.insert("verb".into(), serde_json::Value::String(self.verb.clone()));
        map.insert("confidence".into(), serde_json::json!(self.confidence));
        map.insert("source".into(), serde_json::Value::String(source_str));
        map.insert("created_at".into(), serde_json::Value::String(self.created_at.to_rfc3339()));
        map.insert("usage_count".into(), serde_json::json!(self.usage_count));
        map.insert("tags".into(), serde_json::json!(self.tags));

        match &self.kind {
            EntryKind::Alias { expansion, arg_names } => {
                map.insert("expansion".into(), serde_json::Value::String(expansion.clone()));
                map.insert("arg_names".into(), serde_json::json!(arg_names));
            }
            EntryKind::Procedure { steps, arg_bindings, description } => {
                if let Some(d) = description {
                    map.insert("description".into(), serde_json::Value::String(d.clone()));
                }
                let steps_json: Vec<serde_json::Value> = steps.iter().map(|s| {
                    // Serialize the full StepKind, then merge metadata fields into the same object
                    let mut obj = serde_json::to_value(&s.step)
                        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                    if let Some(m) = obj.as_object_mut() {
                        m.insert("index".into(), serde_json::json!(s.index));
                        m.insert("description".into(), serde_json::json!(s.description));
                        m.insert("depends_on".into(), serde_json::json!(s.depends_on));
                    }
                    obj
                }).collect();
                map.insert("steps".into(), serde_json::json!(steps_json));
                let bindings_json: Vec<serde_json::Value> = arg_bindings.iter().map(|b| {
                    serde_json::json!({
                        "call_position": b.call_position,
                        "label": b.label,
                        "step_index": b.step_index,
                        "placeholder": b.placeholder,
                    })
                }).collect();
                map.insert("arg_bindings".into(), serde_json::json!(bindings_json));
            }
        }

        serde_json::to_string_pretty(&serde_json::Value::Object(map))
            .map_err(|e| anyhow::anyhow!(e))
    }

        pub fn toml_filename(&self) -> String {
        format!("{}.toml", self.verb.replace(' ', "_"))
    }
}

// ── Registry facade ──────────────────────────────────────────────────────────

pub struct Registry {
    pub curated_dir: PathBuf,
    pub staging_dir: PathBuf,
}

impl Registry {
    pub fn new(curated_dir: PathBuf, staging_dir: PathBuf) -> Self {
        Self { curated_dir, staging_dir }
    }

    /// Write a CommandEntry to the appropriate TOML directory.
    pub fn write_toml(&self, entry: &CommandEntry) -> Result<()> {
        let dir = if entry.is_curated() { &self.curated_dir } else { &self.staging_dir };
        let path = dir.join(entry.toml_filename());
        let content = entry.to_toml_file()?;
        std::fs::write(&path, content)
            .with_context(|| format!("Writing {}", path.display()))?;
        tracing::debug!("Wrote {}", path.display());
        Ok(())
    }

    /// Load all entries from a TOML directory (curated or staging).
    pub fn load_all_from_dir(&self, dir: &Path) -> Result<Vec<CommandEntry>> {
        let mut entries = vec![];
        if !dir.exists() { return Ok(entries); }

        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            match self.load_toml(&path) {
                Ok(cmd) => entries.push(cmd),
                Err(e) => tracing::warn!("Skipping {}: {}", path.display(), e),
            }
        }
        Ok(entries)
    }

    /// Parse a `CommandEntry` from a JSON string (same format as on-disk TOML files).
    /// Used by the redb store to deserialize entries without a file path.
    pub fn parse_entry_json(json: &str) -> Result<CommandEntry> {
        let v: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| anyhow::anyhow!("Cannot parse entry JSON: {}", e))?;
        Self::entry_from_value(v)
    }

    fn load_toml(&self, path: &Path) -> Result<CommandEntry> {
        let raw = std::fs::read_to_string(path)?;
        let v: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|_| anyhow::anyhow!("Cannot parse registry file"))?;
        Self::entry_from_value(v)
    }

    fn entry_from_value(v: serde_json::Value) -> Result<CommandEntry> {
        let table = v.as_object().context("Expected JSON object")?;

        let verb = table.get("verb").unwrap_or(&serde_json::Value::Null).as_str().context("verb")?.to_string();
        let id_str = table.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let id = Uuid::parse_str(id_str).unwrap_or_else(|_| Uuid::new_v4());
        let confidence = table.get("confidence").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
        let usage_count = table.get("usage_count").and_then(|v| v.as_i64()).unwrap_or(0) as u32;
        let tags: Vec<String> = table.get("tags")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();

        let source_str = table.get("source").and_then(|v| v.as_str()).unwrap_or("declarative");
        let source = parse_source(source_str);

        let created_at = table.get("created_at")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);

        let kind = if let Some(expansion) = table.get("expansion").and_then(|v| v.as_str()) {
            let arg_names = table.get("arg_names")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default();
            EntryKind::Alias { expansion: expansion.to_string(), arg_names }
        } else {
            let steps = table.get("steps")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter().enumerate().filter_map(|(i, sv)| {
                        let step: StepKind = serde_json::from_value(sv.clone()).ok()?;
                        let index = sv.get("index").and_then(|v| v.as_u64()).unwrap_or(i as u64) as usize;
                        let description = sv.get("description").and_then(|v| v.as_str()).map(|s| s.to_string());
                        let depends_on = sv.get("depends_on")
                            .and_then(|v| v.as_array())
                            .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as usize)).collect())
                            .unwrap_or_default();
                        Some(ProcStep { index, step, description, depends_on })
                    }).collect()
                })
                .unwrap_or_default();
            let arg_bindings = table.get("arg_bindings")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter().filter_map(|bv| {
                        Some(ArgBind {
                            call_position: bv.get("call_position").and_then(|v| v.as_u64())? as usize,
                            label: bv.get("label").and_then(|v| v.as_str())?.to_string(),
                            step_index: bv.get("step_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                            placeholder: bv.get("placeholder").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        })
                    }).collect()
                })
                .unwrap_or_default();
            EntryKind::Procedure {
                steps,
                arg_bindings,
                description: table.get("description").and_then(|v| v.as_str()).map(|s| s.to_string()),
            }
        };

        Ok(CommandEntry { id, verb, kind, tags, source, usage_count, created_at, last_used: None, confidence, embedding: None })
    }
}

fn parse_source(s: &str) -> EntrySource {
    if s.starts_with("inferred:") {
        let parts: Vec<&str> = s.splitn(3, ':').collect();
        EntrySource::Inferred {
            model: parts.get(1).unwrap_or(&"unknown").to_string(),
            confidence: parts.get(2).and_then(|v| v.parse().ok()).unwrap_or(0.5),
        }
    } else if s.starts_with("confirmed:") {
        let conf: f32 = s.splitn(2, ':').nth(1).and_then(|v| v.parse().ok()).unwrap_or(0.9);
        EntrySource::Confirmed { original_confidence: conf }
    } else if s == "wrapped" {
        EntrySource::Wrapped { session_indices: vec![] }
    } else {
        EntrySource::Declarative
    }
}
