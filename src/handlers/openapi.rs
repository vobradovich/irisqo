//! The description of the API (OpenAPI 3.1) at `GET /api/v1/openapi.json`: the SDKs are generated
//! from it.

use super::{job_request::JobRequest, jobs};
use crate::features::{
    flows, history, instances, results,
    schedules::{self, ScheduleRequest},
};
use crate::models::{JobCreateRow, ParamError, Problem};
use axum::{Json, Router, routing::get};
use utoipa::{OpenApi, ToSchema};

/// The body of `PUT /api/v1/schedules/{id}`: the job of every run and when it runs.
#[derive(ToSchema)]
#[allow(dead_code)]
pub(crate) struct ScheduleBody {
    #[serde(flatten)]
    job: JobRequest,
    #[serde(flatten)]
    schedule: ScheduleRequest,
}

#[derive(OpenApi)]
#[openapi(
    info(title = "irisqo", description = "Jobs and schedules over HTTP."),
    paths(
        jobs::create,
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
    components(schemas(JobRequest, ScheduleBody, JobCreateRow, ParamError, Problem))
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
        assert_eq!(operations, 22);
        // the retry policy is flattened into the job: `allOf` of its fields and the others
        let job = &doc["components"]["schemas"]["JobRequest"]["allOf"];
        let job: serde_json::Map<String, serde_json::Value> = job
            .as_array()
            .expect("allOf")
            .iter()
            .filter_map(|part| part["properties"].as_object())
            .flatten()
            .map(|(name, schema)| (name.clone(), schema.clone()))
            .collect();
        for field in [
            "sink",
            "url",
            "flow_key",
            "priority",
            "retry",
            "retry_count",
            "retry_delay",
            "at",
        ] {
            assert!(job.get(field).is_some(), "{field}");
        }
        assert!(job.get("body_raw").is_none(), "an internal field");
        assert_eq!(job["retry_count"]["maximum"], 65535);
        assert_eq!(job["retry_count"]["minimum"], 0);
        // `at` is a time or unix seconds; `method` and `retry` are lists of values
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
        assert!(
            job["retry"]["enum"]
                .as_array()
                .is_some_and(|v| v.contains(&"fibonacci".into()))
        );
        // optional fields are not required
        assert!(
            !doc["components"]["schemas"]["JobRequest"]
                .to_string()
                .contains("\"required\"")
        );
        // the body of a schedule is the job and the rule together
        let body = &doc["components"]["schemas"]["ScheduleBody"]["allOf"];
        assert_eq!(body.as_array().map(Vec::len), Some(2), "{body}");
        let rule = &doc["components"]["schemas"]["ScheduleRequest"]["properties"];
        for field in ["cron", "interval", "tz", "until"] {
            assert!(rule.get(field).is_some(), "{field}");
        }
    }
}
