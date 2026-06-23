//! Scheduled procedures — cron-driven invocation of registered verbs.
//!
//! Schedules are persisted as JSON-in-TOML files in
//! `~/.config/shellmind/schedules/<name>.toml` (same on-disk shape as the
//! registry). Each holds the verb to invoke, frozen positional args, a
//! 5-field cron expression, and `last_run` / `next_run` timestamps that the
//! `sm schedule run` one-shot updates as it fires jobs.
//!
//! `sm schedule run` is meant to be wired into an external scheduler (Unix
//! cron, systemd timers, Windows Task Scheduler). Running it every minute is
//! the simplest setup; runs are idempotent against the persisted next_run.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A single scheduled invocation of a registered verb.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: Uuid,
    pub name: String,
    pub verb: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// 5-field cron expression: `m h dom mon dow`.
    pub cron: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub last_run: Option<DateTime<Utc>>,
    #[serde(default)]
    pub next_run: Option<DateTime<Utc>>,
}

fn default_true() -> bool { true }

impl Schedule {
    /// Construct a new schedule, validating the cron expression and
    /// computing `next_run` from `now`.
    pub fn new(name: &str, verb: &str, args: Vec<String>, cron: &str) -> Result<Self> {
        let _ = parse_cron(cron)?;
        let now = Utc::now();
        let mut sched = Self {
            id: Uuid::new_v4(),
            name: name.to_string(),
            verb: verb.to_string(),
            args,
            cron: cron.to_string(),
            enabled: true,
            created_at: now,
            last_run: None,
            next_run: None,
        };
        sched.next_run = sched.compute_next_after(now)?;
        Ok(sched)
    }

    /// Compute the next fire time strictly after `after`. Returns None if the
    /// cron expression has no future matches (rare — past-only schedules).
    pub fn compute_next_after(&self, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        let sched = parse_cron(&self.cron)?;
        Ok(sched.after(&after).next())
    }

    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        if !self.enabled { return false; }
        match self.next_run {
            Some(nr) => now >= nr,
            None => false,
        }
    }

    pub fn filename(&self) -> String {
        format!("{}.toml", self.name)
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(Into::into)
    }
}

/// Parse a 5- or 6-field cron expression. The `cron` crate's native format is
/// 6-field (seconds prepended) — we accept either, normalising 5-field input
/// to `0 <user expr>` so the schedule fires at the top of the minute.
pub fn parse_cron(s: &str) -> Result<cron::Schedule> {
    let trimmed = s.trim();
    let field_count = trimmed.split_whitespace().count();
    let canonical = match field_count {
        5 => format!("0 {}", trimmed),
        6 => trimmed.to_string(),
        n => anyhow::bail!("cron expression must have 5 or 6 fields, got {}", n),
    };
    cron::Schedule::from_str(&canonical)
        .with_context(|| format!("Invalid cron expression: '{}'", s))
}

// ── Persistence ──────────────────────────────────────────────────────────────

pub struct ScheduleStore {
    dir: PathBuf,
}

impl ScheduleStore {
    pub fn new(dir: &Path) -> Self {
        Self { dir: dir.to_path_buf() }
    }

    pub fn load_all(&self) -> Result<Vec<Schedule>> {
        if !self.dir.exists() { return Ok(vec![]); }
        let mut out = vec![];
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() { continue; }
            if path.extension().and_then(|e| e.to_str()) != Some("toml") { continue; }
            match std::fs::read_to_string(&path) {
                Ok(raw) => match serde_json::from_str::<Schedule>(&raw) {
                    Ok(s) => out.push(s),
                    Err(e) => tracing::warn!("Skipping malformed schedule {}: {}", path.display(), e),
                },
                Err(e) => tracing::warn!("Cannot read {}: {}", path.display(), e),
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn get(&self, name: &str) -> Result<Option<Schedule>> {
        let path = self.dir.join(format!("{}.toml", name));
        if !path.exists() { return Ok(None); }
        let raw = std::fs::read_to_string(&path)?;
        let sched: Schedule = serde_json::from_str(&raw)
            .with_context(|| format!("Parsing schedule '{}'", name))?;
        Ok(Some(sched))
    }

    pub fn put(&self, schedule: &Schedule) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(schedule.filename());
        std::fs::write(&path, schedule.to_json()?)
            .with_context(|| format!("Writing schedule to {}", path.display()))?;
        Ok(())
    }

    pub fn delete(&self, name: &str) -> Result<bool> {
        let path = self.dir.join(format!("{}.toml", name));
        if !path.exists() { return Ok(false); }
        std::fs::remove_file(&path)
            .with_context(|| format!("Deleting {}", path.display()))?;
        Ok(true)
    }
}
