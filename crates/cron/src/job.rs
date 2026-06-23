use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

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

    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        self.matches_minute(now.minute() as i32)
            && self.matches_hour(now.hour() as i32)
            && self.matches_day_of_month(now.day() as i32)
            && self.matches_month(now.month() as i32)
            && self.matches_day_of_week(now.weekday().num_days_from_sunday() as i32)
    }

    fn matches_field(&self, field: &str, value: i32, _max: i32) -> bool {
        if field == "*" {
            return true;
        }
        if let Ok(v) = field.parse::<i32>() {
            return v == value;
        }
        // Handle ranges like "0-5" or lists like "1,3,5"
        if field.contains('-') {
            let parts: Vec<&str> = field.split('-').collect();
            if parts.len() == 2 {
                if let (Ok(start), Ok(end)) = (parts[0].parse::<i32>(), parts[1].parse::<i32>()) {
                    return value >= start && value <= end;
                }
            }
        }
        if field.contains(',') {
            return field
                .split(',')
                .any(|p| p.trim().parse::<i32>().map(|v| v == value).unwrap_or(false));
        }
        false
    }

    fn matches_minute(&self, minute: i32) -> bool {
        self.matches_field(&self.minute, minute, 59)
    }

    fn matches_hour(&self, hour: i32) -> bool {
        self.matches_field(&self.hour, hour, 23)
    }

    fn matches_day_of_month(&self, day: i32) -> bool {
        self.matches_field(&self.day_of_month, day, 31)
    }

    fn matches_month(&self, month: i32) -> bool {
        self.matches_field(&self.month, month, 12)
    }

    fn matches_day_of_week(&self, day: i32) -> bool {
        self.matches_field(&self.day_of_week, day, 7)
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
        let expr = CronExpression::new("0", "9-17", "*", "*", "1-5");
        let now = Utc::now().with_hour(12).unwrap().with_minute(0).unwrap();
        assert!(expr.is_due(now));
    }
}
