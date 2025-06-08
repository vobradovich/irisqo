use super::Error;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "retry")]
#[serde(rename_all = "snake_case")]
pub enum JobRetry {
    #[default]
    None,
    Immediate {
        retry_count: u16,
    },
    Fixed {
        retry_count: u16,
        retry_delay: u32,
    },
    Fibonacci {
        retry_count: u16,
        retry_delay: u32,
    },
}

impl JobRetry {
    pub const fn is_none(&self) -> bool {
        matches!(self, JobRetry::None)
    }

    pub const fn fibonacci(idx: usize) -> u32 {
        if idx >= Self::FIB_ARRAY.len() {
            return Self::FIB_ARRAY[31];
        }
        Self::FIB_ARRAY[idx]
    }

    const FIB_ARRAY: [u32; 32] = Self::fib_array::<32>();

    const fn fib_array<const N: usize>() -> [u32; N] {
        let mut x = [1; N];
        let mut i: usize = 2;
        while i < N {
            let next_x = x[i - 1] + x[i - 2];
            x[i] = next_x;
            i += 1;
        }
        x
    }

    pub fn next_retry_in(self, retry: u16) -> Option<u32> {
        match self {
            JobRetry::None => None,
            JobRetry::Immediate { retry_count } => (retry < retry_count).then_some(0),
            JobRetry::Fixed {
                retry_count,
                retry_delay,
            } => (retry < retry_count).then_some(retry_delay),
            JobRetry::Fibonacci {
                retry_count,
                retry_delay,
            } => (retry < retry_count).then_some(retry_delay * JobRetry::fibonacci(retry as usize)),
        }
    }
}

impl FromStr for JobRetry {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Ok(JobRetry::None);
        }
        let parts: Vec<&str> = s.split(['|', ',']).collect();
        let retry_count: u16 = parts[0]
            .parse()
            .map_err(|_| Error::InvalidParams("retry"))?;

        match parts.len() {
            1 | 2 => Ok(JobRetry::Immediate { retry_count }),
            3 => {
                let retry_delay: u32 = parts[2]
                    .parse()
                    .map_err(|_| Error::InvalidParams("retry"))?;
                let retry = match parts[1] {
                    "exponential" | "fibonacci" => JobRetry::Fibonacci {
                        retry_count,
                        retry_delay,
                    },
                    _ => JobRetry::Fixed {
                        retry_count,
                        retry_delay,
                    },
                };
                Ok(retry)
            }
            _ => Ok(JobRetry::None),
        }
    }
}

#[tokio::test]
async fn job_retry_from_str_err() -> anyhow::Result<()> {
    // arrange
    let s = "test";
    // act
    let job_retry: Result<JobRetry, _> = s.parse();

    // assert
    assert!(job_retry.is_err());
    Ok(())
}

#[tokio::test]
async fn job_retry_from_str_empty() -> anyhow::Result<()> {
    // arrange
    let s = "";
    // act
    let job_retry: Result<JobRetry, _> = s.parse();

    // assert
    assert!(job_retry.is_ok());
    assert_eq!(JobRetry::None, job_retry.unwrap());
    Ok(())
}

#[tokio::test]
async fn job_retry_from_str_fixed() -> anyhow::Result<()> {
    // arrange
    let s = "3";
    // act
    let job_retry: Result<JobRetry, _> = s.parse();

    // assert
    assert!(job_retry.is_ok());
    assert_eq!(JobRetry::Immediate { retry_count: 3 }, job_retry.unwrap());
    Ok(())
}

#[tokio::test]
async fn job_retry_from_str_fibonacci() -> anyhow::Result<()> {
    // arrange
    let s = "3|fibonacci|15";
    // act
    let job_retry: Result<JobRetry, _> = s.parse();

    // assert
    assert!(job_retry.is_ok());
    assert_eq!(
        JobRetry::Fibonacci {
            retry_count: 3,
            retry_delay: 15
        },
        job_retry.unwrap()
    );
    Ok(())
}

#[tokio::test]
async fn job_retry_immediate_until_exhausted() -> anyhow::Result<()> {
    // arrange
    let retry = JobRetry::Immediate { retry_count: 3 };

    // act & assert
    assert_eq!(Some(0), retry.next_retry_in(0));
    assert_eq!(Some(0), retry.next_retry_in(1));
    assert_eq!(Some(0), retry.next_retry_in(2));
    assert_eq!(None, retry.next_retry_in(3));
    Ok(())
}

#[tokio::test]
async fn job_retry_fixed_until_exhausted() -> anyhow::Result<()> {
    // arrange
    let retry = JobRetry::Fixed {
        retry_count: 2,
        retry_delay: 5,
    };

    // act & assert
    assert_eq!(Some(5), retry.next_retry_in(0));
    assert_eq!(Some(5), retry.next_retry_in(1));
    assert_eq!(None, retry.next_retry_in(2));
    Ok(())
}

#[tokio::test]
async fn job_retry_fibonacci_until_exhausted() -> anyhow::Result<()> {
    // arrange
    let retry = JobRetry::Fibonacci {
        retry_count: 4,
        retry_delay: 2,
    };

    // act & assert
    assert_eq!(Some(2 * JobRetry::fibonacci(0)), retry.next_retry_in(0));
    assert_eq!(Some(2 * JobRetry::fibonacci(1)), retry.next_retry_in(1));
    assert_eq!(Some(2 * JobRetry::fibonacci(2)), retry.next_retry_in(2));
    assert_eq!(Some(2 * JobRetry::fibonacci(3)), retry.next_retry_in(3));
    assert_eq!(None, retry.next_retry_in(4));
    Ok(())
}
