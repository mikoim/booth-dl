//! Test-extracts archives without writing anything to disk.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};
use zip::result::ZipError;

/// Result of an archive test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveStatus {
    /// Every entry decompressed and passed its checksum.
    #[serde(rename = "ok")]
    Ok,
    /// Password protected; contents were not verified.
    #[serde(rename = "encrypted")]
    Encrypted,
    /// The archive could not be read completely.
    #[serde(rename = "corrupt")]
    Corrupt,
    /// Not an archive, or a format we don't test (e.g. rar).
    #[serde(rename = "n/a")]
    NotApplicable,
}

const SEVEN_ZIP_MAGIC: &[u8] = &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
const GZIP_MAGIC: &[u8] = &[0x1F, 0x8B];
const TAR_MAGIC_OFFSET: usize = 257;

/// Detects the archive format from the file's magic bytes and reads every entry.
///
/// Returns `Err` only when the file itself cannot be read.
pub fn test_archive(path: &Path) -> io::Result<ArchiveStatus> {
    let mut header = [0u8; 512];
    let header_len = read_up_to(&mut File::open(path)?, &mut header)?;
    let header = &header[..header_len];

    let file = File::open(path)?;
    let status = if header.starts_with(b"PK\x03\x04") || header.starts_with(b"PK\x05\x06") {
        test_zip(file)
    } else if header.starts_with(SEVEN_ZIP_MAGIC) {
        test_7z(path)
    } else if header.starts_with(GZIP_MAGIC) {
        test_gzip(file)
    } else if header.get(TAR_MAGIC_OFFSET..TAR_MAGIC_OFFSET + 5) == Some(b"ustar") {
        test_tar(file)
    } else {
        ArchiveStatus::NotApplicable
    };
    Ok(status)
}

fn read_up_to(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

fn test_zip(file: File) -> ArchiveStatus {
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return ArchiveStatus::Corrupt;
    };
    let mut encrypted = false;
    for i in 0..archive.len() {
        match archive.by_index(i) {
            // Reading an entry to the end verifies its CRC32.
            Ok(mut entry) => {
                if io::copy(&mut entry, &mut io::sink()).is_err() {
                    return ArchiveStatus::Corrupt;
                }
            }
            Err(ZipError::UnsupportedArchive(msg)) if msg == ZipError::PASSWORD_REQUIRED => {
                encrypted = true;
            }
            Err(_) => return ArchiveStatus::Corrupt,
        }
    }
    if encrypted {
        ArchiveStatus::Encrypted
    } else {
        ArchiveStatus::Ok
    }
}

fn test_7z(path: &Path) -> ArchiveStatus {
    use sevenz_rust2::{ArchiveReader, Error, Password};

    let result = ArchiveReader::open(path, Password::empty()).and_then(|mut reader| {
        reader.for_each_entries(|_, data| {
            io::copy(data, &mut io::sink())?;
            Ok(true)
        })
    });
    match result {
        Ok(()) => ArchiveStatus::Ok,
        Err(Error::PasswordRequired | Error::MaybeBadPassword(_)) => ArchiveStatus::Encrypted,
        Err(_) => ArchiveStatus::Corrupt,
    }
}

/// gzip's CRC32 covers the whole payload, so decompressing it all is enough
/// for `.tar.gz` and `.unitypackage`.
fn test_gzip(file: File) -> ArchiveStatus {
    let mut decoder = flate2::read::MultiGzDecoder::new(file);
    match io::copy(&mut decoder, &mut io::sink()) {
        Ok(_) => ArchiveStatus::Ok,
        Err(_) => ArchiveStatus::Corrupt,
    }
}

fn test_tar(file: File) -> ArchiveStatus {
    match read_tar(file) {
        Ok(()) => ArchiveStatus::Ok,
        Err(_) => ArchiveStatus::Corrupt,
    }
}

fn read_tar(file: File) -> io::Result<()> {
    for entry in tar::Archive::new(file).entries()? {
        io::copy(&mut entry?, &mut io::sink())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use super::*;

    fn payload() -> Vec<u8> {
        (0..20_000u32).flat_map(|i| i.to_le_bytes()).collect()
    }

    fn write_zip(dir: &Path, encrypted: bool) -> PathBuf {
        let path = dir.join(if encrypted { "e.zip" } else { "a.zip" });
        let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
        let mut options = zip::write::SimpleFileOptions::default();
        if encrypted {
            options = options.with_aes_encryption(zip::AesMode::Aes256, "pw");
        }
        writer.start_file("data.bin", options).unwrap();
        writer.write_all(&payload()).unwrap();
        writer.finish().unwrap();
        path
    }

    fn write_tar_gz(dir: &Path) -> PathBuf {
        let path = dir.join("a.unitypackage");
        let gz = flate2::write::GzEncoder::new(
            File::create(&path).unwrap(),
            flate2::Compression::default(),
        );
        let mut builder = tar::Builder::new(gz);
        let data = payload();
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "data.bin", &data[..])
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        path
    }

    fn write_7z(dir: &Path, encrypted: bool) -> PathBuf {
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("data.bin"), payload()).unwrap();
        let path = dir.join(if encrypted { "e.7z" } else { "a.7z" });
        if encrypted {
            sevenz_rust2::compress_to_path_encrypted(&src, &path, "pw".into()).unwrap();
        } else {
            sevenz_rust2::compress_to_path(&src, &path).unwrap();
        }
        path
    }

    /// Flips one byte in the middle of the file.
    fn corrupt(path: &Path) {
        let mut bytes = std::fs::read(path).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn zip_ok_corrupt_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let ok = write_zip(dir.path(), false);
        assert_eq!(test_archive(&ok).unwrap(), ArchiveStatus::Ok);
        corrupt(&ok);
        assert_eq!(test_archive(&ok).unwrap(), ArchiveStatus::Corrupt);
        let encrypted = write_zip(dir.path(), true);
        assert_eq!(test_archive(&encrypted).unwrap(), ArchiveStatus::Encrypted);
    }

    #[test]
    fn seven_zip_ok_corrupt_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let ok = write_7z(dir.path(), false);
        assert_eq!(test_archive(&ok).unwrap(), ArchiveStatus::Ok);
        corrupt(&ok);
        assert_eq!(test_archive(&ok).unwrap(), ArchiveStatus::Corrupt);
        let encrypted = write_7z(dir.path(), true);
        assert_eq!(test_archive(&encrypted).unwrap(), ArchiveStatus::Encrypted);
    }

    #[test]
    fn tar_gz_ok_and_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tar_gz(dir.path());
        assert_eq!(test_archive(&path).unwrap(), ArchiveStatus::Ok);
        corrupt(&path);
        assert_eq!(test_archive(&path).unwrap(), ArchiveStatus::Corrupt);
    }

    #[test]
    fn plain_tar_is_tested() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.tar");
        let mut builder = tar::Builder::new(File::create(&path).unwrap());
        let data = payload();
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "data.bin", &data[..])
            .unwrap();
        builder.finish().unwrap();
        assert_eq!(test_archive(&path).unwrap(), ArchiveStatus::Ok);
    }

    #[test]
    fn other_files_are_not_applicable() {
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("a.pdf");
        std::fs::write(&pdf, b"%PDF-1.4\n...").unwrap();
        assert_eq!(test_archive(&pdf).unwrap(), ArchiveStatus::NotApplicable);
        let rar = dir.path().join("a.rar");
        std::fs::write(&rar, b"Rar!\x1a\x07\x01\x00").unwrap();
        assert_eq!(test_archive(&rar).unwrap(), ArchiveStatus::NotApplicable);
        let empty = dir.path().join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(test_archive(&empty).unwrap(), ArchiveStatus::NotApplicable);
    }
}
