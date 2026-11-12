use serde::{Deserialize, Serialize};

pub mod echo;
pub mod history;
pub mod instances;
pub mod live;
pub mod queues;
pub mod results;
pub mod schedules;
pub mod ui;

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct Paging {
    /// Page size: 10 when absent, 100 for the history.
    limit: Option<i32>,
    /// Rows to skip, 0 when absent.
    offset: Option<i32>,
}

impl Paging {
    /// The page size and the rows to skip, with their defaults.
    pub fn or_default(&self) -> (i32, i32) {
        (self.limit.unwrap_or(10), self.offset.unwrap_or(0))
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct PagingResult<T> {
    limit: i32,
    offset: i32,
    data: Vec<T>,
}
