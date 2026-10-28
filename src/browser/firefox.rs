//! Firefox: profiles from `profiles.ini`, plain-text cookies in `cookies.sqlite`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{BOOTH_HOSTS, Browser, Candidate, FoundSession, newest, open_copy};
use crate::config::SESSION_COOKIE;
use crate::state;

/// Expiry values above this are milliseconds (newer Firefox), not seconds.
/// Firefox caps cookie lifetimes at 400 days, so a seconds value never gets here.
const EXPIRY_MS_THRESHOLD: i64 = 100_000_000_000;

/// Directories that may hold `profiles.ini`.
pub(super) fn roots(home: &Path) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let _ = home;
        dirs::config_dir()
            .map(|dir| vec![dir.join("Mozilla").join("Firefox")])
            .unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        let mut roots = Vec::new();
        if let Some(config) = dirs::config_dir() {
            roots.push(config.join("mozilla").join("firefox"));
        }
        roots.push(home.join(".mozilla/firefox"));
        roots.push(home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"));
        roots.push(home.join("snap/firefox/common/.mozilla/firefox"));
        roots
    }
}

struct Profile {
    name: String,
    dir: PathBuf,
}

impl Profile {
    fn matches(&self, wanted: &str) -> bool {
        self.name == wanted || self.dir.file_name().is_some_and(|n| n == wanted)
    }
}

/// Searches every profile listed in the `profiles.ini` of each root.
pub(super) fn find_in_roots(roots: &[PathBuf], profile: Option<&str>) -> Result<FoundSession> {
    let searched = roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let profiles: Vec<Profile> = roots.iter().flat_map(|root| read_profiles(root)).collect();
    if profiles.is_empty() {
        bail!("no Firefox profiles found (searched {searched})");
    }
    let selected: Vec<&Profile> = match profile {
        Some(wanted) => profiles.iter().filter(|p| p.matches(wanted)).collect(),
        None => profiles.iter().collect(),
    };
    if selected.is_empty() {
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        bail!(
            "no Firefox profile named \"{}\"; available: {}",
            profile.unwrap_or_default(),
            names.join(", ")
        );
    }

    let mut candidates = Vec::new();
    for p in selected {
        let db = p.dir.join("cookies.sqlite");
        if db.exists() {
            candidates.extend(
                read_cookies(&db, &p.name)
                    .with_context(|| format!("read cookies from {}", db.display()))?,
            );
        }
    }
    newest(candidates, Browser::Firefox, &searched)
}

/// Minimal `profiles.ini` reader: `[ProfileN]` sections with `Name`, `Path`, `IsRelative`.
fn read_profiles(root: &Path) -> Vec<Profile> {
    let Ok(text) = fs::read_to_string(root.join("profiles.ini")) else {
        return Vec::new();
    };
    let mut profiles = Vec::new();
    let mut section = String::new();
    let (mut name, mut path, mut relative) = (None::<String>, None::<String>, true);
    let mut finish = |section: &str, name: Option<String>, path: Option<String>, relative: bool| {
        if section.starts_with("Profile")
            && let Some(path) = path
        {
            let dir = if relative {
                root.join(&path)
            } else {
                PathBuf::from(&path)
            };
            profiles.push(Profile {
                name: name.unwrap_or(path),
                dir,
            });
        }
    };
    for line in text.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            finish(&section, name.take(), path.take(), relative);
            section = header.to_string();
            relative = true;
        } else if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "Name" => name = Some(value.trim().to_string()),
                "Path" => path = Some(value.trim().to_string()),
                "IsRelative" => relative = value.trim() != "0",
                _ => {}
            }
        }
    }
    finish(&section, name, path, relative);
    profiles
}

fn read_cookies(db: &Path, profile: &str) -> Result<Vec<Candidate>> {
    let copy = open_copy(db)?;
    let mut statement = copy.conn.prepare(
        "SELECT value, lastAccessed, expiry FROM moz_cookies \
         WHERE name = ?1 AND host IN (?2, ?3) \
         AND originAttributes NOT LIKE '%partitionKey=%'",
    )?;
    let rows = statement.query_map((SESSION_COOKIE, BOOTH_HOSTS[0], BOOTH_HOSTS[1]), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let now = i64::try_from(state::now()).unwrap_or(i64::MAX);
    let mut candidates = Vec::new();
    for row in rows {
        let (value, last_access, expiry) = row?;
        let expiry_secs = if expiry > EXPIRY_MS_THRESHOLD {
            expiry / 1000
        } else {
            expiry
        };
        if expiry_secs <= now {
            continue;
        }
        candidates.push(Candidate {
            value,
            profile: profile.to_string(),
            last_access,
        });
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    const FAR_FUTURE: i64 = 4_102_444_800; // 2100-01-01, seconds

    fn write_profiles_ini(root: &Path, entries: &[(&str, &str)]) {
        let mut ini = String::from("[General]\nStartWithLastProfile=1\n\n");
        for (i, (name, path)) in entries.iter().enumerate() {
            ini.push_str(&format!(
                "[Profile{i}]\nName={name}\nIsRelative=1\nPath={path}\n\n"
            ));
        }
        ini.push_str("[Install4F96D1932A9F858E]\nDefault=whatever\nLocked=1\n");
        fs::write(root.join("profiles.ini"), ini).unwrap();
    }

    /// Creates `<root>/<path>/cookies.sqlite` with the given rows
    /// (host, name, value, last_access_us, expiry).
    fn write_cookies(root: &Path, path: &str, rows: &[(&str, &str, &str, i64, i64)]) {
        let dir = root.join(path);
        fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("cookies.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE moz_cookies (id INTEGER PRIMARY KEY, originAttributes TEXT NOT NULL DEFAULT '', \
             name TEXT, value TEXT, host TEXT, path TEXT, expiry INTEGER, lastAccessed INTEGER, \
             creationTime INTEGER, isSecure INTEGER, isHttpOnly INTEGER)",
        )
        .unwrap();
        for (i, (host, name, value, last_access, expiry)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO moz_cookies (originAttributes, name, value, host, path, expiry, lastAccessed) \
                 VALUES (?1, ?2, ?3, ?4, '/', ?5, ?6)",
                (format!("^userContextId={i}"), name, value, host, expiry, last_access),
            )
            .unwrap();
        }
    }

    #[test]
    fn finds_the_newest_session_across_profiles_and_containers() {
        let root = tempfile::tempdir().unwrap();
        write_profiles_ini(root.path(), &[("default", "a.default"), ("work", "b.work")]);
        write_cookies(
            root.path(),
            "a.default",
            &[
                (".booth.pm", SESSION_COOKIE, "old", 100, FAR_FUTURE),
                (".booth.pm", SESSION_COOKIE, "container", 300, FAR_FUTURE),
                (
                    ".example.com",
                    SESSION_COOKIE,
                    "other-site",
                    900,
                    FAR_FUTURE,
                ),
            ],
        );
        write_cookies(
            root.path(),
            "b.work",
            &[("booth.pm", SESSION_COOKIE, "work", 200, FAR_FUTURE)],
        );

        let found = find_in_roots(&[root.path().to_path_buf()], None).unwrap();

        assert_eq!(found.value, "container");
        assert_eq!(found.profile, "default");
    }

    #[test]
    fn ignores_partitioned_cookies() {
        let root = tempfile::tempdir().unwrap();
        write_profiles_ini(root.path(), &[("default", "a.default")]);
        write_cookies(
            root.path(),
            "a.default",
            &[
                (".booth.pm", SESSION_COOKIE, "first-party", 100, FAR_FUTURE),
                (".booth.pm", SESSION_COOKIE, "partitioned", 900, FAR_FUTURE),
            ],
        );
        let conn = Connection::open(root.path().join("a.default/cookies.sqlite")).unwrap();
        conn.execute(
            "UPDATE moz_cookies SET originAttributes = '^partitionKey=%28https%2Cexample.com%29' \
             WHERE value = 'partitioned'",
            [],
        )
        .unwrap();
        drop(conn);

        let found = find_in_roots(&[root.path().to_path_buf()], None).unwrap();

        assert_eq!(found.value, "first-party");
    }

    #[test]
    fn skips_expired_cookies_in_seconds_and_milliseconds() {
        let root = tempfile::tempdir().unwrap();
        write_profiles_ini(root.path(), &[("default", "a.default")]);
        write_cookies(
            root.path(),
            "a.default",
            &[
                (".booth.pm", SESSION_COOKIE, "expired-s", 500, 1_000_000_000),
                (
                    ".booth.pm",
                    SESSION_COOKIE,
                    "expired-ms",
                    400,
                    1_000_000_000_000,
                ),
                (
                    ".booth.pm",
                    SESSION_COOKIE,
                    "valid-ms",
                    100,
                    FAR_FUTURE * 1000,
                ),
            ],
        );
        let found = find_in_roots(&[root.path().to_path_buf()], None).unwrap();
        assert_eq!(found.value, "valid-ms");
    }

    #[test]
    fn profile_can_be_chosen_by_name_or_directory() {
        let root = tempfile::tempdir().unwrap();
        write_profiles_ini(root.path(), &[("default", "a.default"), ("work", "b.work")]);
        write_cookies(
            root.path(),
            "a.default",
            &[(".booth.pm", SESSION_COOKIE, "default", 900, FAR_FUTURE)],
        );
        write_cookies(
            root.path(),
            "b.work",
            &[(".booth.pm", SESSION_COOKIE, "work", 100, FAR_FUTURE)],
        );
        let roots = [root.path().to_path_buf()];
        assert_eq!(find_in_roots(&roots, Some("work")).unwrap().value, "work");
        assert_eq!(find_in_roots(&roots, Some("b.work")).unwrap().value, "work");
        let err = find_in_roots(&roots, Some("nope")).unwrap_err().to_string();
        assert!(err.contains("available: default, work"), "{err}");
    }

    #[test]
    fn missing_profiles_and_cookies_are_errors() {
        let empty = tempfile::tempdir().unwrap();
        let err = find_in_roots(&[empty.path().to_path_buf()], None).unwrap_err();
        assert!(err.to_string().contains("no Firefox profiles found"));

        let root = tempfile::tempdir().unwrap();
        write_profiles_ini(root.path(), &[("default", "a.default")]);
        write_cookies(root.path(), "a.default", &[]);
        let err = find_in_roots(&[root.path().to_path_buf()], None).unwrap_err();
        assert!(err.to_string().contains("log in to BOOTH in firefox"));
    }

    #[test]
    fn reads_absolute_profile_paths() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        write_cookies(
            elsewhere.path(),
            "p",
            &[(".booth.pm", SESSION_COOKIE, "abs", 1, FAR_FUTURE)],
        );
        let ini = format!(
            "[Profile0]\nName=abs\nIsRelative=0\nPath={}\n",
            elsewhere.path().join("p").display()
        );
        fs::write(root.path().join("profiles.ini"), ini).unwrap();
        let found = find_in_roots(&[root.path().to_path_buf()], None).unwrap();
        assert_eq!(found.value, "abs");
    }
}
