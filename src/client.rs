//! HTTP access to BOOTH: session cookie, throttling, retries and fatal-error detection.

use std::fmt;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ureq::Body;
use ureq::http::Response;

use crate::config::SESSION_COOKIE;

const BOOTH_URL: &str = "https://booth.pm";
const ACCOUNTS_URL: &str = "https://accounts.booth.pm";
const USER_AGENT: &str = concat!("booth-dl/", env!("CARGO_PKG_VERSION"));
const MAX_ATTEMPTS: u32 = 3;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
/// Upper bound for images and other in-memory downloads.
const MAX_BYTES_IN_MEMORY: u64 = 64 * 1024 * 1024;

/// Errors after which continuing makes no sense.
#[derive(Debug, PartialEq, Eq)]
pub enum Fatal {
    /// BOOTH redirected to the sign-in page.
    SessionExpired,
    /// Cloudflare answered with a challenge instead of the page.
    Blocked { status: u16 },
}

impl fmt::Display for Fatal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fatal::SessionExpired => {
                write!(
                    f,
                    "the BOOTH session has expired; run `booth-dl login` again"
                )
            }
            Fatal::Blocked { status } => write!(
                f,
                "Cloudflare blocked the request (HTTP {status}); try again later"
            ),
        }
    }
}

impl std::error::Error for Fatal {}

/// True if `error` (or its cause chain) is a [`Fatal`] error.
pub fn is_fatal(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Fatal>().is_some()
}

pub struct BoothClient {
    agent: ureq::Agent,
    booth_url: String,
    accounts_url: String,
    session: Mutex<String>,
    last_page_request: Mutex<Option<Instant>>,
    page_interval: Duration,
    retry_delay: Duration,
}

impl BoothClient {
    pub fn new(session: String) -> BoothClient {
        Self::build(
            session,
            BOOTH_URL,
            ACCOUNTS_URL,
            Duration::from_secs(1),
            Duration::from_secs(2),
        )
    }

    /// Client that sends every request to `base_url`, without delays.
    #[cfg(test)]
    pub fn for_test(session: &str, base_url: &str) -> BoothClient {
        Self::build(
            session.to_string(),
            base_url,
            base_url,
            Duration::ZERO,
            Duration::ZERO,
        )
    }

    fn build(
        session: String,
        booth_url: &str,
        accounts_url: &str,
        page_interval: Duration,
        retry_delay: Duration,
    ) -> BoothClient {
        let agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .user_agent(USER_AGENT)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_per_read(Some(Duration::from_secs(60)))
            .build()
            .into();
        BoothClient {
            agent,
            booth_url: booth_url.to_string(),
            accounts_url: accounts_url.to_string(),
            session: Mutex::new(session),
            last_page_request: Mutex::new(None),
            page_interval,
            retry_delay,
        }
    }

    pub fn booth_url(&self) -> &str {
        &self.booth_url
    }

    pub fn accounts_url(&self) -> &str {
        &self.accounts_url
    }

    /// The latest session value (BOOTH re-issues it on every response).
    pub fn session(&self) -> String {
        self.session.lock().unwrap().clone()
    }

    /// GETs an authenticated page. Returns `None` for 404 and 410.
    pub fn get_text_optional(&self, url: &str) -> Result<Option<String>> {
        let mut response = self.send(url, true, None)?;
        match response.status().as_u16() {
            200 => Ok(Some(
                response
                    .body_mut()
                    .read_to_string()
                    .with_context(|| format!("GET {url}"))?,
            )),
            404 | 410 => Ok(None),
            status => bail!("GET {url}: unexpected HTTP {status}"),
        }
    }

    /// GETs an authenticated page that must exist.
    pub fn get_text(&self, url: &str) -> Result<String> {
        self.get_text_optional(url)?
            .with_context(|| format!("GET {url}: not found"))
    }

    /// Returns the short-lived presigned URL for a download key (`123` or `123?type=flac`).
    pub fn resolve_download_url(&self, key: &str) -> Result<String> {
        let url = format!("{}/downloadables/{key}", self.booth_url);
        let response = self.send(&url, true, None)?;
        if response.status().is_redirection()
            && let Some(location) = header(&response, "location")
        {
            return Ok(location.to_string());
        }
        bail!(
            "GET {url}: expected a redirect, got HTTP {}",
            response.status()
        )
    }

    /// GETs a presigned file URL, starting at byte `offset`. No cookie is sent.
    pub fn get_file(&self, url: &str, offset: u64) -> Result<Response<Body>> {
        self.send(url, false, (offset > 0).then(|| format!("bytes={offset}-")))
    }

    /// GETs only the first byte of a presigned file URL (to read its headers).
    pub fn get_first_byte(&self, url: &str) -> Result<Response<Body>> {
        self.send(url, false, Some("bytes=0-0".to_string()))
    }

    /// GETs a public resource such as an image into memory. No cookie is sent.
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let mut response = self.send(url, false, None)?;
        let status = response.status().as_u16();
        if status != 200 {
            bail!("GET {url}: unexpected HTTP {status}");
        }
        response
            .body_mut()
            .with_config()
            .limit(MAX_BYTES_IN_MEMORY)
            .read_to_vec()
            .with_context(|| format!("GET {url}"))
    }

    /// Sends a GET with retries. Authenticated requests carry the session
    /// cookie, are throttled, and are checked for fatal responses.
    fn send(
        &self,
        url: &str,
        authenticated: bool,
        range: Option<String>,
    ) -> Result<Response<Body>> {
        let mut attempt = 1;
        loop {
            if authenticated {
                self.throttle();
            }
            let mut request = self.agent.get(url);
            if authenticated {
                request = request.header("Cookie", format!("{SESSION_COOKIE}={}", self.session()));
            }
            if let Some(range) = &range {
                request = request.header("Range", range);
            }
            match request.call() {
                Ok(response) => {
                    if authenticated {
                        self.capture_session(&response);
                        check_fatal(&response)?;
                    }
                    let status = response.status().as_u16();
                    if (status == 429 || status >= 500) && attempt < MAX_ATTEMPTS {
                        thread::sleep(retry_after(&response).unwrap_or(self.backoff(attempt)));
                        attempt += 1;
                        continue;
                    }
                    return Ok(response);
                }
                Err(e) if is_transient(&e) && attempt < MAX_ATTEMPTS => {
                    thread::sleep(self.backoff(attempt));
                    attempt += 1;
                }
                Err(e) => return Err(e).with_context(|| format!("GET {url}")),
            }
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        self.retry_delay * 2u32.pow(attempt - 1)
    }

    /// Keeps booth.pm page requests at least `page_interval` apart.
    fn throttle(&self) {
        let mut last = self.last_page_request.lock().unwrap();
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < self.page_interval {
                thread::sleep(self.page_interval - elapsed);
            }
        }
        *last = Some(Instant::now());
    }

    fn capture_session(&self, response: &Response<Body>) {
        let prefix = format!("{SESSION_COOKIE}=");
        for value in response.headers().get_all("set-cookie") {
            if let Some(rest) = value.to_str().ok().and_then(|v| v.strip_prefix(&prefix)) {
                let value = rest.split(';').next().unwrap_or_default();
                if !value.is_empty() {
                    *self.session.lock().unwrap() = value.to_string();
                }
            }
        }
    }
}

fn header<'a>(response: &'a Response<Body>, name: &str) -> Option<&'a str> {
    response.headers().get(name).and_then(|v| v.to_str().ok())
}

fn check_fatal(response: &Response<Body>) -> Result<(), Fatal> {
    if response.status().is_redirection()
        && header(response, "location").is_some_and(|l| l.contains("/users/sign_in"))
    {
        return Err(Fatal::SessionExpired);
    }
    if response.headers().contains_key("cf-mitigated") {
        return Err(Fatal::Blocked {
            status: response.status().as_u16(),
        });
    }
    Ok(())
}

fn retry_after(response: &Response<Body>) -> Option<Duration> {
    let seconds: u64 = header(response, "retry-after")?.trim().parse().ok()?;
    Some(Duration::from_secs(seconds).min(MAX_RETRY_AFTER))
}

fn is_transient(error: &ureq::Error) -> bool {
    matches!(
        error,
        ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::ConnectionFailed
            | ureq::Error::BodyStalled
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::test_server::{Reply, TestServer};

    #[test]
    fn sends_cookie_and_user_agent_and_captures_rotation() {
        let server = TestServer::start(|_| {
            Reply::new(200)
                .header("Set-Cookie", "_plaza_session_nktz7u=new; path=/; secure")
                .body("hello")
        });
        let client = BoothClient::for_test("old", &server.url);
        assert_eq!(
            client.get_text(&format!("{}/page", server.url)).unwrap(),
            "hello"
        );
        assert_eq!(client.session(), "new");
        let request = &server.requests()[0];
        assert_eq!(request.headers["cookie"], "_plaza_session_nktz7u=old");
        assert!(request.headers["user-agent"].starts_with("booth-dl/"));
    }

    #[test]
    fn sign_in_redirect_is_fatal() {
        let server = TestServer::start(|_| {
            Reply::new(302).header("Location", "https://accounts.booth.pm/users/sign_in")
        });
        let client = BoothClient::for_test("s", &server.url);
        let err = client
            .get_text(&format!("{}/library", server.url))
            .unwrap_err();
        assert_eq!(err.downcast_ref::<Fatal>(), Some(&Fatal::SessionExpired));
        assert!(is_fatal(&err));
    }

    #[test]
    fn cloudflare_challenge_is_fatal() {
        let server = TestServer::start(|_| Reply::new(403).header("cf-mitigated", "challenge"));
        let client = BoothClient::for_test("s", &server.url);
        let err = client
            .get_text(&format!("{}/library", server.url))
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<Fatal>(),
            Some(&Fatal::Blocked { status: 403 })
        );
    }

    #[test]
    fn retries_server_errors() {
        let calls = AtomicU32::new(0);
        let server = TestServer::start(move |_| {
            if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                Reply::new(503)
            } else {
                Reply::new(200).body("ok")
            }
        });
        let client = BoothClient::for_test("s", &server.url);
        assert_eq!(client.get_text(&format!("{}/x", server.url)).unwrap(), "ok");
        assert_eq!(server.requests().len(), 3);
    }

    #[test]
    fn truncated_body_is_an_error() {
        let server = TestServer::start(|_| Reply::new(200).body("0123456789").cut_after(5));
        let client = BoothClient::for_test("s", &server.url);
        assert!(client.get_text(&format!("{}/x", server.url)).is_err());
    }

    #[test]
    fn not_found_is_none() {
        let server = TestServer::start(|_| Reply::new(404));
        let client = BoothClient::for_test("s", &server.url);
        assert_eq!(
            client
                .get_text_optional(&format!("{}/x", server.url))
                .unwrap(),
            None
        );
    }

    #[test]
    fn resolves_download_redirect() {
        let server = TestServer::start(|_| {
            Reply::new(302).header(
                "Location",
                "https://s6.booth.pm/f/1/2/a.zip?X-Amz-Expires=180",
            )
        });
        let client = BoothClient::for_test("s", &server.url);
        assert_eq!(
            client.resolve_download_url("2").unwrap(),
            "https://s6.booth.pm/f/1/2/a.zip?X-Amz-Expires=180"
        );
        assert_eq!(server.requests()[0].path, "/downloadables/2");
    }

    #[test]
    fn file_requests_send_range_but_no_cookie() {
        let server = TestServer::start(|_| Reply::new(206).body("x"));
        let client = BoothClient::for_test("s", &server.url);
        client.get_file(&format!("{}/f", server.url), 10).unwrap();
        let request = &server.requests()[0];
        assert_eq!(request.headers["range"], "bytes=10-");
        assert!(!request.headers.contains_key("cookie"));
    }
}
