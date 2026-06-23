use anyhow::{Context, Result};
use redb::{Database, TableDefinition, ReadableTable};
use std::path::Path;

use crate::registry::{CommandEntry, Registry};

const ENTRIES: TableDefinition<&str, &str> = TableDefinition::new("entries");

pub struct RegistryStore {
    db: Database,
}

impl RegistryStore {
    /// Open (or create) a store at `path`. Falls back gracefully — callers should
    /// treat an Err as "store unavailable, use file scan".
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)
            .with_context(|| format!("Opening redb at {}", path.display()))?;
        Ok(Self { db })
    }

    pub fn get(&self, verb: &str) -> Result<Option<CommandEntry>> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(ENTRIES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let Some(guard) = table.get(verb)? else { return Ok(None) };
        let json = guard.value().to_string();
        drop(guard);
        let entry = Registry::parse_entry_json(&json)
            .with_context(|| format!("Deserializing entry for '{}'", verb))?;
        Ok(Some(entry))
    }

    pub fn put(&self, entry: &CommandEntry) -> Result<()> {
        let json = entry.to_toml_file()?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ENTRIES)?;
            table.insert(entry.verb.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn delete(&self, verb: &str) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ENTRIES)?;
            table.remove(verb)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn all_entries(&self) -> Result<Vec<CommandEntry>> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(ENTRIES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut entries = vec![];
        for result in table.iter()? {
            let (_, value) = result?;
            let json = value.value().to_string();
            match Registry::parse_entry_json(&json) {
                Ok(e) => entries.push(e),
                Err(err) => tracing::warn!("Skipping malformed store entry: {}", err),
            }
        }
        Ok(entries)
    }

    pub fn all_verbs(&self) -> Result<Vec<String>> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(ENTRIES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut verbs = vec![];
        for result in table.iter()? {
            let (key, _) = result?;
            verbs.push(key.value().to_string());
        }
        Ok(verbs)
    }

    /// Wipe and rebuild the store from a TOML directory. Returns entry count.
    pub fn rebuild_from_toml_dir(&self, dir: &Path, registry: &Registry) -> Result<usize> {
        let entries = registry.load_all_from_dir(dir)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ENTRIES)?;
            // Clear existing
            let verbs: Vec<String> = table.iter()?
                .filter_map(|r| r.ok().map(|(k, _)| k.value().to_string()))
                .collect();
            for v in &verbs { table.remove(v.as_str())?; }
            // Insert all
            for entry in &entries {
                let json = entry.to_toml_file()?;
                table.insert(entry.verb.as_str(), json.as_str())?;
            }
        }
        write_txn.commit()?;
        Ok(entries.len())
    }
}
