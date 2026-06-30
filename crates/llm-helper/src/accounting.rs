//! Token accounting for LLM tasks.

use serde::{Deserialize, Serialize};
use chrono::Utc;
use std::sync::Arc;

/// A single usage record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub task_id: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_cents: u64,
    pub provider: String,
    pub model: String,
    pub timestamp: String,
}

/// A task budget with tracking.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBudget {
    pub task_id: String,
    pub max_tokens: u64,
    pub max_cost_cents: u64,
    pub used_tokens: u64,
    pub used_cost_cents: u64,
}

impl TaskBudget {
    pub fn new(task_id: impl Into<String>, max_tokens: u64, max_cost_cents: u64) -> Self {
        Self {
            task_id: task_id.into(),
            max_tokens,
            max_cost_cents,
            used_tokens: 0,
            used_cost_cents: 0,
        }
    }

    pub fn record_usage(&mut self, tokens_in: u64, tokens_out: u64, cost_cents: u64) -> Result<(), String> {
        let total = tokens_in + tokens_out;
        if self.used_tokens + total > self.max_tokens {
            return Err(format!("token budget exceeded: {} + {} > {}", self.used_tokens, total, self.max_tokens));
        }
        if self.used_cost_cents + cost_cents > self.max_cost_cents {
            return Err(format!("cost budget exceeded: {} + {} > {}", self.used_cost_cents, cost_cents, self.max_cost_cents));
        }
        self.used_tokens += total;
        self.used_cost_cents += cost_cents;
        Ok(())
    }

    pub fn remaining_tokens(&self) -> u64 {
        self.max_tokens.saturating_sub(self.used_tokens)
    }

    pub fn remaining_cost_cents(&self) -> u64 {
        self.max_cost_cents.saturating_sub(self.used_cost_cents)
    }
}

/// Token accountant that tracks usage across tasks.
pub struct TokenAccountant {
    budgets: std::collections::HashMap<String, TaskBudget>,
    records: Vec<UsageRecord>,
}

impl TokenAccountant {
    pub fn new() -> Self {
        Self {
            budgets: std::collections::HashMap::new(),
            records: Vec::new(),
        }
    }

    pub fn create_budget(&mut self, task_id: impl Into<String>, max_tokens: u64, max_cost_cents: u64) {
        let id = task_id.into();
        self.budgets.insert(id.clone(), TaskBudget::new(id, max_tokens, max_cost_cents));
    }

    pub fn record(&mut self, task_id: &str, tokens_in: u64, tokens_out: u64, cost_cents: u64, provider: &str, model: &str) -> Result<(), String> {
        if let Some(budget) = self.budgets.get_mut(task_id) {
            budget.record_usage(tokens_in, tokens_out, cost_cents)?;
        }
        self.records.push(UsageRecord {
            task_id: task_id.to_string(),
            tokens_in,
            tokens_out,
            cost_cents,
            provider: provider.to_string(),
            model: model.to_string(),
            timestamp: Utc::now().to_rfc3339(),
        });
        Ok(())
    }

    pub fn get_budget(&self, task_id: &str) -> Option<&TaskBudget> {
        self.budgets.get(task_id)
    }

    pub fn total_usage(&self) -> (u64, u64, u64) {
        let mut tokens_in = 0;
        let mut tokens_out = 0;
        let mut cost = 0;
        for r in &self.records {
            tokens_in += r.tokens_in;
            tokens_out += r.tokens_out;
            cost += r.cost_cents;
        }
        (tokens_in, tokens_out, cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_budget_tracks_usage() {
        let mut budget = TaskBudget::new("task-1", 1000, 100);
        assert!(budget.record_usage(100, 50, 10).is_ok());
        assert_eq!(budget.used_tokens, 150);
        assert_eq!(budget.remaining_tokens(), 850);
    }

    #[test]
    fn task_budget_enforces_limit() {
        let mut budget = TaskBudget::new("task-1", 100, 100);
        assert!(budget.record_usage(50, 50, 10).is_ok());
        assert!(budget.record_usage(1, 0, 0).is_err());
    }

    #[test]
    fn accountant_tracks_multiple_tasks() {
        let mut acc = TokenAccountant::new();
        acc.create_budget("task-1", 1000, 100);
        acc.create_budget("task-2", 500, 50);
        acc.record("task-1", 100, 50, 10, "openai", "gpt-4").unwrap();
        acc.record("task-2", 30, 20, 5, "anthropic", "claude").unwrap();
        assert_eq!(acc.total_usage(), (130, 70, 15));
    }
}
