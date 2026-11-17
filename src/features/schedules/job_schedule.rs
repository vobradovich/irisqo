use std::{fmt::Display, str::FromStr};

use crate::models::ParamError;
use chrono::{DateTime, Datelike, DurationRound, TimeDelta, Utc};
use croner::Cron;
use jiff::{Timestamp, tz::TimeZone};

/// When a job repeats: the fields of the `schedules` table (`rule`, `tz`, `until`).
#[derive(Debug, Clone, PartialEq)]
pub struct JobSchedule {
    pub rule: ScheduleRule,
    /// The zone of a cron rule: `0 9 * * *` is 9:00 there, whatever its offset is on that day.
    /// An interval ignores it.
    pub tz: TimeZone,
    /// When the schedule ends: no run after it, the run at `until` is the last one.
    pub until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScheduleRule {
    /// Interval in seconds
    Interval(u32),
    /// Cron string
    Cron(String),
}

impl JobSchedule {
    /// The zone of a stored name; UTC for one that does not parse (it was written by hand).
    pub fn zone(name: &str) -> TimeZone {
        TimeZone::get(name).unwrap_or(TimeZone::UTC)
    }

    /// The name of a zone as it is stored: the zones of schedules come from [`Self::zone`] or
    /// a request, by name, so they have one.
    pub fn zone_name(tz: &TimeZone) -> &str {
        tz.iana_name().unwrap_or("UTC")
    }

    /// A stored schedule: its `rule` text and its `tz` and `until` columns.
    pub fn from_row(
        rule: &str,
        tz: &str,
        until: Option<DateTime<Utc>>,
    ) -> Result<Self, ParamError> {
        Ok(Self {
            rule: rule.parse()?,
            tz: Self::zone(tz),
            until,
        })
    }

    /// The field that makes a schedule with no run from `from` on: `until` when it ends it,
    /// else its rule (`cron`, `interval`); `None` when it runs.
    pub fn never_runs(&self, from: DateTime<Utc>) -> Option<&'static str> {
        self.next(from)
            .is_none()
            .then_some(match (&self.until, &self.rule) {
                (Some(_), _) => "until",
                (None, ScheduleRule::Interval(_)) => "interval",
                (None, ScheduleRule::Cron(_)) => "cron",
            })
    }

    /// `true` when `at` is a usable time: from 1970 (unix 0, as `enqueue_v1` takes it) up to
    /// 9999-12-31.
    pub fn in_range(at: DateTime<Utc>) -> bool {
        at.timestamp() >= 0 && at.year() <= 9999
    }

    /// A time as written in a request: RFC 3339 (`2026-12-31T23:59:59Z`) or unix seconds.
    /// `None` when it is neither, or not a usable time.
    pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
        match s.parse::<i64>() {
            Ok(secs) => Self::from_unix_secs(secs),
            Err(_) => DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|at| at.with_timezone(&Utc))
                .filter(|at| Self::in_range(*at)),
        }
    }

    /// A unix timestamp in seconds as a time, `None` when it is not a usable one.
    pub fn from_unix_secs(secs: i64) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(secs, 0).filter(|at| Self::in_range(*at))
    }

    /// The next run strictly after `after`, `None` when there is none
    /// before `until` or the times are out of range.
    pub fn next(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if !Self::in_range(after) {
            return None;
        }
        let next = match &self.rule {
            ScheduleRule::Interval(interval) => {
                let interval = TimeDelta::seconds(i64::from(*interval));
                if interval.is_zero() {
                    return None;
                }
                // the grid of the interval starts at the unix epoch
                after
                    .duration_trunc(interval)
                    .ok()?
                    .checked_add_signed(interval)?
            }
            // The clock of the zone, as cron follows it: a fixed time that exists twice runs
            // at the first one, one that does not exist runs when the clock jumps over it;
            // a frequent rule runs at the times the clock shows.
            ScheduleRule::Cron(cron) => {
                let after =
                    Timestamp::new(after.timestamp(), after.timestamp_subsec_nanos() as i32)
                        .ok()?
                        .to_zoned(self.tz.clone());
                let next = Cron::from_str(cron)
                    .ok()?
                    .find_next_occurrence(&after, false)
                    .ok()?
                    .timestamp();
                DateTime::from_timestamp(next.as_second(), next.subsec_nanosecond() as u32)?
            }
        };
        (Self::in_range(next) && self.until.is_none_or(|u| next <= u)).then_some(next)
    }
}

impl ScheduleRule {
    /// A cron expression: an interval written as a number is not one. Errors name `cron`.
    pub fn parse_cron(s: &str) -> Result<Self, ParamError> {
        match s
            .parse::<Self>()
            .map_err(|err| ParamError::new("cron", err.detail))?
        {
            cron @ ScheduleRule::Cron(_) => Ok(cron),
            ScheduleRule::Interval(_) => Err(ParamError::new("cron", "is not a cron expression")),
        }
    }
}

impl FromStr for ScheduleRule {
    type Err = ParamError;

    /// Seconds (`300`) or a cron expression of 5 fields, or 6 with seconds first
    /// (fields are separated by spaces or `|`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(ParamError::new("schedule", "is empty"));
        }
        if let Ok(interval) = s.parse::<u32>() {
            return match interval {
                0 => Err(ParamError::new(
                    "interval",
                    "must be a positive number of seconds",
                )),
                interval => Ok(ScheduleRule::Interval(interval)),
            };
        }
        let parts: Vec<&str> = s.split(['|', ' ']).filter(|p| !p.is_empty()).collect();
        if !matches!(parts.len(), 5 | 6) {
            return Err(ParamError::new(
                "cron",
                format!(
                    "needs 5 or 6 fields, got {}: e.g. */15 * * * *",
                    parts.len()
                ),
            ));
        }
        let cron = parts.join(" ");
        Cron::from_str(&cron)
            .map_err(|_| ParamError::new("cron", "is not a valid cron expression"))?;
        Ok(ScheduleRule::Cron(cron))
    }
}

impl Display for ScheduleRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScheduleRule::Interval(interval) => write!(f, "{interval}"),
            ScheduleRule::Cron(cron) => write!(f, "{cron}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("in range")
    }

    fn utc(rule: ScheduleRule) -> JobSchedule {
        JobSchedule {
            rule,
            tz: TimeZone::UTC,
            until: None,
        }
    }

    fn zone(name: &str) -> TimeZone {
        TimeZone::get(name).expect("a known zone")
    }

    /// The first `n` runs after `from` of `rule` in `tz`.
    fn runs(rule: &str, tz: &str, from: DateTime<Utc>, n: usize) -> Vec<DateTime<Utc>> {
        let schedule = JobSchedule::from_row(rule, tz, None).expect("a rule");
        std::iter::successors(Some(from), |t| schedule.next(*t))
            .skip(1)
            .take(n)
            .collect()
    }

    fn utc_time(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    #[test]
    fn a_rule_is_not_empty() {
        assert!("".parse::<ScheduleRule>().is_err());
    }

    #[test]
    fn a_rule_from_seconds() {
        let rule: ScheduleRule = "300".parse().unwrap();
        assert_eq!(rule, ScheduleRule::Interval(300));
        assert_eq!(rule.to_string(), "300");
        assert!("0".parse::<ScheduleRule>().is_err());
    }

    #[test]
    fn a_rule_from_cron() {
        let rule: ScheduleRule = "*/5 * * * *".parse().unwrap();
        assert_eq!(rule, ScheduleRule::Cron("*/5 * * * *".into()));
        assert_eq!(rule.to_string(), "*/5 * * * *");
        // fields separated by `|` (a query string) are stored separated by spaces
        assert_eq!(
            "0|0|9|*|*|*".parse::<ScheduleRule>().unwrap().to_string(),
            "0 0 9 * * *"
        );
    }

    #[test]
    fn a_cron_rule_has_5_or_6_fields() {
        for rule in ["* * * *", "0 0 9 * * * 2030"] {
            let err = rule.parse::<ScheduleRule>().expect_err(rule);
            assert_eq!(err.parameter, "cron", "{rule}");
            assert!(err.detail.starts_with("needs 5 or 6 fields"), "{rule}");
        }
        assert!("61 * * * *".parse::<ScheduleRule>().is_err());
    }

    #[test]
    fn a_number_is_not_a_cron_expression() {
        assert!(ScheduleRule::parse_cron("300").is_err());
        assert!(ScheduleRule::parse_cron("*/5 * * * *").is_ok());
    }

    #[test]
    fn the_next_run_is_on_the_grid() {
        let now = Utc::now();
        let five_min = TimeDelta::minutes(5);
        for rule in ["300", "*/5 * * * *"] {
            let schedule = utc(rule.parse().unwrap());
            assert_eq!(
                schedule.next(now),
                Some(now.duration_trunc(five_min).unwrap() + five_min),
                "{rule}"
            );
        }
    }

    #[test]
    fn the_next_run_is_not_out_of_range() {
        let interval = utc(ScheduleRule::Interval(60));
        let cron = utc("*/5 * * * *".parse().unwrap());
        for schedule in [&interval, &cron] {
            assert_eq!(schedule.next(DateTime::<Utc>::MAX_UTC), None);
            assert_eq!(schedule.next(at(253_402_300_799)), None);
            assert_eq!(schedule.next(at(-1)), None);
        }
        // a zero interval stored before the validation existed
        assert_eq!(utc(ScheduleRule::Interval(0)).next(at(1_000)), None);
    }

    #[test]
    fn the_run_at_until_is_the_last_one() {
        let until = |secs| JobSchedule {
            until: Some(at(secs)),
            ..utc(ScheduleRule::Interval(60))
        };
        assert_eq!(until(1_020).next(at(1_000)), Some(at(1_020)));
        assert_eq!(until(1_019).next(at(1_000)), None);
    }

    #[test]
    fn cron_runs_at_the_local_time_of_its_zone() {
        let at = |y, m, d, h| Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap();
        let nine = |tz| JobSchedule {
            tz,
            ..utc(ScheduleRule::Cron("0 9 * * *".into()))
        };
        assert_eq!(
            nine(TimeZone::UTC).next(at(2025, 1, 1, 0)),
            Some(at(2025, 1, 1, 9))
        );
        // Moscow is UTC+3 all year
        assert_eq!(
            nine(zone("Europe/Moscow")).next(at(2025, 1, 1, 0)),
            Some(at(2025, 1, 1, 6))
        );
        // New York: 9:00 is 14:00 UTC in winter and 13:00 UTC in summer
        let new_york = nine(zone("America/New_York"));
        assert_eq!(new_york.next(at(2025, 1, 1, 0)), Some(at(2025, 1, 1, 14)));
        assert_eq!(new_york.next(at(2025, 7, 1, 0)), Some(at(2025, 7, 1, 13)));
        // an interval ignores the zone
        let hourly = |tz| JobSchedule {
            tz,
            ..utc(ScheduleRule::Interval(3600))
        };
        assert_eq!(
            hourly(zone("America/New_York")).next(at(2025, 1, 1, 0)),
            hourly(TimeZone::UTC).next(at(2025, 1, 1, 0))
        );
    }

    #[test]
    fn days_of_the_week_are_0_to_7_from_sunday() {
        // 2025-01-01 is a Wednesday
        let from = utc_time(2025, 1, 1, 0, 0);
        let sunday = Some(utc_time(2025, 1, 5, 9, 0));
        let monday = Some(utc_time(2025, 1, 6, 9, 0));
        let next = |dow: &str| utc(format!("0 9 * * {dow}").parse().unwrap()).next(from);
        assert_eq!(next("0"), sunday);
        assert_eq!(next("7"), sunday);
        assert_eq!(next("SUN"), sunday);
        assert_eq!(next("1"), monday);
        assert_eq!(next("MON"), monday);
        assert_eq!(next("1-5"), Some(utc_time(2025, 1, 1, 9, 0)));
    }

    #[test]
    fn a_stored_schedule_has_its_zone_and_its_end() {
        let end = Some(at(4_102_444_800));
        let cron = JobSchedule::from_row("0 9 * * *", "Asia/Tokyo", end).expect("cron");
        assert_eq!(cron.tz, zone("Asia/Tokyo"));
        assert_eq!(cron.until, end);
        assert_eq!(
            cron.rule.to_string(),
            "0 9 * * *",
            "the zone and the end are not part of the text"
        );
        assert_eq!(
            JobSchedule::from_row("300", "UTC", None).expect("interval"),
            utc(ScheduleRule::Interval(300))
        );
        // a zone written by hand that does not exist: UTC
        assert_eq!(
            JobSchedule::from_row("0 9 * * *", "Mars/Base", None)
                .expect("cron")
                .tz,
            TimeZone::UTC
        );
    }

    #[test]
    fn cron_next_stays_in_the_future_when_clocks_go_back() {
        // 02:15 in Berlin's second occurrence of the repeated hour.
        let after = utc_time(2025, 10, 26, 1, 15);
        for (rule, expected) in [
            ("30 2 * * *", "2025-10-27T01:30:00Z"),
            // a frequent rule has no gap: it runs in the repeated hour again
            ("* * * * *", "2025-10-26T01:16:00Z"),
            ("0 * * * *", "2025-10-26T02:00:00Z"),
        ] {
            assert_eq!(
                runs(rule, "Europe/Berlin", after, 1),
                [JobSchedule::parse_time(expected).unwrap()],
                "{rule}"
            );
        }
    }

    #[test]
    fn a_fixed_time_runs_once_when_clocks_change() {
        // 2025-03-30: 2:00 -> 3:00, there is no 2:30, the run of that day is at 3:00
        assert_eq!(
            runs(
                "30 2 * * *",
                "Europe/Berlin",
                utc_time(2025, 3, 29, 12, 0),
                2
            ),
            [utc_time(2025, 3, 30, 1, 0), utc_time(2025, 3, 31, 0, 30)]
        );
        // 2025-10-26: 3:00 -> 2:00, 2:30 is there twice, the run is the first one
        assert_eq!(
            runs(
                "30 2 * * *",
                "Europe/Berlin",
                utc_time(2025, 10, 25, 12, 0),
                2
            ),
            [utc_time(2025, 10, 26, 0, 30), utc_time(2025, 10, 27, 1, 30)]
        );
    }

    #[test]
    fn a_frequent_rule_follows_the_clock_when_it_changes() {
        // 2025-03-30: the hour from 2:00 is not there, nor are its runs
        assert_eq!(
            runs(
                "*/30 2 * * *",
                "Europe/Berlin",
                utc_time(2025, 3, 29, 12, 0),
                2
            ),
            [utc_time(2025, 3, 31, 0, 0), utc_time(2025, 3, 31, 0, 30)]
        );
        // 2025-10-26: the hour from 2:00 is there twice, and so are its runs
        assert_eq!(
            runs(
                "*/30 2 * * *",
                "Europe/Berlin",
                utc_time(2025, 10, 25, 12, 0),
                4
            ),
            [
                utc_time(2025, 10, 26, 0, 0),
                utc_time(2025, 10, 26, 0, 30),
                utc_time(2025, 10, 26, 1, 0),
                utc_time(2025, 10, 26, 1, 30)
            ]
        );
    }
}
