// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The HTTP client builder: the one place a `ureq` agent is configured.
//!
//! Every client trusts the operating system's certificate store (company
//! proxies and local servers the person trusts work as in a browser),
//! names itself (`Quire/<version> (+purpose)`), has the timeouts it is
//! given, and does not turn an HTTP error status into an error: callers
//! read the status. Redirects are an explicit choice ([`Redirects`]),
//! because a client that signs requests or sends a password must not
//! follow one to another host.
//!
//! Behind the `net` feature: only the crates P6 allows to reach the
//! network turn it on (`quire-sync` with `http`, `quire-import`,
//! `quire-intel` with `remote-llm`, `quire-ai` with `client`,
//! `quire-app` with `update-check`).

use std::time::Duration;

use ureq::config::ConfigBuilder;
use ureq::typestate::AgentScope;

/// Whether a client follows redirects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirects {
    /// Never: the 3xx reply is returned as it is (the caller may check the
    /// new address and follow it itself). For a client that signs requests
    /// or sends a password, and one that checks every address first.
    Refuse,
    /// Up to this many. `ureq` drops `Authorization` headers on the way.
    Follow(u32),
}

/// How long a client waits. `None` is no limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timeouts {
    /// The whole request, from connecting to the last byte of the reply.
    pub total: Option<Duration>,
    pub connect: Option<Duration>,
    /// Until the reply's status line and headers.
    pub reply: Option<Duration>,
    /// Between two reads of the reply's body (a server gone quiet).
    pub per_read: Option<Duration>,
}

impl Timeouts {
    /// Only a limit for the whole request.
    pub const fn total(d: Duration) -> Self {
        Self { total: Some(d), connect: None, reply: None, per_read: None }
    }
}

/// `Quire/<version>`, or `Quire/<version> (+purpose)`.
pub fn user_agent(purpose: &str) -> String {
    let v = env!("CARGO_PKG_VERSION");
    if purpose.is_empty() { format!("Quire/{v}") } else { format!("Quire/{v} (+{purpose})") }
}

/// A client configuration with Quire's settings, to which a caller may add
/// its own (a proxy, non-standard methods) before `.build()`.
///
/// `purpose` goes in the user agent (`""` for none).
pub fn config(purpose: &str, timeouts: Timeouts, redirects: Redirects) -> ConfigBuilder<AgentScope> {
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
    let max = match redirects {
        Redirects::Refuse => 0,
        Redirects::Follow(n) => n,
    };
    ureq::Agent::config_builder()
        .tls_config(tls)
        .user_agent(user_agent(purpose))
        .timeout_global(timeouts.total)
        .timeout_connect(timeouts.connect)
        .timeout_recv_response(timeouts.reply)
        .timeout_per_read(timeouts.per_read)
        .http_status_as_error(false)
        .max_redirects(max)
}

/// [`config`], built.
pub fn agent(purpose: &str, timeouts: Timeouts, redirects: Redirects) -> ureq::Agent {
    config(purpose, timeouts, redirects).build().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_client_names_itself_trusts_the_system_and_reads_statuses() {
        let a = agent("models", Timeouts { connect: Some(Duration::from_secs(20)), ..Timeouts::default() }, Redirects::Refuse);
        let c = a.config();
        assert_eq!(user_agent("models"), format!("Quire/{} (+models)", env!("CARGO_PKG_VERSION")));
        assert_eq!(user_agent(""), format!("Quire/{}", env!("CARGO_PKG_VERSION")));
        assert!(!c.http_status_as_error());
        assert_eq!(c.max_redirects(), 0);
        assert_eq!(c.timeouts().connect, Some(Duration::from_secs(20)));
        assert_eq!(c.timeouts().global, None);
        assert!(matches!(c.tls_config().root_certs(), ureq::tls::RootCerts::PlatformVerifier));
        let f = agent("", Timeouts::total(Duration::from_secs(15)), Redirects::Follow(10));
        assert_eq!(f.config().max_redirects(), 10);
        assert_eq!(f.config().timeouts().global, Some(Duration::from_secs(15)));
    }
}
