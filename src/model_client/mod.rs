//! Thin async client for small-model inference.
//! Targets OpenAI-compatible APIs (GPT-4o-mini, Groq, local Ollama, etc.)
//! Config lives in shellmind.toml — no hardcoded keys.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub model_name: String,
    pub api_base: String,       // e.g. "https://api.openai.com/v1"
    pub api_key_env: String,    // env var name — never store key in config
    pub max_tokens: u32,
    pub temperature: f32,
    pub timeout_secs: u64,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            model_name: "gpt-4o-mini".to_string(),
            api_base: "https://api.openai.com/v1".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
            max_tokens: 256,
            temperature: 0.1,   // low — we want deterministic expansions
            timeout_secs: 8,
        }
    }
}

pub struct ModelClient {
    pub config: ModelConfig,
    api_key: Option<String>,
}

impl ModelClient {
    pub fn new(config: ModelConfig) -> Self {
        let api_key = std::env::var(&config.api_key_env).ok();
        Self { config, api_key }
    }

    pub fn is_configured(&self) -> bool {
        self.api_key.is_some()
    }

    /// Infer the shell expansion for a raw input, given few-shot examples.
    /// Returns (expanded_command, confidence 0.0–1.0).
    pub async fn infer(
        &self,
        raw_input: &str,
        few_shot: &[(&str, &str)], // (verb, expansion) pairs
    ) -> Result<(String, f32)> {
        let key = self.api_key.as_deref()
            .context("Model API key not set")?;

        let prompt = build_prompt(raw_input, few_shot);

        // Manual HTTP POST — no reqwest dep needed at this layer
        // We use tokio's built-in TCP + write our own minimal JSON request
        // This keeps us dependency-free for now; swap for reqwest when available.
        let response = self.post_completion(key, &prompt).await?;

        parse_inference_response(&response)
    }

    async fn post_completion(&self, api_key: &str, prompt: &str) -> Result<String> {

        // Parse host from api_base
        let base = self.config.api_base.trim_end_matches('/');
        let url = format!("{}/chat/completions", base);

        // Build JSON body manually (no serde_json macro needed — just string concat)
        let escaped = prompt.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
        let body = format!(
            r#"{{"model":"{}","messages":[{{"role":"system","content":"You are a shell command expander. Respond ONLY with JSON: {{\"cmd\":\"...\",\"confidence\":0.0}}"}},{{"role":"user","content":"{}"}}],"max_tokens":{},"temperature":{}}}"#,
            self.config.model_name,
            escaped,
            self.config.max_tokens,
            self.config.temperature,
        );

        // Use std blocking HTTP for simplicity — avoids TLS complexity without reqwest
        // In production: swap to reqwest or hyper
        let output = std::process::Command::new("curl")
            .args([
                "-s", "-X", "POST",
                &url,
                "-H", &format!("Authorization: Bearer {}", api_key),
                "-H", "Content-Type: application/json",
                "--max-time", &self.config.timeout_secs.to_string(),
                "-d", &body,
            ])
            .output()
            .context("curl not available — install curl or add reqwest dependency")?;

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

fn build_prompt(raw_input: &str, few_shot: &[(&str, &str)]) -> String {
    let mut p = String::from("Translate natural shell commands to their expansions.\n\n");

    if !few_shot.is_empty() {
        p.push_str("Known commands for context:\n");
        for (verb, expansion) in few_shot {
            p.push_str(&format!("  {} → {}\n", verb, expansion));
        }
        p.push('\n');
    }

    p.push_str(&format!(
        "Input: \"{}\"\n\nRespond ONLY with JSON: {{\"cmd\":\"<shell command>\",\"confidence\":<0.0-1.0>}}",
        raw_input
    ));
    p
}

fn parse_inference_response(response: &str) -> Result<(String, f32)> {
    // Extract JSON from OpenAI response — find content field, then parse inner JSON
    // Minimal parsing without serde_json macro to stay dep-free
    let content = extract_content_field(response)
        .context("Could not extract content from model response")?;

    let cmd = extract_json_string(&content, "cmd")
        .context("No 'cmd' field in model response")?;
    let confidence = extract_json_f32(&content, "confidence").unwrap_or(0.6);

    Ok((cmd, confidence))
}

fn extract_content_field(s: &str) -> Option<String> {
    // Look for "content":"..." in the response JSON
    let marker = r#""content":""#;
    let start = s.find(marker)? + marker.len();
    let rest = &s[start..];
    let end = find_unescaped_quote(rest)?;
    Some(rest[..end].replace("\\\"", "\"").replace("\\n", "\n"))
}

fn extract_json_string(s: &str, key: &str) -> Option<String> {
    let marker = format!("\"{}\":\"", key);
    let start = s.find(&marker)? + marker.len();
    let rest = &s[start..];
    let end = find_unescaped_quote(rest)?;
    Some(rest[..end].replace("\\\"", "\""))
}

fn extract_json_f32(s: &str, key: &str) -> Option<f32> {
    let marker = format!("\"{}\":", key);
    let start = s.find(&marker)? + marker.len();
    let rest = s[start..].trim_start();
    // Read digits and decimal point
    let num_end = rest.find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .unwrap_or(rest.len());
    rest[..num_end].parse().ok()
}

fn find_unescaped_quote(s: &str) -> Option<usize> {
    let mut prev_backslash = false;
    for (i, c) in s.char_indices() {
        if c == '"' && !prev_backslash { return Some(i); }
        prev_backslash = c == '\\' && !prev_backslash;
    }
    None
}
