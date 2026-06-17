//! CDP (Chrome DevTools Protocol) client for browser automation.
//!
//! Provides a lightweight wrapper over CDP's JSON-RPC WebSocket interface.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Configuration for connecting to a CDP-enabled browser.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdpConfig {
    /// The WebSocket endpoint URL (e.g. `ws://localhost:9222/devtools/browser/...`).
    pub ws_url: String,
    /// Default viewport width.
    pub viewport_width: u32,
    /// Default viewport height.
    pub viewport_height: u32,
    /// Whether to run headless.
    pub headless: bool,
}

impl Default for CdpConfig {
    fn default() -> Self {
        Self {
            ws_url: "ws://localhost:9222".to_string(),
            viewport_width: 1280,
            viewport_height: 720,
            headless: true,
        }
    }
}

/// A live CDP WebSocket connection.
///
/// Phase 1 is a stub that simulates responses for testing. Phase 2 will
/// implement the actual WebSocket JSON-RPC transport.
#[derive(Clone, Debug)]
pub struct CdpConnection {
    /// The endpoint this connection targets.
    pub endpoint: String,
    /// Whether this is a mock connection for testing.
    pub is_mock: bool,
}

impl CdpConnection {
    /// Create a new mock connection for testing.
    #[must_use]
    pub fn mock() -> Self {
        Self {
            endpoint: "mock".to_string(),
            is_mock: true,
        }
    }

    /// Create a connection from a config.
    #[must_use]
    pub fn from_config(config: &CdpConfig) -> Self {
        Self {
            endpoint: config.ws_url.clone(),
            is_mock: false,
        }
    }

    /// Send a CDP command and return the result.
    ///
    /// In Phase 1 (mock), returns a canned response based on the method name.
    pub async fn send(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, CdpError> {
        if self.is_mock {
            return Ok(mock_response(method, params));
        }
        // Phase 2: real WebSocket JSON-RPC call
        Err(CdpError::NotImplemented)
    }
}

/// CDP-specific errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CdpError {
    /// The WebSocket is not connected.
    NotConnected,
    /// The CDP method is not yet implemented.
    NotImplemented,
    /// A CDP protocol error (e.g. invalid parameter).
    ProtocolError(String),
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CdpError::NotConnected => write!(f, "CDP WebSocket not connected"),
            CdpError::NotImplemented => write!(f, "CDP method not implemented"),
            CdpError::ProtocolError(msg) => write!(f, "CDP protocol error: {msg}"),
        }
    }
}

impl std::error::Error for CdpError {}

/// Generate a mock CDP response for testing.
fn mock_response(method: &str, _params: Value) -> Value {
    match method {
        "Page.navigate" => serde_json::json!({
            "frameId": "mock-frame-id",
            "loaderId": "mock-loader-id"
        }),
        "Runtime.evaluate" => serde_json::json!({
            "result": {
                "type": "string",
                "value": "mock evaluation result"
            }
        }),
        "Page.captureScreenshot" => serde_json::json!({
            "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8/5+hHgAHggJ/PchI7wAAAABJRU5ErkJggg=="
        }),
        "DOM.querySelector" => serde_json::json!({
            "nodeId": 1
        }),
        "DOM.getDocument" => serde_json::json!({
            "root": {
                "nodeId": 1,
                "nodeType": 9,
                "nodeName": "#document"
            }
        }),
        _ => serde_json::json!({"mock": true}),
    }
}

/// A high-level CDP browser handle.
///
/// Manages a `CdpConnection` and provides convenience methods for common
/// browser operations.
#[derive(Clone, Debug)]
pub struct CdpBrowser {
    /// The underlying CDP connection.
    pub connection: CdpConnection,
    /// The current URL (if known).
    pub current_url: Option<String>,
}

impl CdpBrowser {
    /// Create a new CDP browser from a config.
    #[must_use]
    pub fn new(config: &CdpConfig) -> Self {
        Self {
            connection: CdpConnection::from_config(config),
            current_url: None,
        }
    }

    /// Create a mock browser for testing.
    #[must_use]
    pub fn mock() -> Self {
        Self {
            connection: CdpConnection::mock(),
            current_url: None,
        }
    }

    /// Navigate to a URL.
    pub async fn navigate(&mut self, url: &str) -> Result<Value, CdpError> {
        let result = self
            .connection
            .send("Page.navigate", serde_json::json!({"url": url}))
            .await?;
        self.current_url = Some(url.to_string());
        Ok(result)
    }

    /// Click an element by CSS selector.
    pub async fn click(&self, selector: &str) -> Result<Value, CdpError> {
        // Phase 1: mock click
        self.connection
            .send(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": format!("document.querySelector('{selector}').click()")
                }),
            )
            .await
    }

    /// Type text into an element by CSS selector.
    pub async fn type_text(&self, selector: &str, text: &str) -> Result<Value, CdpError> {
        self.connection
            .send(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": format!(
                        "document.querySelector('{selector}').value = '{text}'"
                    )
                }),
            )
            .await
    }

    /// Capture a screenshot of the current page.
    pub async fn screenshot(&self) -> Result<Vec<u8>, CdpError> {
        let result = self
            .connection
            .send("Page.captureScreenshot", serde_json::json!({"format": "png"}))
            .await?;
        let data = result
            .get("data")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        Ok(base64::decode(data).unwrap_or_default())
    }

    /// Extract the page text content.
    pub async fn extract_text(&self) -> Result<String, CdpError> {
        let result = self
            .connection
            .send(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": "document.body.innerText"
                }),
            )
            .await?;
        Ok(result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    /// Extract the page HTML.
    pub async fn extract_html(&self) -> Result<String, CdpError> {
        let result = self
            .connection
            .send(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": "document.documentElement.outerHTML"
                }),
            )
            .await?;
        Ok(result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdp_config_default() {
        let config = CdpConfig::default();
        assert_eq!(config.ws_url, "ws://localhost:9222");
        assert_eq!(config.viewport_width, 1280);
        assert_eq!(config.viewport_height, 720);
        assert!(config.headless);
    }

    #[test]
    fn cdp_connection_mock() {
        let conn = CdpConnection::mock();
        assert!(conn.is_mock);
        assert_eq!(conn.endpoint, "mock");
    }

    #[tokio::test]
    async fn mock_navigate_response() {
        let mut browser = CdpBrowser::mock();
        let result = browser.navigate("https://example.com").await.unwrap();
        assert!(result.get("frameId").is_some());
        assert_eq!(browser.current_url, Some("https://example.com".to_string()));
    }

    #[tokio::test]
    async fn mock_click_response() {
        let browser = CdpBrowser::mock();
        let result = browser.click("#submit").await.unwrap();
        assert!(result.get("result").is_some());
    }

    #[tokio::test]
    async fn mock_screenshot_response() {
        let browser = CdpBrowser::mock();
        let data = browser.screenshot().await.unwrap();
        // Should decode the base64 mock PNG
        assert!(!data.is_empty());
    }

    #[tokio::test]
    async fn mock_extract_text_response() {
        let browser = CdpBrowser::mock();
        let text = browser.extract_text().await.unwrap();
        assert_eq!(text, "mock evaluation result");
    }
}
