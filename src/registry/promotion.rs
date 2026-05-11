use anyhow::Result;
use chrono::Utc;

use super::{CommandEntry, EntrySource, Registry};

/// Thresholds governing automatic promotion from staging → curated.
pub struct PromotionPolicy {
    /// How many confirmed uses before auto-promote candidate is flagged
    pub auto_promote_uses: u32,
    /// Days without use before staging entry is flagged for review
    pub decay_days: i64,
    /// Confidence below which an entry is never auto-promoted
    pub min_confidence: f32,
}

impl Default for PromotionPolicy {
    fn default() -> Self {
        Self {
            auto_promote_uses: 5,
            decay_days: 30,
            min_confidence: 0.70,
        }
    }
}

pub struct PromotionResult {
    pub verb: String,
    pub action: PromotionAction,
}

pub enum PromotionAction {
    Promoted,
    FlaggedForReview { reason: String },
    Decayed { days_inactive: i64 },
    Skipped,
}

impl Registry {
    /// Run promotion pass across all staging entries.
    /// Returns a list of actions taken (caller prints summary).
    pub fn run_promotion_pass(&self, policy: &PromotionPolicy) -> Result<Vec<PromotionResult>> {
        let staging = self.load_all_from_dir(&self.staging_dir)?;
        let mut results = vec![];
        let now = Utc::now();

        for mut entry in staging {
            let action = evaluate_entry(&entry, policy, now);

            match &action {
                PromotionAction::Promoted => {
                    entry.confidence = 1.0;
                    entry.source = EntrySource::Confirmed {
                        original_confidence: match &entry.source {
                            EntrySource::Inferred { confidence, .. } => *confidence,
                            _ => 0.9,
                        },
                    };
                    // Write to curated, remove from staging
                    self.write_toml(&entry)?;
                    let staging_path = self.staging_dir.join(entry.toml_filename());
                    if staging_path.exists() {
                        std::fs::remove_file(&staging_path)?;
                    }
                    tracing::info!("Promoted '{}' to curated", entry.verb);
                }
                PromotionAction::Decayed { .. } => {
                    // Mark confidence decay in staging file
                    let mut decayed = entry.clone();
                    decayed.confidence *= 0.8; // decay factor
                    if decayed.confidence < 0.3 {
                        // Too low — remove entirely
                        let p = self.staging_dir.join(entry.toml_filename());
                        if p.exists() { std::fs::remove_file(&p)?; }
                        tracing::info!("Expired staging entry '{}'", entry.verb);
                    } else {
                        self.write_toml(&decayed)?;
                    }
                }
                _ => {}
            }

            results.push(PromotionResult { verb: entry.verb.clone(), action });
        }

        Ok(results)
    }

    /// Explicitly promote a single verb from staging to curated.
    /// Called by `sm confirm <verb>`.
    pub fn confirm(&self, verb: &str) -> Result<bool> {
        let staging = self.load_all_from_dir(&self.staging_dir)?;
        if let Some(mut entry) = staging.into_iter().find(|e| e.verb == verb) {
            entry.confidence = 1.0;
            entry.source = EntrySource::Confirmed {
                original_confidence: match &entry.source {
                    EntrySource::Inferred { confidence, .. } => *confidence,
                    _ => 0.9,
                },
            };
            self.write_toml(&entry)?;
            let staging_path = self.staging_dir.join(entry.toml_filename());
            if staging_path.exists() {
                std::fs::remove_file(&staging_path)?;
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Demote a curated entry back to staging (e.g. after a bad wrap).
    pub fn demote(&self, verb: &str) -> Result<bool> {
        let curated = self.load_all_from_dir(&self.curated_dir)?;
        if let Some(mut entry) = curated.into_iter().find(|e| e.verb == verb) {
            entry.confidence = 0.8;
            entry.source = EntrySource::Inferred {
                model: "manual_demote".to_string(),
                confidence: 0.8,
            };
            self.write_toml(&entry)?;
            let curated_path = self.curated_dir.join(entry.toml_filename());
            if curated_path.exists() {
                std::fs::remove_file(&curated_path)?;
            }
            return Ok(true);
        }
        Ok(false)
    }
}

fn evaluate_entry(
    entry: &CommandEntry,
    policy: &PromotionPolicy,
    now: chrono::DateTime<Utc>,
) -> PromotionAction {
    // Check decay first
    let last_activity = entry.last_used.unwrap_or(entry.created_at);
    let days_inactive = (now - last_activity).num_days();

    if days_inactive >= policy.decay_days {
        return PromotionAction::Decayed { days_inactive };
    }

    // Skip if below confidence floor
    if entry.confidence < policy.min_confidence {
        return PromotionAction::FlaggedForReview {
            reason: format!("Confidence {:.2} below threshold {:.2}", entry.confidence, policy.min_confidence),
        };
    }

    // Auto-promote if used enough times
    if entry.usage_count >= policy.auto_promote_uses {
        return PromotionAction::Promoted;
    }

    PromotionAction::Skipped
}
