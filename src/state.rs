//! Persistent record of saved items and downloaded files (`<out>/.booth-dl/state.json`).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::archive::ArchiveStatus;

const STATE_VERSION: u32 = 1;

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    /// Keyed by item ID.
    pub items: BTreeMap<u64, ItemRecord>,
    /// Keyed by download key (`123`, or `123?type=flac` for music tracks).
    pub files: BTreeMap<String, FileRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemRecord {
    /// Item directory, relative to the output directory.
    pub dir: PathBuf,
    /// Unix time when the sales page was last saved.
    pub page_saved_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRecord {
    pub item_id: u64,
    /// File path, relative to the output directory.
    pub path: PathBuf,
    pub size: u64,
    /// Server ETag without quotes.
    pub etag: String,
    pub checksum: Checksum,
    pub archive: ArchiveStatus,
    /// Unix time of the download.
    pub downloaded_at: u64,
}

/// How a downloaded file was checked against the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Checksum {
    /// MD5 matched the ETag.
    Md5Ok,
    /// The ETag is not an MD5 (multipart upload); only the size was checked.
    SizeOnly,
}

impl State {
    /// Location of the state file below the output directory.
    pub fn path(out: &Path) -> PathBuf {
        out.join(".booth-dl").join("state.json")
    }

    /// Loads the state, or returns an empty one if the file doesn't exist.
    pub fn load(out: &Path) -> Result<State> {
        let path = Self::path(out);
        match fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(State {
                version: STATE_VERSION,
                ..State::default()
            }),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Writes the state atomically (temp file + rename).
    pub fn save(&self, out: &Path) -> Result<()> {
        let path = Self::path(out);
        let dir = path.parent().expect("state path has a parent");
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(())
    }

    /// True if the file is recorded and present on disk with the recorded size.
    pub fn is_complete(&self, out: &Path, key: &str) -> bool {
        self.files.get(key).is_some_and(|record| {
            fs::metadata(out.join(&record.path)).is_ok_and(|meta| meta.len() == record.size)
        })
    }
}

/// Current Unix time in seconds.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record(path: &str, size: u64) -> FileRecord {
        FileRecord {
            item_id: 1,
            path: PathBuf::from(path),
            size,
            etag: "0123456789abcdef0123456789abcdef".into(),
            checksum: Checksum::Md5Ok,
            archive: ArchiveStatus::NotApplicable,
            downloaded_at: 1_700_000_000,
        }
    }

    #[test]
    fn missing_file_loads_empty_state() {
        let out = tempfile::tempdir().unwrap();
        let state = State::load(out.path()).unwrap();
        assert_eq!(state.version, STATE_VERSION);
        assert!(state.items.is_empty() && state.files.is_empty());
    }

    #[test]
    fn save_and_load_round_trip() {
        let out = tempfile::tempdir().unwrap();
        let mut state = State::load(out.path()).unwrap();
        state.items.insert(
            1,
            ItemRecord {
                dir: "Shop/1_Title".into(),
                page_saved_at: Some(5),
            },
        );
        state
            .files
            .insert("10".into(), sample_record("Shop/1_Title/files/a.zip", 3));
        state.save(out.path()).unwrap();

        let text = fs::read_to_string(out.path().join(".booth-dl/state.json")).unwrap();
        assert!(text.contains(r#""checksum": "md5-ok""#));
        assert!(text.contains(r#""archive": "n/a""#));
        assert_eq!(State::load(out.path()).unwrap(), state);
    }

    #[test]
    fn is_complete_checks_presence_and_size() {
        let out = tempfile::tempdir().unwrap();
        let mut state = State::load(out.path()).unwrap();
        state.files.insert("10".into(), sample_record("a.bin", 3));
        assert!(!state.is_complete(out.path(), "10"), "file missing");
        fs::write(out.path().join("a.bin"), b"ab").unwrap();
        assert!(!state.is_complete(out.path(), "10"), "size differs");
        fs::write(out.path().join("a.bin"), b"abc").unwrap();
        assert!(state.is_complete(out.path(), "10"));
        assert!(!state.is_complete(out.path(), "11"), "not recorded");
    }
}
