#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! `ardur-web` — web platform tools for fetch, parse, screenshot, and form-fill.
//!
//! The crate keeps the network/browser surface policy-gated and receipt-chained.
//! The fused runtime performs real cap-token/Cedar verification before invoking
//! tools; these direct tool implementations also fail closed on an empty
//! `ToolContext.cap_token` and honor a test/development Cedar decision marker in
//! `ToolContext.env["ARDUR_CEDAR_DECISION"]`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ardur_browser::{BrowserPolicy, CdpBrowser};
use ardur_runtime::CostTuple;
use ardur_tool_registry::{Capability, Tool, ToolContext, ToolId, ToolOutput, ToolSchema};
use async_trait::async_trait;
use base64::Engine;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use url::{Host, Url};

static RECEIPT_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Web crate result type.
pub type Result<T> = std::result::Result<T, WebError>;

/// Errors from web platform operations.
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    /// Policy denied the operation.
    #[error("policy denied: {reason}")]
    PolicyDenied {
        /// Denial reason.
        reason: String,
    },
    /// Invalid arguments.
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    /// Execution failed.
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
}

/// Policy for web operations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebPolicy {
    /// Host allowlist. Empty means HTTPS public hosts are allowed, but HTTP is
    /// still denied except loopback when explicitly enabled.
    pub allowlist: Vec<String>,
    /// Permit HTTP loopback URLs for local development/tests.
    pub allow_loopback_http: bool,
    /// Maximum response body bytes.
    pub max_body_bytes: usize,
}

impl Default for WebPolicy {
    fn default() -> Self {
        Self {
            allowlist: Vec::new(),
            allow_loopback_http: false,
            max_body_bytes: 1024 * 1024,
        }
    }
}

impl WebPolicy {
    /// Development policy that permits HTTP loopback URLs and all HTTPS hosts.
    #[must_use]
    pub fn dev_loopback() -> Self {
        Self {
            allow_loopback_http: true,
            ..Default::default()
        }
    }

    /// Set a host allowlist.
    #[must_use]
    pub fn with_allowlist<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowlist = hosts.into_iter().map(Into::into).collect();
        self
    }

    /// Validate a URL for the named action.
    pub fn check_url(&self, raw: &str, action: &str) -> std::result::Result<Url, String> {
        let url = Url::parse(raw).map_err(|e| format!("invalid URL `{raw}`: {e}"))?;
        let host = url
            .host()
            .ok_or_else(|| format!("URL `{raw}` has no host"))?;
        let host_str = url.host_str().unwrap_or_default();
        let loopback = host_is_loopback(&host);

        match url.scheme() {
            "https" => {}
            "http" if loopback && self.allow_loopback_http => {}
            other => {
                return Err(format!(
                    "scheme `{other}` is denied for web.{action}; use HTTPS except loopback dev URLs"
                ));
            }
        }

        if !self.allowlist.is_empty()
            && !self
                .allowlist
                .iter()
                .any(|pattern| host_matches(pattern, host_str))
        {
            return Err(format!("host `{host_str}` is not on the web allowlist"));
        }

        Ok(url)
    }
}

fn host_is_loopback(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
    }
}

fn host_matches(pattern: &str, host: &str) -> bool {
    pattern == "*"
        || pattern.eq_ignore_ascii_case(host)
        || pattern
            .strip_prefix("*.")
            .is_some_and(|suffix| host.ends_with(suffix))
}

fn ensure_authorized(
    ctx: &ToolContext,
    cap: Capability,
) -> std::result::Result<(), ardur_tool_registry::ToolError> {
    if ctx.cap_token.0.trim().is_empty() {
        return Err(ardur_tool_registry::ToolError::CapabilityDenied(cap));
    }
    if ctx
        .env
        .get("ARDUR_CEDAR_DECISION")
        .is_some_and(|decision| decision.eq_ignore_ascii_case("deny"))
    {
        return Err(ardur_tool_registry::ToolError::Denied {
            reason: "Cedar policy denied web platform action".to_string(),
        });
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn receipt(action: &str, target: &str) -> Value {
    let now = now_ms();
    let seq = RECEIPT_COUNTER.fetch_add(1, Ordering::Relaxed);
    json!({
        "receipt": {
            "id": format!("wr-{now}-{seq}"),
            "action": action,
            "target": target,
            "timestamp_ms": now
        },
        "policy": { "decision": "allow" },
        "action": action,
        "target": target,
        "permitted": true
    })
}

/// `web.fetch` — fetch HTTPS pages, with loopback HTTP allowed only for dev.
pub struct WebFetchTool {
    policy: WebPolicy,
    client: reqwest::Client,
    schema: ToolSchema,
}

impl WebFetchTool {
    /// Create a fetch tool.
    #[must_use]
    pub fn new(policy: WebPolicy) -> Self {
        Self {
            policy,
            client: reqwest::Client::new(),
            schema: ToolSchema {
                description:
                    "Fetch a URL with HTTPS-only validation (HTTP loopback allowed in dev policy)."
                        .to_string(),
                input_schema: json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}),
                output_schema: json!({"type":"object","properties":{"status":{"type":"integer"},"body":{"type":"string"},"final_url":{"type":"string"}}}),
                examples: vec![],
            },
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn id(&self) -> ToolId {
        ToolId::new("web.fetch")
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        ctx: &ToolContext,
        args: Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        ensure_authorized(ctx, Capability::NetworkOut)?;
        let raw = args.get("url").and_then(Value::as_str).unwrap_or_default();
        let url = self
            .policy
            .check_url(raw, "fetch")
            .map_err(|reason| ardur_tool_registry::ToolError::Denied { reason })?;
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;
        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;
        let truncated = bytes.len() > self.policy.max_body_bytes;
        let body = String::from_utf8_lossy(&bytes[..bytes.len().min(self.policy.max_body_bytes)])
            .into_owned();
        Ok(ToolOutput {
            content: json!({"status": status, "body": body, "final_url": final_url, "truncated": truncated}),
            cost: CostTuple::default(),
            receipt_data: receipt("web.fetch", raw),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![
                Capability::NetworkOut,
                Capability::Custom("web".to_string()),
            ]
        });
        &CAPS
    }
}

/// `web.parse` — extract title, selected text, links, and forms from HTML.
pub struct HtmlParseTool {
    schema: ToolSchema,
}

impl HtmlParseTool {
    /// Create an HTML parse tool.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema: ToolSchema {
                description: "Parse HTML and extract title, selected text, links, and forms."
                    .to_string(),
                input_schema: json!({"type":"object","properties":{"html":{"type":"string"},"selector":{"type":"string"}},"required":["html"]}),
                output_schema: json!({"type":"object"}),
                examples: vec![],
            },
        }
    }
}

impl Default for HtmlParseTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for HtmlParseTool {
    fn id(&self) -> ToolId {
        ToolId::new("web.parse")
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        ctx: &ToolContext,
        args: Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        ensure_authorized(ctx, Capability::Custom("web".to_string()))?;
        let html = args.get("html").and_then(Value::as_str).unwrap_or_default();
        let selector = args
            .get("selector")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let title = first_capture(html, r"(?is)<title[^>]*>(.*?)</title>").unwrap_or_default();
        let selection = if selector.is_empty() {
            Vec::new()
        } else {
            select_text(html, selector)
        };
        let links = extract_links(html);
        let forms = extract_forms(html);
        Ok(ToolOutput {
            content: json!({"title": title, "selection": selection, "links": links, "forms": forms}),
            cost: CostTuple::default(),
            receipt_data: receipt("web.parse", selector),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> =
            std::sync::LazyLock::new(|| vec![Capability::Custom("web".to_string())]);
        &CAPS
    }
}

fn first_capture(input: &str, pattern: &str) -> Option<String> {
    Regex::new(pattern)
        .ok()?
        .captures(input)?
        .get(1)
        .map(|m| html_text(m.as_str()))
}

fn html_text(raw: &str) -> String {
    raw.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_string()
}

fn select_text(html: &str, selector: &str) -> Vec<String> {
    let pattern = if let Some(id) = selector.strip_prefix('#') {
        format!(
            r#"(?is)<([a-z0-9]+)[^>]*\bid=['\"]{}['\"][^>]*>(.*?)</\1>"#,
            regex::escape(id)
        )
    } else {
        let tag = regex::escape(selector);
        format!(r"(?is)<{tag}[^>]*>(.*?)</{tag}>")
    };
    let Ok(regex) = Regex::new(&pattern) else {
        return Vec::new();
    };
    regex
        .captures_iter(html)
        .filter_map(|capture| {
            capture
                .get(capture.len() - 1)
                .map(|m| html_text(m.as_str()))
        })
        .collect()
}

fn extract_links(html: &str) -> Vec<Value> {
    let Ok(regex) = Regex::new(r#"(?is)<a[^>]*\bhref=['\"]([^'\"]+)['\"][^>]*>(.*?)</a>"#) else {
        return Vec::new();
    };
    regex
        .captures_iter(html)
        .filter_map(|capture| {
            Some(json!({
                "href": capture.get(1)?.as_str(),
                "text": html_text(capture.get(2)?.as_str())
            }))
        })
        .collect()
}

fn extract_forms(html: &str) -> Vec<Value> {
    let Ok(form_regex) =
        Regex::new(r#"(?is)<form[^>]*\baction=['\"]([^'\"]*)['\"][^>]*>(.*?)</form>"#)
    else {
        return Vec::new();
    };
    let input_regex = Regex::new(r#"(?is)<input[^>]*\bname=['\"]([^'\"]+)['\"][^>]*>"#).ok();
    form_regex
        .captures_iter(html)
        .filter_map(|capture| {
            let body = capture.get(2)?.as_str();
            let fields: Vec<String> = input_regex
                .as_ref()
                .map(|r| {
                    r.captures_iter(body)
                        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
                        .collect()
                })
                .unwrap_or_default();
            Some(json!({"action": capture.get(1)?.as_str(), "fields": fields}))
        })
        .collect()
}

/// `web.screenshot` — capture a screenshot through a browser backend.
pub struct WebScreenshotTool {
    policy: WebPolicy,
    browser: Arc<tokio::sync::Mutex<CdpBrowser>>,
    schema: ToolSchema,
}

impl WebScreenshotTool {
    /// Create a screenshot tool with a mock browser.
    #[must_use]
    pub fn mock(policy: WebPolicy) -> Self {
        Self::new(policy, CdpBrowser::mock())
    }

    /// Create a screenshot tool with the supplied browser.
    #[must_use]
    pub fn new(policy: WebPolicy, browser: CdpBrowser) -> Self {
        Self {
            policy,
            browser: Arc::new(tokio::sync::Mutex::new(browser)),
            schema: ToolSchema {
                description: "Capture a web screenshot after policy validation.".to_string(),
                input_schema: json!({"type":"object","properties":{"url":{"type":"string"},"confirmed":{"type":"boolean"}},"required":["url"]}),
                output_schema: json!({"type":"object","properties":{"format":{"type":"string"},"data_base64":{"type":"string"}}}),
                examples: vec![],
            },
        }
    }
}

#[async_trait]
impl Tool for WebScreenshotTool {
    fn id(&self) -> ToolId {
        ToolId::new("web.screenshot")
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        ctx: &ToolContext,
        args: Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        ensure_authorized(ctx, Capability::NetworkOut)?;
        let raw = args.get("url").and_then(Value::as_str).unwrap_or_default();
        self.policy
            .check_url(raw, "screenshot")
            .map_err(|reason| ardur_tool_registry::ToolError::Denied { reason })?;
        let mut browser = self.browser.lock().await;
        browser
            .navigate(raw)
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;
        let bytes = browser
            .screenshot()
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;
        Ok(ToolOutput {
            content: json!({"format": "png", "data_base64": base64::engine::general_purpose::STANDARD.encode(bytes)}),
            cost: CostTuple::default(),
            receipt_data: receipt("web.screenshot", raw),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![
                Capability::NetworkOut,
                Capability::Custom("web".to_string()),
            ]
        });
        &CAPS
    }
}

/// `web.form_fill` — fill form fields and optionally submit after confirmation.
pub struct FormFillTool {
    policy: WebPolicy,
    _browser_policy: BrowserPolicy,
    schema: ToolSchema,
}

impl FormFillTool {
    /// Create a mock form-fill tool.
    #[must_use]
    pub fn mock(policy: WebPolicy) -> Self {
        Self {
            policy,
            _browser_policy: BrowserPolicy::permissive(),
            schema: ToolSchema {
                description: "Fill and optionally submit a web form with confirmation gates."
                    .to_string(),
                input_schema: json!({"type":"object","properties":{"url":{"type":"string"},"fields":{"type":"object"},"submit":{"type":"boolean"},"confirmed":{"type":"boolean"}},"required":["url","fields"]}),
                output_schema: json!({"type":"object","properties":{"filled":{"type":"integer"},"submitted":{"type":"boolean"}}}),
                examples: vec![],
            },
        }
    }
}

#[async_trait]
impl Tool for FormFillTool {
    fn id(&self) -> ToolId {
        ToolId::new("web.form_fill")
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        ctx: &ToolContext,
        args: Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        ensure_authorized(ctx, Capability::NetworkOut)?;
        let raw = args.get("url").and_then(Value::as_str).unwrap_or_default();
        self.policy
            .check_url(raw, "form_fill")
            .map_err(|reason| ardur_tool_registry::ToolError::Denied { reason })?;
        let fields = args
            .get("fields")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(Map::new);
        let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
        let confirmed = args
            .get("confirmed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if submit && !confirmed {
            return Err(ardur_tool_registry::ToolError::Denied {
                reason: "confirmation required before submitting a web form".to_string(),
            });
        }
        Ok(ToolOutput {
            content: json!({"filled": fields.len(), "submitted": submit}),
            cost: CostTuple::default(),
            receipt_data: receipt("web.form_fill", raw),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![
                Capability::NetworkOut,
                Capability::Custom("web".to_string()),
            ]
        });
        &CAPS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_rejects_external_http() {
        let err = WebPolicy::default()
            .check_url("http://example.com", "fetch")
            .unwrap_err();
        assert!(err.contains("HTTPS"));
    }

    #[test]
    fn parser_extracts_title() {
        assert_eq!(
            first_capture("<title>Hello</title>", r"(?is)<title[^>]*>(.*?)</title>").unwrap(),
            "Hello"
        );
    }
}
