//! 4-stage resolution pipeline
//!
//! Stage 1: Exact verb match        — curated first, then staging
//! Stage 2: Prefix / Levenshtein    — typo tolerance, abbreviations
//! Stage 3: Embedding similarity    — cosine, boosted by source tag
//!          └─ Disambiguator        — re-rank by args + session, prompt if ambiguous
//! Stage 4: Model intent inference  — few-shot, result → staging

use anyhow::Result;

use crate::disambiguator::{
    self, DisambiguationResult, ScoredCandidate, AUTOSELECT_CEILING, AUTOSELECT_GAP,
};
use crate::embedder::{EmbeddingSource, EmbeddingStore, embed_text_bow};
use crate::model_client::ModelClient;
use crate::registry::{CommandEntry, EntryKind, EntrySource, Registry};
use crate::session::SessionEntry;

pub const EMBED_DIM: usize = 512;
pub const SIMILARITY_THRESHOLD: f32 = 0.72; // slightly lower — disambiguator handles false positives
pub const TOP_K: usize = 5;

// ── Resolution result ─────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Resolution {
    /// Exact verb match
    Exact { entry: CommandEntry, expanded: String },
    /// Similarity / prefix hit, possibly after disambiguation
    Similar { entry: CommandEntry, expanded: String, score: f32 },
    /// User selected from disambiguation prompt
    Disambiguated { entry: CommandEntry, expanded: String, score: f32, from_n: usize },
    /// Model inferred — stored in staging
    Inferred { expanded: String, confidence: f32, stored_as: String },
    /// Nothing matched or user cancelled disambiguation
    Passthrough { raw: String },
}

impl Resolution {
    pub fn as_shell_str(&self) -> &str {
        match self {
            Resolution::Exact { expanded, .. }
            | Resolution::Similar { expanded, .. }
            | Resolution::Disambiguated { expanded, .. }
            | Resolution::Inferred { expanded, .. } => expanded,
            Resolution::Passthrough { raw } => raw,
        }
    }

    pub fn is_passthrough(&self) -> bool {
        matches!(self, Resolution::Passthrough { .. })
    }

    pub fn matched_verb(&self) -> Option<&str> {
        match self {
            Resolution::Exact { entry, .. }
            | Resolution::Similar { entry, .. }
            | Resolution::Disambiguated { entry, .. } => Some(&entry.verb),
            _ => None,
        }
    }
}

// ── Resolver ──────────────────────────────────────────────────────────────────

pub struct Resolver {
    pub registry: Registry,
    pub embeddings: EmbeddingStore,
    pub model: ModelClient,
    entries: Vec<CommandEntry>,
}

impl Resolver {
    pub fn new(registry: Registry, embeddings: EmbeddingStore, model: ModelClient) -> Self {
        Self { registry, embeddings, model, entries: vec![] }
    }

    pub fn refresh(&mut self) -> Result<()> {
        let mut all = self.registry.load_all_from_dir(&self.registry.curated_dir)?;
        all.extend(self.registry.load_all_from_dir(&self.registry.staging_dir)?);
        self.entries = all;
        Ok(())
    }

    /// Main resolution entry point.
    /// `session` — recent history passed in from cli for disambiguator context.
    pub async fn resolve(
        &mut self,
        raw_input: &str,
        session: &[&SessionEntry],
    ) -> Result<Resolution> {
        self.refresh()?;

        let (verb, args) = split_verb_args(raw_input);

        // ── Stage 1: Exact match ──────────────────────────────────────────────
        if let Some(entry) = self.exact_match(&verb) {
            let expanded = expand_entry(&entry, &args)?;
            return Ok(Resolution::Exact { entry, expanded });
        }

        // ── Stage 2: Prefix / Levenshtein ────────────────────────────────────
        {
            let prefix_candidates = self.prefix_candidates(&verb);
            if !prefix_candidates.is_empty() {
                match disambiguator::evaluate(prefix_candidates, &args, session) {
                    DisambiguationResult::Clear(c) => {
                        let expanded = expand_entry(&c.entry, &args)?;
                        return Ok(Resolution::Similar {
                            entry: c.entry,
                            expanded,
                            score: c.final_score,
                        });
                    }
                    DisambiguationResult::Ambiguous(candidates) => {
                        if let Some(idx) = disambiguator::prompt(&candidates, raw_input) {
                            let chosen = &candidates[idx];
                            let expanded = expand_entry(&chosen.entry, &args)?;
                            return Ok(Resolution::Disambiguated {
                                entry: chosen.entry.clone(),
                                expanded,
                                score: chosen.final_score,
                                from_n: candidates.len(),
                            });
                        }
                        // User cancelled — fall through to similarity search
                    }
                    DisambiguationResult::Passthrough => {}
                }
            }
        }

        // ── Stage 3: Embedding similarity ────────────────────────────────────
        if self.embeddings.len() > 0 {
            let query_vec = embed_text_bow(raw_input, EMBED_DIM);
            let hits = self.embeddings.search(&query_vec, TOP_K, SIMILARITY_THRESHOLD);

            // Build ScoredCandidate list from hits
            let sim_candidates: Vec<ScoredCandidate> = hits.iter()
                .filter_map(|(hit_verb, score)| {
                    self.entries.iter()
                        .find(|e| &e.verb == hit_verb)
                        .cloned()
                        .map(|entry| ScoredCandidate::from_similarity(entry, *score))
                })
                .collect();

            if !sim_candidates.is_empty() {
                match disambiguator::evaluate(sim_candidates, &args, session) {
                    DisambiguationResult::Clear(c) => {
                        let expanded = expand_entry(&c.entry, &args)?;
                        tracing::info!(
                            "[similar:{:.3}] {} → {}",
                            c.final_score, c.entry.verb, expanded
                        );
                        return Ok(Resolution::Similar {
                            entry: c.entry,
                            expanded,
                            score: c.final_score,
                        });
                    }
                    DisambiguationResult::Ambiguous(candidates) => {
                        if let Some(idx) = disambiguator::prompt(&candidates, raw_input) {
                            let chosen = &candidates[idx];
                            let expanded = expand_entry(&chosen.entry, &args)?;
                            return Ok(Resolution::Disambiguated {
                                entry: chosen.entry.clone(),
                                expanded,
                                score: chosen.final_score,
                                from_n: candidates.len(),
                            });
                        }
                        // User cancelled — fall through to model
                    }
                    DisambiguationResult::Passthrough => {}
                }
            }

            // ── Stage 4: Model inference ──────────────────────────────────────
            let few_shot: Vec<(&str, &str)> = hits.iter()
                .filter_map(|(v, _)| {
                    self.entries.iter().find(|e| &e.verb == v).map(|e| {
                        let expansion = match &e.kind {
                            EntryKind::Alias { expansion, .. } => expansion.as_str(),
                            EntryKind::Procedure { description, .. } =>
                                description.as_deref().unwrap_or("(procedure)"),
                        };
                        (e.verb.as_str(), expansion)
                    })
                })
                .collect();

            if self.model.is_configured() {
                match self.model.infer(raw_input, &few_shot).await {
                    Ok((expanded, confidence)) => {
                        let staged_verb = derive_verb_from_input(raw_input);
                        let stage_entry = make_inferred_entry(
                            &staged_verb, &expanded, confidence,
                            &self.model.config.model_name,
                        );
                        let vec = embed_text_bow(raw_input, EMBED_DIM);
                        self.embeddings.upsert(&staged_verb, EmbeddingSource::Staging, vec);
                        let _ = self.registry.write_toml(&stage_entry);
                        let _ = self.embeddings.save();
                        return Ok(Resolution::Inferred {
                            expanded, confidence, stored_as: staged_verb,
                        });
                    }
                    Err(e) => tracing::warn!("Model inference failed: {}", e),
                }
            }
        }

        Ok(Resolution::Passthrough { raw: raw_input.to_string() })
    }

    pub fn record_usage(&mut self, verb: &str) -> Result<()> {
        let curated_path = self.registry.curated_dir.join(format!("{}.toml", verb));
        let staging_path = self.registry.staging_dir.join(format!("{}.toml", verb));
        let path = if curated_path.exists() { curated_path } else { staging_path };
        if !path.exists() { return Ok(()); }
        if let Some(entry) = self.entries.iter_mut().find(|e| e.verb == verb) {
            entry.usage_count += 1;
            entry.last_used = Some(chrono::Utc::now());
            self.registry.write_toml(entry)?;
        }
        Ok(())
    }

    pub fn reindex(&mut self) -> Result<usize> {
        let mut count = 0;
        for entry in &self.entries {
            if self.embeddings.len() == 0 || !self.embeddings.has_verb(&entry.verb) {
                let text = entry_to_embed_text(entry);
                let vec = embed_text_bow(&text, EMBED_DIM);
                let source = if entry.is_curated() {
                    if entry.is_wrapped() { EmbeddingSource::Wrapped } else { EmbeddingSource::Curated }
                } else {
                    EmbeddingSource::Staging
                };
                self.embeddings.upsert(&entry.verb, source, vec);
                count += 1;
            }
        }
        if count > 0 { self.embeddings.save()?; }
        Ok(count)
    }

    // ── Private ───────────────────────────────────────────────────────────────

    fn exact_match(&self, verb: &str) -> Option<CommandEntry> {
        self.entries.iter()
            .find(|e| e.verb == verb && e.is_curated())
            .cloned()
            .or_else(|| self.entries.iter().find(|e| e.verb == verb).cloned())
    }

    /// Returns ALL prefix/levenshtein candidates (not just the top one).
    /// The disambiguator decides whether to pick or prompt.
    fn prefix_candidates(&self, verb: &str) -> Vec<ScoredCandidate> {
        if verb.len() < 3 { return vec![]; }

        let mut candidates: Vec<ScoredCandidate> = self.entries.iter()
            .filter_map(|e| {
                let base_score = if e.verb.starts_with(verb) {
                    Some(0.95f32)
                } else if verb.starts_with(&e.verb) {
                    Some(0.90)
                } else {
                    let dist = levenshtein(&e.verb, verb);
                    if dist <= 2 { Some(0.85 - (dist as f32 * 0.03)) } else { None }
                };

                base_score.map(|s| {
                    // Curated entries get a score bump at this stage
                    let score = if e.is_curated() { s } else { s * 0.92 };
                    ScoredCandidate::from_prefix(e.clone(), score)
                })
            })
            .collect();

        candidates.sort_by(|a, b| b.final_score.partial_cmp(&a.final_score).unwrap_or(std::cmp::Ordering::Equal));
        candidates
    }
}

// ── Free functions ────────────────────────────────────────────────────────────

pub fn split_verb_args(input: &str) -> (String, Vec<String>) {
    let mut parts = input.trim().splitn(2, ' ');
    let verb = parts.next().unwrap_or("").to_string();
    let args: Vec<String> = parts.next()
        .map(|r| r.split_whitespace().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    (verb, args)
}

pub fn expand_entry(entry: &CommandEntry, args: &[String]) -> Result<String> {
    match &entry.kind {
        EntryKind::Alias { expansion, arg_names } => {
            let mut result = expansion.clone();
            for (i, name) in arg_names.iter().enumerate() {
                if let Some(val) = args.get(i) {
                    result = result.replace(&format!("{{{}}}", name), val);
                }
            }
            for (i, val) in args.iter().enumerate() {
                result = result.replace(&format!("${}", i + 1), val);
            }
            Ok(result)
        }
        EntryKind::Procedure { .. } => {
            let args_str = args.join(" ");
            Ok(format!("sm __exec {} {}", entry.verb, args_str))
        }
    }
}

fn derive_verb_from_input(input: &str) -> String {
    let words: Vec<&str> = input.split_whitespace().take(3).collect();
    words.join("_")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_lowercase()
}

fn make_inferred_entry(verb: &str, expansion: &str, confidence: f32, model: &str) -> CommandEntry {
    CommandEntry {
        id: uuid::Uuid::new_v4(),
        verb: verb.to_string(),
        kind: EntryKind::Alias { expansion: expansion.to_string(), arg_names: vec![] },
        tags: vec![],
        source: EntrySource::Inferred { model: model.to_string(), confidence },
        usage_count: 0,
        created_at: chrono::Utc::now(),
        last_used: None,
        confidence,
        embedding: None,
    }
}

fn entry_to_embed_text(entry: &CommandEntry) -> String {
    match &entry.kind {
        EntryKind::Alias { expansion, arg_names } =>
            format!("{} {} {}", entry.verb, arg_names.join(" "), expansion),
        EntryKind::Procedure { steps, description, .. } => {
            let descs: Vec<&str> = steps.iter()
                .filter_map(|s| s.description.as_deref()).collect();
            format!("{} {} {}", entry.verb, description.as_deref().unwrap_or(""), descs.join(" "))
        }
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in 0..=m { dp[i][0] = i; }
    for j in 0..=n { dp[0][j] = j; }
    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = if a[i-1] == b[j-1] { dp[i-1][j-1] }
                       else { 1 + dp[i-1][j].min(dp[i][j-1]).min(dp[i-1][j-1]) };
        }
    }
    dp[m][n]
}
