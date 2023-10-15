use std::{fmt::Display, str::FromStr};

use super::Error;
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

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct ScheduleRow {
    pub schedule_id: String,
    pub schedule: String,
    pub until: Option<i64>,
    pub last_id: Option<i64>,
    pub last_at: Option<i64>,
    pub next_id: Option<i64>,
    pub next_at: Option<i64>,
    pub inactive: bool,
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
