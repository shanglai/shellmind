//! Agennect agent dispatcher — `sm a.n …`.
//!
//! Preconfigured for https://agennect.com (override via `AGENNECT_BASE_URL`).
//! Entry point is the Lister agent: `sm a.n lister <goal>` returns top-used +
//! semantically-matched agents from the marketplace. The result is cached so
//! follow-ups can address agents by index (`sm a.n 2 …`) or by name
//! (`sm a.n pipelinebot …`) without re-querying Lister.
//!
//! Dispatch path is LLM-free: HTTP POST + JSON parsing only. The "which agent
//! should I run" decision lives entirely in the verb name the user typed.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::platform::paths::ShellmindPaths;

/// Built-in Lister agent id — the entry point to the marketplace.
pub const LISTER_AGENT_NAME: &str = "agennect-lister-v1";

fn base_url() -> String {
    std::env::var("AGENNECT_BASE_URL")
        .unwrap_or_else(|_| "https://agennect.com".to_string())
}

fn agent_url(agent_name: &str) -> String {
    format!("{}/agents/{}", base_url(), agent_name)
}

fn card_url(agent_name: &str) -> String {
    format!("{}/.well-known/agent.json", agent_url(agent_name))
}

fn run_url(agent_name: &str) -> String {
    format!("{}/run", agent_url(agent_name))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn http_client(timeout_secs: u64) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent(concat!("shellmind/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(Into::into)
}

// ── Schemas (tolerant of unknown fields) ─────────────────────────────────────

/// Minimal A2A agent card — only the fields shellmind reads. Unknown fields
/// in the wire payload are ignored.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentCard {
    pub name: String,
    #[serde(default)] pub url: Option<String>,
    #[serde(default)] pub description: Option<String>,
    #[serde(default)] pub version: Option<String>,
    #[serde(default)] pub capabilities: Vec<String>,
}

/// One entry in Lister's `top_used` or `matches` array.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentMatch {
    pub name: String,
    #[serde(default)] pub provider: Option<String>,
    #[serde(default)] pub description: Option<String>,
    #[serde(default)] pub capabilities: Vec<String>,
    #[serde(default)] pub cost_label: Option<String>,
    #[serde(default)] pub hosting: Option<String>,
    #[serde(default)] pub agent_card_url: Option<String>,
}

/// Lister's structured payload, found at `result.parts[1].data`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListerData {
    #[serde(default)] pub top_used: Vec<AgentMatch>,
    #[serde(default)] pub matches: Vec<AgentMatch>,
}

/// Persisted snapshot of the most recent Lister response, used to resolve
/// follow-up calls like `sm a.n 0 <args>` or `sm a.n <name> <args>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedListing {
    pub goal: String,
    pub cached_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)] pub top_used: Vec<AgentMatch>,
    #[serde(default)] pub matches: Vec<AgentMatch>,
}

impl CachedListing {
    fn path(paths: &ShellmindPaths) -> PathBuf {
        paths.agennect_dir().join("last_listing.json")
    }

    pub fn save(&self, paths: &ShellmindPaths) -> Result<()> {
        std::fs::create_dir_all(paths.agennect_dir())?;
        std::fs::write(Self::path(paths), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn load(paths: &ShellmindPaths) -> Result<Option<Self>> {
        let p = Self::path(paths);
        if !p.exists() { return Ok(None); }
        let raw = std::fs::read_to_string(&p)?;
        Ok(Some(serde_json::from_str(&raw)?))
    }

    /// Flat index: `top_used` comes first, then `matches`.
    pub fn at_index(&self, idx: usize) -> Option<&AgentMatch> {
        self.top_used.iter().chain(self.matches.iter()).nth(idx)
    }

    pub fn find_by_name(&self, name: &str) -> Option<&AgentMatch> {
        self.top_used.iter()
            .chain(self.matches.iter())
            .find(|m| m.name.eq_ignore_ascii_case(name))
    }
}

// ── Card fetching with disk cache (24h TTL) ──────────────────────────────────

pub async fn fetch_card(agent_name: &str, paths: &ShellmindPaths) -> Result<AgentCard> {
    let cache_path = paths.agennect_dir().join(format!("card_{}.json", sanitize(agent_name)));

    if let Ok(meta) = std::fs::metadata(&cache_path) {
        if let Ok(modified) = meta.modified() {
            let age_secs = modified.elapsed().unwrap_or_default().as_secs();
            if age_secs < 86_400 {
                if let Ok(raw) = std::fs::read_to_string(&cache_path) {
                    if let Ok(card) = serde_json::from_str::<AgentCard>(&raw) {
                        tracing::debug!("Using cached card for {} (age {}s)", agent_name, age_secs);
                        return Ok(card);
                    }
                }
            }
        }
    }

    let url = card_url(agent_name);
    tracing::debug!("Fetching agent card: {}", url);
    let client = http_client(8)?;
    let resp = client.get(&url).send().await
        .with_context(|| format!("GET {}", url))?;
    if !resp.status().is_success() {
        anyhow::bail!("Card fetch failed: HTTP {} from {}", resp.status(), url);
    }
    let text = resp.text().await?;
    let card: AgentCard = serde_json::from_str(&text)
        .with_context(|| format!("Parsing agent card from {}", url))?;

    std::fs::create_dir_all(paths.agennect_dir())?;
    let _ = std::fs::write(&cache_path, &text);
    Ok(card)
}

// ── Run an agent's /run endpoint ─────────────────────────────────────────────

async fn run_agent(agent_name: &str, goal: &str) -> Result<serde_json::Value> {
    let url = run_url(agent_name);
    tracing::debug!("POST {} (goal={:?})", url, goal);
    let client = http_client(30)?;
    let resp = client.post(&url)
        .json(&serde_json::json!({ "goal": goal }))
        .send().await
        .with_context(|| format!("POST {}", url))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("HTTP {} from {}: {}", status, url, body.chars().take(400).collect::<String>());
    }
    resp.json().await.with_context(|| format!("Parsing JSON from {}", url))
}

/// Extract Lister's structured payload (`result.parts[1].data`).
fn extract_lister_data(resp: &serde_json::Value) -> Result<ListerData> {
    let parts = resp.get("result")
        .and_then(|r| r.get("parts"))
        .and_then(|p| p.as_array())
        .context("Lister response missing result.parts array")?;
    let part = parts.get(1)
        .context("Lister response result.parts[1] missing (expected the structured data part)")?;
    let data = part.get("data")
        .context("Lister response result.parts[1].data missing")?;
    serde_json::from_value(data.clone())
        .context("Parsing ListerData from result.parts[1].data")
}

// ── Top-level dispatch (called from cli/mod.rs) ──────────────────────────────

pub async fn dispatch(rest: &[String], paths: &ShellmindPaths) -> Result<()> {
    let Some(first) = rest.first() else {
        print_help();
        return Ok(());
    };

    match first.as_str() {
        "help" | "--help" | "-h" => { print_help(); return Ok(()); }
        "list" | "last" => return show_last_listing(paths),
        "refresh" => {
            let _ = fetch_card(LISTER_AGENT_NAME, paths).await?;
            eprintln!("  \x1b[32m✓\x1b[0m Refreshed Lister card.");
            return Ok(());
        }
        _ => {}
    }

    let rest_args = &rest[1..];

    // Numeric index → resolve via cached listing
    if let Ok(idx) = first.parse::<usize>() {
        let listing = CachedListing::load(paths)?
            .context("No cached listing. Run `sm a.n lister <goal>` first.")?;
        let entry = listing.at_index(idx)
            .with_context(|| format!("Index {} out of range (0..{})",
                idx, listing.top_used.len() + listing.matches.len()))?
            .clone();
        eprintln!("  \x1b[90m[{}]\x1b[0m → {}", idx, entry.name);
        return run_named_agent(&entry.name, rest_args, paths).await;
    }

    // Named agent — Lister is the only one with a baked-in pretty presenter
    if first.eq_ignore_ascii_case("lister") {
        run_lister(rest_args, paths).await
    } else {
        run_named_agent(first, rest_args, paths).await
    }
}

async fn run_lister(args: &[String], paths: &ShellmindPaths) -> Result<()> {
    let goal = args.join(" ");
    if goal.is_empty() {
        anyhow::bail!("Usage: sm a.n lister <goal text...>");
    }

    eprintln!("  \x1b[90m→\x1b[0m Asking Lister: \x1b[33m{:?}\x1b[0m", goal);
    let _card = fetch_card(LISTER_AGENT_NAME, paths).await?;
    let resp = run_agent(LISTER_AGENT_NAME, &goal).await?;
    let listing = extract_lister_data(&resp)?;

    let cached = CachedListing {
        goal: goal.clone(),
        cached_at: chrono::Utc::now(),
        top_used: listing.top_used.clone(),
        matches: listing.matches.clone(),
    };
    cached.save(paths)?;

    present_listing(&listing);
    Ok(())
}

async fn run_named_agent(agent_name: &str, args: &[String], paths: &ShellmindPaths) -> Result<()> {
    let goal = args.join(" ");
    if goal.is_empty() {
        anyhow::bail!("Usage: sm a.n {} <args...>", agent_name);
    }

    eprintln!("  \x1b[90m→\x1b[0m {} \x1b[33m{:?}\x1b[0m", agent_name, goal);
    let _card = fetch_card(agent_name, paths).await?;
    let resp = run_agent(agent_name, &goal).await?;
    print_generic_response(&resp);
    Ok(())
}

// ── Presentation ─────────────────────────────────────────────────────────────

fn present_listing(listing: &ListerData) {
    let mut idx = 0usize;
    if !listing.top_used.is_empty() {
        println!("\n  \x1b[1mTOP USED\x1b[0m");
        for m in &listing.top_used { print_match(idx, m); idx += 1; }
    }
    if !listing.matches.is_empty() {
        println!("\n  \x1b[1mMATCHES\x1b[0m");
        for m in &listing.matches { print_match(idx, m); idx += 1; }
    }
    if listing.top_used.is_empty() && listing.matches.is_empty() {
        println!("\n  No matches.");
        return;
    }
    println!("\n  Pick: \x1b[33msm a.n <index>\x1b[0m <args...>");
    println!("  Or:   \x1b[33msm a.n <name>\x1b[0m  <args...>\n");
}

fn print_match(idx: usize, m: &AgentMatch) {
    let provider = m.provider.as_deref().unwrap_or("?");
    let host = m.hosting.as_deref()
        .map(|h| format!(" \x1b[90m[{}]\x1b[0m", h))
        .unwrap_or_default();
    println!("  \x1b[33m[{}]\x1b[0m \x1b[1m{}\x1b[0m \x1b[90m({})\x1b[0m{}",
        idx, m.name, provider, host);
    if let Some(desc) = &m.description {
        if !desc.is_empty() { println!("      {}", desc); }
    }
    if !m.capabilities.is_empty() {
        println!("      \x1b[90mcaps:\x1b[0m {}", m.capabilities.join(", "));
    }
    if let Some(cost) = &m.cost_label {
        if !cost.is_empty() { println!("      \x1b[90mcost:\x1b[0m {}", cost); }
    }
    if let Some(card_url) = &m.agent_card_url {
        if !card_url.is_empty() { println!("      \x1b[90mcard:\x1b[0m {}", card_url); }
    }
}

/// Generic A2A response printer. Tries the common shapes in order:
/// 1. `result.parts[*].text`  — concatenate all text parts
/// 2. `result.text`           — single text field
/// 3. pretty-printed JSON     — fallback
fn print_generic_response(resp: &serde_json::Value) {
    if let Some(parts) = resp.get("result").and_then(|r| r.get("parts")).and_then(|p| p.as_array()) {
        let mut any_printed = false;
        for part in parts {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                println!("{}", text);
                any_printed = true;
            }
        }
        if any_printed { return; }
    }
    if let Some(text) = resp.get("result").and_then(|r| r.get("text")).and_then(|t| t.as_str()) {
        println!("{}", text);
        return;
    }
    if let Some(text) = resp.get("text").and_then(|t| t.as_str()) {
        println!("{}", text);
        return;
    }
    println!("{}", serde_json::to_string_pretty(resp).unwrap_or_default());
}

fn show_last_listing(paths: &ShellmindPaths) -> Result<()> {
    let cached = CachedListing::load(paths)?
        .context("No cached listing. Run `sm a.n lister <goal>` first.")?;
    eprintln!("  Last listing for goal: \x1b[33m{:?}\x1b[0m", cached.goal);
    eprintln!("  Cached at: \x1b[90m{}\x1b[0m",
        cached.cached_at.format("%Y-%m-%d %H:%M UTC"));
    present_listing(&ListerData {
        top_used: cached.top_used,
        matches: cached.matches,
    });
    Ok(())
}

fn print_help() {
    eprintln!(r#"
sm a.n — Agennect agent dispatcher (preconfigured for https://agennect.com)

USAGE
  sm a.n lister <goal text...>         Search Agennect's marketplace via Lister
  sm a.n <index> <args...>             Run agent at <index> from the last listing
  sm a.n <agent-name> <args...>        Run a specific agent by name
  sm a.n list                          Show the last cached listing
  sm a.n refresh                       Re-fetch the Lister card

EXAMPLES
  sm a.n lister "find me a pipeline builder"
  sm a.n 0 make_pipeline "ingest then sample"
  sm a.n pipelinebot make_pipeline "..."

ENV
  AGENNECT_BASE_URL                    Override the default https://agennect.com
"#);
}
