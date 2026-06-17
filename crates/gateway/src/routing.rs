use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteTarget {
    pub channel_id: String,
    pub handler: String,
}

impl RouteTarget {
    pub fn new(channel_id: &str, handler: &str) -> Self {
        Self {
            channel_id: channel_id.to_string(),
            handler: handler.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    pub id: String,
    pub name: String,
    pub priority: i32,
    pub condition: String,
    pub targets: Vec<RouteTarget>,
    pub enabled: bool,
}

impl RoutingRule {
    pub fn new(name: &str, condition: &str) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            name: name.to_string(),
            priority: 0,
            condition: condition.to_string(),
            targets: Vec::new(),
            enabled: true,
        }
    }

    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    pub fn add_target(mut self, target: RouteTarget) -> Self {
        self.targets.push(target);
        self
    }

    pub fn disable(mut self) -> Self {
        self.enabled = false;
        self
    }
}

#[derive(Debug, Clone)]
pub struct Router {
    rules: std::sync::Arc<std::sync::RwLock<Vec<RoutingRule>>>,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    pub fn new() -> Self {
        Self {
            rules: std::sync::Arc::new(std::sync::RwLock::new(Vec::new())),
        }
    }

    pub fn add_rule(&self, rule: RoutingRule) -> crate::error::Result<()> {
        let mut rules = self.rules.write().map_err(|_| {
            crate::error::GatewayError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        rules.push(rule);
        rules.sort_by(|a, b| b.priority.cmp(&a.priority));
        Ok(())
    }

    pub fn route(
        &self,
        message: &crate::message::Message,
    ) -> crate::error::Result<Vec<RouteTarget>> {
        let rules = self.rules.read().map_err(|_| {
            crate::error::GatewayError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;

        for rule in rules.iter() {
            if !rule.enabled {
                continue;
            }
            // Simple condition matching: check if condition is in message content
            if message.content.contains(&rule.condition) || rule.condition == "*" {
                return Ok(rule.targets.clone());
            }
        }

        Err(crate::error::GatewayError::RoutingFailed(format!(
            "No matching route for message: {}",
            message.id
        )))
    }

    pub fn list_rules(&self) -> crate::error::Result<Vec<RoutingRule>> {
        let rules = self.rules.read().map_err(|_| {
            crate::error::GatewayError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(rules.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, MessageType};

    #[test]
    fn test_route_target_creation() {
        let target = RouteTarget::new("ch-1", "handler1");
        assert_eq!(target.channel_id, "ch-1");
        assert_eq!(target.handler, "handler1");
    }

    #[test]
    fn test_routing_rule_creation() {
        let rule = RoutingRule::new("test-rule", "hello")
            .with_priority(10)
            .add_target(RouteTarget::new("ch-1", "handler1"));
        assert_eq!(rule.name, "test-rule");
        assert_eq!(rule.priority, 10);
        assert_eq!(rule.targets.len(), 1);
    }

    #[test]
    fn test_router_add_and_route() {
        let router = Router::new();
        let rule = RoutingRule::new("greeting", "hello")
            .add_target(RouteTarget::new("ch-1", "greeting_handler"));
        router.add_rule(rule).unwrap();

        let msg = Message::new("ch-2", "user1", "hello world", MessageType::Text);
        let targets = router.route(&msg).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].channel_id, "ch-1");
    }

    #[test]
    fn test_router_no_match() {
        let router = Router::new();
        let rule =
            RoutingRule::new("specific", "xyz").add_target(RouteTarget::new("ch-1", "handler"));
        router.add_rule(rule).unwrap();

        let msg = Message::new("ch-2", "user1", "hello", MessageType::Text);
        assert!(router.route(&msg).is_err());
    }

    #[test]
    fn test_router_disabled_rule() {
        let router = Router::new();
        let rule = RoutingRule::new("disabled", "hello")
            .add_target(RouteTarget::new("ch-1", "handler"))
            .disable();
        router.add_rule(rule).unwrap();

        let msg = Message::new("ch-2", "user1", "hello", MessageType::Text);
        assert!(router.route(&msg).is_err());
    }

    #[test]
    fn test_router_priority_order() {
        let router = Router::new();
        let rule1 = RoutingRule::new("low", "test")
            .with_priority(1)
            .add_target(RouteTarget::new("ch-1", "handler1"));
        let rule2 = RoutingRule::new("high", "test")
            .with_priority(10)
            .add_target(RouteTarget::new("ch-2", "handler2"));
        router.add_rule(rule1).unwrap();
        router.add_rule(rule2).unwrap();

        let rules = router.list_rules().unwrap();
        assert_eq!(rules[0].priority, 10);
        assert_eq!(rules[1].priority, 1);
    }
}
