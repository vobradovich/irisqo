use crate::{db, models::AppState};
use axum::{
    extract::{Path, RawQuery, State},
    http::{StatusCode, HeaderMap, header}, response::IntoResponse,
};
use problemdetails::Problem;
use std::sync::Arc;

pub async fn create(
    State(state): State<Arc<AppState>>,
    Path(url): Path<String>,
    query: RawQuery,
    // body: RawBody
) -> Result<impl IntoResponse, Problem> {
    let protocol = "http";
    let full_url = match query.0 {
        Some(qs) => format!("{protocol}://{url}?{qs}"),
        None => format!("{protocol}://{url}"),
    };
    let id = db::jobs::enqueue(&state.pool, protocol, &full_url).await?;
    let mut headers = HeaderMap::new();
    headers.insert(header::LOCATION, format!("/jobs/{}", id).parse().unwrap() );
    headers.insert("job-id", id.into() );
    Ok((StatusCode::CREATED, headers))
}
