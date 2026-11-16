//! The description of the API (OpenAPI 3.1) at `GET /api/v1/openapi.json`: the SDKs are generated
//! from it.

use super::{job_request::JobRequest, jobs};
use crate::features::{callbacks::CallbackWebhook, flows, history, instances, results, schedules};
use crate::models::{JobCreateRow, ParamError, Problem};
use axum::{Json, Router, routing::get};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    info(title = "irisqo", description = "Jobs and schedules over HTTP."),
    paths(
        jobs::create,
        jobs::list,
        jobs::get_by_id,
        jobs::delete_by_id,
        jobs::cancel,
        jobs::retry,
        schedules::http::upsert,
        schedules::http::get_all,
        schedules::http::get_by_id,
        schedules::http::pause,
        schedules::http::resume,
        schedules::http::delete,
        flows::http::get_all,
        flows::http::get_undeclared,
        flows::http::get_by_id,
        flows::http::upsert,
        flows::http::pause,
        flows::http::resume,
        flows::http::delete,
        instances::http::get_all,
        history::get_history_by_id,
        results::http::result_by_id,
        results::http::result_by_id_raw,
    ),
    components(schemas(JobRequest, JobCreateRow, ParamError, Problem, CallbackWebhook))
)]
pub struct ApiDoc;

pub fn routes() -> Router {
    Router::new().route("/openapi.json", get(|| async { Json(ApiDoc::openapi()) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_has_the_job_api() {
        let doc = serde_json::to_value(ApiDoc::openapi()).expect("json");
        for path in [
            "/api/v1/jobs",
            "/api/v1/schedules/{id}",
            "/api/v1/flows/{id}",
            "/api/v1/instances",
        ] {
            assert!(doc["paths"].get(path).is_some(), "{path}");
        }
        // an operation id names a client method: every one is its own
        let mut ids: Vec<_> = doc["paths"]
            .as_object()
            .expect("paths")
            .values()
            .flat_map(|path| path.as_object().expect("path").values())
            .map(|operation| operation["operationId"].as_str().expect("operationId"))
            .collect();
        let operations = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), operations, "{ids:?}");
        assert_eq!(operations, 23);
        let schemas = &doc["components"]["schemas"];
        let job = schemas["JobRequest"]["properties"]
            .as_object()
            .expect("properties");
        for field in [
            "sink", "url", "flow_key", "priority", "retry", "at", "callback",
        ] {
            assert!(job.get(field).is_some(), "{field}");
        }
        assert!(job.get("body_raw").is_none(), "an internal field");
        for field in ["url", "mode", "retry", "timeout"] {
            assert!(
                schemas["JobCallback"]["properties"].get(field).is_some(),
                "{field}"
            );
        }
        // the policy is an object tagged by `policy`
        assert!(
            schemas["JobRetry"].to_string().contains("fibonacci"),
            "{}",
            schemas["JobRetry"]
        );
        // `at` is a time or unix seconds; `method` is a list of values
        assert_eq!(
            job["at"]["oneOf"].as_array().map(Vec::len),
            Some(2),
            "{}",
            job["at"]
        );
        assert!(
            job["method"]["enum"]
                .as_array()
                .is_some_and(|v| v.contains(&"GET".into()))
        );
        // optional fields are not required
        assert!(
            !doc["components"]["schemas"]["JobRequest"]
                .to_string()
                .contains("\"required\"")
        );
        // a job repeats with the fields of a schedule
        for field in ["cron", "interval", "tz", "until"] {
            assert!(job.get(field).is_some(), "{field}");
        }
    }
}
