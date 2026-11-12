use serde::Deserialize;

use crate::models::{ParamError, QUEUE_RULE, is_queue};

/// The limits of a queue as the API or the form describes them; what is absent takes its
/// default.
#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
pub struct QueueRequest {
    /// Jobs running at once, 1 to 100000; 1 when absent.
    #[schema(minimum = 1, maximum = 100_000)]
    pub parallelism: Option<i32>,
    /// Jobs go in id order (`true`, when absent) or any due job may go.
    pub ordered: Option<bool>,
    /// Jobs started per `rate_period`, 1 to 1000000; no limit when absent.
    #[schema(minimum = 1, maximum = 1_000_000)]
    pub rate: Option<i32>,
    /// Whole seconds, 1 to 31536000; 1 when absent.
    #[schema(minimum = 1, maximum = 31_536_000)]
    pub rate_period: Option<u32>,
}

/// A queue to store: every field is checked and the defaults are filled in. Only
/// [`QueueRequest::validate`] makes one.
#[derive(Debug, PartialEq)]
pub struct QueueCreate {
    pub(super) queue: String,
    pub(super) parallelism: i32,
    pub(super) ordered: bool,
    pub(super) rate: Option<i32>,
    pub(super) rate_period: u32,
}

impl QueueRequest {
    /// The queue `queue` to store, or every invalid field.
    pub fn validate(self, queue: &str) -> Result<QueueCreate, Vec<ParamError>> {
        let mut errors = Vec::new();
        if !is_queue(queue) {
            errors.push(ParamError::new("queue", QUEUE_RULE));
        }
        let parallelism = self.parallelism.unwrap_or(1);
        if !(1..=100_000).contains(&parallelism) {
            errors.push(ParamError::new(
                "parallelism",
                "must be a number from 1 to 100000",
            ));
        }
        if self
            .rate
            .is_some_and(|rate| !(1..=1_000_000).contains(&rate))
        {
            errors.push(ParamError::new(
                "rate",
                "must be a number from 1 to 1000000",
            ));
        }
        let rate_period = self.rate_period.unwrap_or(1);
        if !(1..=31_536_000).contains(&rate_period) {
            errors.push(ParamError::new(
                "rate_period",
                "must be a whole number of seconds from 1 to 31536000",
            ));
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(QueueCreate {
            queue: queue.to_owned(),
            parallelism,
            ordered: self.ordered.unwrap_or(true),
            rate: self.rate,
            rate_period,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_limits() {
        let create = QueueRequest::default()
            .validate("orders:42")
            .expect("valid");
        assert_eq!(
            create,
            QueueCreate {
                queue: "orders:42".into(),
                parallelism: 1,
                ordered: true,
                rate: None,
                rate_period: 1
            }
        );
        let bad = QueueRequest {
            parallelism: Some(0),
            rate: Some(-1),
            rate_period: Some(0),
            ..QueueRequest::default()
        };
        assert_eq!(bad.validate("a").expect_err("invalid").len(), 3);
    }

    #[test]
    fn the_name_is_checked() {
        for queue in ["Orders", "", "a b"] {
            let errors = QueueRequest::default().validate(queue).expect_err(queue);
            assert_eq!(errors[0].parameter, "queue");
        }
    }
}
