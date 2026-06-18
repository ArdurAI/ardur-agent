//! Browser automation tools implementing the [`Tool`] trait.
//!
//! Each tool wraps a CDP operation and is capability-gated, policy-checked,
//! human-confirmed when necessary, and receipt-chained.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use url::Url;

use ardur_runtime::CostTuple;
use ardur_tool_registry::{Capability, Tool, ToolContext, ToolId, ToolOutput, ToolSchema};

use crate::BrowserTool;
use crate::cdp::CdpBrowser;
use crate::policy::{BrowserPolicy, ConfirmationLevel};
use crate::receipt::{BrowserActionReceipt, BrowserReceipt};

/// Shared browser state for all tools.
#[derive(Clone)]
pub struct SharedBrowser {
    /// The CDP browser handle.
    pub browser: CdpBrowser,
    /// The active policy.
    pub policy: BrowserPolicy,
    /// Browser-action receipt chain for this shared browser session.
    pub receipts: BrowserActionReceipt,
}

impl SharedBrowser {
    /// Create a new shared browser state.
    #[must_use]
    pub fn new(browser: CdpBrowser, policy: BrowserPolicy) -> Self {
        Self {
            browser,
            policy,
            receipts: BrowserActionReceipt::new(),
        }
    }

    fn record_receipt(&mut self, receipt: BrowserReceipt) -> BrowserReceipt {
        self.receipts.push_and_clone(receipt)
    }
}

fn ensure_authorized(
    ctx: &ToolContext,
    cap: Capability,
) -> Result<(), ardur_tool_registry::ToolError> {
    if ctx.cap_token.0.trim().is_empty() {
        return Err(ardur_tool_registry::ToolError::CapabilityDenied(cap));
    }
    if ctx
        .env
        .get("ARDUR_CEDAR_DECISION")
        .is_some_and(|decision| decision.eq_ignore_ascii_case("deny"))
    {
        return Err(ardur_tool_registry::ToolError::Denied {
            reason: "Cedar policy denied browser automation action".to_string(),
        });
    }
    Ok(())
}

fn confirmed(args: &serde_json::Value) -> bool {
    args.get("confirmed")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn current_domain(browser: &SharedBrowser) -> Result<String, ardur_tool_registry::ToolError> {
    if browser
        .policy
        .allowlist
        .iter()
        .any(|entry| entry.domain == "*")
    {
        return Ok("*".to_string());
    }
    let Some(url) = browser.browser.current_url.as_deref() else {
        return Err(ardur_tool_registry::ToolError::Denied {
            reason: "browser action requires a current page URL for site/action policy".to_string(),
        });
    };
    let parsed = Url::parse(url).map_err(|e| ardur_tool_registry::ToolError::Denied {
        reason: format!("current browser URL `{url}` is invalid: {e}"),
    })?;
    Ok(parsed.host_str().unwrap_or_default().to_string())
}

fn receipt_payload(
    receipt: &BrowserReceipt,
    action: &str,
    target: &str,
    extra: serde_json::Value,
) -> serde_json::Value {
    json!({
        "action": action,
        "target": target,
        "permitted": true,
        "policy": { "decision": "allow" },
        "receipt": receipt.to_receipt_json(),
        "extra": extra,
    })
}

fn deny_receipt(
    browser: &mut SharedBrowser,
    action: &str,
    target: &str,
    reason: String,
) -> ardur_tool_registry::ToolError {
    let receipt = BrowserReceipt::new(action, target, false, Some(reason.clone()));
    let _stored = browser.record_receipt(receipt);
    ardur_tool_registry::ToolError::Denied { reason }
}

/// `browser.navigate` — navigate to a URL.
pub struct NavigateTool {
    browser: Arc<tokio::sync::RwLock<SharedBrowser>>,
}

impl NavigateTool {
    /// Create a new navigate tool.
    #[must_use]
    pub fn new(browser: Arc<tokio::sync::RwLock<SharedBrowser>>) -> Self {
        Self { browser }
    }
}

#[async_trait]
impl Tool for NavigateTool {
    fn id(&self) -> ToolId {
        ToolId::new("browser.navigate")
    }

    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            description: "Navigate the browser to a URL.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to navigate to" },
                    "confirmed": { "type": "boolean", "description": "Human confirmation for sensitive navigation" }
                },
                "required": ["url"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "success": { "type": "boolean" },
                    "frame_id": { "type": "string" }
                }
            }),
            examples: vec![],
        })
    }

    async fn invoke(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        ensure_authorized(ctx, Capability::NetworkOut)?;
        let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let mut guard = self.browser.write().await;

        if let Err(reason) = guard.policy.check_url_for_action(url, "navigate") {
            return Err(deny_receipt(&mut guard, "navigate", url, reason));
        }
        if let Err(reason) = guard
            .policy
            .check_confirmation("navigate", confirmed(&args))
        {
            return Err(deny_receipt(&mut guard, "navigate", url, reason));
        }
        if let Err(reason) = guard.policy.check_injection(url) {
            return Err(deny_receipt(&mut guard, "navigate", url, reason));
        }

        let result = guard
            .browser
            .navigate(url)
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;

        let frame_id = result
            .get("frameId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let receipt = guard.record_receipt(BrowserReceipt::new("navigate", url, true, None));

        Ok(ToolOutput {
            content: json!({ "success": true, "frame_id": frame_id }),
            cost: CostTuple::default(),
            receipt_data: receipt_payload(&receipt, "navigate", url, json!({"frame_id": frame_id})),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![
                Capability::NetworkOut,
                Capability::Custom(String::from("browser")),
            ]
        });
        &CAPS
    }
}

#[async_trait]
impl BrowserTool for NavigateTool {
    fn confirmation_level(&self) -> ConfirmationLevel {
        ConfirmationLevel::ExternalConsequences
    }
}

/// `browser.click` — click an element by CSS selector.
pub struct ClickTool {
    browser: Arc<tokio::sync::RwLock<SharedBrowser>>,
}

impl ClickTool {
    /// Create a new click tool.
    #[must_use]
    pub fn new(browser: Arc<tokio::sync::RwLock<SharedBrowser>>) -> Self {
        Self { browser }
    }
}

#[async_trait]
impl Tool for ClickTool {
    fn id(&self) -> ToolId {
        ToolId::new("browser.click")
    }

    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            description: "Click an element on the page by CSS selector.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector of the element to click" },
                    "confirmed": { "type": "boolean", "description": "Human confirmation for sensitive clicks" }
                },
                "required": ["selector"]
            }),
            output_schema: json!({"type": "object", "properties": {"success": {"type": "boolean"}}}),
            examples: vec![],
        })
    }

    async fn invoke(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        ensure_authorized(ctx, Capability::NetworkOut)?;
        let selector = args.get("selector").and_then(|v| v.as_str()).unwrap_or("");
        let mut guard = self.browser.write().await;
        let domain = current_domain(&guard)?;

        if let Err(reason) = guard.policy.check_action(&domain, "click") {
            return Err(deny_receipt(&mut guard, "click", selector, reason));
        }
        if let Err(reason) = guard.policy.check_confirmation("click", confirmed(&args)) {
            return Err(deny_receipt(&mut guard, "click", selector, reason));
        }
        if let Err(reason) = guard.policy.check_injection(selector) {
            return Err(deny_receipt(&mut guard, "click", selector, reason));
        }

        guard
            .browser
            .click(selector)
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;
        let receipt = guard.record_receipt(BrowserReceipt::new("click", selector, true, None));

        Ok(ToolOutput {
            content: json!({ "success": true }),
            cost: CostTuple::default(),
            receipt_data: receipt_payload(&receipt, "click", selector, json!({"domain": domain})),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![
                Capability::NetworkOut,
                Capability::Custom(String::from("browser")),
            ]
        });
        &CAPS
    }
}

#[async_trait]
impl BrowserTool for ClickTool {
    fn confirmation_level(&self) -> ConfirmationLevel {
        ConfirmationLevel::ExternalConsequences
    }
}

/// `browser.type` — type text into an input field.
pub struct TypeTool {
    browser: Arc<tokio::sync::RwLock<SharedBrowser>>,
}

impl TypeTool {
    /// Create a new type tool.
    #[must_use]
    pub fn new(browser: Arc<tokio::sync::RwLock<SharedBrowser>>) -> Self {
        Self { browser }
    }
}

#[async_trait]
impl Tool for TypeTool {
    fn id(&self) -> ToolId {
        ToolId::new("browser.type")
    }

    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            description: "Type text into an input field by CSS selector.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector of the input field" },
                    "text": { "type": "string", "description": "Text to type" },
                    "confirmed": { "type": "boolean", "description": "Human confirmation for sensitive form entry" }
                },
                "required": ["selector", "text"]
            }),
            output_schema: json!({"type": "object", "properties": {"success": {"type": "boolean"}}}),
            examples: vec![],
        })
    }

    async fn invoke(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        ensure_authorized(ctx, Capability::NetworkOut)?;
        let selector = args.get("selector").and_then(|v| v.as_str()).unwrap_or("");
        let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let mut guard = self.browser.write().await;
        let domain = current_domain(&guard)?;

        if let Err(reason) = guard.policy.check_action(&domain, "type") {
            return Err(deny_receipt(&mut guard, "type", selector, reason));
        }
        if let Err(reason) = guard.policy.check_confirmation("type", confirmed(&args)) {
            return Err(deny_receipt(&mut guard, "type", selector, reason));
        }
        if let Err(reason) = guard.policy.check_injection(selector) {
            return Err(deny_receipt(&mut guard, "type", selector, reason));
        }
        if let Err(reason) = guard.policy.check_injection(text) {
            return Err(deny_receipt(&mut guard, "type", selector, reason));
        }

        guard
            .browser
            .type_text(selector, text)
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;
        let receipt = guard.record_receipt(BrowserReceipt::new("type", selector, true, None));

        Ok(ToolOutput {
            content: json!({ "success": true }),
            cost: CostTuple::default(),
            receipt_data: receipt_payload(&receipt, "type", selector, json!({"domain": domain})),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![
                Capability::NetworkOut,
                Capability::Custom(String::from("browser")),
            ]
        });
        &CAPS
    }
}

#[async_trait]
impl BrowserTool for TypeTool {
    fn confirmation_level(&self) -> ConfirmationLevel {
        ConfirmationLevel::ExternalConsequences
    }
}

/// `browser.screenshot` — capture a PNG screenshot.
pub struct ScreenshotTool {
    browser: Arc<tokio::sync::RwLock<SharedBrowser>>,
}

impl ScreenshotTool {
    /// Create a new screenshot tool.
    #[must_use]
    pub fn new(browser: Arc<tokio::sync::RwLock<SharedBrowser>>) -> Self {
        Self { browser }
    }
}

#[async_trait]
impl Tool for ScreenshotTool {
    fn id(&self) -> ToolId {
        ToolId::new("browser.screenshot")
    }

    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            description: "Capture a PNG screenshot of the current page.".to_string(),
            input_schema: json!({"type": "object", "properties": {"format": {"type": "string", "enum": ["png", "jpeg"], "default": "png"}}}),
            output_schema: json!({"type": "object", "properties": {"data": {"type": "string"}, "format": {"type": "string"}}}),
            examples: vec![],
        })
    }

    async fn invoke(
        &self,
        ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        ensure_authorized(ctx, Capability::NetworkOut)?;
        let mut guard = self.browser.write().await;
        let data = guard
            .browser
            .screenshot()
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;

        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
        let receipt = guard.record_receipt(BrowserReceipt::new("screenshot", "page", true, None));

        Ok(ToolOutput {
            content: json!({ "data": b64, "format": "png" }),
            cost: CostTuple::default(),
            receipt_data: receipt_payload(&receipt, "screenshot", "page", json!({"format": "png"})),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![
                Capability::NetworkOut,
                Capability::Custom(String::from("browser")),
            ]
        });
        &CAPS
    }
}

#[async_trait]
impl BrowserTool for ScreenshotTool {
    fn confirmation_level(&self) -> ConfirmationLevel {
        ConfirmationLevel::None
    }
}

/// `browser.extract` — extract text or HTML from the page.
pub struct ExtractTool {
    browser: Arc<tokio::sync::RwLock<SharedBrowser>>,
}

impl ExtractTool {
    /// Create a new extract tool.
    #[must_use]
    pub fn new(browser: Arc<tokio::sync::RwLock<SharedBrowser>>) -> Self {
        Self { browser }
    }
}

#[async_trait]
impl Tool for ExtractTool {
    fn id(&self) -> ToolId {
        ToolId::new("browser.extract")
    }

    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            description: "Extract text or HTML from the current page.".to_string(),
            input_schema: json!({"type": "object", "properties": {"format": {"type": "string", "enum": ["text", "html"], "default": "text"}}}),
            output_schema: json!({"type": "object", "properties": {"content": {"type": "string"}, "format": {"type": "string"}}}),
            examples: vec![],
        })
    }

    async fn invoke(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        ensure_authorized(ctx, Capability::NetworkOut)?;
        let format = args
            .get("format")
            .and_then(|v| v.as_str())
            .unwrap_or("text");
        let mut guard = self.browser.write().await;

        let content = if format == "html" {
            guard
                .browser
                .extract_html()
                .await
                .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?
        } else {
            guard
                .browser
                .extract_text()
                .await
                .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?
        };
        let receipt = guard.record_receipt(BrowserReceipt::new("extract", format, true, None));

        Ok(ToolOutput {
            content: json!({ "content": content, "format": format }),
            cost: CostTuple::default(),
            receipt_data: receipt_payload(&receipt, "extract", format, json!({"format": format})),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![
                Capability::NetworkOut,
                Capability::Custom(String::from("browser")),
            ]
        });
        &CAPS
    }
}

#[async_trait]
impl BrowserTool for ExtractTool {
    fn confirmation_level(&self) -> ConfirmationLevel {
        ConfirmationLevel::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigate_tool_id() {
        let tool = NavigateTool::new(Arc::new(tokio::sync::RwLock::new(SharedBrowser::new(
            CdpBrowser::mock(),
            BrowserPolicy::default(),
        ))));
        assert_eq!(tool.id().as_str(), "browser.navigate");
    }

    #[test]
    fn click_tool_id() {
        let tool = ClickTool::new(Arc::new(tokio::sync::RwLock::new(SharedBrowser::new(
            CdpBrowser::mock(),
            BrowserPolicy::default(),
        ))));
        assert_eq!(tool.id().as_str(), "browser.click");
    }

    #[test]
    fn type_tool_id() {
        let tool = TypeTool::new(Arc::new(tokio::sync::RwLock::new(SharedBrowser::new(
            CdpBrowser::mock(),
            BrowserPolicy::default(),
        ))));
        assert_eq!(tool.id().as_str(), "browser.type");
    }

    #[test]
    fn screenshot_tool_id() {
        let tool = ScreenshotTool::new(Arc::new(tokio::sync::RwLock::new(SharedBrowser::new(
            CdpBrowser::mock(),
            BrowserPolicy::default(),
        ))));
        assert_eq!(tool.id().as_str(), "browser.screenshot");
    }

    #[test]
    fn extract_tool_id() {
        let tool = ExtractTool::new(Arc::new(tokio::sync::RwLock::new(SharedBrowser::new(
            CdpBrowser::mock(),
            BrowserPolicy::default(),
        ))));
        assert_eq!(tool.id().as_str(), "browser.extract");
    }
}
