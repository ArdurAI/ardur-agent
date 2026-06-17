#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! ardur-browser — CDP-based browser automation tools for the Ardur agent.
//!
//! Plan family: §6.3 (`plans/6.3-browser-automation-blueprint.md`).
//!
//! # Phase 1
//!
//! - [`BrowserTool`] trait — the common interface for all browser operations.
//! - [`CdpBrowser`] — Chrome DevTools Protocol client wrapping a headless
//!   Chromium/Chrome instance.
//! - [`NavigateTool`] (`browser.navigate`) — navigate to a URL.
//! - [`ClickTool`] (`browser.click`) — click an element by selector.
//! - [`TypeTool`] (`browser.type`) — type text into an input field.
//! - [`ScreenshotTool`] (`browser.screenshot`) — capture a PNG screenshot.
//! - [`ExtractTool`] (`browser.extract`) — extract text/HTML from the page.
//! - [`BrowserPolicy`] — site/action allowlists with human confirmation.
//! - [`BrowserReceipt`] — every UI action is receipted for audit.
//!
//! All tools are capability-gated via [`Capability::NetworkOut`] and
//! [`Capability::Custom("browser")`].

mod cdp;
mod error;
mod policy;
mod receipt;
mod tools;

pub use cdp::{CdpBrowser, CdpConfig, CdpConnection};
pub use error::{BrowserError, Result};
pub use policy::{BrowserPolicy, SiteAction, ConfirmationLevel};
pub use receipt::{BrowserReceipt, BrowserActionReceipt};
pub use tools::{ClickTool, ExtractTool, NavigateTool, ScreenshotTool, SharedBrowser, TypeTool};

use ardur_tool_registry::{Capability, Tool, ToolContext, ToolId, ToolOutput, ToolSchema};
use async_trait::async_trait;

/// The shared browser context that all browser tools operate against.
///
/// Holds the CDP connection, the active policy, and the receipt chain.
#[derive(Clone, Debug)]
pub struct BrowserContext {
    /// The CDP connection to the browser.
    pub cdp: CdpConnection,
    /// The policy governing what sites and actions are permitted.
    pub policy: BrowserPolicy,
    /// The receipt chain for audit trail.
    pub receipts: Vec<BrowserReceipt>,
}

impl BrowserContext {
    /// Create a new browser context with the given CDP connection and policy.
    #[must_use]
    pub fn new(cdp: CdpConnection, policy: BrowserPolicy) -> Self {
        Self {
            cdp,
            policy,
            receipts: Vec::new(),
        }
    }

    /// Record a receipt for an action taken in this context.
    pub fn record_receipt(&mut self, receipt: BrowserReceipt) {
        self.receipts.push(receipt);
    }
}

/// Shared browser capability required by all browser tools.
pub static BROWSER_CAPABILITY: std::sync::LazyLock<Capability> = std::sync::LazyLock::new(|| {
    Capability::Custom(String::from("browser"))
});

/// The base trait for all browser automation tools.
///
/// Every browser tool requires [`Capability::NetworkOut`] and the custom
/// `browser` capability. All actions are receipted and policy-gated.
#[async_trait]
pub trait BrowserTool: Tool {
    /// The confirmation level required before this tool may run.
    fn confirmation_level(&self) -> ConfirmationLevel;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_context_new() {
        let cdp = CdpConnection::mock();
        let policy = BrowserPolicy::default();
        let ctx = BrowserContext::new(cdp, policy);
        assert!(ctx.receipts.is_empty());
    }

    #[test]
    fn browser_context_record_receipt() {
        let cdp = CdpConnection::mock();
        let policy = BrowserPolicy::default();
        let mut ctx = BrowserContext::new(cdp, policy);
        let receipt = BrowserReceipt::new(
            "navigate",
            "https://example.com",
            true,
            None,
        );
        ctx.record_receipt(receipt);
        assert_eq!(ctx.receipts.len(), 1);
    }

    #[test]
    fn browser_capability_is_custom() {
        match *BROWSER_CAPABILITY {
            Capability::Custom(ref s) => assert_eq!(s, "browser"),
            _ => panic!("BROWSER_CAPABILITY should be Custom"),
        }
    }
}
