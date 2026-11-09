use std::{fmt::Display, str::FromStr};

use crate::models::ParamError;
use chrono::{DateTime, Datelike, DurationRound, TimeDelta, Utc};
use cron::Schedule;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[serde(rename_all = "snake_case")]
pub enum JobSchedule {
    /// Interval in seconds
    Interval { interval: u32 },
    /// Cron string
    Cron { cron: String },
}

impl JobSchedule {
    /// `true` when `at` is a usable time: after 1970, up to 9999-12-31.
    pub fn in_range(at: DateTime<Utc>) -> bool {
        at.timestamp() > 0 && at.year() <= 9999
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
            JobSchedule::Cron { cron } => Schedule::from_str(cron).ok()?.after(&after).next()?,
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
        })
    }
}

impl Display for JobSchedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self {
            JobSchedule::Interval { interval } => write!(f, "{}", interval),
            JobSchedule::Cron { cron } => write!(f, "{}", cron),
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
            cron: "0 */5 * * * *".into()
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
