//! The `sync` command: list the library, save sales pages, download and verify files.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::archive::{self, ArchiveStatus};
use crate::client::{BoothClient, is_fatal};
use crate::download;
use crate::item;
use crate::layout::{item_dir_name, sanitize_component, with_id_suffix};
use crate::library::{self, LibraryItem};
use crate::state::{self, Checksum, FileRecord, ItemRecord, State};

pub struct SyncOptions {
    pub out: PathBuf,
    /// Number of parallel downloads.
    pub jobs: usize,
    /// Re-check ETags of downloaded files and refresh sales pages.
    pub check_updates: bool,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub downloaded: usize,
    pub updated: usize,
    pub skipped: usize,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

struct Job {
    /// Download key, see [`crate::library::LibraryFile::key`].
    key: String,
    item_id: u64,
    /// Relative to the output directory.
    path: PathBuf,
    /// Set for complete files: download again only if the server ETag differs from this one.
    recorded_etag: Option<String>,
}

pub fn run(client: &BoothClient, options: &SyncOptions) -> Result<Summary> {
    let mut state = State::load(&options.out)?;
    let library = library::fetch_library(client)?;
    let mut summary = Summary::default();
    for title in &library.unsupported {
        summary.warnings.push(format!(
            "{title}: no supported download buttons in the library"
        ));
    }
    save_pages(client, &library.items, &mut state, options, &mut summary)?;
    let jobs = plan_jobs(&library.items, &state, options, &mut summary);
    run_jobs(client, jobs, state, options, &mut summary)?;
    Ok(summary)
}

fn item_dir(item: &LibraryItem) -> PathBuf {
    PathBuf::from(sanitize_component(&item.shop_name))
        .join(item_dir_name(item.item_id, &item.title))
}

fn save_pages(
    client: &BoothClient,
    items: &[LibraryItem],
    state: &mut State,
    options: &SyncOptions,
    summary: &mut Summary,
) -> Result<()> {
    for item in items {
        let record = state
            .items
            .entry(item.item_id)
            .or_insert_with(|| ItemRecord {
                dir: item_dir(item),
                page_saved_at: None,
            });
        if record.page_saved_at.is_some() && !options.check_updates {
            continue;
        }
        let dir = options.out.join(&record.dir);
        eprintln!("Saving sales page: {}", record.dir.display());
        match item::save_item_page(client, item, &dir) {
            Ok(()) => {
                record.page_saved_at = Some(state::now());
                state.save(&options.out)?;
            }
            Err(e) if is_fatal(&e) => return Err(e),
            Err(e) => summary
                .failures
                .push(format!("{}: sales page: {e:#}", dir.display())),
        }
    }
    Ok(())
}

fn plan_jobs(
    items: &[LibraryItem],
    state: &State,
    options: &SyncOptions,
    summary: &mut Summary,
) -> Vec<Job> {
    let mut used: HashSet<PathBuf> = state.files.values().map(|f| f.path.clone()).collect();
    let mut jobs = Vec::new();
    for item in items {
        let files_dir = state.items[&item.item_id].dir.join("files");
        for file in &item.files {
            let key = file.key();
            let job = match state.files.get(&key) {
                Some(record) => {
                    let complete = state.is_complete(&options.out, &key);
                    if complete && !options.check_updates {
                        summary.skipped += 1;
                        continue;
                    }
                    Job {
                        key,
                        item_id: item.item_id,
                        path: record.path.clone(),
                        recorded_etag: complete.then(|| record.etag.clone()),
                    }
                }
                None => {
                    let mut path = files_dir.join(sanitize_component(&file.name));
                    if used.contains(&path) {
                        path = files_dir.join(with_id_suffix(&file.name, file.downloadable_id));
                    }
                    used.insert(path.clone());
                    Job {
                        key,
                        item_id: item.item_id,
                        path,
                        recorded_etag: None,
                    }
                }
            };
            jobs.push(job);
        }
    }
    jobs
}

fn run_jobs(
    client: &BoothClient,
    jobs: Vec<Job>,
    state: State,
    options: &SyncOptions,
    summary: &mut Summary,
) -> Result<()> {
    let queue = Mutex::new(jobs.into_iter());
    let state = Mutex::new(state);
    let shared_summary = Mutex::new(std::mem::take(summary));
    let fatal: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let multi = MultiProgress::new();

    thread::scope(|scope| {
        for _ in 0..options.jobs.max(1) {
            scope.spawn(|| {
                loop {
                    if fatal.lock().unwrap().is_some() {
                        break;
                    }
                    let Some(job) = queue.lock().unwrap().next() else {
                        break;
                    };
                    let progress = multi.add(progress_bar(&job.path));
                    let result = process(client, &job, &options.out, &progress);
                    progress.finish_and_clear();
                    let record = match result {
                        Ok(Some(record)) => record,
                        Ok(None) => {
                            shared_summary.lock().unwrap().skipped += 1;
                            continue;
                        }
                        Err(e) if is_fatal(&e) => {
                            *fatal.lock().unwrap() = Some(e);
                            break;
                        }
                        Err(e) => {
                            let message = format!("{}: {e:#}", job.path.display());
                            let _ = multi.println(format!("Failed {message}"));
                            shared_summary.lock().unwrap().failures.push(message);
                            continue;
                        }
                    };
                    if let Err(e) = record_file(&state, &options.out, &job.key, &record) {
                        *fatal.lock().unwrap() = Some(e);
                        break;
                    }
                    let updated = job.recorded_etag.is_some();
                    let verb = if updated { "Updated" } else { "Downloaded" };
                    let _ = multi.println(format!("{verb} {}", record.path.display()));
                    let mut summary = shared_summary.lock().unwrap();
                    if updated {
                        summary.updated += 1;
                    } else {
                        summary.downloaded += 1;
                    }
                    summary.warnings.extend(warning(&record));
                }
            });
        }
    });

    *summary = shared_summary.into_inner().unwrap();
    match fatal.into_inner().unwrap() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Downloads and verifies one file. Returns `None` if it is complete and unchanged on the server.
fn process(
    client: &BoothClient,
    job: &Job,
    out: &Path,
    progress: &ProgressBar,
) -> Result<Option<FileRecord>> {
    let dest = out.join(&job.path);
    if let Some(recorded) = &job.recorded_etag {
        if download::remote_etag(client, &job.key)? == *recorded {
            return Ok(None);
        }
        move_to_old(&dest)?;
    }
    let downloaded = download::download_file(client, &job.key, &dest, progress)?;
    let archive =
        archive::test_archive(&dest).with_context(|| format!("read {}", dest.display()))?;
    Ok(Some(FileRecord {
        item_id: job.item_id,
        path: job.path.clone(),
        size: downloaded.size,
        etag: downloaded.etag,
        checksum: downloaded.checksum,
        archive,
        downloaded_at: state::now(),
    }))
}

/// Stores the record and saves the state, so every finished file survives an interruption.
fn record_file(state: &Mutex<State>, out: &Path, key: &str, record: &FileRecord) -> Result<()> {
    let mut state = state.lock().unwrap();
    state.files.insert(key.to_string(), record.clone());
    state.save(out)
}

/// Moves `files/<name>` to `files/.old/<name>.<unix time>`.
fn move_to_old(path: &Path) -> Result<()> {
    let dir = path
        .parent()
        .context("file path has no parent")?
        .join(".old");
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let name = path.file_name().context("file path has no name")?;
    let target = dir.join(format!("{}.{}", name.to_string_lossy(), state::now()));
    fs::rename(path, &target)
        .with_context(|| format!("move {} to {}", path.display(), target.display()))
}

fn warning(record: &FileRecord) -> Option<String> {
    let path = record.path.display();
    match (record.archive, record.checksum) {
        (ArchiveStatus::Corrupt, Checksum::Md5Ok) => Some(format!(
            "{path}: archive is corrupt although its MD5 matches the server (the source file is broken)"
        )),
        (ArchiveStatus::Corrupt, Checksum::SizeOnly) => Some(format!(
            "{path}: archive is corrupt (only the size could be checked)"
        )),
        (ArchiveStatus::Encrypted, _) => Some(format!(
            "{path}: archive is password protected; contents not verified"
        )),
        _ => None,
    }
}

fn progress_bar(path: &Path) -> ProgressBar {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    ProgressBar::new(0)
        .with_style(
            ProgressStyle::with_template(
                "{wide_msg} {bytes:>10}/{total_bytes:<10} {bytes_per_sec:>12}",
            )
            .expect("valid template"),
        )
        .with_message(name)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use md5::{Digest, Md5};

    use super::*;
    use crate::library::LibraryFile;
    use crate::test_server::{Reply, TestServer};

    const PAGE: &str = include_str!("../tests/fixtures/library_page.html");
    const LAST_PAGE: &str = include_str!("../tests/fixtures/library_last_page.html");

    fn md5_hex(bytes: &[u8]) -> String {
        Md5::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A fake BOOTH with the fixture library. File `<id>` contains `content-<id>`,
    /// or `changed-<id>` for 2001 once `changed` is set.
    fn fake_booth(changed: Arc<AtomicBool>) -> TestServer {
        TestServer::start(move |request| {
            let path = request.path.as_str();
            if path == "/library?page=1" {
                return Reply::new(200).body(PAGE);
            }
            if path == "/library?page=2" {
                return Reply::new(200).body(LAST_PAGE);
            }
            if path.starts_with("/library") {
                return Reply::new(200).body("<html></html>");
            }
            if let Some(id) = path.strip_prefix("/downloadables/") {
                let location = format!("http://{}/file/{id}", request.headers["host"]);
                return Reply::new(302).header("Location", &location);
            }
            if let Some(id) = path.strip_prefix("/file/") {
                let prefix = if id == "2001" && changed.load(Ordering::SeqCst) {
                    "changed"
                } else {
                    "content"
                };
                let body = format!("{prefix}-{id}");
                let reply = Reply::new(if request.headers.contains_key("range") {
                    206
                } else {
                    200
                })
                .header("ETag", &format!("\"{}\"", md5_hex(body.as_bytes())));
                return if request.headers.get("range").map(String::as_str) == Some("bytes=0-0") {
                    reply
                        .header("Content-Range", &format!("bytes 0-0/{}", body.len()))
                        .body(&body.as_bytes()[..1])
                } else {
                    reply.body(body)
                };
            }
            Reply::new(404)
        })
    }

    fn options(out: &Path, check_updates: bool) -> SyncOptions {
        SyncOptions {
            out: out.to_path_buf(),
            jobs: 2,
            check_updates,
        }
    }

    fn file_downloads(server: &TestServer) -> usize {
        server
            .requests()
            .iter()
            .filter(|r| r.path.starts_with("/file/") && !r.headers.contains_key("range"))
            .count()
    }

    #[test]
    fn downloads_everything_then_skips_on_the_next_run() {
        let server = fake_booth(Arc::default());
        let client = BoothClient::for_test("s", &server.url);
        let out = tempfile::tempdir().unwrap();

        let first = run(&client, &options(out.path(), false)).unwrap();
        assert_eq!((first.downloaded, first.skipped), (6, 0));
        assert!(first.failures.is_empty(), "{:?}", first.failures);
        // The fixture's card without download buttons.
        assert_eq!(first.warnings.len(), 1, "{:?}", first.warnings);
        let track = out
            .path()
            .join("Music Shop/1004_Sample Album/files/01-track.flac");
        assert_eq!(fs::read_to_string(track).unwrap(), "content-2005?type=flac");
        let one = out
            .path()
            .join("Sample Shop/1001_Sample Item One/files/one.pdf");
        assert_eq!(fs::read_to_string(one).unwrap(), "content-2001");
        assert!(
            out.path()
                .join("Sample Shop/1001_Sample Item One/description.md")
                .exists()
        );
        let state = State::load(out.path()).unwrap();
        assert_eq!(state.files.len(), 6);
        assert!(state.files.values().all(|f| f.checksum == Checksum::Md5Ok));

        let second = run(&client, &options(out.path(), false)).unwrap();
        assert_eq!((second.downloaded, second.skipped), (0, 6));
        assert_eq!(file_downloads(&server), 6);
    }

    #[test]
    fn check_updates_redownloads_only_changed_files() {
        let changed = Arc::new(AtomicBool::new(false));
        let server = fake_booth(changed.clone());
        let client = BoothClient::for_test("s", &server.url);
        let out = tempfile::tempdir().unwrap();
        run(&client, &options(out.path(), false)).unwrap();

        changed.store(true, Ordering::SeqCst);
        let summary = run(&client, &options(out.path(), true)).unwrap();

        assert_eq!((summary.updated, summary.skipped), (1, 5));
        let files = out.path().join("Sample Shop/1001_Sample Item One/files");
        assert_eq!(
            fs::read_to_string(files.join("one.pdf")).unwrap(),
            "changed-2001"
        );
        let old: Vec<_> = fs::read_dir(files.join(".old")).unwrap().collect();
        assert_eq!(old.len(), 1);
    }

    #[test]
    fn expired_session_aborts() {
        let server = TestServer::start(|_| {
            Reply::new(302).header("Location", "https://accounts.booth.pm/users/sign_in")
        });
        let client = BoothClient::for_test("s", &server.url);
        let out = tempfile::tempdir().unwrap();
        let err = run(&client, &options(out.path(), false)).unwrap_err();
        assert!(is_fatal(&err));
    }

    #[test]
    fn duplicate_file_names_get_an_id_suffix() {
        let items = vec![LibraryItem {
            item_id: 1,
            title: "T".into(),
            shop_name: "S".into(),
            files: vec![
                LibraryFile {
                    downloadable_id: 10,
                    format: None,
                    name: "a.zip".into(),
                },
                LibraryFile {
                    downloadable_id: 11,
                    format: None,
                    name: "a.zip".into(),
                },
            ],
        }];
        let mut state = State::default();
        state.items.insert(
            1,
            ItemRecord {
                dir: "S/1_T".into(),
                page_saved_at: None,
            },
        );
        let out = tempfile::tempdir().unwrap();
        let jobs = plan_jobs(
            &items,
            &state,
            &options(out.path(), false),
            &mut Summary::default(),
        );
        let paths: Vec<PathBuf> = jobs.into_iter().map(|j| j.path).collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("S/1_T/files/a.zip"),
                PathBuf::from("S/1_T/files/a (11).zip")
            ]
        );
    }
}
