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

impl JobSchedule {
    pub fn now_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }
    
    pub fn next(&self, after_unix_sec: i64) -> Option<i64> {
        let dt = Utc.timestamp_opt(after_unix_sec, 0).unwrap();
        match &self {
            JobSchedule::Interval { interval } => Some(after_unix_sec - (after_unix_sec % *interval as i64) + *interval as i64),
            JobSchedule::Cron { cron } => Schedule::from_str(&cron)
                .unwrap()
                .after(&dt)
                .map(|dt| dt.timestamp())
                .next(),
        }
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
        return Err(Error::InvalidParams("schedule"));
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
    let next = schedule.next(now_secs);

    // assert
    assert!(next.is_some());
    assert_eq!(now_sec_to_5_min + 300, next.unwrap());
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
    let next = schedule.next(now_secs);

    // assert
    assert!(next.is_some());
    assert_eq!(now_sec_to_5_min + 300, next.unwrap());
    Ok(())
}
