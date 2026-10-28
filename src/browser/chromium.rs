//! Chrome-family browsers on Linux: encrypted cookies in `<profile>/Cookies`.
//!
//! `encrypted_value` is `v10` or `v11` followed by AES-128-CBC ciphertext.
//! The key is PBKDF2-HMAC-SHA1(password, "saltysalt", 1 iteration); the
//! password is "peanuts" for `v10` and the "Safe Storage" secret from the
//! freedesktop Secret Service (via `secret-tool`) for `v11`. Since database
//! version 24 the plaintext starts with SHA256(host_key).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockModeDecrypt, KeyIvInit};
use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use super::{BOOTH_HOSTS, Browser, Candidate, FoundSession, newest, open_copy};
use crate::config::SESSION_COOKIE;
use crate::state;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

const IV: [u8; 16] = [b' '; 16];
/// Microseconds between 1601-01-01 (Chrome's epoch) and 1970-01-01.
const CHROME_EPOCH_OFFSET_US: i64 = 11_644_473_600_000_000;
/// Database version that added SHA256(host_key) in front of the plaintext.
const HOST_HASH_VERSION: i64 = 24;

/// Data directory under the user config dir and Secret Service application name.
fn layout(browser: Browser) -> (&'static str, &'static str) {
    match browser {
        Browser::Chrome => ("google-chrome", "chrome"),
        Browser::Chromium => ("chromium", "chromium"),
        Browser::Brave => ("BraveSoftware/Brave-Browser", "brave"),
        Browser::Edge => ("microsoft-edge", "microsoft-edge"),
        Browser::Firefox => unreachable!("Firefox is not a Chrome-family browser"),
    }
}

pub(super) fn find_session(browser: Browser, profile: Option<&str>) -> Result<FoundSession> {
    let (dir, app) = layout(browser);
    let root = dirs::config_dir()
        .context("cannot determine the user config directory")?
        .join(dir);
    find_in_root(browser, &root, profile, || secret_tool_password(app))
}

/// Searches the profiles under `root`. `keyring` is called at most once, and
/// only if a `v11` cookie needs decrypting.
fn find_in_root(
    browser: Browser,
    root: &Path,
    profile: Option<&str>,
    keyring: impl FnOnce() -> Result<Vec<u8>>,
) -> Result<FoundSession> {
    let profiles = list_profiles(root);
    if profiles.is_empty() {
        bail!("no {browser} profiles found in {}", root.display());
    }
    let selected: Vec<&String> = match profile {
        Some(wanted) => profiles.iter().filter(|p| *p == wanted).collect(),
        None => profiles.iter().collect(),
    };
    if selected.is_empty() {
        bail!(
            "no {browser} profile named \"{}\"; available: {}",
            profile.unwrap_or_default(),
            profiles.join(", ")
        );
    }

    let mut keyring = Some(keyring);
    let mut v11_password: Option<Vec<u8>> = None;
    let mut candidates = Vec::new();
    for name in selected {
        let Some(db) = cookie_db(&root.join(name)) else {
            continue;
        };
        for row in read_rows(&db)? {
            let value = if !row.value.is_empty() {
                row.value
            } else {
                let password = match row.encrypted.get(..3) {
                    Some(b"v11") => {
                        if v11_password.is_none() {
                            let fetch = keyring.take().expect("keyring is fetched once");
                            v11_password = Some(fetch()?);
                        }
                        v11_password.as_deref().expect("set above")
                    }
                    _ => b"peanuts".as_slice(),
                };
                let plain = decrypt(&row.encrypted, password, row.version, &row.host)?;
                String::from_utf8_lossy(&plain).into_owned()
            };
            candidates.push(Candidate {
                value,
                profile: name.clone(),
                last_access: row.last_access,
            });
        }
    }
    newest(candidates, browser, &root.display().to_string())
}

/// Profile directories: `Default` and `Profile N`, sorted.
fn list_profiles(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n == "Default" || n.starts_with("Profile "))
        .collect();
    names.sort();
    names
}

fn cookie_db(profile_dir: &Path) -> Option<PathBuf> {
    ["Cookies", "Network/Cookies"]
        .iter()
        .map(|rel| profile_dir.join(rel))
        .find(|p| p.exists())
}

struct Row {
    host: String,
    value: String,
    encrypted: Vec<u8>,
    /// Microseconds since the Unix epoch.
    last_access: i64,
    version: i64,
}

/// Reads the non-expired BOOTH session rows of one cookie database.
fn read_rows(db: &Path) -> Result<Vec<Row>> {
    let copy = open_copy(db)?;
    let version: i64 = copy
        .conn
        .query_row("SELECT value FROM meta WHERE key = 'version'", [], |row| {
            row.get::<_, String>(0)
        })
        .map(|v| v.parse().unwrap_or(0))
        .unwrap_or(0);
    let mut statement = copy.conn.prepare(
        "SELECT host_key, value, encrypted_value, last_access_utc, expires_utc FROM cookies \
         WHERE name = ?1 AND host_key IN (?2, ?3)",
    )?;
    let rows = statement.query_map((SESSION_COOKIE, BOOTH_HOSTS[0], BOOTH_HOSTS[1]), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    let now_us = i64::try_from(state::now())
        .unwrap_or(i64::MAX / 1_000_000)
        .saturating_mul(1_000_000);
    let mut result = Vec::new();
    for row in rows {
        let (host, value, encrypted, last_access, expires) =
            row.with_context(|| format!("read cookies from {}", db.display()))?;
        // `expires_utc == 0` marks a session cookie, which never expires on disk.
        if expires != 0 && expires - CHROME_EPOCH_OFFSET_US <= now_us {
            continue;
        }
        result.push(Row {
            host,
            value,
            encrypted,
            last_access: last_access - CHROME_EPOCH_OFFSET_US,
            version,
        });
    }
    Ok(result)
}

/// Decrypts a `v10`/`v11` `encrypted_value` with the given password.
fn decrypt(encrypted: &[u8], password: &[u8], version: i64, host: &str) -> Result<Vec<u8>> {
    let (prefix, ciphertext) = encrypted.split_at(encrypted.len().min(3));
    if prefix != b"v10" && prefix != b"v11" {
        bail!(
            "unsupported cookie encryption \"{}\"",
            String::from_utf8_lossy(prefix)
        );
    }
    if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        bail!("unsupported cookie format (bad ciphertext length)");
    }
    let key = pbkdf2::pbkdf2_hmac_array::<sha1::Sha1, 16>(password, b"saltysalt", 1);
    let mut buf = ciphertext.to_vec();
    let plain = Aes128CbcDec::new(&key.into(), &IV.into())
        .decrypt_padded::<Pkcs7>(&mut buf)
        .map_err(|_| anyhow!("cannot decrypt the cookie (wrong keyring password?)"))?;
    if version < HOST_HASH_VERSION {
        return Ok(plain.to_vec());
    }
    match plain.split_at_checked(32) {
        Some((hash, value)) if hash == Sha256::digest(host.as_bytes()).as_slice() => {
            Ok(value.to_vec())
        }
        _ => bail!("unsupported cookie format (domain hash mismatch)"),
    }
}

/// The browser's "Safe Storage" password from the freedesktop Secret Service.
fn secret_tool_password(app: &str) -> Result<Vec<u8>> {
    let output = match Command::new("secret-tool")
        .args(["lookup", "application", app])
        .output()
    {
        Ok(output) => output,
        Err(e) if e.kind() == io::ErrorKind::NotFound => bail!(
            "`secret-tool` is not installed; install libsecret-tools (Debian/Ubuntu) \
             or libsecret (Fedora), or paste the cookie with `booth-dl login`"
        ),
        Err(e) => return Err(e).context("run secret-tool"),
    };
    let mut password = output.stdout;
    if password.last() == Some(&b'\n') {
        password.pop();
    }
    if !output.status.success() || password.is_empty() {
        bail!(
            "secret-tool found no \"{app}\" password in the keyring ({}); is the keyring unlocked?",
            output.status
        );
    }
    Ok(password)
}

#[cfg(test)]
mod tests {
    use aes::cipher::BlockModeEncrypt;
    use rusqlite::Connection;

    use super::*;

    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    const FAR_FUTURE_US: i64 = 4_102_444_800_000_000 + CHROME_EPOCH_OFFSET_US; // 2100-01-01

    /// Test-only inverse of `decrypt`.
    fn encrypt(
        value: &[u8],
        prefix: &[u8; 3],
        password: &[u8],
        version: i64,
        host: &str,
    ) -> Vec<u8> {
        let mut plain = Vec::new();
        if version >= HOST_HASH_VERSION {
            plain.extend_from_slice(&Sha256::digest(host.as_bytes()));
        }
        plain.extend_from_slice(value);
        let key = pbkdf2::pbkdf2_hmac_array::<sha1::Sha1, 16>(password, b"saltysalt", 1);
        let len = plain.len();
        plain.resize(len + 16 - len % 16, 0);
        let ciphertext = Aes128CbcEnc::new(&key.into(), &IV.into())
            .encrypt_padded::<Pkcs7>(&mut plain, len)
            .unwrap()
            .to_vec();
        [prefix.as_slice(), &ciphertext].concat()
    }

    struct TestRow<'a> {
        host: &'a str,
        value: &'a str,
        encrypted: Vec<u8>,
        last_access: i64,
        expires: i64,
    }

    fn write_db(path: &Path, version: i64, rows: &[TestRow]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR); \
             CREATE TABLE cookies (host_key TEXT, name TEXT, value TEXT, encrypted_value BLOB, \
             last_access_utc INTEGER, expires_utc INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meta VALUES ('version', ?1)",
            [version.to_string()],
        )
        .unwrap();
        for row in rows {
            conn.execute(
                "INSERT INTO cookies VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (
                    row.host,
                    SESSION_COOKIE,
                    row.value,
                    &row.encrypted,
                    row.last_access,
                    row.expires,
                ),
            )
            .unwrap();
        }
    }

    fn encrypted_row<'a>(
        value: &str,
        prefix: &[u8; 3],
        password: &[u8],
        version: i64,
        last_access: i64,
    ) -> TestRow<'a> {
        TestRow {
            host: ".booth.pm",
            value: "",
            encrypted: encrypt(value.as_bytes(), prefix, password, version, ".booth.pm"),
            last_access: CHROME_EPOCH_OFFSET_US + last_access,
            expires: FAR_FUTURE_US,
        }
    }

    fn no_keyring() -> Result<Vec<u8>> {
        panic!("keyring must not be used")
    }

    #[test]
    fn decrypts_v10_with_host_hash() {
        let root = tempfile::tempdir().unwrap();
        write_db(
            &root.path().join("Default/Cookies"),
            24,
            &[encrypted_row("v10-value", b"v10", b"peanuts", 24, 5)],
        );
        let found = find_in_root(Browser::Chrome, root.path(), None, no_keyring).unwrap();
        assert_eq!(found.value, "v10-value");
        assert_eq!(found.profile, "Default");
    }

    #[test]
    fn decrypts_v11_with_the_keyring_password_once() {
        let root = tempfile::tempdir().unwrap();
        write_db(
            &root.path().join("Default/Cookies"),
            24,
            &[encrypted_row("older", b"v11", b"secret", 24, 1)],
        );
        write_db(
            &root.path().join("Profile 1/Cookies"),
            24,
            &[encrypted_row("newer", b"v11", b"secret", 24, 9)],
        );
        let calls = std::cell::Cell::new(0);
        let found = find_in_root(Browser::Chrome, root.path(), None, || {
            calls.set(calls.get() + 1);
            Ok(b"secret".to_vec())
        })
        .unwrap();
        assert_eq!(found.value, "newer");
        assert_eq!(found.profile, "Profile 1");
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn old_databases_have_no_host_hash() {
        let root = tempfile::tempdir().unwrap();
        write_db(
            &root.path().join("Default/Cookies"),
            23,
            &[encrypted_row("v23", b"v10", b"peanuts", 23, 1)],
        );
        let found = find_in_root(Browser::Chromium, root.path(), None, no_keyring).unwrap();
        assert_eq!(found.value, "v23");
    }

    #[test]
    fn plaintext_values_and_network_cookies_path() {
        let root = tempfile::tempdir().unwrap();
        write_db(
            &root.path().join("Default/Network/Cookies"),
            24,
            &[TestRow {
                host: "booth.pm",
                value: "plain",
                encrypted: Vec::new(),
                last_access: CHROME_EPOCH_OFFSET_US,
                expires: 0,
            }],
        );
        let found = find_in_root(Browser::Brave, root.path(), None, no_keyring).unwrap();
        assert_eq!(found.value, "plain");
    }

    #[test]
    fn skips_expired_cookies() {
        let root = tempfile::tempdir().unwrap();
        let mut expired = encrypted_row("expired", b"v10", b"peanuts", 24, 9);
        expired.expires = CHROME_EPOCH_OFFSET_US + 1;
        write_db(
            &root.path().join("Default/Cookies"),
            24,
            &[expired, encrypted_row("valid", b"v10", b"peanuts", 24, 1)],
        );
        let found = find_in_root(Browser::Chrome, root.path(), None, no_keyring).unwrap();
        assert_eq!(found.value, "valid");
    }

    #[test]
    fn rejects_host_hash_mismatch_and_unknown_prefix() {
        let hash_mismatch = encrypt(b"x", b"v10", b"peanuts", 24, "evil.example");
        let err = decrypt(&hash_mismatch, b"peanuts", 24, ".booth.pm").unwrap_err();
        assert!(err.to_string().contains("domain hash mismatch"));

        let v20 = [b"v20".as_slice(), &[0u8; 32]].concat();
        let err = decrypt(&v20, b"peanuts", 24, ".booth.pm").unwrap_err();
        assert!(
            err.to_string()
                .contains("unsupported cookie encryption \"v20\"")
        );
    }

    #[test]
    fn wrong_password_cannot_decrypt() {
        let encrypted = encrypt(b"value", b"v11", b"right", 24, ".booth.pm");
        let err = decrypt(&encrypted, b"wrong", 24, ".booth.pm").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("cannot decrypt") || message.contains("domain hash mismatch"),
            "{message}"
        );
    }

    #[test]
    fn keyring_failure_is_reported() {
        let root = tempfile::tempdir().unwrap();
        write_db(
            &root.path().join("Default/Cookies"),
            24,
            &[encrypted_row("v", b"v11", b"secret", 24, 1)],
        );
        let err = find_in_root(Browser::Chrome, root.path(), None, || {
            bail!("keyring is locked")
        })
        .unwrap_err();
        assert!(err.to_string().contains("keyring is locked"));
    }

    #[test]
    fn unknown_profile_lists_available_ones() {
        let root = tempfile::tempdir().unwrap();
        write_db(&root.path().join("Default/Cookies"), 24, &[]);
        fs::create_dir_all(root.path().join("System Profile")).unwrap();
        let err = find_in_root(Browser::Chrome, root.path(), Some("Profile 9"), no_keyring)
            .unwrap_err()
            .to_string();
        assert!(err.contains("available: Default"), "{err}");
    }
}
