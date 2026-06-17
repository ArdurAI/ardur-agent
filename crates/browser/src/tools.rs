//! Browser automation tools implementing the [`Tool`] trait.
//!
//! Each tool wraps a CDP operation and is capability-gated, policy-checked,
//! and receipted.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use ardur_runtime::CostTuple;
use ardur_tool_registry::{Capability, Tool, ToolContext, ToolId, ToolOutput, ToolSchema};

use crate::cdp::CdpBrowser;
use crate::policy::{BrowserPolicy, ConfirmationLevel};
use crate::receipt::BrowserReceipt;
use crate::{BrowserContext, BrowserTool};

/// Shared browser state for all tools.
#[derive(Clone)]
pub struct SharedBrowser {
    /// The CDP browser handle.
    pub browser: CdpBrowser,
    /// The active policy.
    pub policy: BrowserPolicy,
}

impl SharedBrowser {
    /// Create a new shared browser state.
    #[must_use]
    pub fn new(browser: CdpBrowser, policy: BrowserPolicy) -> Self {
        Self { browser, policy }
    }
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
                    "url": { "type": "string", "description": "The URL to navigate to" }
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
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let mut guard = self.browser.write().await;

        // Policy check
        if let Err(reason) = guard.policy.check_url(url) {
            return Err(ardur_tool_registry::ToolError::ExecutionFailed(format!(
                "policy denied: {reason}"
            )));
        }

        // Injection check
        if let Err(reason) = guard.policy.check_injection(url) {
            return Err(ardur_tool_registry::ToolError::ExecutionFailed(format!(
                "injection blocked: {reason}"
            )));
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

        Ok(ToolOutput {
            content: json!({ "success": true, "frame_id": frame_id }),
            cost: CostTuple::default(),
            receipt_data: json!({
                "action": "navigate",
                "url": url,
                "permitted": true
            }),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![Capability::NetworkOut, Capability::Custom(String::from("browser"))]
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
                    "selector": { "type": "string", "description": "CSS selector of the element to click" }
                },
                "required": ["selector"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "success": { "type": "boolean" }
                }
            }),
            examples: vec![],
        })
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        let selector = args
            .get("selector")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let guard = self.browser.read().await;

        // Injection check on selector
        if let Err(reason) = guard.policy.check_injection(selector) {
            return Err(ardur_tool_registry::ToolError::ExecutionFailed(format!(
                "injection blocked: {reason}"
            )));
        }

        guard
            .browser
            .click(selector)
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;

        Ok(ToolOutput {
            content: json!({ "success": true }),
            cost: CostTuple::default(),
            receipt_data: json!({
                "action": "click",
                "selector": selector,
                "permitted": true
            }),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![Capability::NetworkOut, Capability::Custom(String::from("browser"))]
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
                    "text": { "type": "string", "description": "Text to type" }
                },
                "required": ["selector", "text"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "success": { "type": "boolean" }
                }
            }),
            examples: vec![],
        })
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        let selector = args
            .get("selector")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let guard = self.browser.read().await;

        // Injection check
        if let Err(reason) = guard.policy.check_injection(selector) {
            return Err(ardur_tool_registry::ToolError::ExecutionFailed(format!(
                "injection blocked: {reason}"
            )));
        }
        if let Err(reason) = guard.policy.check_injection(text) {
            return Err(ardur_tool_registry::ToolError::ExecutionFailed(format!(
                "injection blocked: {reason}"
            )));
        }

        guard
            .browser
            .type_text(selector, text)
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;

        Ok(ToolOutput {
            content: json!({ "success": true }),
            cost: CostTuple::default(),
            receipt_data: json!({
                "action": "type",
                "selector": selector,
                "permitted": true
            }),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![Capability::NetworkOut, Capability::Custom(String::from("browser"))]
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
            input_schema: json!({
                "type": "object",
                "properties": {
                    "format": { "type": "string", "enum": ["png", "jpeg"], "default": "png" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "data": { "type": "string", "description": "Base64-encoded image data" },
                    "format": { "type": "string" }
                }
            }),
            examples: vec![],
        })
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        let guard = self.browser.read().await;

        let data = guard
            .browser
            .screenshot()
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;

        let b64 = base64::encode(&data);

        Ok(ToolOutput {
            content: json!({ "data": b64, "format": "png" }),
            cost: CostTuple::default(),
            receipt_data: json!({
                "action": "screenshot",
                "permitted": true
            }),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![Capability::NetworkOut, Capability::Custom(String::from("browser"))]
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
            input_schema: json!({
                "type": "object",
                "properties": {
                    "format": { "type": "string", "enum": ["text", "html"], "default": "text" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string" },
                    "format": { "type": "string" }
                }
            }),
            examples: vec![],
        })
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        let format = args
            .get("format")
            .and_then(|v| v.as_str())
            .unwrap_or("text");

        let guard = self.browser.read().await;

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

        Ok(ToolOutput {
            content: json!({ "content": content, "format": format }),
            cost: CostTuple::default(),
            receipt_data: json!({
                "action": "extract",
                "format": format,
                "permitted": true
            }),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![Capability::NetworkOut, Capability::Custom(String::from("browser"))]
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
