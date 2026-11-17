use super::Error;
use serde::{Deserialize, Deserializer, Serialize, de};
use std::fmt;

/// The retry policy of a job; a job without one (`Option::None`) is not retried.
/// In JSON an object, `{"policy": "exponential", "count": 5, "delay": 2, "max_delay": 300}`, or
/// the text of `_retry` (see [`JobRetry::deserialize_optional`]). Fields that are not known are
/// ignored: a stored policy stays readable by a version that does not know them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum JobRetry {
    /// The same delay before every retry; no delay retries at once.
    Fixed {
        /// The number of retries before fail job.
        #[serde(rename = "count")]
        retry_count: u16,
        /// The delay in seconds between retries.
        #[serde(rename = "delay", default)]
        retry_delay: u32,
    },
    /// `delay` × 1, 1, 2, 3, 5, 8, …
    Fibonacci {
        /// The number of retries before fail job.
        #[serde(rename = "count")]
        retry_count: u16,
        /// The base delay in seconds between retries.
        #[serde(rename = "delay")]
        retry_delay: u32,
        /// The longest delay in seconds; 24 hours when absent.
        #[serde(rename = "max_delay", skip_serializing_if = "Option::is_none", default)]
        retry_max_delay: Option<u32>,
    },
    /// `delay` × 1, 2, 4, 8, 16, …
    Exponential {
        /// The number of retries before fail job.
        #[serde(rename = "count")]
        retry_count: u16,
        /// The base delay in seconds between retries.
        #[serde(rename = "delay")]
        retry_delay: u32,
        /// The longest delay in seconds; 24 hours when absent.
        #[serde(rename = "max_delay", skip_serializing_if = "Option::is_none", default)]
        retry_max_delay: Option<u32>,
    },
}

/// Upper bound for a single retry delay in seconds (24 hours), unless the policy sets it:
/// `retry_delay` of `Fixed`, `retry_max_delay` of a backoff.
pub const MAX_RETRY_DELAY: u32 = 24 * 60 * 60;

impl JobRetry {
    /// No retries, said out loud: where an absent policy means a default (a callback), `none` is
    /// this one.
    pub const NONE: Self = Self::Fixed {
        retry_count: 0,
        retry_delay: 0,
    };

    /// The form of the `_retry` parameter.
    pub const RULE: &str =
        "must be none, N, N|fixed|S, N|fibonacci|S|M or N|exponential|S|M (M optional)";

    /// The most retries, as `enqueue_v1` takes them.
    pub const MAX_COUNT: u16 = 9999;

    /// The longest `delay` and `max_delay` in seconds (7 days), as `enqueue_v1` takes them.
    pub const MAX_DELAY: u32 = 604_800;

    /// What is wrong with a count above [`Self::MAX_COUNT`].
    const COUNT_RULE: &str = "count must be at most 9999";

    /// What is wrong with a delay above [`Self::MAX_DELAY`].
    const DELAY_RULE: &str = "delay and max_delay must be at most 604800 seconds";

    /// `true` when the count is at most [`Self::MAX_COUNT`].
    fn count_fits(self) -> bool {
        self.retry_count() <= Self::MAX_COUNT
    }

    /// `true` when the delays are at most [`Self::MAX_DELAY`].
    fn delays_fit(self) -> bool {
        match self {
            JobRetry::Fixed { retry_delay, .. } => retry_delay <= Self::MAX_DELAY,
            JobRetry::Fibonacci {
                retry_delay,
                retry_max_delay,
                ..
            }
            | JobRetry::Exponential {
                retry_delay,
                retry_max_delay,
                ..
            } => retry_delay.max(retry_max_delay.unwrap_or(0)) <= Self::MAX_DELAY,
        }
    }

    /// For `deserialize_with`: a policy of a request, the object or the text of `_retry`
    /// (`"5|exponential|2"`). Absent, `null` and `""` are no policy (the default of the place);
    /// `"none"` is [`Self::NONE`]. A count above [`Self::MAX_COUNT`] and delays above [`Self::MAX_DELAY`] are refused.
    pub fn deserialize_optional<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Self>, D::Error> {
        struct Policy;
        impl<'de> de::Visitor<'de> for Policy {
            type Value = Option<JobRetry>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a retry policy: an object, or text as N|exponential|S|M")
            }

            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
                d.deserialize_any(self)
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
                JobRetry::parse(text).map_err(|_| E::custom(JobRetry::RULE))
            }

            fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                let retry = JobRetry::deserialize(de::value::MapAccessDeserializer::new(map))?;
                if !retry.count_fits() {
                    Err(de::Error::custom(JobRetry::COUNT_RULE))
                } else if !retry.delays_fit() {
                    Err(de::Error::custom(JobRetry::DELAY_RULE))
                } else {
                    Ok(Some(retry))
                }
            }
        }
        deserializer.deserialize_option(Policy)
    }

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
    /// (`M`, the longest delay, is optional). Empty is no policy (the default of the place);
    /// `none` is [`Self::NONE`]: no retries, said out loud.
    pub fn parse(s: &str) -> Result<Option<Self>, Error> {
        let invalid = || Error::invalid("retry", Self::RULE);
        let parts: Vec<&str> = s.split(['|', ',']).collect();
        let (count, kind, delay, max) = match parts.as_slice() {
            [""] => return Ok(None),
            ["none"] => return Ok(Some(Self::NONE)),
            [count] => (*count, "fixed", None, None),
            [count, kind] => (*count, *kind, None, None),
            [count, kind, delay] => (*count, *kind, Some(*delay), None),
            [count, kind, delay, max] => (*count, *kind, Some(*delay), Some(*max)),
            _ => return Err(invalid()),
        };
        let seconds = |part: Option<&str>| {
            part.map(|p| {
                p.parse::<u32>()
                    .ok()
                    .filter(|&s| s <= Self::MAX_DELAY)
                    .ok_or_else(invalid)
            })
            .transpose()
        };
        let retry_count: u16 = count
            .parse()
            .ok()
            .filter(|&n| n <= Self::MAX_COUNT)
            .ok_or_else(invalid)?;
        match (kind, seconds(delay)?, seconds(max)?) {
            ("fixed", retry_delay, None) => Ok(Some(JobRetry::Fixed {
                retry_count,
                retry_delay: retry_delay.unwrap_or(0),
            })),
            ("fibonacci", Some(retry_delay), retry_max_delay) => Ok(Some(JobRetry::Fibonacci {
                retry_count,
                retry_delay,
                retry_max_delay,
            })),
            ("exponential", Some(retry_delay), retry_max_delay) => {
                Ok(Some(JobRetry::Exponential {
                    retry_count,
                    retry_delay,
                    retry_max_delay,
                }))
            }
            // a typo must not silently become `fixed`
            _ => Err(invalid()),
        }
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
        assert_eq!(parse("none"), Ok(Some(JobRetry::NONE)));
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
            "3|fixed|2147483648",
            "65536",
            "-1",
        ] {
            assert!(parse(s).is_err(), "{s}");
        }
    }

    #[test]
    fn deserialize() {
        let read = |s: &str| serde_json::from_str::<JobRetry>(s).ok();
        assert_eq!(read(r#"{"policy":"fixed","count":3}"#), Some(fixed(3, 0)));
        assert_eq!(
            read(r#"{"policy":"exponential","count":3,"delay":1,"max_delay":9}"#),
            Some(JobRetry::Exponential {
                retry_count: 3,
                retry_delay: 1,
                retry_max_delay: Some(9)
            })
        );
        // a backoff needs its delay, a policy its kind
        for s in [
            r#"{"policy":"fibonacci","count":3}"#,
            r#"{"count":3}"#,
            r#"{"policy":"fixd","count":3}"#,
        ] {
            assert_eq!(read(s), None, "{s}");
        }
        // a field it does not know (a newer version wrote it) is ignored
        assert_eq!(
            read(r#"{"policy":"fixed","count":3,"jitter":true}"#),
            Some(fixed(3, 0))
        );
        assert_eq!(
            serde_json::to_string(&JobRetry::Fibonacci {
                retry_count: 3,
                retry_delay: 1,
                retry_max_delay: None
            })
            .unwrap(),
            r#"{"policy":"fibonacci","count":3,"delay":1}"#
        );
    }

    #[test]
    fn a_request_has_the_object_or_the_text() {
        #[derive(Deserialize)]
        struct Request {
            #[serde(default, deserialize_with = "JobRetry::deserialize_optional")]
            retry: Option<JobRetry>,
        }
        let read = |s: &str| serde_json::from_str::<Request>(s).map(|r| r.retry).ok();
        for s in [r#"{}"#, r#"{"retry":null}"#, r#"{"retry":""}"#] {
            assert_eq!(read(s), Some(None), "{s}");
        }
        assert_eq!(read(r#"{"retry":"none"}"#), Some(Some(JobRetry::NONE)));
        assert!(read(r#"{"retry":"9999|fixed|604800"}"#).is_some());
        let backoff = Some(Some(JobRetry::Exponential {
            retry_count: 5,
            retry_delay: 2,
            retry_max_delay: None,
        }));
        assert_eq!(read(r#"{"retry":"5|exponential|2"}"#), backoff);
        assert_eq!(
            read(r#"{"retry":{"policy":"exponential","count":5,"delay":2}}"#),
            backoff
        );
        for s in [
            r#"{"retry":"5|exponential"}"#,
            r#"{"retry":5}"#,
            r#"{"retry":{"policy":"fixed","count":1,"delay":604801}}"#,
            r#"{"retry":{"policy":"fixed","count":10000}}"#,
            r#"{"retry":"10000"}"#,
            r#"{"retry":"1|fixed|604801"}"#,
        ] {
            assert_eq!(read(s), None, "{s}");
        }
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
