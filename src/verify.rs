//! The `verify` command: re-check downloaded files without network access.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::archive::{self, ArchiveStatus};
use crate::download::md5_file;
use crate::state::{Checksum, State};

#[derive(Debug, Default)]
pub struct VerifySummary {
    pub checked: usize,
    pub problems: Vec<String>,
}

pub fn run(out: &Path) -> Result<VerifySummary> {
    let mut state = State::load(out)?;
    let mut summary = VerifySummary::default();
    for record in state.files.values_mut() {
        let path = out.join(&record.path);
        eprintln!("Verifying {}", record.path.display());
        summary.checked += 1;
        let Ok(meta) = fs::metadata(&path) else {
            summary
                .problems
                .push(format!("{}: missing", path.display()));
            continue;
        };
        if meta.len() != record.size {
            summary.problems.push(format!(
                "{}: size {} differs from the recorded {}",
                path.display(),
                meta.len(),
                record.size
            ));
            continue;
        }
        if record.checksum == Checksum::Md5Ok {
            let actual = md5_file(&path).with_context(|| format!("read {}", path.display()))?;
            if actual != record.etag {
                summary.problems.push(format!(
                    "{}: MD5 {actual} differs from the server's {}",
                    path.display(),
                    record.etag
                ));
                continue;
            }
        }
        record.archive =
            archive::test_archive(&path).with_context(|| format!("read {}", path.display()))?;
        if record.archive == ArchiveStatus::Corrupt {
            summary
                .problems
                .push(format!("{}: archive is corrupt", path.display()));
        }
    }
    state.save(out)?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use super::*;
    use crate::state::FileRecord;

    fn record(path: &str, bytes: &[u8]) -> FileRecord {
        FileRecord {
            item_id: 1,
            path: PathBuf::from(path),
            size: bytes.len() as u64,
            etag: {
                use md5::{Digest, Md5};
                Md5::digest(bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect()
            },
            checksum: Checksum::Md5Ok,
            archive: ArchiveStatus::NotApplicable,
            downloaded_at: 0,
        }
    }

    fn zip_bytes() -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut buf);
        // Stored, so that a flipped byte in the middle lands in entry data.
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("a.txt", options).unwrap();
        writer.write_all(&[7u8; 10_000]).unwrap();
        writer.finish().unwrap();
        buf.into_inner()
    }

    #[test]
    fn reports_missing_changed_and_corrupt_files() {
        let out = tempfile::tempdir().unwrap();
        let mut state = State::default();

        fs::write(out.path().join("good.pdf"), b"good").unwrap();
        state.files.insert("1".into(), record("good.pdf", b"good"));

        state.files.insert("2".into(), record("missing.pdf", b"x"));

        fs::write(out.path().join("changed.pdf"), b"evil").unwrap();
        state
            .files
            .insert("3".into(), record("changed.pdf", b"good"));

        // A zip whose recorded MD5 matches, but whose entry data is broken.
        let mut broken = zip_bytes();
        let mid = broken.len() / 2;
        broken[mid] ^= 0xFF;
        fs::write(out.path().join("broken.zip"), &broken).unwrap();
        state
            .files
            .insert("4".into(), record("broken.zip", &broken));

        let zip = zip_bytes();
        fs::write(out.path().join("ok.zip"), &zip).unwrap();
        state.files.insert("5".into(), record("ok.zip", &zip));
        state.save(out.path()).unwrap();

        let summary = run(out.path()).unwrap();

        assert_eq!(summary.checked, 5);
        assert_eq!(summary.problems.len(), 3, "{:?}", summary.problems);
        let state = State::load(out.path()).unwrap();
        assert_eq!(state.files["4"].archive, ArchiveStatus::Corrupt);
        assert_eq!(state.files["5"].archive, ArchiveStatus::Ok);
    }
}
