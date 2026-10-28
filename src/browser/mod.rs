//! Reading the BOOTH session cookie from a web browser's cookie store.
//!
//! Cookie values are secrets: nothing in this module prints them, and
//! `FoundSession`'s `Debug` output redacts the value.

#[cfg(target_os = "linux")]
mod chromium;
mod firefox;

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use tempfile::TempDir;

use crate::config::SESSION_COOKIE;

/// Host names BOOTH sets the session cookie for.
const BOOTH_HOSTS: [&str; 2] = [".booth.pm", "booth.pm"];

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Browser {
    Firefox,
    Chrome,
    Chromium,
    Brave,
    Edge,
}

impl fmt::Display for Browser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Browser::Firefox => "firefox",
            Browser::Chrome => "chrome",
            Browser::Chromium => "chromium",
            Browser::Brave => "brave",
            Browser::Edge => "edge",
        })
    }
}

/// The BOOTH session cookie and the browser profile it came from.
pub struct FoundSession {
    pub value: String,
    pub profile: String,
}

impl fmt::Debug for FoundSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FoundSession")
            .field("value", &"<redacted>")
            .field("profile", &self.profile)
            .finish()
    }
}

/// Finds the most recently used BOOTH session cookie in `browser`, optionally
/// limited to one profile.
pub fn find_session(browser: Browser, profile: Option<&str>) -> Result<FoundSession> {
    let home = dirs::home_dir().context("cannot determine the home directory")?;
    match browser {
        Browser::Firefox => firefox::find_in_roots(&firefox::roots(&home), profile),
        #[cfg(target_os = "linux")]
        Browser::Chrome | Browser::Chromium | Browser::Brave | Browser::Edge => {
            chromium::find_session(browser, profile)
        }
        #[cfg(not(target_os = "linux"))]
        _ => bail!(
            "reading cookies from {browser} is supported only on Linux \
             (on Windows its cookies use App-Bound Encryption); \
             use --from-browser firefox or paste the cookie with `booth-dl login`"
        ),
    }
}

/// One session cookie found in a profile.
struct Candidate {
    value: String,
    profile: String,
    /// Last access, in microseconds since the Unix epoch.
    last_access: i64,
}

/// Picks the most recently used candidate (the first one on ties) whose value
/// looks like a cookie. `browser` and `searched` only shape the error messages.
fn newest(candidates: Vec<Candidate>, browser: Browser, searched: &str) -> Result<FoundSession> {
    if candidates.is_empty() {
        bail!(
            "no BOOTH session ({SESSION_COOKIE}) found in {browser} ({searched}); \
             log in to BOOTH in {browser} first"
        );
    }
    let mut best: Option<Candidate> = None;
    for candidate in candidates {
        if !is_cookie_value(&candidate.value) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|b| candidate.last_access > b.last_access)
        {
            best = Some(candidate);
        }
    }
    let best = best.with_context(|| {
        format!("the BOOTH session cookie in {browser} has an unsupported format")
    })?;
    Ok(FoundSession {
        value: best.value,
        profile: best.profile,
    })
}

/// Cookie values are non-empty printable ASCII without spaces.
fn is_cookie_value(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// A private, writable copy of a browser's SQLite database. The browser may
/// hold a lock on the original and keep recent writes in its `-wal` file (or
/// an unfinished transaction in its `-journal` file).
struct DbCopy {
    // Field order matters: the connection must close before the directory is removed.
    conn: Connection,
    _dir: TempDir,
}

fn copy_file(source: &Path, target: &Path) -> io::Result<()> {
    io::copy(&mut File::open(source)?, &mut File::create(target)?)?;
    Ok(())
}

fn open_copy(db: &Path) -> Result<DbCopy> {
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let dir = builder.tempdir().context("create a temporary directory")?;
    let name = db.file_name().context("database path has no file name")?;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut source = db.as_os_str().to_os_string();
        source.push(suffix);
        let source = Path::new(&source);
        if source.exists() {
            let mut target = name.to_os_string();
            target.push(suffix);
            // Plain open + copy (not `fs::copy`): std opens with full sharing,
            // so a running browser on Windows cannot block the read.
            copy_file(source, &dir.path().join(target))
                .with_context(|| format!("copy {}", source.display()))?;
        }
    }
    // Opened read-write so SQLite can apply the copied WAL; only the copy is touched.
    let conn = Connection::open(dir.path().join(name))
        .with_context(|| format!("open a copy of {}", db.display()))?;
    Ok(DbCopy { conn, _dir: dir })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(value: &str, last_access: i64) -> Candidate {
        Candidate {
            value: value.to_string(),
            profile: format!("p{last_access}"),
            last_access,
        }
    }

    #[test]
    fn newest_valid_candidate_wins() {
        let found = newest(
            vec![
                candidate("old", 1),
                candidate("new", 3),
                candidate("bad value", 5),
                candidate("tie", 3),
            ],
            Browser::Firefox,
            "x",
        )
        .unwrap();
        assert_eq!(found.value, "new");
        assert_eq!(found.profile, "p3");
    }

    #[test]
    fn no_candidates_asks_to_log_in() {
        let err = newest(vec![], Browser::Chrome, "/somewhere").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("log in to BOOTH in chrome"), "{message}");
        assert!(message.contains("/somewhere"), "{message}");
    }

    #[test]
    fn only_invalid_candidates_is_an_unsupported_format() {
        let err = newest(
            vec![candidate("", 1), candidate("a\u{1}b", 2)],
            Browser::Brave,
            "x",
        )
        .unwrap_err();
        assert!(err.to_string().contains("unsupported format"));
    }

    #[test]
    fn copies_database_with_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("Cookies");
        let conn = Connection::open(&db).unwrap();
        conn.pragma_update(None, "journal_mode", "wal").unwrap();
        conn.execute_batch("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('in-wal');")
            .unwrap();
        // Keep `conn` open so the row stays in the -wal file, like a running browser.
        assert!(dir.path().join("Cookies-wal").exists());

        let copy = open_copy(&db).unwrap();
        let v: String = copy
            .conn
            .query_row("SELECT v FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(v, "in-wal");
        drop(conn);
    }

    #[cfg(unix)]
    #[test]
    fn temp_copy_is_private_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("Cookies");
        Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE t (v TEXT);")
            .unwrap();

        let copy = open_copy(&db).unwrap();
        let copy_dir = Path::new(copy.conn.path().unwrap())
            .parent()
            .unwrap()
            .to_path_buf();
        let mode = fs::metadata(&copy_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{mode:o}");

        drop(copy);
        assert!(!copy_dir.exists());
    }

    #[test]
    fn debug_output_redacts_the_value() {
        let found = FoundSession {
            value: "secret-cookie".into(),
            profile: "Default".into(),
        };
        let debug = format!("{found:?}");
        assert!(!debug.contains("secret-cookie"), "{debug}");
        assert!(debug.contains("Default"));
    }

    #[test]
    fn browser_names() {
        assert_eq!(Browser::Edge.to_string(), "edge");
        assert_eq!(Browser::Firefox.to_string(), "firefox");
    }
}
