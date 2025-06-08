use serde::{Deserialize, Serialize};

pub mod echo;
pub mod history;
pub mod instances;
pub mod live;
pub mod results;
pub mod schedules;

#[derive(Deserialize)]
pub struct Paging {
    limit: Option<i32>,
    offset: Option<i32>,
}

#[derive(Serialize)]
pub struct PagingResult<T> {
    limit: i32,
    offset: i32,
    data: Vec<T>,
}
