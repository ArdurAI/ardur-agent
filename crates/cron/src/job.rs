use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::{CronError, CronExpr, next_execution};

pub type JobId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum JobStatus {
    Pending,
    Scheduled,
    Running,
    Completed,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CronExpression {
    pub minute: String,       // 0-59 or *
    pub hour: String,         // 0-23 or *
    pub day_of_month: String, // 1-31 or *
    pub month: String,        // 1-12 or *
    pub day_of_week: String,  // 0-7 or *
}

impl CronExpression {
    pub fn new(
        minute: &str,
        hour: &str,
        day_of_month: &str,
        month: &str,
        day_of_week: &str,
    ) -> Self {
        Self {
            minute: minute.to_string(),
            hour: hour.to_string(),
            day_of_month: day_of_month.to_string(),
            month: month.to_string(),
            day_of_week: day_of_week.to_string(),
        }
    }

    pub fn every_minute() -> Self {
        Self::new("*", "*", "*", "*", "*")
    }

    pub fn hourly() -> Self {
        Self::new("0", "*", "*", "*", "*")
    }

    pub fn daily() -> Self {
        Self::new("0", "0", "*", "*", "*")
    }

    /// Parse and validate the five fields into a [`CronExpr`].
    ///
    /// This is the single source of truth for what a field may contain —
    /// wildcards, ranges, lists, **steps** (`*/15`, `1-10/2`), month/weekday
    /// **names** (`JAN`, `MON`), and any combination — and it **rejects**
    /// out-of-range values, zero steps, and otherwise-unparseable fields rather
    /// than silently accepting an expression that can never fire.
    ///
    /// # Errors
    ///
    /// Returns [`CronError::Parse`] if any field is malformed.
    pub fn compile(&self) -> Result<CronExpr, CronError> {
        CronExpr::parse(&format!(
            "{} {} {} {} {}",
            self.minute, self.hour, self.day_of_month, self.month, self.day_of_week
        ))
    }

    /// Whether this expression is due at `now`.
    ///
    /// Delegates to the full parser ([`compile`](Self::compile)) so step, list,
    /// combined-range, and named fields all match, and the standard cron
    /// day-of-month/day-of-week OR rule applies. **Fail-closed:** an unparseable
    /// expression is never due (surface the reason via [`compile`](Self::compile)
    /// or [`validate`](Self::validate) at construction time instead of matching
    /// nothing silently).
    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        self.compile()
            .map(|expr| expr.matches(&now))
            .unwrap_or(false)
    }

    /// The next time strictly after `from` at which this expression is due.
    ///
    /// # Errors
    ///
    /// Returns [`CronError::Parse`] if the expression is malformed, or
    /// [`CronError::NoNextExecution`] if no match exists within a one-year
    /// search horizon (e.g. an impossible date combination).
    pub fn next_after(&self, from: DateTime<Utc>) -> Result<DateTime<Utc>, CronError> {
        next_execution(&self.compile()?, from)
    }

    /// Validate that every field parses, without evaluating a time.
    ///
    /// # Errors
    ///
    /// Returns [`CronError::Parse`] if any field is malformed.
    pub fn validate(&self) -> Result<(), CronError> {
        self.compile().map(|_| ())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: JobId,
    pub name: String,
    pub expression: CronExpression,
    pub command: String,
    pub status: JobStatus,
    pub created_at: DateTime<Utc>,
    pub last_run: Option<DateTime<Utc>>,
    pub next_run: Option<DateTime<Utc>>,
    pub run_count: u64,
    pub metadata: HashMap<String, String>,
}

impl CronJob {
    pub fn new(name: &str, expression: CronExpression, command: &str) -> Self {
        let ts = uuid::Timestamp::from_unix(
            uuid::timestamp::context::NoContext,
            Utc::now().timestamp() as u64,
            0,
        );
        Self {
            id: Uuid::new_v7(ts).to_string(),
            name: name.to_string(),
            expression,
            command: command.to_string(),
            status: JobStatus::Pending,
            created_at: Utc::now(),
            last_run: None,
            next_run: None,
            run_count: 0,
            metadata: HashMap::new(),
        }
    }

    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        self.expression.is_due(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    #[test]
    fn test_cron_expression_every_minute() {
        let expr = CronExpression::every_minute();
        let now = Utc::now();
        assert!(expr.is_due(now));
    }

    #[test]
    fn test_cron_expression_specific_time() {
        let expr = CronExpression::new("30", "14", "*", "*", "*");
        let now = Utc::now().with_hour(14).unwrap().with_minute(30).unwrap();
        assert!(expr.is_due(now));
    }

    #[test]
    fn test_cron_job_creation() {
        let job = CronJob::new("test-job", CronExpression::hourly(), "echo hello");
        assert_eq!(job.name, "test-job");
        assert_eq!(job.command, "echo hello");
        assert_eq!(job.status, JobStatus::Pending);
        assert_eq!(job.run_count, 0);
    }

    #[test]
    fn test_cron_expression_range() {
        // ARD-R2: use a fixed weekday instant, not `Utc::now()`, so the assertion
        // is deterministic (the old test failed every weekend). 2024-01-03 is a
        // Wednesday at 12:00 — inside 9-17 on a Mon-Fri schedule.
        let expr = CronExpression::new("0", "9-17", "*", "*", "1-5");
        let wednesday_noon = Utc.with_ymd_and_hms(2024, 1, 3, 12, 0, 0).unwrap();
        assert!(expr.is_due(wednesday_noon));
        // The same schedule is not due on a Saturday.
        let saturday_noon = Utc.with_ymd_and_hms(2024, 1, 6, 12, 0, 0).unwrap();
        assert!(!expr.is_due(saturday_noon));
    }

    #[test]
    fn step_expression_fires_at_its_multiples() {
        // ARD-H8: `*/15` used to silently never fire under the old matcher.
        let expr = CronExpression::new("*/15", "*", "*", "*", "*");
        let at_15 = Utc.with_ymd_and_hms(2024, 1, 3, 12, 15, 0).unwrap();
        let at_7 = Utc.with_ymd_and_hms(2024, 1, 3, 12, 7, 0).unwrap();
        assert!(expr.is_due(at_15), "*/15 must fire at minute 15");
        assert!(!expr.is_due(at_7), "*/15 must not fire at minute 7");
    }

    #[test]
    fn combined_range_and_list_fires() {
        // ARD-M0b: `1-5,10` — the old matcher's `-` branch dropped the list half.
        let expr = CronExpression::new("0", "0", "1-5,10", "*", "*");
        for day in [1, 5, 10] {
            let dt = Utc.with_ymd_and_hms(2024, 1, day, 0, 0, 0).unwrap();
            assert!(expr.is_due(dt), "day {day} in 1-5,10 must fire");
        }
        let day7 = Utc.with_ymd_and_hms(2024, 1, 7, 0, 0, 0).unwrap();
        assert!(!expr.is_due(day7), "day 7 is outside 1-5,10");
    }

    #[test]
    fn day_of_month_or_day_of_week_semantics() {
        // ARD-M0b: `0 0 13 * 5` fires on the 13th OR any Friday (standard cron),
        // not only on a Friday-the-13th.
        let expr = CronExpression::new("0", "0", "13", "*", "5");
        // 2024-09-13 is a Friday (both match).
        assert!(expr.is_due(Utc.with_ymd_and_hms(2024, 9, 13, 0, 0, 0).unwrap()));
        // 2024-01-13 is a Saturday — day-of-month matches, still fires.
        assert!(expr.is_due(Utc.with_ymd_and_hms(2024, 1, 13, 0, 0, 0).unwrap()));
        // 2024-01-05 is a Friday — day-of-week matches, still fires.
        assert!(expr.is_due(Utc.with_ymd_and_hms(2024, 1, 5, 0, 0, 0).unwrap()));
        // 2024-01-06 is a Saturday, not the 13th — neither matches.
        assert!(!expr.is_due(Utc.with_ymd_and_hms(2024, 1, 6, 0, 0, 0).unwrap()));
    }

    #[test]
    fn named_fields_parse_and_fire() {
        let expr = CronExpression::new("0", "0", "*", "JAN", "MON");
        // 2024-01-01 is a Monday in January.
        assert!(expr.is_due(Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()));
        // February is outside JAN.
        assert!(!expr.is_due(Utc.with_ymd_and_hms(2024, 2, 5, 0, 0, 0).unwrap()));
    }

    #[test]
    fn invalid_expression_fails_closed_and_validates() {
        // Out-of-range and garbage fields are rejected by `validate`/`compile`
        // and, fail-closed, are never due.
        let out_of_range = CronExpression::new("99", "*", "*", "*", "*");
        assert!(out_of_range.validate().is_err());
        assert!(!out_of_range.is_due(Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()));

        let garbage = CronExpression::new("abc", "*", "*", "*", "*");
        assert!(garbage.validate().is_err());

        let zero_step = CronExpression::new("*/0", "*", "*", "*", "*");
        assert!(zero_step.validate().is_err());
    }

    #[test]
    fn next_after_advances_to_the_next_step_boundary() {
        let expr = CronExpression::new("*/15", "*", "*", "*", "*");
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 12, 5, 0).unwrap();
        let next = expr.next_after(from).expect("has a next fire time");
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 1, 12, 15, 0).unwrap());
    }
}
