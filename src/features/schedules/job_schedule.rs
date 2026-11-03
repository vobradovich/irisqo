use std::{fmt::Display, str::FromStr};

use crate::models::Error;
use chrono::{TimeZone, Utc};
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

/// Latest accepted unix timestamp in seconds: 9999-12-31T23:59:59Z.
pub const MAX_UNIX_SECS: i64 = 253_402_300_799;

impl JobSchedule {
    pub fn now_secs() -> i64 {
        Utc::now().timestamp()
    }

    /// `true` when `secs` is a usable unix timestamp (`1..=MAX_UNIX_SECS`).
    pub const fn is_valid_unix_secs(secs: i64) -> bool {
        secs > 0 && secs <= MAX_UNIX_SECS
    }

    /// The next run strictly after `after_unix_sec`, `None` when there is none
    /// before `until_unix_sec` or the timestamps are out of range.
    pub fn next(&self, after_unix_sec: i64, until_unix_sec: Option<i64>) -> Option<i64> {
        if !Self::is_valid_unix_secs(after_unix_sec) {
            return None;
        }
        let next = match &self {
            JobSchedule::Interval { interval } => {
                let interval = i64::from(*interval);
                if interval == 0 {
                    return None;
                }
                (after_unix_sec - after_unix_sec % interval).checked_add(interval)?
            }
            JobSchedule::Cron { cron } => {
                let dt = Utc.timestamp_opt(after_unix_sec, 0).single()?;
                Schedule::from_str(cron)
                    .ok()?
                    .after(&dt)
                    .next()?
                    .timestamp()
            }
        };
        let in_range = Self::is_valid_unix_secs(next) && until_unix_sec.is_none_or(|u| next <= u);
        in_range.then_some(next)
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
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(Error::InvalidParams("schedule"));
        }
        let interval = s.parse::<u32>();
        if let Ok(val) = interval {
            if val == 0 {
                return Err(Error::InvalidParams("interval"));
            }
            return Ok(JobSchedule::Interval { interval: val });
        }
        let mut parts: Vec<&str> = s.split(['|', ' ']).collect();
        let fixed: String = match parts.len() {
            5 => {
                parts.insert(0, "0");
                Ok(parts.join(" "))
            }
            6 => Ok(parts.join(" ")),
            7 => Ok(parts.join(" ")),
            _ => Err(Error::InvalidParams("schedule")),
        }?;
        let schedule = Schedule::from_str(&fixed);
        if let Ok(s) = schedule {
            return Ok(JobSchedule::Cron {
                cron: s.to_string(),
            });
        }
        Err(Error::InvalidParams("schedule"))
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

#[tokio::test]
async fn schedule_from_str_interval_after() -> anyhow::Result<()> {
    // arrange
    let s = "300";
    let now_secs = JobSchedule::now_secs();
    let now_sec_to_5_min: i64 = now_secs - (now_secs % 300);

    // act
    let schedule: JobSchedule = s.parse().unwrap();
    let next = schedule.next(now_secs, None);

    // assert
    assert!(next.is_some());
    assert_eq!(now_sec_to_5_min + 300, next.unwrap());
    Ok(())
}

#[tokio::test]
async fn schedule_from_str_interval_after_until() -> anyhow::Result<()> {
    // arrange
    let s = "300";
    let now_secs = JobSchedule::now_secs();

    // act
    let schedule: JobSchedule = s.parse().unwrap();
    let next = schedule.next(now_secs, Some(now_secs));

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
    let now_secs = JobSchedule::now_secs();

    let now_sec_to_5_min: i64 = now_secs - (now_secs % 300);

    // act
    let schedule: JobSchedule = s.parse().unwrap();
    let next = schedule.next(now_secs, None);

    // assert
    assert!(next.is_some());
    assert_eq!(now_sec_to_5_min + 300, next.unwrap());
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
        assert_eq!(schedule.next(i64::MAX, None), None);
        assert_eq!(schedule.next(MAX_UNIX_SECS, None), None);
        assert_eq!(schedule.next(-1, None), None);
    }
    // a zero interval stored before the validation existed
    assert_eq!(
        JobSchedule::Interval { interval: 0 }.next(1_000, None),
        None
    );
}

#[test]
fn schedule_next_until() {
    let schedule = JobSchedule::Interval { interval: 60 };
    assert_eq!(schedule.next(1_000, Some(1_020)), Some(1_020));
    assert_eq!(schedule.next(1_000, Some(1_019)), None);
}
