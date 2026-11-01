// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Opt-in check for a newer release on GitHub.
//!
//! Off until the user turns it on (Settings → About, or the last tour card).
//! When on, Quire asks the GitHub Releases API at most once a day. The request
//! carries no identifiers: just `User-Agent: Quire/<version>`. Nothing is
//! downloaded or installed; a newer version shows a link to its release page.

use std::time::{SystemTime, UNIX_EPOCH};

use crossbeam_channel::Receiver;

use crate::workspace::{NoticeLevel, Workspace};

/// Seconds between automatic checks.
const CHECK_EVERY_SECS: i64 = 24 * 60 * 60;

/// The version this build reports.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Without the leading `v`, e.g. `0.4.1`.
    pub version: String,
    /// The release page.
    pub url: String,
}

/// A parsed `MAJOR.MINOR.PATCH[-pre]` version, ordered as `SemVer` orders them
/// (a pre-release sorts before its release).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    core: (u64, u64, u64),
    pre: Option<String>,
}

impl Version {
    fn parse(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches('v');
        let s = s.split('+').next()?; // build metadata never affects order
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) if !p.is_empty() => (c, Some(p.to_string())),
            Some(_) => return None,
            None => (s, None),
        };
        let mut it = core.split('.').map(str::parse::<u64>);
        let v = (it.next()?.ok()?, it.next()?.ok()?, it.next()?.ok()?);
        if it.next().is_some() {
            return None;
        }
        Some(Self { core: v, pre })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        self.core.cmp(&other.core).then_with(|| match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => cmp_pre(a, b),
        })
    }
}

/// Dot-separated identifiers: numbers compare numerically and sort before words.
fn cmp_pre(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut x, mut y) = (a.split('.'), b.split('.'));
    loop {
        match (x.next(), y.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) => {
                let o = match (p.parse::<u64>(), q.parse::<u64>()) {
                    (Ok(m), Ok(n)) => m.cmp(&n),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => p.cmp(q),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
        }
    }
}

/// Is `candidate` a newer version than `current`? Unparseable input is never newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (Version::parse(candidate), Version::parse(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

/// `https://github.com/OWNER/REPO` → the "latest release" API URL.
pub fn latest_release_api(repo_url: &str) -> Option<String> {
    let rest = repo_url.trim_end_matches('/').strip_prefix("https://github.com/")?;
    let mut parts = rest.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(format!("https://api.github.com/repos/{owner}/{repo}/releases/latest"))
}

/// Read the API's answer: `tag_name` and `html_url` of the latest
/// non-draft, non-pre-release.
pub fn parse_latest(json: &str) -> Result<Release, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("unexpected reply: {e}"))?;
    let tag = v.get("tag_name").and_then(serde_json::Value::as_str).ok_or("reply has no tag_name")?;
    let url = v.get("html_url").and_then(serde_json::Value::as_str).unwrap_or_default();
    if v.get("draft").and_then(serde_json::Value::as_bool) == Some(true)
        || v.get("prerelease").and_then(serde_json::Value::as_bool) == Some(true)
    {
        return Err("latest release is a draft or pre-release".into());
    }
    if Version::parse(tag).is_none() {
        return Err(format!("“{tag}” isn't a version"));
    }
    Ok(Release { version: tag.trim_start_matches('v').to_string(), url: url.to_string() })
}

#[cfg(feature = "update-check")]
fn fetch_latest(api_url: &str) -> Result<Release, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .user_agent(format!("Quire/{CURRENT}"))
        .tls_config(ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build())
        .build()
        .into();
    let body = agent
        .get(api_url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("couldn't reach GitHub: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("couldn't read the reply: {e}"))?;
    parse_latest(&body)
}

#[cfg(not(feature = "update-check"))]
fn fetch_latest(_api_url: &str) -> Result<Release, String> {
    Err("this build was made without the update check".into())
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// What the update check knows right now.
#[derive(Debug)]
pub struct UpdateState {
    /// Where to ask. Defaults to this repository's releases.
    pub api_url: Option<String>,
    rx: Option<Receiver<Result<Release, String>>>,
    /// A started check was asked for by the user (so report "up to date" too).
    manual: bool,
    /// A newer release, if one was found and not skipped.
    pub available: Option<Release>,
    /// The last check's outcome, for Settings → About.
    pub status: Option<String>,
}

impl Default for UpdateState {
    fn default() -> Self {
        // QUIRE_UPDATE_URL points the check elsewhere (a fork's releases, or a test server).
        let api_url =
            std::env::var("QUIRE_UPDATE_URL").ok().filter(|u| !u.is_empty()).or_else(|| latest_release_api(env!("CARGO_PKG_REPOSITORY")));
        Self { api_url, rx: None, manual: false, available: None, status: None }
    }
}

impl UpdateState {
    pub const fn checking(&self) -> bool {
        self.rx.is_some()
    }
}

impl Workspace {
    /// Ask GitHub for the latest release in the background. `manual` checks
    /// run even when automatic checks are off, and report "up to date".
    pub fn check_for_updates(&mut self, manual: bool) {
        if self.updates.rx.is_some() {
            return;
        }
        let Some(url) = self.updates.api_url.clone() else {
            self.updates.status = Some("This build doesn't know where its releases are.".into());
            return;
        };
        self.settings.last_update_check = now_secs();
        let _ = self.settings.save();
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(fetch_latest(&url));
        });
        self.updates.rx = Some(rx);
        self.updates.manual = manual;
        self.updates.status = Some("Checking…".into());
    }

    /// Start the daily check when it's on and due.
    pub(crate) fn maybe_check_updates(&mut self) {
        if self.settings.check_updates
            && self.updates.rx.is_none()
            && now_secs().saturating_sub(self.settings.last_update_check) >= CHECK_EVERY_SECS
        {
            self.check_for_updates(false);
        }
    }

    /// Collect a finished check. Returns true when something changed.
    pub(crate) fn poll_updates(&mut self) -> bool {
        let Some(rx) = &self.updates.rx else { return false };
        let Ok(res) = rx.try_recv() else { return false };
        self.updates.rx = None;
        let manual = std::mem::take(&mut self.updates.manual);
        match res {
            Ok(r) if is_newer(&r.version, CURRENT) && (manual || r.version != self.settings.skip_update_version) => {
                self.updates.status = Some(format!("Quire {} is available.", r.version));
                self.notify(NoticeLevel::Info, format!("Quire {} is available. Click the status bar to see what's new.", r.version));
                self.updates.available = Some(r);
            }
            Ok(_) => {
                self.updates.available = None;
                self.updates.status = Some(format!("Quire {CURRENT} is the latest version."));
                if manual {
                    self.notify(NoticeLevel::Info, format!("You're on the latest version ({CURRENT})."));
                }
            }
            Err(e) => {
                self.updates.status = Some(format!("Couldn't check: {e}"));
                if manual {
                    self.notify(NoticeLevel::Warn, format!("Couldn't check for updates: {e}"));
                }
            }
        }
        true
    }

    /// Stop showing the available update until an even newer one appears.
    pub fn skip_update(&mut self) {
        if let Some(r) = self.updates.available.take() {
            self.settings.skip_update_version = r.version;
            let _ = self.settings.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_like_semver() {
        assert!(is_newer("0.4.1", "0.4.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "1.0.0-rc.2"));
        assert!(is_newer("1.0.0-rc.10", "1.0.0-rc.2"));
        assert!(is_newer("1.0.0-rc.1", "1.0.0-beta"));
        assert!(!is_newer("0.4.0", "0.4.0"));
        assert!(!is_newer("0.4.0+build.7", "0.4.0"));
        assert!(!is_newer("0.3.9", "0.4.0"));
        assert!(!is_newer("nightly", "0.4.0"));
        assert!(!is_newer("1.2", "0.4.0"));
        assert!(!is_newer("1.2.3.4", "0.4.0"));
    }

    #[test]
    fn api_url_and_reply() {
        assert_eq!(
            latest_release_api("https://github.com/BhanuNexus/quire").as_deref(),
            Some("https://api.github.com/repos/BhanuNexus/quire/releases/latest")
        );
        assert_eq!(latest_release_api("https://gitlab.com/a/b"), None);
        assert_eq!(latest_release_api("https://github.com/a"), None);
        let r = parse_latest(
            r#"{"tag_name":"v0.4.1","html_url":"https://github.com/x/y/releases/tag/v0.4.1","draft":false,"prerelease":false}"#,
        )
        .unwrap();
        assert_eq!(r, Release { version: "0.4.1".into(), url: "https://github.com/x/y/releases/tag/v0.4.1".into() });
        assert!(parse_latest(r#"{"tag_name":"v0.5.0-rc.1","prerelease":true}"#).is_err());
        assert!(parse_latest(r#"{"message":"Not Found"}"#).is_err());
        assert!(parse_latest("<html>").is_err());
    }

    /// Serve one canned HTTP reply on a local port.
    #[cfg(feature = "update-check")]
    fn serve_once(body: &'static str) -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/repos/x/y/releases/latest", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            while r.read_line(&mut line).unwrap() > 2 {
                line.clear();
            }
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        url
    }

    #[cfg(feature = "update-check")]
    fn wait(w: &mut Workspace) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !w.poll_updates() {
            assert!(std::time::Instant::now() < deadline, "update check timed out");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(feature = "update-check")]
    #[test]
    fn finds_and_skips_a_newer_release() {
        let mut w = Workspace::new(crate::Settings { check_updates: true, ..Default::default() });
        w.settings.last_update_check = 0;
        w.updates.api_url =
            Some(serve_once(r#"{"tag_name":"v99.0.0","html_url":"https://example.invalid/r","draft":false,"prerelease":false}"#));
        w.maybe_check_updates();
        assert!(w.updates.checking());
        wait(&mut w);
        assert_eq!(w.updates.available.as_ref().map(|r| r.version.as_str()), Some("99.0.0"));
        assert!(w.settings.last_update_check > 0);
        // Not due again for a day.
        w.maybe_check_updates();
        assert!(!w.updates.checking());

        w.skip_update();
        assert!(w.updates.available.is_none());
        w.updates.api_url =
            Some(serve_once(r#"{"tag_name":"v99.0.0","html_url":"https://example.invalid/r","draft":false,"prerelease":false}"#));
        w.check_for_updates(false);
        wait(&mut w);
        assert!(w.updates.available.is_none(), "a skipped version stays hidden");
    }

    #[cfg(feature = "update-check")]
    #[test]
    fn manual_check_reports_up_to_date_and_off_means_off() {
        let mut w = Workspace::new(crate::Settings::default());
        w.settings.last_update_check = 0;
        w.maybe_check_updates();
        assert!(!w.updates.checking(), "automatic checks are off by default");
        w.updates.api_url =
            Some(serve_once(r#"{"tag_name":"v0.0.1","html_url":"https://example.invalid/r","draft":false,"prerelease":false}"#));
        w.check_for_updates(true);
        wait(&mut w);
        assert!(w.updates.available.is_none());
        assert!(w.updates.status.as_deref().unwrap_or_default().contains("latest version"));
        assert!(w.notices.iter().any(|n| n.text.contains("latest version")));
    }
}
