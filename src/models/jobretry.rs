use super::Error;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "retry")]
#[serde(rename_all = "snake_case")]
pub enum JobRetry {
    #[default]
    None,
    Immediate {
        retry_count: u32,
    },
    Fixed {
        retry_count: u32,
        retry_delay: u32,
    },
    Fibonacci {
        retry_count: u32,
        retry_delay: u32,
    },
}

impl JobRetry {
    pub const fn is_none(&self) -> bool {
        match self {
            JobRetry::None => true,
            _ => false,
        }
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
}

impl FromStr for JobRetry {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Ok(JobRetry::None);
        }
        let parts: Vec<&str> = s.split(['|', ',']).collect();
        let retry_count: u32 = parts[0]
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
                return Ok(retry);
            }
            _ => Ok(JobRetry::None),
        }
    }
}

#[tokio::test]
async fn job_retry_from_str_err() -> anyhow::Result<()> {
    // arrange
    let s = "test".to_string();
    // act
    let job_retry: Result<JobRetry, _> = s.parse();

    // assert
    assert!(job_retry.is_err());
    Ok(())
}

#[tokio::test]
async fn job_retry_from_str_empty() -> anyhow::Result<()> {
    // arrange
    let s = "".to_string();
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
    let s = "3".to_string();
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
    let s = "3|fibonacci|15".to_string();
    // act
    let job_retry: Result<JobRetry, _> = s.parse();

    // assert
    assert!(job_retry.is_ok());
    assert_eq!(JobRetry::Fibonacci { retry_count: 3, retry_delay: 15 }, job_retry.unwrap());
    Ok(())
}
