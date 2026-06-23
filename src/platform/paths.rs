use std::path::PathBuf;
use anyhow::{Context, Result};

/// All paths shellmind uses, resolved once at startup for the current OS.
#[derive(Debug, Clone)]
pub struct ShellmindPaths {
    /// ~/.config/shellmind (Linux/Mac) or %APPDATA%\shellmind (Windows)
    pub config_dir: PathBuf,
    /// config_dir/registry/curated/   — TOML source of truth
    pub curated_toml_dir: PathBuf,
    /// config_dir/registry/staging/   — TOML staging area
    pub staging_toml_dir: PathBuf,
    /// config_dir/db/curated.redb
    pub curated_db: PathBuf,
    /// config_dir/db/staging.redb
    pub staging_db: PathBuf,
    /// config_dir/db/embeddings.bin   — shared flat embedding store
    pub embeddings_bin: PathBuf,
    /// config_dir/session/history.bin — ring buffer persistence
    pub session_history: PathBuf,
    /// config_dir/schedules/         — scheduled procedure definitions
    pub schedules_dir: PathBuf,
    /// config_dir/shellmind.toml      — user config
    pub user_config: PathBuf,
}

impl ShellmindPaths {
    pub fn resolve() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .context("Cannot locate config directory for this OS")?
            .join("shellmind");

        let registry = config_dir.join("registry");
        let db = config_dir.join("db");

        Ok(Self {
            curated_toml_dir: registry.join("curated"),
            staging_toml_dir: registry.join("staging"),
            curated_db: db.join("curated.redb"),
            staging_db: db.join("staging.redb"),
            embeddings_bin: db.join("embeddings.bin"),
            session_history: config_dir.join("session").join("history.bin"),
            schedules_dir: config_dir.join("schedules"),
            user_config: config_dir.join("shellmind.toml"),
            config_dir,
        })
    }

    /// Create all directories that need to exist at startup.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.config_dir,
            &self.curated_toml_dir,
            &self.staging_toml_dir,
            &self.schedules_dir,
            self.curated_db.parent().unwrap(),
            self.session_history.parent().unwrap(),
        ] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("Failed to create directory: {}", dir.display()))?;
        }
        Ok(())
    }
}
