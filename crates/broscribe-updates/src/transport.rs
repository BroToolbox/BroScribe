//! HTTPS transfers with bounded bodies/socket I/O and cooperative cancellation.
//! Native certificate evaluation can outlive socket budgets or cancellation.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use broscribe_core::Result;
use broscribe_platform::TrustStore;

use crate::{codes, fail};

/// Cooperative cancellation shared with the caller (UI or CLI).
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// [`codes::CANCELLED`] once cancelled.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(fail(codes::CANCELLED, "the update was cancelled"));
        }
        Ok(())
    }
}

/// One GET of a whole body.
pub trait Transport: Send + Sync {
    /// Writes the body of `url` to `sink` and returns its length. More than
    /// `limit` bytes is [`codes::SIZE_MISMATCH`]; a redirect outside the release
    /// policy is [`codes::REDIRECT_REFUSED`]. A 404 from the built-in feed
    /// reports [`broscribe_core::Error::NotFound`]; other HTTP or transport
    /// failures use [`codes::TRANSFER_FAILED`]. `cancel` is honoured between
    /// reads ([`codes::CANCELLED`]).
    fn fetch(&self, url: &str, limit: u64, sink: &mut dyn Write, cancel: &Cancel) -> Result<u64>;
}

const CHUNK: usize = 64 * 1024;

/// [`Transport`] over `ureq`: HTTPS only, bounded GitHub release redirects, no content coding,
/// TLS uses the injected platform strategy. Native evaluation may perform OS
/// retrieval outside socket deadlines; cancellation can wait for that handshake.
pub struct HttpsTransport {
    agent: ureq::Agent,
}

impl HttpsTransport {
    /// Fails closed when the trust store is unreadable or empty.
    pub fn new(trust: &dyn TrustStore) -> Result<Self> {
        let policy = trust.tls_trust()?;
        if policy.root_count() == 0 {
            return Err(fail(
                "trust_store_unavailable",
                "the system trust store has no root certificates",
            ));
        }
        let roots = match policy {
            broscribe_platform::TlsTrust::Platform { .. } => ureq::tls::RootCerts::PlatformVerifier,
            broscribe_platform::TlsTrust::Certificates(roots) => ureq::tls::RootCerts::from(
                roots
                    .iter()
                    .map(|der| ureq::tls::Certificate::from_der(der).to_owned()),
            ),
        };
        let agent = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .accept_encoding("identity")
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_per_read(Some(Duration::from_secs(30)))
            .user_agent(concat!("broscribe-updater/", env!("CARGO_PKG_VERSION")))
            .tls_config(ureq::tls::TlsConfig::builder().root_certs(roots).build())
            .build()
            .into();
        Ok(Self { agent })
    }
}

impl Transport for HttpsTransport {
    fn fetch(&self, url: &str, limit: u64, sink: &mut dyn Write, cancel: &Cancel) -> Result<u64> {
        let response = follow_release_redirects(url, cancel, |url| {
            // ureq messages can carry URLs; report only the failure kind.
            let response = self.agent.get(url).call().map_err(|e| {
                fail(
                    codes::TRANSFER_FAILED,
                    format!("request failed: {}", kind(&e)),
                )
            })?;
            let status = response.status().as_u16();
            if (300..400).contains(&status) {
                let mut locations = response.headers().get_all("location").iter();
                let location = locations.next().and_then(|value| value.to_str().ok());
                let location = location.filter(|_| locations.next().is_none());
                return Ok(Hop::Redirect(status, location.map(str::to_owned)));
            }
            Ok(Hop::Final(response))
        })?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(status_error(url, status));
        }
        if let Some(announced) = response.body().content_length()
            && announced > limit
        {
            return Err(fail(
                codes::SIZE_MISMATCH,
                format!("the server announces {announced} bytes; at most {limit} are expected"),
            ));
        }
        let mut reader = response.into_body().into_reader();
        copy_bounded(&mut reader, sink, limit, cancel)
    }
}

fn status_error(url: &str, status: u16) -> broscribe_core::Error {
    if status == 404 && url == broscribe_core::config::RELEASE_FEED {
        return broscribe_core::Error::NotFound("the latest release has no update envelope".into());
    }
    fail(
        codes::TRANSFER_FAILED,
        format!("the server answered {status}"),
    )
}

const MAX_REDIRECTS: usize = 5;
const GITHUB_RELEASE_PATH: &str = "https://github.com/BroToolbox/BroScribe/releases/";

enum Hop<T> {
    Redirect(u16, Option<String>),
    Final(T),
}

fn github_release_url(url: &str) -> Option<(Option<&str>, &str)> {
    let path = url.split('?').next().unwrap_or(url);
    let path = path.strip_prefix(GITHUB_RELEASE_PATH)?;
    let valid = |part: &str| {
        !matches!(part, "" | "." | "..")
            && part
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || b"._-".contains(&ch))
    };
    let parts: Vec<_> = path.split('/').collect();
    match parts.as_slice() {
        ["latest", "download", name] if valid(name) => Some((None, name)),
        ["download", tag, name] if valid(tag) && valid(name) => Some((Some(tag), name)),
        _ => None,
    }
}

fn github_asset_url(url: &str) -> bool {
    crate::settings::check_https_url(url).is_ok()
        && url
            .strip_prefix("https://")
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(host, path)| {
                !path.starts_with('/')
                    && !path.contains('\\')
                    && matches!(
                        host,
                        "objects.githubusercontent.com"
                            | "release-assets.githubusercontent.com"
                            | "github-releases.githubusercontent.com"
                    )
            })
}

fn follow_release_redirects<T>(
    url: &str,
    cancel: &Cancel,
    mut get: impl FnMut(&str) -> Result<Hop<T>>,
) -> Result<T> {
    crate::settings::check_https_url(url)?;
    let release = github_release_url(url);
    let mut current = url.to_owned();
    for hops in 0..=MAX_REDIRECTS {
        cancel.check()?;
        match get(&current)? {
            Hop::Final(response) => return Ok(response),
            Hop::Redirect(status, Some(next))
                if release.is_some()
                    && hops < MAX_REDIRECTS
                    && matches!(status, 301 | 302 | 303 | 307 | 308)
                    && (github_asset_url(&next)
                        || (hops == 0
                            && crate::settings::check_https_url(&next).is_ok()
                            && !next.contains('?')
                            && matches!(
                                (release, github_release_url(&next)),
                                (Some((None, from)), Some((Some(_), to))) if from == to
                            ))) =>
            {
                // A fresh GET carries neither authorization nor a referrer.
                current = next;
            }
            Hop::Redirect(status, _) => {
                return Err(fail(
                    codes::REDIRECT_REFUSED,
                    format!("the server redirected ({status}) outside the release redirect policy"),
                ));
            }
        }
    }
    unreachable!("the final redirect cannot be followed")
}

fn kind(error: &ureq::Error) -> &'static str {
    match error {
        ureq::Error::Timeout(_) => "timed out",
        ureq::Error::Io(_) => "connection error",
        ureq::Error::Tls(_) => "TLS failure",
        ureq::Error::RequireHttpsOnly(_) => "not HTTPS",
        ureq::Error::TooManyRedirects => "redirected",
        _ => "transfer error",
    }
}

/// Copies at most `limit` bytes, checking `cancel` between reads.
fn copy_bounded(
    reader: &mut dyn Read,
    sink: &mut dyn Write,
    limit: u64,
    cancel: &Cancel,
) -> Result<u64> {
    let mut buf = vec![0; CHUNK];
    let mut total = 0u64;
    loop {
        cancel.check()?;
        let read = reader.read(&mut buf);
        // A cancellation during the read (the final, end-of-body one
        // included) wins over its result.
        cancel.check()?;
        let n = match read {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(fail(
                    codes::TRANSFER_FAILED,
                    format!("reading the body failed: {}", e.kind()),
                ));
            }
        };
        total += n as u64;
        if total > limit {
            return Err(fail(
                codes::SIZE_MISMATCH,
                format!("the server sent more than {limit} bytes"),
            ));
        }
        sink.write_all(&buf[..n])
            .map_err(|e| broscribe_core::Error::io("writing the download", e))?;
    }
}

#[derive(Default)]
struct FakeTransportState {
    bodies: HashMap<String, Vec<u8>>,
    redirects: HashMap<String, Option<String>>,
    cancel_after: Option<(Cancel, u64)>,
    requests: Vec<String>,
}

/// Deterministic [`Transport`] serving in-memory bodies with the same
/// bounds, redirect policy and cancellation points as [`HttpsTransport`].
#[derive(Clone, Default)]
pub struct FakeTransport {
    state: Arc<Mutex<FakeTransportState>>,
}

impl FakeTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn serve(&self, url: &str, body: Vec<u8>) {
        self.state
            .lock()
            .unwrap()
            .bodies
            .insert(url.to_owned(), body);
    }

    /// Answers `url` with a redirect.
    pub fn redirect(&self, url: &str) {
        self.state
            .lock()
            .unwrap()
            .redirects
            .insert(url.to_owned(), None);
    }

    /// Answers with a redirect to an explicit URL.
    pub fn redirect_to(&self, url: &str, location: &str) {
        self.state
            .lock()
            .unwrap()
            .redirects
            .insert(url.to_owned(), Some(location.to_owned()));
    }

    /// Cancels `cancel` once `bytes` bytes of any body were delivered.
    pub fn cancel_after(&self, cancel: Cancel, bytes: u64) {
        self.state.lock().unwrap().cancel_after = Some((cancel, bytes));
    }

    /// URLs requested so far.
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }
}

struct CancellingReader<'a> {
    body: &'a [u8],
    sent: u64,
    trigger: Option<(Cancel, u64)>,
}

impl Read for CancellingReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some((cancel, at)) = &self.trigger
            && self.sent >= *at
        {
            cancel.cancel();
        }
        let n = buf.len().min(self.body.len()).min(4096);
        buf[..n].copy_from_slice(&self.body[..n]);
        self.body = &self.body[n..];
        self.sent += n as u64;
        Ok(n)
    }
}

impl Transport for FakeTransport {
    fn fetch(&self, url: &str, limit: u64, sink: &mut dyn Write, cancel: &Cancel) -> Result<u64> {
        let original = url;
        let (body, trigger) = follow_release_redirects(url, cancel, |url| {
            let mut state = self.state.lock().unwrap();
            state.requests.push(url.to_owned());
            if let Some(location) = state.redirects.get(url) {
                return Ok(Hop::Redirect(302, location.clone()));
            }
            let body = state
                .bodies
                .get(url)
                .cloned()
                .ok_or_else(|| status_error(original, 404))?;
            Ok(Hop::Final((body, state.cancel_after.clone())))
        })?;
        let mut reader = CancellingReader {
            body: &body,
            sent: 0,
            trigger,
        };
        copy_bounded(&mut reader, sink, limit, cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_github_release_redirects_reach_asset_hosts() {
        let feed = broscribe_core::config::RELEASE_FEED;
        let asset = "https://release-assets.githubusercontent.com/path?sig=token";
        let second = "https://objects.githubusercontent.com/next";
        let fake = FakeTransport::new();
        let tagged = "https://github.com/BroToolbox/BroScribe/releases/download/v1.0.0/broscribe-update.json";
        fake.redirect_to(feed, tagged);
        fake.redirect_to(tagged, asset);
        fake.redirect_to(asset, second);
        fake.serve(second, b"verified downstream".to_vec());
        let mut body = Vec::new();
        assert_eq!(fake.fetch(feed, 19, &mut body, &Cancel::new()).unwrap(), 19);
        assert_eq!(fake.requests(), [feed, tagged, asset, second]);
        assert_eq!(body, b"verified downstream");

        for (start, end) in [
            (feed, "http://objects.githubusercontent.com/next"),
            (feed, "https://user@objects.githubusercontent.com/next"),
            (
                feed,
                "https://objects.githubusercontent.com.evil.example/next",
            ),
            (feed, "https://objects.githubusercontent.com:8443/next"),
            (feed, "https://objects.githubusercontent.com/next#fragment"),
            (
                feed,
                "https://github.com/BroToolbox/BroScribe/releases/download/v1.0.0/a",
            ),
            (
                "https://github.com/other/project/releases/download/v1.0.0/a",
                asset,
            ),
            (
                "https://github.com/BroToolbox/BroScribe/releases/download/v1.0.0/a/b",
                asset,
            ),
            ("https://mirror.example/feed", asset),
            (asset, second),
        ] {
            let fake = FakeTransport::new();
            fake.redirect_to(start, end);
            let err = fake
                .fetch(start, 100, &mut Vec::new(), &Cancel::new())
                .unwrap_err();
            assert_eq!(err.code(), codes::REDIRECT_REFUSED, "{start} -> {end}");
            assert_eq!(fake.requests(), [start]);
        }
    }

    #[test]
    fn redirects_stop_after_five_hops() {
        let fake = FakeTransport::new();
        let start = "https://github.com/BroToolbox/BroScribe/releases/download/v1.0.0/a.zip";
        let asset = "https://github-releases.githubusercontent.com/asset";
        fake.redirect_to(start, asset);
        fake.redirect_to(asset, asset);
        let error = fake
            .fetch(start, 100, &mut Vec::new(), &Cancel::new())
            .unwrap_err();
        assert_eq!(error.code(), codes::REDIRECT_REFUSED);
        assert_eq!(fake.requests().len(), 6);
    }
}
