//! Bearer-token authentication for the API and the gated proxies.
//!
//! # What is actually being protected
//!
//! It is tempting to think a lending daemon has nothing worth stealing. It
//! has three things:
//!
//! 1. **The hardware itself.** The proxy ports are a free GPU for anyone on
//!    the network who finds them.
//! 2. **The switches.** `POST /admin/lending/disable` silently stops the
//!    machine ever lending again. Nothing breaks loudly; the cluster just
//!    quietly loses its capacity.
//! 3. **The owner's presence.** This is the one that is easy to miss.
//!    `/status` and `/metrics` report whether the owner is at their desk,
//!    whether the session is locked, and whether they are watching
//!    something. That is a log of when a specific human is at home and what
//!    they are doing, readable by anything on the LAN. It deserves a token
//!    for privacy reasons even where the compute does not.
//!
//! # Scopes
//!
//! | Scope | Covers |
//! | ----- | ------ |
//! | [`Scope::Borrow`] | the gated proxy listeners — using the hardware |
//! | [`Scope::Read`] | `/status`, `/metrics` — observing the owner |
//! | [`Scope::Admin`] | `/admin/*` — changing what the daemon will lend |
//!
//! `/healthz` is deliberately never authenticated: it reports only
//! liveness, and container orchestrators and uptime monitors need it without
//! credentials.
//!
//! # Defaults
//!
//! Two rules, chosen so that securing the daemon never depends on the
//! operator having read the manual:
//!
//! - **Loopback is trusted** (configurable), for reading and, on Windows,
//!   for pausing only. A direct local client needs no token to read
//!   `/status`, so the tray app and `vacancyd status` work with no setup.
//!   A TCP connection does not show which account opened it, so on Windows
//!   the actions that can increase lending or rewrite the Administrators-only
//!   configuration need an account the service can verify, over the admin
//!   pipe, or an admin token; `vacancyd::local_admin` holds that rule. Other
//!   platforms still grant every admin action to loopback this release. The
//!   trust covers direct local clients only: a loopback request that carries
//!   a proxy forwarding header, whose `Host` is not a loopback name, or
//!   that a browser marks as sent from another site, is judged like a
//!   remote request (see [`loopback_trust_refusal`]). A token presented on
//!   loopback is honoured when it carries the scope.
//! - **Remote listeners require credentials.** Configuration validation
//!   rejects a non-loopback API or proxy listener when no tokens exist. An
//!   explicit `allow_unauthenticated_remote` escape hatch exists for a
//!   separately secured network, but is off by default.
//! - **With no tokens configured, `Admin` is still refused off-box.** This
//!   remains defense in depth for an explicitly insecure remote deployment.
//!
//! Configure a single token and the posture flips to closed: every remote
//! request then needs a token carrying the right scope. "If you configured
//! auth, it is enforced" is the least surprising rule available.
//!
//! # What this is not
//!
//! Not a user system, not OAuth, not per-request signing. Shared bearer
//! tokens over a trusted network, which is the right weight for a daemon
//! whose blast radius is one workstation's spare cycles. There is no
//! rate limiting or lockout of failed attempts; a token long enough to be
//! unguessable is doing that work instead, which is why `vacancyd gen-token`
//! exists.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use axum::http::HeaderMap;
use serde::Deserialize;

/// What a token is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Send work through the gated proxies.
    Borrow,
    /// Read `/status` and `/metrics` — including owner presence.
    Read,
    /// Change the lending switches.
    Admin,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Borrow => "borrow",
            Self::Read => "read",
            Self::Admin => "admin",
        }
    }
}

/// One credential, as written in the config file.
///
/// The value can be given three ways, and inline is the discouraged one.
/// `value_file` mirrors the common `_FILE` convention, so Docker secrets and
/// Kubernetes secret mounts work
/// without anything extra.
/// `Debug` is implemented by hand below rather than derived: `Config`
/// derives `Debug`, so a single `tracing::debug!("{config:?}")` anywhere
/// would otherwise print every inline secret into the log.
#[derive(Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenConfig {
    /// Identifies the credential in logs and metrics. **Never the secret.**
    pub name: String,
    pub scopes: Vec<Scope>,
    /// The token, inline. Discouraged: it puts a secret in a file that gets
    /// copied, backed up and occasionally committed.
    #[serde(default)]
    pub value: Option<String>,
    /// Read the token from this environment variable.
    #[serde(default)]
    pub value_env: Option<String>,
    /// Read the token from this file. Trailing whitespace is trimmed, so a
    /// file written by `echo` works.
    #[serde(default)]
    pub value_file: Option<PathBuf>,
}

impl std::fmt::Debug for TokenConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenConfig")
            .field("name", &self.name)
            .field("scopes", &self.scopes)
            .field("value", &self.value.as_ref().map(|_| "<redacted>"))
            .field("value_env", &self.value_env)
            .field("value_file", &self.value_file)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    /// Accept direct requests from 127.0.0.1 / ::1 without a token. Which
    /// admin actions such a request may take is limited separately; see
    /// `vacancyd::local_admin`.
    pub trust_loopback: bool,
    /// Permit API and proxy listeners outside loopback with no credentials.
    /// This exposes borrowed hardware and owner-presence data; use only when
    /// another network boundary provides the access control.
    pub allow_unauthenticated_remote: bool,
    #[serde(rename = "token")]
    pub tokens: Vec<TokenConfig>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            trust_loopback: true,
            allow_unauthenticated_remote: false,
            tokens: Vec::new(),
        }
    }
}

struct ResolvedToken {
    name: String,
    scopes: BTreeSet<Scope>,
    secret: Vec<u8>,
}

/// Why a request was turned away. Distinguished because they mean different
/// things to a client: retry with a credential, versus stop asking.
#[derive(Debug, PartialEq, Eq)]
pub enum Denial {
    /// No credential presented. 401.
    Missing,
    /// Credential presented but unrecognised. 401.
    Invalid,
    /// Credential recognised but not permitted this scope. 403.
    Forbidden,
    /// A loopback request that loopback trust does not cover (it came
    /// through a proxy or names a foreign host), with no credential able to
    /// authorise it because none is configured. 403.
    NotDirect,
}

impl Denial {
    pub fn status(&self) -> axum::http::StatusCode {
        match self {
            Self::Missing | Self::Invalid => axum::http::StatusCode::UNAUTHORIZED,
            Self::Forbidden | Self::NotDirect => axum::http::StatusCode::FORBIDDEN,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Invalid => "invalid",
            Self::Forbidden => "forbidden",
            Self::NotDirect => "not_direct",
        }
    }
}

/// How a request came to be allowed — carried so logs and metrics can say
/// *which* credential is borrowing the machine.
#[derive(Debug, PartialEq, Eq)]
pub enum Grant {
    Loopback,
    Token(String),
    /// Allowed only because no tokens are configured at all.
    Unauthenticated,
}

impl Grant {
    pub fn label(&self) -> &str {
        match self {
            Self::Loopback => "loopback",
            Self::Token(n) => n,
            Self::Unauthenticated => "anonymous",
        }
    }
}

pub struct Authenticator {
    trust_loopback: bool,
    allow_unauthenticated_remote: bool,
    tokens: Vec<ResolvedToken>,
}

/// Names the credentials it holds, never their values. Written out for the
/// same reason as [`TokenConfig`]'s: the derived form would print secrets.
impl std::fmt::Debug for Authenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authenticator")
            .field("trust_loopback", &self.trust_loopback)
            .field(
                "tokens",
                &self.tokens.iter().map(|t| &t.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Authenticator {
    /// Resolve every configured token at startup.
    ///
    /// Deliberately fails hard on a missing env var or unreadable file. The
    /// alternative — carrying on with one fewer credential — produces a
    /// daemon that looks configured, silently rejects the client that
    /// mattered, and takes an afternoon to diagnose.
    pub fn new(cfg: &AuthConfig) -> Result<Self> {
        let mut tokens = Vec::new();
        let mut seen = BTreeSet::new();
        for t in &cfg.tokens {
            if t.name.trim().is_empty() {
                bail!("auth token name must not be empty");
            }
            if !seen.insert(t.name.clone()) {
                bail!("duplicate auth token name '{}'", t.name);
            }
            if t.scopes.is_empty() {
                bail!("auth token '{}' grants no scopes", t.name);
            }
            let sources = [
                t.value.is_some(),
                t.value_env.is_some(),
                t.value_file.is_some(),
            ]
            .iter()
            .filter(|x| **x)
            .count();
            if sources != 1 {
                bail!(
                    "auth token '{}': set exactly one of value, value_env, value_file (found {sources})",
                    t.name
                );
            }
            let secret = if let Some(v) = &t.value {
                v.clone()
            } else if let Some(var) = &t.value_env {
                std::env::var(var).with_context(|| {
                    format!(
                        "auth token '{}': environment variable {var} is not set",
                        t.name
                    )
                })?
            } else {
                let path = t.value_file.as_ref().expect("checked above");
                std::fs::read_to_string(path)
                    .with_context(|| {
                        format!("auth token '{}': reading {}", t.name, path.display())
                    })?
                    .trim_end()
                    .to_string()
            };
            if secret.trim().is_empty() {
                bail!("auth token '{}' resolved to an empty value", t.name);
            }
            if secret.len() < 16 {
                // Not a hard failure: an operator may have a reason. But an
                // unrate-limited bearer token this short is guessable, and
                // saying so once at startup is cheap.
                tracing::warn!(
                    token = t.name,
                    len = secret.len(),
                    "auth token is short; prefer `vacancyd gen-token`"
                );
            }
            tokens.push(ResolvedToken {
                name: t.name.clone(),
                scopes: t.scopes.iter().copied().collect(),
                secret: secret.into_bytes(),
            });
        }
        Ok(Self {
            trust_loopback: cfg.trust_loopback,
            allow_unauthenticated_remote: cfg.allow_unauthenticated_remote,
            tokens,
        })
    }

    pub fn is_configured(&self) -> bool {
        !self.tokens.is_empty()
    }

    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Decide whether this request may proceed at `scope`.
    ///
    /// `listener_port` is the local port the request arrived on, when known;
    /// a loopback `Host` header naming a different port is then not trusted.
    pub fn check(
        &self,
        scope: Scope,
        headers: &HeaderMap,
        peer: IpAddr,
        listener_port: Option<u16>,
    ) -> Result<Grant, Denial> {
        let indirect_loopback = if self.trust_loopback && peer.is_loopback() {
            match loopback_trust_refusal(headers, listener_port) {
                None => {
                    // A token carrying the scope still names its holder, so an
                    // admin token is honoured on loopback too: loopback trust
                    // alone no longer covers every admin action on Windows
                    // (see `vacancyd::local_admin`).
                    let token = bearer(headers).and_then(|presented| self.matching(presented));
                    return Ok(match token {
                        Some(t) if t.scopes.contains(&scope) => Grant::Token(t.name.clone()),
                        _ => Grant::Loopback,
                    });
                }
                Some(reason) => {
                    tracing::debug!(reason, "loopback request not trusted");
                    true
                }
            }
        } else {
            false
        };
        let presented = bearer(headers);
        if self.tokens.is_empty() {
            // A loopback listener with no tokens is valid configuration, so
            // anonymous access here would hand a proxied client or a
            // rebinding web page exactly what loopback trust just withheld.
            if indirect_loopback && !self.allow_unauthenticated_remote {
                return Err(Denial::NotDirect);
            }
            // No credentials exist, so nothing could ever authenticate. Keep
            // reading and borrowing working as they did before auth existed,
            // but never hand out the switches to an anonymous caller.
            return match scope {
                Scope::Admin => Err(Denial::Forbidden),
                _ => Ok(Grant::Unauthenticated),
            };
        }
        let Some(presented) = presented else {
            return Err(Denial::Missing);
        };

        match self.matching(presented) {
            None => Err(Denial::Invalid),
            Some(t) if t.scopes.contains(&scope) => Ok(Grant::Token(t.name.clone())),
            Some(_) => Err(Denial::Forbidden),
        }
    }

    /// The configured token equal to `presented`, if any.
    fn matching(&self, presented: &str) -> Option<&ResolvedToken> {
        // Compare against every token without an early exit, so the time
        // taken does not reveal which credential matched or how many are
        // configured.
        let mut matched: Option<&ResolvedToken> = None;
        for t in &self.tokens {
            if constant_time_eq(&t.secret, presented.as_bytes()) {
                matched = Some(t);
            }
        }
        matched
    }
}

/// Headers a reverse proxy adds to say whom it is forwarding for.
const FORWARDING_HEADERS: [&str; 6] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "via",
];

/// Why a request from a loopback peer is not a direct local client, if it is
/// not.
///
/// Loopback trust exists for the owner's own tools, which connect straight
/// to the port and name it by a loopback address. Two other things also
/// arrive from loopback:
///
/// - **A reverse proxy or tunnel on the same machine.** Every client behind
///   it would inherit full trust. A proxy that says so with a forwarding
///   header is caught here. One that adds no header, such as a raw TCP port
///   forward or an SSH tunnel, cannot be told apart from a local client; a
///   machine with such a forwarder must set `trust_loopback = false`.
/// - **A web page using DNS rebinding.** The page's host name resolves to
///   127.0.0.1, so the browser treats the daemon as the page's own origin and
///   lets the script read `/status`. The browser always sends the page's host
///   name as `Host`, so requiring a loopback name refuses it.
///
/// - **A web page sending a cross-site request.** A plain HTML form can POST
///   to `http://127.0.0.1:<port>/` with a loopback `Host`, and the browser
///   delivers it without asking. It marks the request with
///   `Sec-Fetch-Site: cross-site` and the page's `Origin`, so either signal
///   refuses it. An `Origin` of the machine itself (a loopback address or
///   `localhost`) or of a Tauri desktop app (`tauri://localhost`,
///   `http://tauri.localhost`) is local software and keeps its trust. Native
///   clients, including the owner console's service client, send neither
///   header.
///
/// A request without a `Host` header did not come from a browser and is
/// accepted. `listener_port` is checked when the caller knows it.
pub fn loopback_trust_refusal(
    headers: &HeaderMap,
    listener_port: Option<u16>,
) -> Option<&'static str> {
    if FORWARDING_HEADERS
        .iter()
        .any(|name| headers.contains_key(*name))
    {
        return Some("forwarded");
    }
    if headers
        .get_all("sec-fetch-site")
        .iter()
        .any(|value| value.as_bytes().eq_ignore_ascii_case(b"cross-site"))
    {
        return Some("cross_site");
    }
    if headers
        .get_all(axum::http::header::ORIGIN)
        .iter()
        .any(|origin| !origin_is_local(origin))
    {
        return Some("foreign_origin");
    }
    let host = headers.get(axum::http::header::HOST)?;
    if host_is_local(host, listener_port) {
        None
    } else {
        Some("foreign_host")
    }
}

/// A browser `Origin` that names local software: `http` or `https` on a
/// loopback address or `localhost` (any port), or a Tauri app's own origin.
/// The opaque origin `null`, sent by sandboxed frames and some redirects, is
/// not local.
fn origin_is_local(value: &axum::http::HeaderValue) -> bool {
    let Ok(origin) = value.to_str() else {
        return false;
    };
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    let Ok(authority) = authority.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    if authority.as_str().contains('@') {
        return false;
    }
    let host = authority.host();
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    match scheme.as_str() {
        "http" | "https" => loopback || host.eq_ignore_ascii_case("tauri.localhost"),
        "tauri" => host.eq_ignore_ascii_case("localhost") && authority.port().is_none(),
        _ => false,
    }
}

/// `localhost` or a loopback IP literal, with the listener's port when one is
/// known (a missing port means 80).
fn host_is_local(value: &axum::http::HeaderValue, listener_port: Option<u16>) -> bool {
    let Ok(authority) = value
        .to_str()
        .unwrap_or_default()
        .parse::<axum::http::uri::Authority>()
    else {
        return false;
    };
    if authority.as_str().contains('@') {
        return false;
    }
    let host = authority.host();
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    let loopback_name = host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    let port_matches = match listener_port {
        None => true,
        Some(expected) => authority.port_u16().unwrap_or(80) == expected,
    };
    loopback_name && port_matches
}

/// Extract a bearer token, tolerating the casing variations clients use.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, value) = raw.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| value.trim())
        .filter(|v| !v.is_empty())
}

/// Compare two secrets without leaking their contents through timing.
///
/// Hand-rolled rather than pulling a crate in: it is six lines, and the
/// property it needs — no early exit on the first differing byte — is
/// visible on inspection. Length is compared first and does leak, which is
/// accepted: token *length* is not the secret, and rejecting on length is
/// what every practical implementation does.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Generate a token with 256 bits of entropy from the OS RNG.
///
/// URL-safe base64 without padding, so it survives shells, YAML, `.env`
/// files and HTTP headers without quoting.
pub fn generate_token() -> Result<String> {
    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw)
        .map_err(|e| anyhow::anyhow!("reading from the OS random source: {e}"))?;
    Ok(base64_url(&raw))
}

/// Minimal URL-safe base64 (RFC 4648 §5, unpadded).
///
/// Written out rather than adding a dependency for one call site.
fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        let idx = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        // 3 input bytes -> 4 chars; 2 -> 3; 1 -> 2.
        for i in idx.iter().take(chunk.len() + 1) {
            out.push(ALPHABET[*i as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const LOOPBACK: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    const REMOTE: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 1, 10, 5));

    fn hdrs(auth: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(a) = auth {
            h.insert(
                axum::http::header::AUTHORIZATION,
                HeaderValue::from_str(a).unwrap(),
            );
        }
        h
    }

    fn with_tokens(tokens: Vec<TokenConfig>) -> Authenticator {
        Authenticator::new(&AuthConfig {
            trust_loopback: true,
            allow_unauthenticated_remote: false,
            tokens,
        })
        .unwrap()
    }

    fn token(name: &str, scopes: Vec<Scope>, value: &str) -> TokenConfig {
        TokenConfig {
            name: name.into(),
            scopes,
            value: Some(value.into()),
            value_env: None,
            value_file: None,
        }
    }

    #[test]
    fn unconfigured_still_refuses_remote_admin() {
        // Defense in depth for a config that explicitly opts into anonymous
        // remote read/borrow access: the switches still stay closed.
        let a = with_tokens(vec![]);
        assert_eq!(
            a.check(Scope::Admin, &hdrs(None), REMOTE, None),
            Err(Denial::Forbidden)
        );
        // Read and borrow are the scopes the explicit escape hatch exposes.
        assert_eq!(
            a.check(Scope::Read, &hdrs(None), REMOTE, None),
            Ok(Grant::Unauthenticated)
        );
        assert_eq!(
            a.check(Scope::Borrow, &hdrs(None), REMOTE, None),
            Ok(Grant::Unauthenticated)
        );
    }

    #[test]
    fn loopback_is_trusted_including_for_admin() {
        // The tray app and `vacancyd status` must work with no config. What
        // a loopback grant may do with the admin scope is decided afterwards
        // by `vacancyd::local_admin::permits`.
        let a = with_tokens(vec![]);
        assert_eq!(
            a.check(Scope::Admin, &hdrs(None), LOOPBACK, None),
            Ok(Grant::Loopback)
        );
    }

    #[test]
    fn a_scoped_token_on_loopback_is_reported_as_the_token() {
        // On Windows a plain loopback grant keeps read and pause only, so an
        // admin token must still be recognised when it arrives on loopback.
        let a = with_tokens(vec![
            token("op", vec![Scope::Admin], "operator-token-value"),
            token("prom", vec![Scope::Read], "read-token-bbbbbbbb"),
        ]);
        assert_eq!(
            a.check(
                Scope::Admin,
                &hdrs(Some("Bearer operator-token-value")),
                LOOPBACK,
                None
            ),
            Ok(Grant::Token("op".into()))
        );
        // A token without the scope, or an unknown one, leaves plain loopback
        // trust in place, exactly as before tokens were honoured here.
        for presented in [
            "Bearer read-token-bbbbbbbb",
            "Bearer not-a-configured-token",
        ] {
            assert_eq!(
                a.check(Scope::Admin, &hdrs(Some(presented)), LOOPBACK, None),
                Ok(Grant::Loopback),
                "{presented}"
            );
        }
    }

    fn with_header(mut headers: HeaderMap, name: &'static str, value: &str) -> HeaderMap {
        headers.insert(name, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn a_forwarded_loopback_request_is_judged_like_a_remote_one() {
        // A TLS proxy on the same machine delivers every LAN client from
        // 127.0.0.1. Its forwarding headers must stop them inheriting trust.
        let a = with_tokens(vec![token(
            "op",
            vec![Scope::Admin],
            "operator-token-value",
        )]);
        for name in [
            "forwarded",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "x-real-ip",
            "via",
        ] {
            let proxied = with_header(hdrs(None), name, "203.0.113.9");
            assert_eq!(
                a.check(Scope::Admin, &proxied, LOOPBACK, Some(9811)),
                Err(Denial::Missing),
                "{name}"
            );
            let proxied = with_header(hdrs(Some("Bearer operator-token-value")), name, "x");
            assert_eq!(
                a.check(Scope::Admin, &proxied, LOOPBACK, Some(9811)),
                Ok(Grant::Token("op".into())),
                "{name}: a token still works through the proxy"
            );
        }
    }

    #[test]
    fn a_forwarded_loopback_request_is_refused_when_no_token_exists() {
        // With no tokens nothing could authorise it, and anonymous access is
        // exactly what the missing loopback trust withholds.
        let a = with_tokens(vec![]);
        let proxied = with_header(hdrs(None), "x-forwarded-for", "203.0.113.9");
        for scope in [Scope::Read, Scope::Borrow, Scope::Admin] {
            assert_eq!(
                a.check(scope, &proxied, LOOPBACK, Some(9811)),
                Err(Denial::NotDirect),
                "{scope:?}"
            );
        }
        assert_eq!(
            Denial::NotDirect.status(),
            axum::http::StatusCode::FORBIDDEN
        );
        // The explicit escape hatch still means what it says, but never for admin.
        let open = Authenticator::new(&AuthConfig {
            trust_loopback: true,
            allow_unauthenticated_remote: true,
            tokens: vec![],
        })
        .unwrap();
        assert_eq!(
            open.check(Scope::Read, &proxied, LOOPBACK, Some(9811)),
            Ok(Grant::Unauthenticated)
        );
        assert_eq!(
            open.check(Scope::Admin, &proxied, LOOPBACK, Some(9811)),
            Err(Denial::Forbidden)
        );
    }

    #[test]
    fn a_rebinding_host_name_does_not_get_loopback_trust() {
        // DNS rebinding: the page's own host name resolves to 127.0.0.1 and
        // the browser sends it as Host.
        let a = with_tokens(vec![]);
        for host in [
            "attacker.example:9811",
            "attacker.example",
            "localhost.attacker.example:9811",
            "sub.localhost:9811",
            "127.0.0.1.nip.io:9811",
            "user@127.0.0.1:9811",
            "",
        ] {
            let request = with_header(hdrs(None), "host", host);
            for scope in [Scope::Read, Scope::Admin, Scope::Borrow] {
                assert_eq!(
                    a.check(scope, &request, LOOPBACK, Some(9811)),
                    Err(Denial::NotDirect),
                    "{host} {scope:?}"
                );
            }
        }
    }

    #[test]
    fn local_clients_naming_loopback_keep_their_trust() {
        // The console, `vacancyd status` and curl name the port by a
        // loopback address; a request with no Host is not from a browser.
        let a = with_tokens(vec![]);
        for host in [
            "127.0.0.1:9811",
            "localhost:9811",
            "LOCALHOST:9811",
            "[::1]:9811",
            "127.0.0.2:9811",
        ] {
            let request = with_header(hdrs(None), "host", host);
            assert_eq!(
                a.check(Scope::Admin, &request, LOOPBACK, Some(9811)),
                Ok(Grant::Loopback),
                "{host}"
            );
        }
        assert_eq!(
            a.check(Scope::Read, &hdrs(None), LOOPBACK, Some(9811)),
            Ok(Grant::Loopback)
        );
    }

    #[test]
    fn a_loopback_host_for_another_port_is_a_forwarder() {
        // `ssh -L 9000:127.0.0.1:9811` delivers a remote client whose Host
        // names the forwarding port.
        let a = with_tokens(vec![]);
        let tunnelled = with_header(hdrs(None), "host", "localhost:9000");
        assert_eq!(
            a.check(Scope::Read, &tunnelled, LOOPBACK, Some(9811)),
            Err(Denial::NotDirect)
        );
        let no_port = with_header(hdrs(None), "host", "localhost");
        assert_eq!(
            a.check(Scope::Read, &no_port, LOOPBACK, Some(9811)),
            Err(Denial::NotDirect)
        );
        assert_eq!(
            a.check(Scope::Read, &no_port, LOOPBACK, Some(80)),
            Ok(Grant::Loopback)
        );
    }

    #[test]
    fn a_cross_site_browser_request_does_not_get_loopback_trust() {
        // A form on any web page can POST to 127.0.0.1 with a loopback Host.
        let a = with_tokens(vec![]);
        let local = with_header(hdrs(None), "host", "127.0.0.1:11435");
        let refused = [
            ("origin", "https://attacker.example"),
            ("origin", "http://attacker.example:11435"),
            ("origin", "null"),
            ("origin", "http://127.0.0.1.nip.io"),
            ("origin", "http://user@127.0.0.1"),
            ("origin", "chrome-extension://abcdefghijklmnop"),
            ("origin", "tauri://attacker.example"),
            ("origin", "garbage"),
            ("sec-fetch-site", "cross-site"),
            ("sec-fetch-site", "Cross-Site"),
        ];
        for (name, value) in refused {
            let request = with_header(local.clone(), name, value);
            for scope in [Scope::Borrow, Scope::Read, Scope::Admin] {
                assert_eq!(
                    a.check(scope, &request, LOOPBACK, Some(11435)),
                    Err(Denial::NotDirect),
                    "{name}: {value} {scope:?}"
                );
            }
        }
        // With tokens configured, such a request needs one like a remote
        // client does.
        let tokened = with_tokens(vec![token("t", vec![Scope::Borrow], "0123456789abcdef")]);
        let request = with_header(local.clone(), "origin", "https://attacker.example");
        assert_eq!(
            tokened.check(Scope::Borrow, &request, LOOPBACK, Some(11435)),
            Err(Denial::Missing)
        );
    }

    #[test]
    fn local_and_desktop_app_origins_keep_loopback_trust() {
        let a = with_tokens(vec![]);
        let local = with_header(hdrs(None), "host", "127.0.0.1:11435");
        let trusted = [
            ("origin", "http://localhost:3000"),
            ("origin", "http://127.0.0.1:8080"),
            ("origin", "https://localhost"),
            ("origin", "http://[::1]:5173"),
            // The owner console's webview, in both of Tauri's URL forms.
            ("origin", "tauri://localhost"),
            ("origin", "http://tauri.localhost"),
            ("origin", "https://tauri.localhost"),
            ("sec-fetch-site", "same-origin"),
            ("sec-fetch-site", "same-site"),
            ("sec-fetch-site", "none"),
        ];
        for (name, value) in trusted {
            let request = with_header(local.clone(), name, value);
            assert_eq!(
                a.check(Scope::Borrow, &request, LOOPBACK, Some(11435)),
                Ok(Grant::Loopback),
                "{name}: {value}"
            );
        }
    }

    #[test]
    fn loopback_trust_can_be_switched_off() {
        let a = Authenticator::new(&AuthConfig {
            trust_loopback: false,
            allow_unauthenticated_remote: false,
            tokens: vec![token("t", vec![Scope::Read], "0123456789abcdef")],
        })
        .unwrap();
        assert_eq!(
            a.check(Scope::Read, &hdrs(None), LOOPBACK, None),
            Err(Denial::Missing)
        );
    }

    #[test]
    fn configuring_any_token_closes_the_door() {
        // The rule that makes this predictable: once auth exists, it is
        // enforced - read and borrow are no longer open to anonymous callers.
        let a = with_tokens(vec![token(
            "cluster",
            vec![Scope::Borrow],
            "s3cret-token-value",
        )]);
        assert_eq!(
            a.check(Scope::Read, &hdrs(None), REMOTE, None),
            Err(Denial::Missing)
        );
        assert_eq!(
            a.check(
                Scope::Borrow,
                &hdrs(Some("Bearer s3cret-token-value")),
                REMOTE,
                None
            ),
            Ok(Grant::Token("cluster".into()))
        );
    }

    #[test]
    fn scopes_are_enforced_separately() {
        let a = with_tokens(vec![
            token("cluster", vec![Scope::Borrow], "borrow-token-aaaaaa"),
            token("prom", vec![Scope::Read], "read-token-bbbbbbbb"),
        ]);
        let borrow = hdrs(Some("Bearer borrow-token-aaaaaa"));
        // A borrowing credential must not be able to read owner presence...
        assert_eq!(
            a.check(Scope::Read, &borrow, REMOTE, None),
            Err(Denial::Forbidden)
        );
        // ...nor flip the switches.
        assert_eq!(
            a.check(Scope::Admin, &borrow, REMOTE, None),
            Err(Denial::Forbidden)
        );
        assert_eq!(
            a.check(Scope::Borrow, &borrow, REMOTE, None),
            Ok(Grant::Token("cluster".into()))
        );
        let read = hdrs(Some("Bearer read-token-bbbbbbbb"));
        assert_eq!(
            a.check(Scope::Read, &read, REMOTE, None),
            Ok(Grant::Token("prom".into()))
        );
        assert_eq!(
            a.check(Scope::Borrow, &read, REMOTE, None),
            Err(Denial::Forbidden)
        );
    }

    #[test]
    fn a_wrong_token_is_invalid_not_forbidden() {
        // The distinction matters to a client: retry with a credential
        // versus stop asking.
        let a = with_tokens(vec![token("t", vec![Scope::Read], "the-real-token-value")]);
        assert_eq!(
            a.check(
                Scope::Read,
                &hdrs(Some("Bearer wrong-token-value")),
                REMOTE,
                None
            ),
            Err(Denial::Invalid)
        );
        assert_eq!(
            Denial::Invalid.status(),
            axum::http::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            Denial::Forbidden.status(),
            axum::http::StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn bearer_parsing_is_lenient_about_case_but_not_scheme() {
        let a = with_tokens(vec![token("t", vec![Scope::Read], "0123456789abcdef")]);
        for h in ["bearer 0123456789abcdef", "BEARER 0123456789abcdef"] {
            assert!(
                a.check(Scope::Read, &hdrs(Some(h)), REMOTE, None).is_ok(),
                "{h}"
            );
        }
        // A different scheme is not a bearer token.
        assert_eq!(
            a.check(
                Scope::Read,
                &hdrs(Some("Basic 0123456789abcdef")),
                REMOTE,
                None
            ),
            Err(Denial::Missing)
        );
    }

    #[test]
    fn token_sources_are_mutually_exclusive() {
        let mut t = token("t", vec![Scope::Read], "0123456789abcdef");
        t.value_env = Some("SOME_VAR".into());
        let err = Authenticator::new(&AuthConfig {
            trust_loopback: true,
            allow_unauthenticated_remote: false,
            tokens: vec![t],
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("exactly one of"), "{err}");
    }

    #[test]
    fn a_missing_env_var_fails_startup_rather_than_running_degraded() {
        let t = TokenConfig {
            name: "t".into(),
            scopes: vec![Scope::Read],
            value: None,
            value_env: Some("VACANCYD_DEFINITELY_UNSET_XYZ".into()),
            value_file: None,
        };
        assert!(Authenticator::new(&AuthConfig {
            trust_loopback: true,
            allow_unauthenticated_remote: false,
            tokens: vec![t],
        })
        .is_err());
    }

    #[test]
    fn duplicate_names_and_empty_scopes_are_rejected() {
        let dup = vec![
            token("same", vec![Scope::Read], "0123456789abcdef"),
            token("same", vec![Scope::Admin], "fedcba9876543210"),
        ];
        assert!(Authenticator::new(&AuthConfig {
            trust_loopback: true,
            allow_unauthenticated_remote: false,
            tokens: dup
        })
        .is_err());
        assert!(Authenticator::new(&AuthConfig {
            trust_loopback: true,
            allow_unauthenticated_remote: false,
            tokens: vec![token("none", vec![], "0123456789abcdef")],
        })
        .is_err());
        assert!(Authenticator::new(&AuthConfig {
            trust_loopback: true,
            allow_unauthenticated_remote: false,
            tokens: vec![token("", vec![Scope::Read], "0123456789abcdef")],
        })
        .is_err());
    }

    #[test]
    fn constant_time_eq_agrees_with_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn generated_tokens_are_long_unique_and_url_safe() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_ne!(a, b);
        // 32 bytes -> 43 unpadded base64 chars.
        assert_eq!(a.len(), 43, "{a}");
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{a}"
        );
    }

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 test vectors, in the URL-safe unpadded alphabet.
        assert_eq!(base64_url(b"f"), "Zg");
        assert_eq!(base64_url(b"fo"), "Zm8");
        assert_eq!(base64_url(b"foo"), "Zm9v");
        assert_eq!(base64_url(b"foobar"), "Zm9vYmFy");
        // Exercises both of the characters that differ from standard base64.
        assert_eq!(base64_url(&[0xfb, 0xef]), "--8");
        assert_eq!(base64_url(&[0xff, 0xef]), "_-8");
    }
}
