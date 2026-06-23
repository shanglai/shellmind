use anyhow::{Context, Result};
use std::path::Path;

use crate::embedder::{EmbeddingStore, embed_text, EMBED_DIM};
use crate::platform::paths::ShellmindPaths;
use crate::registry::{EntryKind, Registry};

/// Open `path` in the user's preferred editor and block until it exits.
pub fn open_in_editor(path: &Path) -> Result<()> {
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| {
            // Last-resort fallbacks in order
            for e in &["nano", "vi", "notepad"] {
                if which(e) { return e.to_string(); }
            }
            "vi".to_string()
        });

    let status = std::process::Command::new(&editor)
        .arg(path)
        .status()
        .with_context(|| format!("Failed to launch editor '{}'", editor))?;

    if !status.success() {
        anyhow::bail!("Editor '{}' exited with status {}", editor, status);
    }
    Ok(())
}

/// Reload the entry for `verb` from disk and re-embed it.
pub fn reload_and_reindex(verb: &str, paths: &ShellmindPaths) -> Result<()> {
    let registry = Registry::new(
        paths.curated_toml_dir.clone(),
        paths.staging_toml_dir.clone(),
    );

    // Find which store it lives in
    let all = registry.load_all_from_dir(&registry.curated_dir)?;
    let (entry, source_tag) = if let Some(e) = all.iter().find(|e| e.verb == verb).cloned() {
        (e, crate::embedder::EmbeddingSource::Curated)
    } else {
        let staging = registry.load_all_from_dir(&registry.staging_dir)?;
        let e = staging.into_iter().find(|e| e.verb == verb)
            .with_context(|| format!("'{}' not found after edit — was the verb changed?", verb))?;
        let tag = if e.is_wrapped() {
            crate::embedder::EmbeddingSource::Wrapped
        } else {
            crate::embedder::EmbeddingSource::Staging
        };
        (e, tag)
    };

    let embed_str = match &entry.kind {
        EntryKind::Alias { expansion, arg_names } =>
            format!("{} {} {}", verb, arg_names.join(" "), expansion),
        EntryKind::Procedure { description, .. } =>
            format!("{} {}", verb, description.as_deref().unwrap_or("")),
    };

    let vec = embed_text(&embed_str, EMBED_DIM);
    let mut store = EmbeddingStore::load_or_create(&paths.embeddings_bin, EMBED_DIM)?;
    store.upsert(verb, source_tag, vec);
    store.save()?;

    Ok(())
}

/// Returns true if `name` resolves to an executable on PATH.
fn which(name: &str) -> bool {
    std::process::Command::new(if cfg!(windows) { "where" } else { "which" })
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
