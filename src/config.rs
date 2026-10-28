//! Loading and storing the BOOTH session cookie.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Name of the cookie that holds the BOOTH session.
pub const SESSION_COOKIE: &str = "_plaza_session_nktz7u";

/// Environment variable that overrides the stored session.
pub const SESSION_ENV: &str = "BOOTH_SESSION";

#[derive(Debug, PartialEq)]
pub struct Session {
    pub value: String,
    /// True when the value came from [`SESSION_ENV`]; such a value is never written back.
    pub from_env: bool,
}

/// `<config dir>/booth-dl/session` (`~/.config` on Linux, `%APPDATA%` on Windows).
pub fn session_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("cannot determine the user config directory")?;
    Ok(dir.join("booth-dl").join("session"))
}

/// Loads the session from the environment or the session file.
pub fn load_session() -> Result<Session> {
    load_session_from(std::env::var(SESSION_ENV).ok(), &session_path()?)
}

pub fn save_session(value: &str) -> Result<()> {
    save_session_to(&session_path()?, value)
}

fn load_session_from(env: Option<String>, path: &Path) -> Result<Session> {
    if let Some(value) = env.map(|v| normalize_session_input(&v))
        && !value.is_empty()
    {
        return Ok(Session {
            value,
            from_env: true,
        });
    }
    match fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => Ok(Session {
            value: text.trim().to_string(),
            from_env: false,
        }),
        Ok(_) => bail!("{} is empty; run `booth-dl login`", path.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            bail!("not logged in; run `booth-dl login` or set {SESSION_ENV}")
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Writes the session file atomically, readable only by the owner on Unix.
fn save_session_to(path: &Path, value: &str) -> Result<()> {
    let dir = path.parent().expect("session path has a parent");
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    write_private(&tmp, value).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn write_private(path: &Path, value: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode` only applies when the file is created.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(value.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &Path, value: &str) -> io::Result<()> {
    fs::write(path, value)
}

/// Accepts either the bare cookie value or `name=value`, with surrounding whitespace.
pub fn normalize_session_input(input: &str) -> String {
    let input = input.trim();
    input
        .strip_prefix(SESSION_COOKIE)
        .and_then(|rest| rest.strip_prefix('='))
        .unwrap_or(input)
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_pasted_values() {
        assert_eq!(normalize_session_input("  abc \n"), "abc");
        assert_eq!(normalize_session_input("_plaza_session_nktz7u=abc"), "abc");
    }

    #[test]
    fn env_overrides_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session");
        fs::write(&path, "from-file").unwrap();
        let session = load_session_from(Some("from-env".into()), &path).unwrap();
        assert_eq!(
            session,
            Session {
                value: "from-env".into(),
                from_env: true
            }
        );
    }

    #[test]
    fn reads_file_when_env_is_absent_or_blank() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session");
        fs::write(&path, "from-file\n").unwrap();
        for env in [None, Some("  ".to_string())] {
            let session = load_session_from(env, &path).unwrap();
            assert_eq!(session.value, "from-file");
            assert!(!session.from_env);
        }
    }

    #[test]
    fn missing_session_asks_to_log_in() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_session_from(None, &dir.path().join("session")).unwrap_err();
        assert!(err.to_string().contains("booth-dl login"));
    }

    #[test]
    fn save_creates_parent_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("session");
        save_session_to(&path, "secret").unwrap();
        save_session_to(&path, "rotated").unwrap();
        assert_eq!(load_session_from(None, &path).unwrap().value, "rotated");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
