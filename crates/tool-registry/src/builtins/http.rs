//! [`HttpFetchTool`] — fetch a web resource over HTTP(S) with a side-effect-free
//! method (`GET`/`HEAD`), an explicit host allowlist, and SSRF defence.
//!
//! # ⚠️ Security posture
//!
//! A naive HTTP-fetch tool is a textbook **Server-Side Request Forgery (SSRF)**
//! primitive: a prompt that controls the URL can reach the host's loopback
//! services, RFC 1918 LAN, or a cloud metadata endpoint (`169.254.169.254`).
//! This tool is **strict by default** to make that hard:
//!
//! - Only `GET` and `HEAD` are permitted — the tool never mutates remote state.
//! - With no allowlist and no private-IP opt-in it fetches **only localhost**,
//!   so a freshly-registered tool cannot be steered onto the public internet or
//!   the LAN. Configure an allowlist to widen it.
//! - Every URL — including every redirect hop — is re-validated: the host is
//!   checked against the allowlist, then resolved and each resulting IP is
//!   rejected if it is loopback-but-not-localhost, RFC 1918, link-local, or
//!   unique-local, unless private-IP access was explicitly granted.
//! - The validated addresses are pinned into the request (reqwest
//!   `resolve_to_addrs`) so the connection lands on the IP we vetted, closing
//!   the DNS-rebind window between the check and the connect.
//!
//! Redirects are followed **manually** (the client's own redirect follower is
//! disabled) precisely so the allowlist + IP checks run again on each `Location`
//! — an allowlisted page that 302s to `169.254.169.254` is refused, not
//! followed. As with the [`files`](super::files) tools this is a guard, not a
//! sandbox; pair it with the §11 capability + Cedar layers when the prompt is
//! untrusted. The capability it declares ([`Capability::NetworkOut`]) is what
//! those layers gate against.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::Method;
use reqwest::redirect::Policy;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use url::{Host, Url};

use ardur_runtime::CostTuple;

use crate::capability::Capability;
use crate::error::ToolError;
use crate::tool::{Tool, ToolContext, ToolId, ToolOutput, ToolSchema};

/// Default body ceiling: 1 MiB.
const DEFAULT_MAX_BYTES: usize = 1024 * 1024;
/// Default per-request wall-clock ceiling, in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Default cap on the number of redirects followed before erroring.
const DEFAULT_REDIRECT_LIMIT: usize = 5;

/// A host-allowlist entry pattern.
///
/// - `*` — every host (DEV ONLY; see [`HttpFetchTool::with_allowlist`]).
/// - `*.example.com` — any subdomain of `example.com` (but not the apex).
/// - `example.com` / `127.0.0.1` — an exact host match (case-insensitive).
fn pattern_matches(pattern: &str, host: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // `*.example.com` matches `a.example.com`, not the bare `example.com`.
        return host
            .strip_suffix(suffix)
            .is_some_and(|head| head.ends_with('.'));
    }
    host.eq_ignore_ascii_case(pattern)
}

/// Whether `host` denotes localhost — the bare name or any loopback IP literal.
fn host_is_localhost(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
    }
}

/// Whether `ip` is an internal/non-routable address an untrusted fetch must not
/// be allowed to reach (the SSRF blocklist).
///
/// Covers loopback, RFC 1918 private, link-local, unspecified, this-host
/// (`0.0.0.0/8`), CGNAT (`100.64.0.0/10`), protocol-assignment / documentation
/// / benchmarking ranges, multicast, reserved, and broadcast IPv4; and
/// loopback, unspecified, link-local (`fe80::/10`), unique-local (`fc00::/7`),
/// NAT64 (`64:ff9b::/96`), 6to4 (`2002::/16`), and IPv4-compatible IPv6
/// (`::a.b.c.d`) — mapping IPv4-in-IPv6 down to its v4 form first so an
/// `::ffff:10.0.0.1`, `64:ff9b::10.0.0.1`, `2002:0a00:0001::`, or
/// `::10.0.0.1` cannot smuggle a private address past the check.
fn is_internal_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                // 0.0.0.0/8 "this host" range — not caught by is_unspecified()
                // (which only tests for the exact address 0.0.0.0).
                || octets[0] == 0
                // 198.18.0.0/15 benchmarking (RFC 2544).
                || (octets[0] == 198 && (octets[1] & 0xFE) == 18)
                // 100.64.0.0/10 carrier-grade NAT (RFC 6598).
                || (octets[0] == 100 && (octets[1] & 0xC0) == 64)
                // 192.0.0.0/24 IETF protocol assignments and 192.0.2.0/24
                // TEST-NET-1 documentation range.
                || (octets[0] == 192 && octets[1] == 0 && (octets[2] == 0 || octets[2] == 2))
                // TEST-NET-2 (RFC 5737).
                || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
                // TEST-NET-3 (RFC 5737).
                || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
                || v4.is_multicast()
                || v4.is_broadcast()
                // 240.0.0.0/4 reserved for future use.
                || octets[0] >= 240
        }
        IpAddr::V6(v6) => {
            // Check native IPv6 loopback first — ::1 must be classified as
            // internal before the IPv4-compatible extraction below rewrites it
            // to 0.0.0.1 (which is only caught by the 0.0.0.0/8 check, not by
            // is_loopback).
            if v6.is_loopback() {
                return true;
            }
            // Normalise IPv4-mapped (::ffff:a.b.c.d) down to its v4 address.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_internal_ip(IpAddr::V4(mapped));
            }
            let segs = v6.segments();
            let v4_from_segments = |hi: u16, lo: u16| {
                Ipv4Addr::new(
                    (hi >> 8) as u8,
                    (hi & 0xFF) as u8,
                    (lo >> 8) as u8,
                    (lo & 0xFF) as u8,
                )
            };
            // Catch the deprecated IPv4-compatible form (::a.b.c.d): the first
            // 96 bits (6 segments) are zero but segs[5] is 0x0000 (not 0xFFFF).
            // to_ipv4_mapped only handles ::ffff:0:0/96; this catches ::/96
            // which to_ipv4_mapped misses.
            if segs[0] == 0
                && segs[1] == 0
                && segs[2] == 0
                && segs[3] == 0
                && segs[4] == 0
                && segs[5] == 0
            {
                // segs[6..8] hold the embedded v4 address (or all-zero for ::).
                let v4 = v4_from_segments(segs[6], segs[7]);
                return is_internal_ip(IpAddr::V4(v4));
            }
            // NAT64 well-known prefix 64:ff9b::/96 embeds an IPv4 address in
            // the low 32 bits. Recurse through the IPv4 classifier so all
            // private/reserved ranges remain blocked through the translation.
            if segs[0] == 0x0064
                && segs[1] == 0xff9b
                && segs[2] == 0
                && segs[3] == 0
                && segs[4] == 0
                && segs[5] == 0
            {
                let v4 = v4_from_segments(segs[6], segs[7]);
                return is_internal_ip(IpAddr::V4(v4));
            }
            // 6to4 2002::/16 embeds the IPv4 address in bits 16..48 (segments
            // 1 and 2). If that embedded address is internal, the 6to4 address
            // is also unsafe for an untrusted fetch.
            if segs[0] == 0x2002 {
                let v4 = v4_from_segments(segs[1], segs[2]);
                return is_internal_ip(IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || (segs[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
                || (segs[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
        }
    }
}

/// Arguments to an `http.fetch` invocation.
#[derive(Deserialize)]
struct FetchArgs {
    /// Absolute `http`/`https` URL to fetch.
    url: String,
    /// `GET` (default) or `HEAD`; nothing with side effects.
    #[serde(default)]
    method: Option<String>,
    /// Extra request headers.
    #[serde(default)]
    headers: Map<String, Value>,
    /// Body ceiling in bytes; the response is truncated past it.
    #[serde(default)]
    max_bytes: Option<usize>,
    /// Per-request wall-clock ceiling in seconds.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// A tool that fetches a URL and returns `{ status, headers, body,
/// body_truncated, bytes_read, final_url, elapsed_ms }`.
///
/// Construct with [`HttpFetchTool::new`] and narrow or widen it with the
/// chainable `with_*` builders. **Read the [module security posture](self)
/// before registering one** — by default it can reach only localhost.
pub struct HttpFetchTool {
    schema: ToolSchema,
    allowlist: Vec<String>,
    allow_private_ips: bool,
    max_bytes: usize,
    redirect_limit: usize,
    caps: Vec<Capability>,
}

impl Default for HttpFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpFetchTool {
    /// The id [`HttpFetchTool`] registers under.
    pub const ID: &'static str = "http.fetch";

    /// A strict-by-default fetch tool: no allowlist, no private-IP access, a
    /// 1 MiB body ceiling, and at most 5 redirects.
    ///
    /// In this configuration it fetches **only localhost** — widen it with
    /// [`with_allowlist`](Self::with_allowlist) or
    /// [`with_private_ip_access`](Self::with_private_ip_access).
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            description: "Fetch a web page over HTTP(S) with GET or HEAD. Returns the status, \
                          response headers, body (UTF-8 lossy, truncated at max_bytes), the final \
                          URL after redirects, and elapsed time."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Absolute http:// or https:// URL to fetch."
                    },
                    "method": {
                        "type": "string",
                        "enum": ["GET", "HEAD"],
                        "description": "Request method (default GET). Only side-effect-free \
                                        methods are permitted."
                    },
                    "headers": {
                        "type": "object",
                        "description": "Extra request headers.",
                        "additionalProperties": { "type": "string" }
                    },
                    "max_bytes": {
                        "type": "integer",
                        "description": "Body ceiling in bytes (default 1048576).",
                        "minimum": 0
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Per-request wall-clock ceiling in seconds (default 30).",
                        "minimum": 1
                    }
                },
                "required": ["url"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "status": { "type": "integer" },
                    "headers": { "type": "object" },
                    "body": { "type": "string" },
                    "body_truncated": { "type": "boolean" },
                    "bytes_read": { "type": "integer" },
                    "final_url": { "type": "string" },
                    "elapsed_ms": { "type": "integer" }
                },
                "required": [
                    "status", "headers", "body", "body_truncated",
                    "bytes_read", "final_url", "elapsed_ms"
                ]
            }),
            examples: vec![],
        };
        Self {
            schema,
            allowlist: Vec::new(),
            allow_private_ips: false,
            max_bytes: DEFAULT_MAX_BYTES,
            redirect_limit: DEFAULT_REDIRECT_LIMIT,
            caps: vec![Capability::NetworkOut],
        }
    }

    /// Confine the tool to hosts matching `hosts`.
    ///
    /// Each pattern is an exact host (`example.com`), a wildcard subdomain
    /// (`*.example.com`, which matches `a.example.com` but not the apex), or `*`
    /// for **every** host. `*` disables host filtering and is for local
    /// development only — it leaves SSRF defence resting on the private-IP check
    /// alone.
    #[must_use]
    pub fn with_allowlist(mut self, hosts: Vec<String>) -> Self {
        if hosts.iter().any(|h| h == "*") {
            tracing::warn!(
                target: "ardur_tool_registry::builtins::http",
                "http.fetch registered with a `*` allowlist — every host is reachable; \
                 this is for local development only"
            );
        }
        self.allowlist = hosts;
        self
    }

    /// Permit (`true`) or forbid (`false`, the default) URLs that resolve to
    /// private, loopback-but-not-localhost, or link-local IPs.
    ///
    /// Enable only for a deployment that genuinely needs to reach internal
    /// hosts; it removes the SSRF blocklist.
    #[must_use]
    pub fn with_private_ip_access(mut self, allow: bool) -> Self {
        self.allow_private_ips = allow;
        self
    }

    /// Override the default 1 MiB body ceiling.
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Override the default redirect cap (5).
    #[must_use]
    pub fn with_redirect_limit(mut self, limit: usize) -> Self {
        self.redirect_limit = limit;
        self
    }

    /// Gate A — the host allowlist (string match, no DNS).
    ///
    /// A configured allowlist requires a pattern match; an empty allowlist is
    /// the strict default that permits only localhost unless private-IP access
    /// was granted.
    fn check_host_allowed(&self, host: &Host<&str>, host_str: &str) -> Result<(), ToolError> {
        if !self.allowlist.is_empty() {
            if self.allowlist.iter().any(|p| pattern_matches(p, host_str)) {
                return Ok(());
            }
            return Err(ToolError::Denied {
                reason: format!("host `{host_str}` is not on the http.fetch allowlist"),
            });
        }
        if self.allow_private_ips || host_is_localhost(host) {
            return Ok(());
        }
        Err(ToolError::Denied {
            reason: format!(
                "host `{host_str}` is refused: http.fetch has no allowlist and so permits only \
                 localhost. Configure an allowlist to reach other hosts."
            ),
        })
    }

    /// Resolve `host`/`port` to socket addresses and apply Gate B — the SSRF
    /// IP blocklist — to each, returning the vetted addresses to pin the
    /// connection to.
    ///
    /// `is_localhost` marks a request whose target *is* localhost (the bare name
    /// or a loopback literal): the dev exception is honoured by skipping the IP
    /// check. A non-localhost host that merely *resolves* to a loopback or
    /// private address is still refused — that is the DNS-rebind defence.
    async fn resolve_and_vet(
        &self,
        host: &Host<&str>,
        host_str: &str,
        port: u16,
        is_localhost: bool,
    ) -> Result<Vec<SocketAddr>, ToolError> {
        let addrs: Vec<SocketAddr> = match host {
            Host::Ipv4(ip) => vec![SocketAddr::new(IpAddr::V4(*ip), port)],
            Host::Ipv6(ip) => vec![SocketAddr::new(IpAddr::V6(*ip), port)],
            Host::Domain(d) => tokio::net::lookup_host((*d, port))
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("could not resolve `{d}`: {e}")))?
                .collect(),
        };

        if addrs.is_empty() {
            return Err(ToolError::ExecutionFailed(format!(
                "host `{host_str}` resolved to no addresses"
            )));
        }

        if !self.allow_private_ips && !is_localhost {
            for addr in &addrs {
                if is_internal_ip(addr.ip()) {
                    return Err(ToolError::Denied {
                        reason: format!(
                            "host `{host_str}` resolves to a private/internal address \
                             ({}); refusing to fetch (SSRF defence)",
                            addr.ip()
                        ),
                    });
                }
            }
        }

        Ok(addrs)
    }
}

#[async_trait]
impl Tool for HttpFetchTool {
    fn id(&self) -> ToolId {
        ToolId::new(Self::ID)
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let args: FetchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let method = match args.method.as_deref().unwrap_or("GET") {
            "GET" => Method::GET,
            "HEAD" => Method::HEAD,
            other => {
                return Err(ToolError::Denied {
                    reason: format!(
                        "method `{other}` is not permitted; http.fetch allows only GET and HEAD"
                    ),
                });
            }
        };

        let max_bytes = args.max_bytes.unwrap_or(self.max_bytes);
        let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

        // `Url::parse` rejects relative URLs (`RelativeUrlWithoutBase`), so a
        // bare path never reaches the request path.
        let mut current = Url::parse(&args.url)
            .map_err(|e| ToolError::InvalidArgs(format!("invalid url `{}`: {e}", args.url)))?;

        let start = Instant::now();
        let mut redirects = 0usize;

        loop {
            let scheme = current.scheme();
            if scheme != "http" && scheme != "https" {
                return Err(ToolError::Denied {
                    reason: format!("scheme `{scheme}` is not permitted; only http and https"),
                });
            }

            let host = current.host().ok_or_else(|| ToolError::Denied {
                reason: format!("url `{current}` has no host"),
            })?;
            let host_str = current
                .host_str()
                .ok_or_else(|| {
                    ToolError::Internal(anyhow::anyhow!(
                        "url has host but host_str returned None: {current}"
                    ))
                })?
                .to_string();
            let port = current
                .port_or_known_default()
                .ok_or_else(|| ToolError::Denied {
                    reason: format!("url `{current}` has no port and no known default"),
                })?;

            // Gate A then Gate B, on every hop — including redirect targets.
            let is_localhost = host_is_localhost(&host);
            self.check_host_allowed(&host, &host_str)?;
            let addrs = self
                .resolve_and_vet(&host, &host_str, port, is_localhost)
                .await?;

            // Pin the connection to the exact addresses we vetted, and follow
            // redirects ourselves so each hop is re-checked above.
            let client = reqwest::Client::builder()
                .redirect(Policy::none())
                .timeout(timeout)
                .resolve_to_addrs(&host_str, &addrs)
                .build()
                .map_err(|e| {
                    ToolError::Internal(anyhow::anyhow!("failed to build http client: {e}"))
                })?;

            let mut request = client.request(method.clone(), current.clone());
            for (name, value) in &args.headers {
                // Denylist headers that a prompt-controlled fetch must not set —
                // prevents credential smuggling, cache poisoning, and request
                // routing attacks against internal services behind an allowlisted host.
                let lower = name.to_ascii_lowercase();
                if matches!(
                    lower.as_str(),
                    "authorization"
                        | "proxy-authorization"
                        | "cookie"
                        | "set-cookie"
                        | "host"
                        | "forwarded"
                        | "x-forwarded-for"
                        | "x-forwarded-host"
                        | "x-forwarded-proto"
                        | "x-real-ip"
                        | "via"
                        | "connection"
                        | "transfer-encoding"
                        | "content-length"
                        | "upgrade"
                ) {
                    continue;
                }
                if let Some(v) = value.as_str() {
                    request = request.header(name, v);
                }
            }

            let mut resp = request.send().await.map_err(|e| {
                if e.is_timeout() {
                    ToolError::Timeout
                } else {
                    ToolError::Internal(anyhow::anyhow!("request to `{current}` failed: {e}"))
                }
            })?;

            let status = resp.status();
            if status.is_redirection() {
                if let Some(location) = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                {
                    redirects += 1;
                    if redirects > self.redirect_limit {
                        return Err(ToolError::ExecutionFailed(format!(
                            "too many redirects (exceeded limit of {})",
                            self.redirect_limit
                        )));
                    }
                    // Resolve the `Location` against the current URL so a
                    // relative redirect target is made absolute.
                    current = current.join(location).map_err(|e| {
                        ToolError::ExecutionFailed(format!(
                            "invalid redirect target `{location}`: {e}"
                        ))
                    })?;
                    continue;
                }
            }

            // Terminal response — read the body up to the ceiling.
            let headers = {
                let mut map = Map::new();
                for (name, value) in resp.headers() {
                    map.insert(
                        name.as_str().to_string(),
                        Value::String(String::from_utf8_lossy(value.as_bytes()).into_owned()),
                    );
                }
                Value::Object(map)
            };

            let mut body = Vec::new();
            let mut truncated = false;
            loop {
                let chunk = resp.chunk().await.map_err(|e| {
                    ToolError::Internal(anyhow::anyhow!("reading body of `{current}` failed: {e}"))
                })?;
                let Some(chunk) = chunk else { break };
                let remaining = max_bytes.saturating_sub(body.len());
                if chunk.len() > remaining {
                    body.extend_from_slice(&chunk[..remaining]);
                    truncated = true;
                    break;
                }
                body.extend_from_slice(&chunk);
            }

            let content = json!({
                "status": status.as_u16(),
                "headers": headers,
                "body": String::from_utf8_lossy(&body),
                "body_truncated": truncated,
                "bytes_read": body.len(),
                "final_url": current.as_str(),
                "elapsed_ms": start.elapsed().as_millis() as u64,
            });

            return Ok(ToolOutput {
                content: content.clone(),
                cost: CostTuple::default(),
                receipt_data: content,
            });
        }
    }

    fn required_capabilities(&self) -> &[Capability] {
        &self.caps
    }
}
