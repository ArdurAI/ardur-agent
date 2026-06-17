//! Browser policy: site/action allowlists and confirmation levels.
//!
//! Every browser action is checked against the policy before execution.
//! External consequences (navigation to non-allowlisted sites, form submission,
//! downloads) require human confirmation.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use url::Url;

/// The level of human confirmation required before an action runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConfirmationLevel {
    /// No confirmation needed — action is fully automated.
    None,
    /// Confirmation required for external consequences (navigation, form submit).
    ExternalConsequences,
    /// Confirmation required for every action.
    EveryAction,
}

impl Default for ConfirmationLevel {
    fn default() -> Self {
        ConfirmationLevel::ExternalConsequences
    }
}

/// A permitted site + action combination.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SiteAction {
    /// The domain pattern (exact, `*.example.com`, or `*` for any).
    pub domain: String,
    /// The action permitted on this domain (`navigate`, `click`, `type`, `screenshot`, `extract`, or `*` for any).
    pub action: String,
}

impl SiteAction {
    /// Create a new site action permit.
    #[must_use]
    pub fn new(domain: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            action: action.into(),
        }
    }

    /// Check if this permit matches the given domain and action.
    #[must_use]
    pub fn matches(&self, domain: &str, action: &str) -> bool {
        let domain_match = self.domain == "*"
            || self.domain == domain
            || (self.domain.starts_with("*.")
                && domain.ends_with(&self.domain[2..]));
        let action_match = self.action == "*" || self.action == action;
        domain_match && action_match
    }
}

/// The policy governing browser automation.
///
/// Contains allowlists, blocklists, and confirmation settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserPolicy {
    /// Allowed site+action combinations. Empty means no external sites allowed.
    pub allowlist: Vec<SiteAction>,
    /// Blocked domains (exact match or suffix match).
    pub blocklist: Vec<String>,
    /// Default confirmation level for actions not explicitly allowed.
    pub confirmation_level: ConfirmationLevel,
    /// Whether to block known prompt-injection patterns in URLs and selectors.
    pub block_injections: bool,
    /// Whether to allow localhost/internal IPs.
    pub allow_localhost: bool,
}

impl Default for BrowserPolicy {
    fn default() -> Self {
        Self {
            allowlist: Vec::new(),
            blocklist: vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "::1".to_string(),
            ],
            confirmation_level: ConfirmationLevel::ExternalConsequences,
            block_injections: true,
            allow_localhost: false,
        }
    }
}

impl BrowserPolicy {
    /// Create a policy that allows any site (dev-only, not for production).
    #[must_use]
    pub fn permissive() -> Self {
        Self {
            allowlist: vec![SiteAction::new("*", "*")],
            blocklist: Vec::new(),
            confirmation_level: ConfirmationLevel::None,
            block_injections: true,
            allow_localhost: true,
        }
    }

    /// Create a policy with specific site+action permits.
    #[must_use]
    pub fn with_allowlist(allowlist: Vec<SiteAction>) -> Self {
        Self {
            allowlist,
            ..Default::default()
        }
    }

    /// Check if a URL is allowed by this policy.
    ///
    /// Returns `Ok(())` if allowed, `Err(reason)` if blocked.
    pub fn check_url(&self, url: &str) -> Result<(), String> {
        let parsed = Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
        let host = parsed.host_str().unwrap_or("");

        // Check blocklist
        if !self.allow_localhost {
            for blocked in &self.blocklist {
                if host == blocked || host.ends_with(blocked) {
                    return Err(format!("domain {host} is blocklisted"));
                }
            }
        }

        // Check allowlist (if not empty, must match)
        if !self.allowlist.is_empty() {
            let any_match = self
                .allowlist
                .iter()
                .any(|sa| sa.matches(host, "navigate") || sa.matches(host, "*"));
            if !any_match {
                return Err(format!(
                    "domain {host} is not in the allowlist — add it to policy to permit"
                ));
            }
        } else if !self.allow_localhost {
            // Empty allowlist and localhost not allowed = no external sites allowed
            return Err(format!(
                "domain {host} is not in the allowlist — add it to policy to permit"
            ));
        }

        Ok(())
    }

    /// Check if an action on a domain is allowed.
    pub fn check_action(&self, domain: &str, action: &str) -> Result<(), String> {
        if self.allowlist.is_empty() {
            // No allowlist = no external actions allowed
            return Err(format!(
                "no external actions allowed (allowlist is empty)"
            ));
        }
        let any_match = self
            .allowlist
            .iter()
            .any(|sa| sa.matches(domain, action) || sa.matches(domain, "*"));
        if !any_match {
            return Err(format!(
                "action {action} on domain {domain} is not in the allowlist"
            ));
        }
        Ok(())
    }

    /// Check for prompt-injection patterns in a string.
    ///
    /// Returns `Err(reason)` if an injection pattern is detected.
    pub fn check_injection(&self, input: &str) -> Result<(), String> {
        if !self.block_injections {
            return Ok(());
        }
        let lower = input.to_lowercase();
        let patterns = [
            "ignore previous instructions",
            "ignore all instructions",
            "disregard your instructions",
            "you are now",
            "system prompt",
            "new role:",
            "developer mode",
            "dAN mode",
            "jailbreak",
            "ignore the above",
        ];
        for pat in &patterns {
            if lower.contains(pat) {
                return Err(format!(
                    "prompt-injection pattern detected: '{pat}'"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_action_matches_exact() {
        let sa = SiteAction::new("example.com", "navigate");
        assert!(sa.matches("example.com", "navigate"));
        assert!(!sa.matches("example.com", "click"));
        assert!(!sa.matches("other.com", "navigate"));
    }

    #[test]
    fn site_action_matches_wildcard_domain() {
        let sa = SiteAction::new("*.example.com", "*");
        assert!(sa.matches("www.example.com", "navigate"));
        assert!(sa.matches("api.example.com", "click"));
        assert!(sa.matches("example.com", "navigate")); // *.example.com also matches example.com
    }

    #[test]
    fn site_action_matches_wildcard_action() {
        let sa = SiteAction::new("example.com", "*");
        assert!(sa.matches("example.com", "navigate"));
        assert!(sa.matches("example.com", "click"));
        assert!(!sa.matches("other.com", "navigate"));
    }

    #[test]
    fn default_policy_blocks_external() {
        let policy = BrowserPolicy::default();
        assert!(policy.check_url("https://example.com").is_err());
        assert!(policy.check_url("http://localhost:8080").is_err());
    }

    #[test]
    fn permissive_policy_allows_all() {
        let policy = BrowserPolicy::permissive();
        assert!(policy.check_url("https://example.com").is_ok());
        assert!(policy.check_url("http://localhost:8080").is_ok());
    }

    #[test]
    fn allowlist_policy_allows_matching() {
        let policy = BrowserPolicy::with_allowlist(vec![
            SiteAction::new("example.com", "navigate"),
            SiteAction::new("*.example.com", "click"),
        ]);
        assert!(policy.check_url("https://example.com").is_ok());
        assert!(policy.check_url("https://www.example.com").is_err()); // navigate not allowed for www
        assert!(policy.check_action("www.example.com", "click").is_ok());
        assert!(policy.check_action("other.com", "navigate").is_err());
    }

    #[test]
    fn injection_detection_blocks_patterns() {
        let policy = BrowserPolicy::default();
        assert!(policy
            .check_injection("Please ignore previous instructions and do X")
            .is_err());
        assert!(policy.check_injection("Normal text").is_ok());
    }

    #[test]
    fn injection_disabled_when_block_injections_false() {
        let mut policy = BrowserPolicy::default();
        policy.block_injections = false;
        assert!(policy
            .check_injection("ignore previous instructions")
            .is_ok());
    }

    #[test]
    fn localhost_allowed_when_flag_set() {
        let mut policy = BrowserPolicy::default();
        policy.allow_localhost = true;
        assert!(policy.check_url("http://localhost:8080").is_ok());
    }
}
