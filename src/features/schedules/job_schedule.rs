use std::{fmt::Display, str::FromStr};

use crate::models::ParamError;
use chrono::{DateTime, Datelike, DurationRound, TimeDelta, Utc};
use chrono_tz::Tz;
use cron::Schedule;

#[derive(Debug, Clone, PartialEq)]
pub enum JobSchedule {
    /// Interval in seconds
    Interval { interval: u32 },
    /// Cron string, in the time zone `tz`: `0 9 * * *` is 9:00 there, whatever its offset is
    /// on that day.
    Cron { cron: String, tz: Tz },
}

impl JobSchedule {
    /// The zone of a stored name; UTC for one that does not parse (it was written by hand).
    pub fn zone(name: &str) -> Tz {
        name.parse().unwrap_or_default()
    }

    /// A cron expression: an interval written as a number is not one. Errors name `cron`.
    pub fn parse_cron(s: &str) -> Result<Self, ParamError> {
        match s
            .parse::<Self>()
            .map_err(|err| ParamError::new("cron", err.detail))?
        {
            cron @ JobSchedule::Cron { .. } => Ok(cron),
            JobSchedule::Interval { .. } => {
                Err(ParamError::new("cron", "is not a cron expression"))
            }
        }
    }

    /// A stored schedule: its `schedule` text and its `tz` column.
    pub fn from_row(schedule: &str, tz: &str) -> Result<Self, ParamError> {
        Ok(schedule.parse::<Self>()?.in_zone(Self::zone(tz)))
    }

    /// The same schedule in the zone `tz`. An interval has none.
    pub fn in_zone(self, tz: Tz) -> Self {
        match self {
            JobSchedule::Cron { cron, .. } => JobSchedule::Cron { cron, tz },
            interval @ JobSchedule::Interval { .. } => interval,
        }
    }

    /// The zone of a cron expression, UTC for an interval.
    pub fn tz(&self) -> Tz {
        match self {
            JobSchedule::Cron { tz, .. } => *tz,
            JobSchedule::Interval { .. } => Tz::UTC,
        }
    }

    /// `true` when `at` is a usable time: after 1970, up to 9999-12-31.
    pub fn in_range(at: DateTime<Utc>) -> bool {
        at.timestamp() > 0 && at.year() <= 9999
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
    pub fn next(
        &self,
        after: DateTime<Utc>,
        until: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        if !Self::in_range(after) {
            return None;
        }
        let next = match self {
            JobSchedule::Interval { interval } => {
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
            JobSchedule::Cron { cron, tz } => Schedule::from_str(cron)
                .ok()?
                .after(&after.with_timezone(tz))
                .next()?
                .with_timezone(&Utc),
        };
        (Self::in_range(next) && until.is_none_or(|u| next <= u)).then_some(next)
    }
}

impl Default for JobSchedule {
    fn default() -> Self {
        JobSchedule::Interval {
            interval: 60 * 60 * 24,
        }
    }
}

impl FromStr for JobSchedule {
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
                interval => Ok(JobSchedule::Interval { interval }),
            };
        }
        let mut parts: Vec<&str> = s.split(['|', ' ']).filter(|p| !p.is_empty()).collect();
        match parts.len() {
            5 => parts.insert(0, "0"),
            6 | 7 => {}
            n => {
                return Err(ParamError::new(
                    "cron",
                    format!("needs 5 or 6 fields, got {n}: e.g. */15 * * * *"),
                ));
            }
        }
        let schedule = Schedule::from_str(&parts.join(" "))
            .map_err(|_| ParamError::new("cron", "is not a valid cron expression"))?;
        Ok(JobSchedule::Cron {
            cron: schedule.to_string(),
            tz: Tz::UTC,
        })
    }
}

impl Display for JobSchedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self {
            JobSchedule::Interval { interval } => write!(f, "{}", interval),
            JobSchedule::Cron { cron, .. } => write!(f, "{}", cron),
        }
    }
}

#[tokio::test]
async fn schedule_from_str_err() -> anyhow::Result<()> {
    // arrange
    let s = "";
    // act
    let schedule: Result<JobSchedule, _> = s.parse();

    // assert
    assert!(schedule.is_err());
    Ok(())
}

#[tokio::test]
async fn schedule_from_str_interval() -> anyhow::Result<()> {
    // arrange
    let s = "300";
    // act
    let schedule: Result<JobSchedule, _> = s.parse();

    // assert
    assert!(schedule.is_ok());
    let ss = schedule.unwrap();
    assert_eq!(JobSchedule::Interval { interval: 300 }, ss);
    assert_eq!(s, ss.to_string());
    Ok(())
}

#[cfg(test)]
fn at(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(secs, 0).expect("in range")
}

#[tokio::test]
async fn schedule_from_str_interval_after() -> anyhow::Result<()> {
    // arrange
    let s = "300";
    let now = Utc::now();
    let five_min = TimeDelta::minutes(5);

    // act
    let schedule: JobSchedule = s.parse().unwrap();
    let next = schedule.next(now, None);

    // assert
    assert_eq!(next, Some(now.duration_trunc(five_min).unwrap() + five_min));
    Ok(())
}

#[tokio::test]
async fn schedule_from_str_interval_after_until() -> anyhow::Result<()> {
    // arrange
    let s = "300";
    let now = Utc::now();

    // act
    let schedule: JobSchedule = s.parse().unwrap();
    let next = schedule.next(now, Some(now));

    // assert
    assert!(next.is_none());
    Ok(())
}

#[tokio::test]
async fn schedule_from_str_cron() -> anyhow::Result<()> {
    // arrange
    let s = "*/5 * * * *";
    // act
    let schedule: Result<JobSchedule, _> = s.parse();

    // assert
    assert!(schedule.is_ok());
    let ss = schedule.unwrap();
    assert_eq!(
        JobSchedule::Cron {
            cron: "0 */5 * * * *".into(),
            tz: Tz::UTC
        },
        ss
    );
    assert_eq!("0 */5 * * * *", ss.to_string());
    Ok(())
}

#[tokio::test]
async fn schedule_from_str_cron_after() -> anyhow::Result<()> {
    // arrange
    let s = "*/5 * * * *";
    let now = Utc::now();
    let five_min = TimeDelta::minutes(5);

    // act
    let schedule: JobSchedule = s.parse().unwrap();
    let next = schedule.next(now, None);

    // assert
    assert_eq!(next, Some(now.duration_trunc(five_min).unwrap() + five_min));
    Ok(())
}

#[test]
fn schedule_from_str_interval_zero_err() {
    assert!("0".parse::<JobSchedule>().is_err());
}

#[test]
fn schedule_next_out_of_range_none() {
    let interval = JobSchedule::Interval { interval: 60 };
    let cron: JobSchedule = "*/5 * * * *".parse().unwrap();
    for schedule in [&interval, &cron] {
        assert_eq!(schedule.next(DateTime::<Utc>::MAX_UTC, None), None);
        assert_eq!(schedule.next(at(253_402_300_799), None), None);
        assert_eq!(schedule.next(at(-1), None), None);
    }
    // a zero interval stored before the validation existed
    assert_eq!(
        JobSchedule::Interval { interval: 0 }.next(at(1_000), None),
        None
    );
}

#[test]
fn schedule_next_until() {
    let schedule = JobSchedule::Interval { interval: 60 };
    assert_eq!(schedule.next(at(1_000), Some(at(1_020))), Some(at(1_020)));
    assert_eq!(schedule.next(at(1_000), Some(at(1_019))), None);
}

#[test]
fn cron_runs_at_the_local_time_of_its_zone() {
    use chrono::TimeZone;
    let at = |y, m, d, h| Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap();
    let nine = |tz| JobSchedule::Cron {
        cron: "0 0 9 * * *".into(),
        tz,
    };
    assert_eq!(
        nine(Tz::UTC).next(at(2025, 1, 1, 0), None),
        Some(at(2025, 1, 1, 9))
    );
    // Moscow is UTC+3 all year
    assert_eq!(
        nine(chrono_tz::Europe::Moscow).next(at(2025, 1, 1, 0), None),
        Some(at(2025, 1, 1, 6))
    );
    // New York: 9:00 is 14:00 UTC in winter and 13:00 UTC in summer
    let new_york = nine(chrono_tz::America::New_York);
    assert_eq!(
        new_york.next(at(2025, 1, 1, 0), None),
        Some(at(2025, 1, 1, 14))
    );
    assert_eq!(
        new_york.next(at(2025, 7, 1, 0), None),
        Some(at(2025, 7, 1, 13))
    );
    // an interval has no zone
    let hourly = JobSchedule::Interval { interval: 3600 };
    assert_eq!(hourly.clone().in_zone(chrono_tz::America::New_York), hourly);
    assert_eq!(hourly.tz(), Tz::UTC);
}

#[test]
fn a_stored_schedule_has_its_zone() {
    let cron = JobSchedule::from_row("0 0 9 * * *", "Asia/Tokyo").expect("cron");
    assert_eq!(cron.tz(), chrono_tz::Asia::Tokyo);
    assert_eq!(
        cron.to_string(),
        "0 0 9 * * *",
        "the zone is not part of the text"
    );
    assert_eq!(
        JobSchedule::from_row("300", "Asia/Tokyo").expect("interval"),
        JobSchedule::Interval { interval: 300 }
    );
    // a zone written by hand that does not exist: UTC
    assert_eq!(
        JobSchedule::from_row("0 0 9 * * *", "Mars/Base")
            .expect("cron")
            .tz(),
        Tz::UTC
    );
}

#[test]
fn a_time_that_does_not_exist_is_skipped_and_one_that_exists_twice_runs_once() {
    use chrono::TimeZone;
    let utc = |y, m, d, h, min| Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap();
    let runs = |from: DateTime<Utc>, n: usize| {
        let schedule = JobSchedule::Cron {
            cron: "0 30 2 * * *".into(),
            tz: chrono_tz::Europe::Berlin,
        };
        std::iter::successors(Some(from), |t| schedule.next(*t, None))
            .skip(1)
            .take(n)
            .collect::<Vec<_>>()
    };
    // 2025-03-30: 2:00 -> 3:00, there is no 2:30, the run of that day is skipped
    assert_eq!(
        runs(utc(2025, 3, 29, 12, 0), 2),
        [utc(2025, 3, 31, 0, 30), utc(2025, 4, 1, 0, 30)]
    );
    // 2025-10-26: 3:00 -> 2:00, 2:30 is there twice, the run is the first one
    assert_eq!(
        runs(utc(2025, 10, 25, 12, 0), 2),
        [utc(2025, 10, 26, 0, 30), utc(2025, 10, 27, 1, 30)]
    );
}
