//! Downloading one file: presigned URL, Range resume, MD5 and ETag check.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use indicatif::ProgressBar;
use md5::{Digest, Md5};
use ureq::Body;
use ureq::http::Response;

use crate::client::{BoothClient, is_fatal};
use crate::state::Checksum;

const MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, PartialEq, Eq)]
pub struct Downloaded {
    pub size: u64,
    /// ETag without quotes.
    pub etag: String,
    pub checksum: Checksum,
}

/// The MD5 of a complete download did not match the server's ETag.
#[derive(Debug)]
struct ChecksumMismatch {
    expected: String,
    actual: String,
}

impl std::fmt::Display for ChecksumMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MD5 {} does not match ETag {}",
            self.actual, self.expected
        )
    }
}

impl std::error::Error for ChecksumMismatch {}

/// ETag and total size of a server file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteFile {
    etag: String,
    size: u64,
}

impl RemoteFile {
    fn from_response(response: &Response<Body>) -> Result<RemoteFile> {
        let header = |name| response.headers().get(name).and_then(|v| v.to_str().ok());
        let etag = header("etag")
            .map(|v| v.trim().trim_matches('"').to_string())
            .context("response has no ETag")?;
        let size = match header("content-range") {
            // "bytes 500000-965438/965439"
            Some(range) => range.rsplit('/').next().and_then(|s| s.parse().ok()),
            None => header("content-length").and_then(|s| s.parse().ok()),
        }
        .context("response has no usable Content-Length or Content-Range")?;
        Ok(RemoteFile { etag, size })
    }
}

/// Downloads the file `key` (see [`crate::library::LibraryFile::key`]) to `dest`, resuming `<dest>.part` if possible.
///
/// `<dest>.part.etag` stores `<etag> <size>` of the partial download, so a
/// resume only appends bytes of the same server file.
pub fn download_file(
    client: &BoothClient,
    key: &str,
    dest: &Path,
    progress: &ProgressBar,
) -> Result<Downloaded> {
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let part = with_suffix(dest, ".part");
    let meta = with_suffix(dest, ".part.etag");
    let mut attempt = 1;
    loop {
        match try_download(client, key, dest, &part, &meta, progress) {
            Ok(downloaded) => return Ok(downloaded),
            Err(e) => {
                if e.downcast_ref::<ChecksumMismatch>().is_some() {
                    discard(&part, &meta)?;
                }
                if is_fatal(&e) || attempt >= MAX_ATTEMPTS {
                    return Err(e);
                }
                progress.suspend(|| eprintln!("retrying {}: {e:#}", dest.display()));
                attempt += 1;
            }
        }
    }
}

fn try_download(
    client: &BoothClient,
    key: &str,
    dest: &Path,
    part: &Path,
    meta: &Path,
    progress: &ProgressBar,
) -> Result<Downloaded> {
    let part_len = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    let saved = read_meta(meta).filter(|saved| part_len <= saved.size);
    let remote = match saved {
        // An earlier attempt received every byte but failed afterwards.
        Some(saved) if part_len == saved.size => saved,
        Some(saved) => fetch(client, key, part, meta, Some(saved), part_len, progress)?,
        None => fetch(client, key, part, meta, None, 0, progress)?,
    };

    let written = fs::metadata(part)?.len();
    if written != remote.size {
        bail!("incomplete download: {written} of {} bytes", remote.size);
    }
    let checksum = if is_md5(&remote.etag) {
        let actual = md5_file(part)?;
        if actual != remote.etag {
            return Err(anyhow!(ChecksumMismatch {
                expected: remote.etag,
                actual,
            }));
        }
        Checksum::Md5Ok
    } else {
        Checksum::SizeOnly
    };
    fs::rename(part, dest).with_context(|| format!("rename to {}", dest.display()))?;
    let _ = fs::remove_file(meta);
    Ok(Downloaded {
        size: remote.size,
        etag: remote.etag,
        checksum,
    })
}

/// Requests the file (from `offset` when resuming `saved`) and writes the body to `part`.
fn fetch(
    client: &BoothClient,
    key: &str,
    part: &Path,
    meta: &Path,
    saved: Option<RemoteFile>,
    offset: u64,
    progress: &ProgressBar,
) -> Result<RemoteFile> {
    let url = client.resolve_download_url(key)?;
    let response = client.get_file(&url, offset)?;
    let status = response.status().as_u16();
    if status != 200 && status != 206 {
        bail!("download {key}: unexpected HTTP {status}");
    }
    let remote = RemoteFile::from_response(&response)?;
    let mut file = if status == 206 && saved.as_ref() == Some(&remote) {
        progress.set_position(offset);
        OpenOptions::new().append(true).open(part)?
    } else if status == 200 {
        // Truncate the data before recording its ETag: a crash in between must
        // not leave old bytes that a later resume would treat as this file's.
        let file = File::create(part)?;
        fs::write(meta, format!("{} {}", remote.etag, remote.size))?;
        progress.set_position(0);
        file
    } else {
        discard(part, meta)?;
        bail!("the file changed on the server since the partial download");
    };
    progress.set_length(remote.size);
    copy_with_progress(response.into_body().into_reader(), &mut file, progress)?;
    file.sync_all()?;
    Ok(remote)
}

/// Reads the server ETag of a downloadable with a one-byte request.
pub fn remote_etag(client: &BoothClient, key: &str) -> Result<String> {
    let url = client.resolve_download_url(key)?;
    // Presigned URLs are signed for GET only, so HEAD is not an option.
    let response = client.get_first_byte(&url)?;
    match response.status().as_u16() {
        200 | 206 => Ok(RemoteFile::from_response(&response)?.etag),
        status => bail!("download {key}: unexpected HTTP {status}"),
    }
}

fn read_meta(meta: &Path) -> Option<RemoteFile> {
    let text = fs::read_to_string(meta).ok()?;
    let (etag, size) = text.trim().split_once(' ')?;
    Some(RemoteFile {
        etag: etag.to_string(),
        size: size.parse().ok()?,
    })
}

fn discard(part: &Path, meta: &Path) -> io::Result<()> {
    for path in [part, meta] {
        match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn copy_with_progress(
    mut reader: impl Read,
    file: &mut File,
    progress: &ProgressBar,
) -> io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        file.write_all(&buf[..n])?;
        progress.inc(n as u64);
    }
}

/// Hex MD5 of a file.
pub fn md5_file(path: &Path) -> io::Result<String> {
    let mut hasher = Md5::new();
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// True for a plain MD5 ETag; multipart uploads look like `<hex>-<parts>`.
fn is_md5(etag: &str) -> bool {
    etag.len() == 32 && etag.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::test_server::{Reply, Request, TestServer};

    fn content() -> Vec<u8> {
        (0..50_000u32).flat_map(|i| i.to_le_bytes()).collect()
    }

    fn md5_of(bytes: &[u8]) -> String {
        Md5::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn range_start(request: &Request) -> Option<u64> {
        request
            .headers
            .get("range")?
            .strip_prefix("bytes=")?
            .trim_end_matches('-')
            .parse()
            .ok()
    }

    /// 200 with the whole body, or 206 when the request has a Range header.
    fn file_reply(request: &Request, body: &[u8], etag: &str) -> Reply {
        match range_start(request) {
            Some(start) => Reply::new(206)
                .header("ETag", &format!("\"{etag}\""))
                .header(
                    "Content-Range",
                    &format!("bytes {start}-{}/{}", body.len() - 1, body.len()),
                )
                .body(&body[start as usize..]),
            None => Reply::new(200)
                .header("ETag", &format!("\"{etag}\""))
                .body(body),
        }
    }

    /// Serves `/downloadables/<id>` as a redirect to `/file`, and `/file` via `file`.
    /// `file` gets the request and how many `/file` requests came before it.
    fn start(file: impl Fn(&Request, usize) -> Reply + Send + Sync + 'static) -> TestServer {
        let count = Mutex::new(0);
        TestServer::start(move |request| {
            if request.path.starts_with("/downloadables/") {
                let location = format!("http://{}/file", request.headers["host"]);
                return Reply::new(302).header("Location", &location);
            }
            let mut count = count.lock().unwrap();
            let reply = file(request, *count);
            *count += 1;
            reply
        })
    }

    fn file_requests(server: &TestServer) -> Vec<Request> {
        server
            .requests()
            .into_iter()
            .filter(|r| r.path == "/file")
            .collect()
    }

    #[test]
    fn downloads_and_verifies_md5() {
        let body = content();
        let etag = md5_of(&body);
        let server = start({
            let (body, etag) = (body.clone(), etag.clone());
            move |request, _| file_reply(request, &body, &etag)
        });
        let client = BoothClient::for_test("s", &server.url);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("files").join("a.bin");

        let downloaded = download_file(&client, "9", &dest, &ProgressBar::hidden()).unwrap();

        assert_eq!(
            downloaded,
            Downloaded {
                size: body.len() as u64,
                etag,
                checksum: Checksum::Md5Ok
            }
        );
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert!(!with_suffix(&dest, ".part").exists());
        assert!(!with_suffix(&dest, ".part.etag").exists());
    }

    #[test]
    fn multipart_etag_is_checked_by_size_only() {
        let body = content();
        let server = start({
            let body = body.clone();
            move |request, _| file_reply(request, &body, "0123456789abcdef0123456789abcdef-3")
        });
        let client = BoothClient::for_test("s", &server.url);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.bin");
        let downloaded = download_file(&client, "9", &dest, &ProgressBar::hidden()).unwrap();
        assert_eq!(downloaded.checksum, Checksum::SizeOnly);
    }

    #[test]
    fn resumes_after_a_dropped_connection() {
        let body = content();
        let etag = md5_of(&body);
        let server = start({
            let (body, etag) = (body.clone(), etag.clone());
            move |request, n| {
                let reply = file_reply(request, &body, &etag);
                if n == 0 { reply.cut_after(1000) } else { reply }
            }
        });
        let client = BoothClient::for_test("s", &server.url);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.bin");

        download_file(&client, "9", &dest, &ProgressBar::hidden()).unwrap();

        assert_eq!(fs::read(&dest).unwrap(), body);
        let requests = file_requests(&server);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].headers["range"], "bytes=1000-");
    }

    #[test]
    fn restarts_when_the_server_file_changed() {
        let old = content();
        let new: Vec<u8> = content().into_iter().rev().collect();
        let new_etag = md5_of(&new);
        let server = start({
            let (old, new, new_etag) = (old.clone(), new.clone(), new_etag.clone());
            move |request, n| match n {
                0 => file_reply(request, &old, &md5_of(&old)).cut_after(1000),
                _ => file_reply(request, &new, &new_etag),
            }
        });
        let client = BoothClient::for_test("s", &server.url);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.bin");

        let downloaded = download_file(&client, "9", &dest, &ProgressBar::hidden()).unwrap();

        assert_eq!(downloaded.etag, new_etag);
        assert_eq!(fs::read(&dest).unwrap(), new);
        // Cut download, rejected resume (206 with another ETag), fresh download.
        assert_eq!(file_requests(&server).len(), 3);
    }

    #[test]
    fn restarts_when_the_server_ignores_range() {
        let body = content();
        let etag = md5_of(&body);
        let server = start({
            let (body, etag) = (body.clone(), etag.clone());
            move |_, n| {
                let reply = Reply::new(200)
                    .header("ETag", &format!("\"{etag}\""))
                    .body(body.clone());
                if n == 0 { reply.cut_after(1000) } else { reply }
            }
        });
        let client = BoothClient::for_test("s", &server.url);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.bin");
        download_file(&client, "9", &dest, &ProgressBar::hidden()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
    }

    #[test]
    fn retries_an_expired_presigned_url() {
        let body = content();
        let etag = md5_of(&body);
        let server = start({
            let (body, etag) = (body.clone(), etag.clone());
            move |request, n| {
                if n == 0 {
                    Reply::new(403)
                } else {
                    file_reply(request, &body, &etag)
                }
            }
        });
        let client = BoothClient::for_test("s", &server.url);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.bin");
        download_file(&client, "9", &dest, &ProgressBar::hidden()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
    }

    #[test]
    fn gives_up_after_repeated_checksum_mismatches() {
        let body = content();
        let server = start({
            let body = body.clone();
            move |request, _| file_reply(request, &body, "0123456789abcdef0123456789abcdef")
        });
        let client = BoothClient::for_test("s", &server.url);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.bin");

        let err = download_file(&client, "9", &dest, &ProgressBar::hidden()).unwrap_err();

        assert!(err.to_string().contains("does not match ETag"));
        assert!(!dest.exists());
        assert!(!with_suffix(&dest, ".part").exists());
        assert_eq!(file_requests(&server).len(), MAX_ATTEMPTS as usize);
    }

    #[test]
    fn remote_etag_uses_a_one_byte_range() {
        let body = content();
        let server = start({
            let body = body.clone();
            move |_, _| {
                Reply::new(206)
                    .header("ETag", "\"abc\"")
                    .header("Content-Range", &format!("bytes 0-0/{}", body.len()))
                    .body(&body[..1])
            }
        });
        let client = BoothClient::for_test("s", &server.url);
        assert_eq!(remote_etag(&client, "9").unwrap(), "abc");
        assert_eq!(file_requests(&server)[0].headers["range"], "bytes=0-0");
    }
}
