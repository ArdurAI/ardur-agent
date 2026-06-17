//! Next-execution time computation for cron expressions.

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};

use crate::{CronError, CronExpr};

/// Compute the next execution time strictly after `from`.
///
/// # Errors
///
/// Returns `CronError::NoNextExecution` if no next time can be found
/// (e.g., impossible combination like Feb 30).
///
/// # Panics
///
/// Panics if the search exceeds a reasonable bound (should not happen).
pub fn next_execution(expr: &CronExpr, from: DateTime<Utc>) -> Result<DateTime<Utc>, CronError> {
    // Start searching from the next minute boundary
    let mut candidate = from + Duration::minutes(1);
    candidate = candidate
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();

    for _ in 0..(366 * 24 * 60 + 10) {
        // ~1 year of minutes
        if matches(expr, &candidate) {
            return Ok(candidate);
        }
        candidate = candidate + Duration::minutes(1);
    }

    Err(CronError::NoNextExecution)
}

fn matches(expr: &CronExpr, dt: &DateTime<Utc>) -> bool {
    let minute = dt.minute() as u8;
    let hour = dt.hour() as u8;
    let day = dt.day() as u8;
    let month = dt.month() as u8;
    let weekday = dt.weekday().num_days_from_sunday() as u8;

    expr.minute.contains(minute)
        && expr.hour.contains(hour)
        && expr.day.contains(day)
        && expr.month.contains(month)
        && expr.weekday.contains(weekday)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn test_next_every_minute() {
        let expr = CronExpr::parse("* * * * *").unwrap();
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let next = next_execution(&expr, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 1, 12, 1, 0).unwrap());
    }

    #[test]
    fn test_next_hourly() {
        let expr = CronExpr::parse("0 * * * *").unwrap();
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 12, 5, 0).unwrap();
        let next = next_execution(&expr, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 1, 13, 0, 0).unwrap());
    }

    #[test]
    fn test_next_daily() {
        let expr = CronExpr::parse("0 0 * * *").unwrap();
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let next = next_execution(&expr, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap());
    }

    #[test]
    fn test_next_weekday() {
        let expr = CronExpr::parse("0 9 * * 1-5").unwrap();
        let from = Utc.with_ymd_and_hms(2024, 1, 5, 10, 0, 0).unwrap(); // Friday
        let next = next_execution(&expr, from).unwrap();
        // Next Monday Jan 8
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 8, 9, 0, 0).unwrap());
    }

    #[test]
    fn test_next_specific_month() {
        let expr = CronExpr::parse("0 0 1 JAN *").unwrap();
        let from = Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap();
        let next = next_execution(&expr, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn test_next_step_minutes() {
        let expr = CronExpr::parse("*/15 * * * *").unwrap();
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 12, 5, 0).unwrap();
        let next = next_execution(&expr, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 1, 12, 15, 0).unwrap());
    }
}
