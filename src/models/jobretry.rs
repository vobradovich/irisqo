use super::Error;
use serde::{Deserialize, Serialize};

/// The retry policy of a job; a job without one (`Option::None`) is not retried.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "retry", rename_all = "snake_case")]
pub enum JobRetry {
    /// The same delay before every retry; no delay retries at once.
    Fixed {
        /// The number of retries before fail job.
        retry_count: u16,
        /// The delay in seconds between retries.
        #[serde(default)]
        retry_delay: u32,
    },
    /// `retry_delay` × 1, 1, 2, 3, 5, 8, …
    Fibonacci {
        /// The number of retries before fail job.
        retry_count: u16,
        /// The base delay in seconds between retries.
        retry_delay: u32,
        /// The longest delay in seconds; [`MAX_RETRY_DELAY`] when absent.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        retry_max_delay: Option<u32>,
    },
    /// `retry_delay` × 1, 2, 4, 8, 16, …
    Exponential {
        /// The number of retries before fail job.
        retry_count: u16,
        /// The base delay in seconds between retries.
        retry_delay: u32,
        /// The longest delay in seconds; [`MAX_RETRY_DELAY`] when absent.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        retry_max_delay: Option<u32>,
    },
}

/// Upper bound for a single retry delay in seconds (24 hours), unless the policy sets it:
/// `retry_delay` of `Fixed`, `retry_max_delay` of a backoff.
pub const MAX_RETRY_DELAY: u32 = 24 * 60 * 60;

impl JobRetry {
    /// The form of the `_retry` parameter.
    pub const RULE: &str =
        "must be N, N|fixed|S, N|fibonacci|S|M or N|exponential|S|M (M optional)";

    pub const fn retry_count(self) -> u16 {
        match self {
            JobRetry::Fixed { retry_count, .. }
            | JobRetry::Fibonacci { retry_count, .. }
            | JobRetry::Exponential { retry_count, .. } => retry_count,
        }
    }

    /// The longest delay in seconds: `retry_max_delay` of a backoff, [`MAX_RETRY_DELAY`] when
    /// it is not set and for `Fixed` (its own delay is not capped).
    pub fn retry_max_delay(self) -> u32 {
        match self {
            JobRetry::Fixed { .. } => MAX_RETRY_DELAY,
            JobRetry::Fibonacci {
                retry_max_delay, ..
            }
            | JobRetry::Exponential {
                retry_max_delay, ..
            } => retry_max_delay.unwrap_or(MAX_RETRY_DELAY),
        }
    }

    /// Past the table the series only grows: it saturates, the delay is capped anyway.
    fn fibonacci(idx: usize) -> u32 {
        Self::FIB_ARRAY.get(idx).copied().unwrap_or(u32::MAX)
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

    /// Delay in seconds before the retry number `attempt` (0-based), a backoff capped by
    /// [`Self::retry_max_delay`]; `None` when the retries are exhausted.
    pub fn delay(self, attempt: u16) -> Option<u32> {
        (attempt < self.retry_count()).then(|| {
            let (retry_delay, factor) = match self {
                JobRetry::Fixed { retry_delay, .. } => return retry_delay,
                JobRetry::Fibonacci { retry_delay, .. } => {
                    (retry_delay, Self::fibonacci(usize::from(attempt)))
                }
                JobRetry::Exponential { retry_delay, .. } => {
                    (retry_delay, 2u32.saturating_pow(u32::from(attempt)))
                }
            };
            retry_delay
                .saturating_mul(factor)
                .min(self.retry_max_delay())
        })
    }

    /// [`Self::delay`] spread by ±20 %, so that jobs failed together do not retry
    /// together; the spread up stops at [`Self::retry_max_delay`], or at the delay when that is
    /// longer (an explicit `fixed` one).
    pub fn jittered_delay(self, attempt: u16) -> Option<u32> {
        let delay = self.delay(attempt)?;
        Some(with_jitter(delay).min(self.retry_max_delay().max(delay)))
    }

    /// Parses `N` (retries at once), `N|fixed|S`, `N|fibonacci|S|M` or `N|exponential|S|M`
    /// (`M`, the longest delay, is optional); empty or `none` means no retries.
    pub fn parse(s: &str) -> Result<Option<Self>, Error> {
        let invalid = || Error::invalid("retry", Self::RULE);
        let parts: Vec<&str> = s.split(['|', ',']).collect();
        let (count, kind, delay, max) = match parts.as_slice() {
            [""] | ["none"] => return Ok(None),
            [count] => (*count, "fixed", None, None),
            [count, kind] => (*count, *kind, None, None),
            [count, kind, delay] => (*count, *kind, Some(*delay), None),
            [count, kind, delay, max] => (*count, *kind, Some(*delay), Some(*max)),
            _ => return Err(invalid()),
        };
        let seconds = |part: Option<&str>| {
            part.map(|p| p.parse::<u32>().map_err(|_| invalid()))
                .transpose()
        };
        let retry_count: u16 = count.parse().map_err(|_| invalid())?;
        let (retry_delay, retry_max_delay) = (seconds(delay)?, seconds(max)?);
        let retry = match (kind, retry_delay, retry_max_delay) {
            ("fixed", retry_delay, None) => JobRetry::Fixed {
                retry_count,
                retry_delay: retry_delay.unwrap_or(0),
            },
            ("fibonacci", Some(retry_delay), retry_max_delay) => JobRetry::Fibonacci {
                retry_count,
                retry_delay,
                retry_max_delay,
            },
            ("exponential", Some(retry_delay), retry_max_delay) => JobRetry::Exponential {
                retry_count,
                retry_delay,
                retry_max_delay,
            },
            // a typo must not silently become `fixed`
            _ => return Err(invalid()),
        };
        Ok(Some(retry))
    }
}

/// Spreads a retry delay by ±20 %.
fn with_jitter(delay: u32) -> u32 {
    let spread = delay / 5;
    if spread == 0 {
        return delay;
    }
    (delay - spread).saturating_add(fastrand::u32(..=2 * spread))
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn fixed(retry_count: u16, retry_delay: u32) -> JobRetry {
        JobRetry::Fixed {
            retry_count,
            retry_delay,
        }
    }

    #[test]
    fn parse() {
        let parse = |s| JobRetry::parse(s).map_err(|e| e.to_string());
        assert_eq!(parse(""), Ok(None));
        assert_eq!(parse("3"), Ok(Some(fixed(3, 0))));
        assert_eq!(parse("3|fixed"), Ok(Some(fixed(3, 0))));
        assert_eq!(parse("3|fixed|15"), Ok(Some(fixed(3, 15))));
        assert_eq!(
            parse("3,fibonacci,15"),
            Ok(Some(JobRetry::Fibonacci {
                retry_count: 3,
                retry_delay: 15,
                retry_max_delay: None
            }))
        );
        assert_eq!(
            parse("5|exponential|2|60"),
            Ok(Some(JobRetry::Exponential {
                retry_count: 5,
                retry_delay: 2,
                retry_max_delay: Some(60)
            }))
        );
        for s in [
            "test",
            "3|fibonaci|15",
            "3|fibonacci",
            "3|exponential",
            "3|fixed|15|60",
            "3|exponential|x",
            "3|exponential|2|x",
            "3|exponential|2|60|1",
            "-1",
        ] {
            assert!(parse(s).is_err(), "{s}");
        }
    }

    #[test]
    fn deserialize() {
        let read = |s: &str| serde_json::from_str::<JobRetry>(s).ok();
        assert_eq!(
            read(r#"{"retry":"fixed","retry_count":3}"#),
            Some(fixed(3, 0))
        );
        assert_eq!(
            read(r#"{"retry":"exponential","retry_count":3,"retry_delay":1,"retry_max_delay":9}"#),
            Some(JobRetry::Exponential {
                retry_count: 3,
                retry_delay: 1,
                retry_max_delay: Some(9)
            })
        );
        assert_eq!(read(r#"{"retry":"fibonacci","retry_count":3}"#), None);
        assert_eq!(
            serde_json::to_string(&JobRetry::Fibonacci {
                retry_count: 3,
                retry_delay: 1,
                retry_max_delay: None
            })
            .unwrap(),
            r#"{"retry":"fibonacci","retry_count":3,"retry_delay":1}"#
        );
    }

    #[test]
    fn fixed_until_exhausted() {
        let retry = fixed(2, 5);
        assert_eq!(Some(5), retry.delay(0));
        assert_eq!(Some(5), retry.delay(1));
        assert_eq!(None, retry.delay(2));
        // at once
        let retry = fixed(3, 0);
        assert_eq!(Some(0), retry.delay(2));
        assert_eq!(None, retry.delay(3));
    }

    #[test]
    fn fibonacci_until_exhausted() {
        let retry = JobRetry::Fibonacci {
            retry_count: 4,
            retry_delay: 2,
            retry_max_delay: None,
        };
        let delays: Vec<_> = (0..5).map(|i| retry.delay(i)).collect();
        assert_eq!(delays, [Some(2), Some(2), Some(4), Some(6), None]);
    }

    #[test]
    fn exponential_until_exhausted() {
        let retry = JobRetry::Exponential {
            retry_count: 5,
            retry_delay: 3,
            retry_max_delay: None,
        };
        let delays: Vec<_> = (0..6).map(|i| retry.delay(i)).collect();
        assert_eq!(
            delays,
            [Some(3), Some(6), Some(12), Some(24), Some(48), None]
        );
    }

    #[test]
    fn max_delay_caps_the_backoff() {
        for retry in [
            JobRetry::Fibonacci {
                retry_count: 10,
                retry_delay: 10,
                retry_max_delay: Some(25),
            },
            JobRetry::Exponential {
                retry_count: 10,
                retry_delay: 10,
                retry_max_delay: Some(25),
            },
        ] {
            assert_eq!(retry.delay(0), Some(10), "{retry:?}");
            assert_eq!(retry.delay(9), Some(25), "{retry:?}");
        }
        // an explicit max goes past the global cap
        let retry = JobRetry::Exponential {
            retry_count: 100,
            retry_delay: 10,
            retry_max_delay: Some(2 * MAX_RETRY_DELAY),
        };
        assert_eq!(retry.delay(9), Some(5_120));
        assert_eq!(retry.delay(99), Some(2 * MAX_RETRY_DELAY));
    }

    #[test]
    fn a_backoff_without_a_max_is_capped() {
        let fib = JobRetry::Fibonacci {
            retry_count: u16::MAX,
            retry_delay: u32::MAX,
            retry_max_delay: None,
        };
        assert_eq!(fib.delay(40), Some(MAX_RETRY_DELAY));
        let fib = JobRetry::Fibonacci {
            retry_count: 100,
            retry_delay: 2_000,
            retry_max_delay: None,
        };
        assert_eq!(fib.delay(31), Some(MAX_RETRY_DELAY));
        let exp = JobRetry::Exponential {
            retry_count: u16::MAX,
            retry_delay: 1,
            retry_max_delay: None,
        };
        assert_eq!(exp.delay(u16::MAX - 1), Some(MAX_RETRY_DELAY));
        // the delay of `fixed` is explicit
        assert_eq!(fixed(1, u32::MAX).delay(0), Some(u32::MAX));
    }

    #[test]
    fn jitter_stays_within_the_policy() {
        let retry = JobRetry::Exponential {
            retry_count: 10,
            retry_delay: 100,
            retry_max_delay: Some(150),
        };
        for _ in 0..1000 {
            let d = retry.jittered_delay(0).expect("retry");
            assert!((80..=120).contains(&d), "{d}");
            // the spread up is capped by the max
            let d = retry.jittered_delay(9).expect("retry");
            assert!((120..=150).contains(&d), "{d}");
            // and by the delay itself when it is past the max: 2 days of `fixed`
            let d = fixed(1, 2 * MAX_RETRY_DELAY)
                .jittered_delay(0)
                .expect("retry");
            assert!(
                (2 * MAX_RETRY_DELAY - 2 * MAX_RETRY_DELAY / 5..=2 * MAX_RETRY_DELAY).contains(&d),
                "{d}"
            );
        }
        assert_eq!(fixed(1, 5).jittered_delay(1), None);
    }

    #[test]
    fn jitter_bounds() {
        assert_eq!(with_jitter(0), 0);
        assert_eq!(with_jitter(4), 4);
        for _ in 0..1000 {
            let d = with_jitter(100);
            assert!((80..=120).contains(&d), "{d}");
        }
        assert!(with_jitter(u32::MAX) >= u32::MAX - u32::MAX / 5);
    }
}
