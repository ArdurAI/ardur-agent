//! Cron expression parsing for standard 5-field cron.
//!
//! Fields: minute hour day month weekday
//! - minute: 0-59
//! - hour: 0-23
//! - day: 1-31
//! - month: 1-12 (or names)
//! - weekday: 0-7 (0/7 = Sunday, or names)
//!
//! Supports:
//! - `*` (wildcard)
//! - ranges: `1-5`
//! - lists: `1,3,5`
//! - steps: `*/2`, `1-10/2`
//! - names: `JAN`, `FEB`, `SUN`, `MON`, etc.

use chrono::{DateTime, Datelike, Timelike, Utc};

use crate::CronError;

/// A parsed cron expression.
#[derive(Debug, Clone, PartialEq)]
pub struct CronExpr {
    /// Minute field (0-59).
    pub minute: Field,
    /// Hour field (0-23).
    pub hour: Field,
    /// Day of month field (1-31).
    pub day: Field,
    /// Month field (1-12).
    pub month: Field,
    /// Weekday field (0-7, where 0 and 7 are Sunday).
    pub weekday: Field,
}

/// A single cron field, expanded to all matching values.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// Sorted, deduplicated matching values.
    pub values: Vec<u8>,
    /// Whether the field was a bare `*` wildcard. Distinguishes an unrestricted
    /// field from one that happens to enumerate every value (`0-6` on weekday),
    /// which the day-of-month / day-of-week OR rule in [`CronExpr::matches`]
    /// depends on.
    pub star: bool,
}

impl Field {
    /// Check if a value matches this field.
    pub fn contains(&self, value: u8) -> bool {
        self.values.binary_search(&value).is_ok()
    }
}

impl CronExpr {
    /// Whether `dt` (interpreted in UTC) matches this expression.
    ///
    /// Minute, hour, and month must all match. For day-of-month and day-of-week
    /// this follows the standard (Vixie) cron rule: when **both** are restricted
    /// (neither is a bare `*`), the expression fires when **either** matches;
    /// when at least one is `*`, both must match (the `*` one always does). So
    /// `0 0 13 * 5` fires on the 13th *or* any Friday, not only Friday-the-13th.
    pub fn matches(&self, dt: &DateTime<Utc>) -> bool {
        let minute = dt.minute() as u8;
        let hour = dt.hour() as u8;
        let day = dt.day() as u8;
        let month = dt.month() as u8;
        let weekday = dt.weekday().num_days_from_sunday() as u8;

        if !(self.minute.contains(minute)
            && self.hour.contains(hour)
            && self.month.contains(month))
        {
            return false;
        }

        let day_matches = self.day.contains(day);
        let weekday_matches = self.weekday.contains(weekday);
        if !self.day.star && !self.weekday.star {
            day_matches || weekday_matches
        } else {
            day_matches && weekday_matches
        }
    }
}

impl CronExpr {
    /// Parse a 5-field cron expression string.
    ///
    /// # Errors
    ///
    /// Returns `CronError::Parse` if the expression is malformed.
    pub fn parse(expr: &str) -> Result<Self, CronError> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(CronError::Parse(format!(
                "expected 5 fields, got {}: '{}'",
                parts.len(),
                expr
            )));
        }
        Ok(CronExpr {
            minute: parse_field(parts[0], 0, 59, false)?,
            hour: parse_field(parts[1], 0, 23, false)?,
            day: parse_field(parts[2], 1, 31, false)?,
            month: parse_field(parts[3], 1, 12, true)?,
            weekday: parse_field(parts[4], 0, 7, true)?,
        })
    }
}

fn parse_field(input: &str, min: u8, max: u8, allow_names: bool) -> Result<Field, CronError> {
    // A bare `*` is unrestricted (relevant to the day-of-month/day-of-week OR
    // rule); `*/n`, ranges, and lists are all restricted even if they enumerate
    // every value.
    let star = input.trim() == "*";
    let mut values = Vec::new();
    for part in input.split(',') {
        let (range_str, step) = if let Some(idx) = part.find('/') {
            let (left, right) = part.split_at(idx);
            let step_val = right[1..]
                .parse::<u8>()
                .map_err(|e| CronError::Parse(format!("invalid step '{}': {}", part, e)))?;
            if step_val == 0 {
                return Err(CronError::Parse(format!("zero step in '{}'", part)));
            }
            (left, step_val)
        } else {
            (part, 1)
        };

        let (start, end) = if range_str == "*" {
            (min, max)
        } else if let Some(idx) = range_str.find('-') {
            let s = parse_value(&range_str[..idx], allow_names, min, max)?;
            let e = parse_value(&range_str[idx + 1..], allow_names, min, max)?;
            (s, e)
        } else {
            let v = parse_value(range_str, allow_names, min, max)?;
            (v, v)
        };

        let mut v = start;
        while v <= end {
            values.push(v);
            // Prevent overflow when adding step
            if step == 0 || v > max {
                break;
            }
            let next = v.saturating_add(step);
            if next <= v || next > end {
                break;
            }
            v = next;
        }
    }

    values.sort_unstable();
    values.dedup();
    // Clamp weekday 7 to 0
    if allow_names && max == 7 {
        for v in &mut values {
            if *v == 7 {
                *v = 0;
            }
        }
        values.sort_unstable();
        values.dedup();
    }
    Ok(Field { values, star })
}

fn parse_value(s: &str, allow_names: bool, min: u8, max: u8) -> Result<u8, CronError> {
    let s = s.trim();
    if allow_names {
        let upper = s.to_ascii_uppercase();
        if let Some(v) = parse_month_name(&upper) {
            return Ok(v);
        }
        if let Some(v) = parse_weekday_name(&upper) {
            return Ok(v);
        }
    }
    let v: u8 = s
        .parse()
        .map_err(|e| CronError::Parse(format!("invalid number '{}': {}", s, e)))?;
    if v < min || v > max {
        return Err(CronError::Parse(format!(
            "value {} out of range [{}-{}]",
            v, min, max
        )));
    }
    Ok(v)
}

fn parse_month_name(name: &str) -> Option<u8> {
    match name {
        "JAN" => Some(1),
        "FEB" => Some(2),
        "MAR" => Some(3),
        "APR" => Some(4),
        "MAY" => Some(5),
        "JUN" => Some(6),
        "JUL" => Some(7),
        "AUG" => Some(8),
        "SEP" => Some(9),
        "OCT" => Some(10),
        "NOV" => Some(11),
        "DEC" => Some(12),
        _ => None,
    }
}

fn parse_weekday_name(name: &str) -> Option<u8> {
    match name {
        "SUN" => Some(0),
        "MON" => Some(1),
        "TUE" => Some(2),
        "WED" => Some(3),
        "THU" => Some(4),
        "FRI" => Some(5),
        "SAT" => Some(6),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wildcard() {
        let f = parse_field("*", 0, 59, false).unwrap();
        assert_eq!(f.values.len(), 60);
        assert!(f.contains(0));
        assert!(f.contains(59));
    }

    #[test]
    fn test_range() {
        let f = parse_field("1-5", 0, 59, false).unwrap();
        assert_eq!(f.values, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_list() {
        let f = parse_field("1,3,5", 0, 59, false).unwrap();
        assert_eq!(f.values, vec![1, 3, 5]);
    }

    #[test]
    fn test_step() {
        let f = parse_field("*/15", 0, 59, false).unwrap();
        assert_eq!(f.values, vec![0, 15, 30, 45]);
    }

    #[test]
    fn test_range_step() {
        let f = parse_field("1-10/2", 0, 59, false).unwrap();
        assert_eq!(f.values, vec![1, 3, 5, 7, 9]);
    }

    #[test]
    fn test_month_names() {
        let f = parse_field("JAN,MAR,DEC", 1, 12, true).unwrap();
        assert_eq!(f.values, vec![1, 3, 12]);
    }

    #[test]
    fn test_weekday_names() {
        let f = parse_field("MON,WED,FRI", 0, 7, true).unwrap();
        assert_eq!(f.values, vec![1, 3, 5]);
    }

    #[test]
    fn test_weekday_sunday_both() {
        let f = parse_field("0,7", 0, 7, true).unwrap();
        assert_eq!(f.values, vec![0]);
    }

    #[test]
    fn test_parse_expr() {
        let expr = CronExpr::parse("*/15 0 1,15 * 1-5").unwrap();
        assert_eq!(expr.minute.values, vec![0, 15, 30, 45]);
        assert_eq!(expr.hour.values, vec![0]);
        assert_eq!(expr.day.values, vec![1, 15]);
        assert_eq!(expr.month.values.len(), 12);
        assert_eq!(expr.weekday.values, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_parse_expr_with_names() {
        let expr = CronExpr::parse("0 0 1 JAN *").unwrap();
        assert_eq!(expr.month.values, vec![1]);
    }

    #[test]
    fn test_invalid_field_count() {
        let err = CronExpr::parse("* * *").unwrap_err();
        assert!(err.to_string().contains("expected 5 fields"));
    }

    #[test]
    fn test_out_of_range() {
        let err = parse_field("60", 0, 59, false).unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn test_zero_step() {
        let err = parse_field("*/0", 0, 59, false).unwrap_err();
        assert!(err.to_string().contains("zero step"));
    }
}
