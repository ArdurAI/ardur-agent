//! Search egress policy: domain allowlists and blocklists.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A rule for a domain.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DomainRule {
    Allow(String),
    Block(String),
}

/// Policy governing search egress.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPolicy {
    /// Allowed domains (empty = allow all except blocked).
    pub allowlist: Vec<String>,
    /// Blocked domains.
    pub blocklist: Vec<String>,
    /// Require source citations.
    pub require_citations: bool,
}

impl SearchPolicy {
    pub fn permissive() -> Self {
        Self { allowlist: vec![], blocklist: vec![], require_citations: true }
    }

    pub fn with_allowlist(domains: Vec<String>) -> Self {
        Self { allowlist: domains, blocklist: vec![], require_citations: true }
    }

    pub fn check_domain(&self, domain: &str) -> Result<(), String> {
        for blocked in &self.blocklist {
            if domain == blocked || domain.ends_with(blocked) {
                return Err(format!("domain {domain} is blocklisted"));
            }
        }
        if !self.allowlist.is_empty() {
            let allowed = self.allowlist.iter().any(|d| domain == d || domain.ends_with(d));
            if !allowed {
                return Err(format!("domain {domain} is not in the allowlist"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocklist_blocks() {
        let policy = SearchPolicy {
            blocklist: vec!["bad.com".to_string()],
            ..Default::default()
        };
        assert!(policy.check_domain("bad.com").is_err());
        assert!(policy.check_domain("good.com").is_ok());
    }

    #[test]
    fn allowlist_enforces() {
        let policy = SearchPolicy::with_allowlist(vec!["good.com".to_string()]);
        assert!(policy.check_domain("good.com").is_ok());
        assert!(policy.check_domain("bad.com").is_err());
    }
}
