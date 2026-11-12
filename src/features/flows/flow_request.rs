use serde::Deserialize;

use crate::models::{FlowKey, ParamError};

/// The limits of a flow as the API or the form describes them; what is absent takes its
/// default. A flow limits how many jobs run at once and how fast they start; it promises no order.
#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
pub struct FlowRequest {
    /// Jobs running at once, 1 to 100000; 1 when absent.
    #[schema(minimum = 1, maximum = 100_000)]
    pub concurrency: Option<i32>,
    /// Jobs started per `rate_period`, 1 to 1000000; no limit when absent.
    #[schema(minimum = 1, maximum = 1_000_000)]
    pub rate: Option<i32>,
    /// Whole seconds, 1 to 31536000; 1 when absent.
    #[schema(minimum = 1, maximum = 31_536_000)]
    pub rate_period: Option<i32>,
}

/// A flow to store: every field is checked and the defaults are filled in. Only
/// [`FlowRequest::validate`] makes one.
#[derive(Debug, PartialEq)]
pub struct FlowCreate {
    pub(super) flow_key: FlowKey,
    pub(super) concurrency: i32,
    pub(super) rate: Option<i32>,
    pub(super) rate_period: i32,
}

impl FlowRequest {
    /// The flow `flow_key` to store, or every invalid field.
    pub fn validate(self, flow_key: &str) -> Result<FlowCreate, Vec<ParamError>> {
        let mut errors = Vec::new();
        let flow_key = FlowKey::try_from(flow_key.to_owned())
            .map_err(|rule| errors.push(ParamError::new("flow_key", rule)))
            .ok();
        let concurrency = self.concurrency.unwrap_or(1);
        if !(1..=100_000).contains(&concurrency) {
            errors.push(ParamError::new(
                "concurrency",
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
        let Some(flow_key) = flow_key.filter(|_| errors.is_empty()) else {
            return Err(errors);
        };
        Ok(FlowCreate {
            flow_key,
            concurrency,
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
        let create = FlowRequest::default().validate("orders:42").expect("valid");
        assert_eq!(
            create,
            FlowCreate {
                flow_key: FlowKey::try_from("orders:42".to_owned()).expect("valid"),
                concurrency: 1,
                rate: None,
                rate_period: 1
            }
        );
        let bad = FlowRequest {
            concurrency: Some(0),
            rate: Some(-1),
            rate_period: Some(0),
        };
        assert_eq!(bad.validate("a").expect_err("invalid").len(), 3);
    }

    #[test]
    fn the_name_is_checked() {
        for flow in ["Orders", "", "a b"] {
            let errors = FlowRequest::default().validate(flow).expect_err(flow);
            assert_eq!(errors[0].parameter, "flow_key");
        }
    }
}
