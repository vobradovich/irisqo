use serde::{Serialize, Deserialize};

pub mod instances;
pub mod live;
pub mod results;
pub mod schedules;
pub mod history;

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
