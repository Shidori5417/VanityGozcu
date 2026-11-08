//! Comment-preserving edits of the config file: the `[[targets]]` list, the
//! `[auto_claim] enabled` switch and the per-target `auto_claim` flag.
//!
//! Only the engine process calls these, so the file has a single writer. Writes go
//! through a temp file plus rename so a crash never leaves a half-written config.

use std::path::{Path, PathBuf};

use thiserror::Error;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value, value};

#[derive(Debug, Error)]
pub enum ConfigEditError {
    #[error("cannot read config file {path}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot write config file {path}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("config file is not valid TOML: {0}")]
    Parse(#[from] toml_edit::TomlError),
    #[error("`targets` in the config file is not a [[targets]] table list; edit it by hand")]
    UnsupportedShape,
    #[error("target {0:?} is already in the config file")]
    AlreadyPresent(String),
    #[error("target {0:?} is not in the config file")]
    NotFound(String),
}

fn load(path: &Path) -> Result<DocumentMut, ConfigEditError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(ConfigEditError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    Ok(raw.parse()?)
}

fn save(path: &Path, doc: &DocumentMut) -> Result<(), ConfigEditError> {
    let write_err = |source| ConfigEditError::Write {
        path: path.to_owned(),
        source,
    };
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, doc.to_string()).map_err(write_err)?;
    std::fs::rename(&tmp, path).map_err(write_err)
}

fn code_of(table: &Table) -> Option<String> {
    table
        .get("code")
        .and_then(Item::as_str)
        .map(str::to_lowercase)
}

fn targets_mut(doc: &mut DocumentMut) -> Result<&mut ArrayOfTables, ConfigEditError> {
    doc.entry("targets")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or(ConfigEditError::UnsupportedShape)
}

/// Appends a `[[targets]]` entry for the (already normalized) `code`, creating the
/// file if it does not exist.
pub fn add_target(path: &Path, code: &str) -> Result<(), ConfigEditError> {
    let mut doc = load(path)?;
    let targets = targets_mut(&mut doc)?;
    if targets.iter().any(|t| code_of(t).as_deref() == Some(code)) {
        return Err(ConfigEditError::AlreadyPresent(code.to_owned()));
    }
    let mut table = Table::new();
    table.insert("code", value(code));
    targets.push(table);
    save(path, &doc)
}

/// Removes the `[[targets]]` entry whose code equals `code` (case-insensitively).
pub fn remove_target(path: &Path, code: &str) -> Result<(), ConfigEditError> {
    let mut doc = load(path)?;
    let targets = targets_mut(&mut doc)?;
    let index = targets
        .iter()
        .position(|t| code_of(t).as_deref() == Some(code))
        .ok_or_else(|| ConfigEditError::NotFound(code.to_owned()))?;
    targets.remove(index);
    save(path, &doc)
}

/// Sets `key = flag`, keeping the position and any comment of an existing entry.
fn set_bool(table: &mut dyn toml_edit::TableLike, key: &str, flag: bool) {
    let mut new = Value::from(flag);
    if let Some(old) = table.get(key).and_then(Item::as_value) {
        *new.decor_mut() = old.decor().clone();
    }
    table.insert(key, Item::Value(new));
}

/// Sets `[auto_claim] enabled`, creating the table when missing.
pub fn set_auto_claim_enabled(path: &Path, enabled: bool) -> Result<(), ConfigEditError> {
    let mut doc = load(path)?;
    let section = doc
        .entry("auto_claim")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or(ConfigEditError::UnsupportedShape)?;
    set_bool(section, "enabled", enabled);
    save(path, &doc)
}

/// Sets `auto_claim = flag` on the `[[targets]]` entry for `code`.
pub fn set_target_auto_claim(path: &Path, code: &str, flag: bool) -> Result<(), ConfigEditError> {
    let mut doc = load(path)?;
    let targets = targets_mut(&mut doc)?;
    let table = targets
        .iter_mut()
        .find(|t| code_of(t).as_deref() == Some(code))
        .ok_or_else(|| ConfigEditError::NotFound(code.to_owned()))?;
    set_bool(table, "auto_claim", flag);
    save(path, &doc)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::{Config, Secrets};

    const SAMPLE: &str = "\
# my watcher
[general]
default_interval_secs = 30  # keep this comment

# first target
[[targets]]
code = \"Alpha\"   # inline note
priority = \"high\"

[[targets]]
code = \"beta\"
";

    fn write_sample() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, SAMPLE).unwrap();
        (dir, path)
    }

    fn codes(path: &Path) -> Vec<String> {
        let raw = std::fs::read_to_string(path).unwrap();
        let loaded = Config::parse(&raw, Secrets::default()).unwrap();
        loaded.config.targets.into_iter().map(|t| t.code).collect()
    }

    #[test]
    fn add_preserves_comments_and_existing_entries() {
        let (_dir, path) = write_sample();
        add_target(&path, "gamma").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        for needle in [
            "# my watcher",
            "# keep this comment",
            "# first target",
            "# inline note",
            "priority = \"high\"",
        ] {
            assert!(raw.contains(needle), "lost {needle:?} in:\n{raw}");
        }
        assert_eq!(codes(&path), ["alpha", "beta", "gamma"]);
    }

    #[test]
    fn remove_drops_only_the_matching_entry() {
        let (_dir, path) = write_sample();
        remove_target(&path, "alpha").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# my watcher") && raw.contains("# keep this comment"));
        assert_eq!(codes(&path), ["beta"]);
    }

    #[test]
    fn add_then_remove_round_trips() {
        let (_dir, path) = write_sample();
        add_target(&path, "gamma").unwrap();
        remove_target(&path, "gamma").unwrap();
        assert_eq!(codes(&path), ["alpha", "beta"]);
    }

    #[test]
    fn duplicates_and_missing_are_rejected() {
        let (_dir, path) = write_sample();
        assert!(matches!(
            add_target(&path, "alpha"),
            Err(ConfigEditError::AlreadyPresent(_))
        ));
        assert!(matches!(
            remove_target(&path, "nope"),
            Err(ConfigEditError::NotFound(_))
        ));
        assert_eq!(codes(&path), ["alpha", "beta"]);
    }

    #[test]
    fn creates_the_file_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.toml");
        add_target(&path, "abc").unwrap();
        assert_eq!(codes(&path), ["abc"]);
    }

    #[test]
    fn inline_targets_array_is_refused_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        let raw = "targets = [{ code = \"abc\" }]\n";
        std::fs::write(&path, raw).unwrap();
        assert!(matches!(
            add_target(&path, "xyz"),
            Err(ConfigEditError::UnsupportedShape)
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
    }

    #[test]
    fn auto_claim_switch_is_created_and_updated_in_place() {
        let (_dir, path) = write_sample();
        set_auto_claim_enabled(&path, true).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("# my watcher") && raw.contains("# first target"),
            "{raw}"
        );
        let doc: DocumentMut = raw.parse().unwrap();
        assert_eq!(doc["auto_claim"]["enabled"].as_bool(), Some(true));
        assert_eq!(codes(&path), ["alpha", "beta"]);

        set_auto_claim_enabled(&path, false).unwrap();
        let doc: DocumentMut = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(doc["auto_claim"]["enabled"].as_bool(), Some(false));
    }

    #[test]
    fn auto_claim_switch_keeps_neighbours_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        let raw = "[auto_claim]\nenabled = false   # deliberately off\ntarget_guild_id = \"1\"\nmax_attempts = 3\n";
        std::fs::write(&path, raw).unwrap();
        set_auto_claim_enabled(&path, true).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            out,
            "[auto_claim]\nenabled = true   # deliberately off\ntarget_guild_id = \"1\"\nmax_attempts = 3\n"
        );
    }

    #[test]
    fn target_auto_claim_flag_is_set_and_replaced() {
        let (_dir, path) = write_sample();
        set_target_auto_claim(&path, "beta", false).unwrap();
        set_target_auto_claim(&path, "alpha", true).unwrap();
        set_target_auto_claim(&path, "beta", true).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("# inline note") && raw.contains("# first target"),
            "{raw}"
        );
        let loaded = Config::parse(&raw, Secrets::default()).unwrap();
        let flags: Vec<_> = loaded.config.targets.iter().map(|t| t.auto_claim).collect();
        assert_eq!(flags, [Some(true), Some(true)]);
        assert!(matches!(
            set_target_auto_claim(&path, "nope", true),
            Err(ConfigEditError::NotFound(_))
        ));
    }
}
