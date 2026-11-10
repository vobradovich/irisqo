use super::{JobSchedule, ScheduleRule};
use crate::handlers::{
    http::{KEY_RULE, TIME_RULE, TZ_RULE, is_key, job_headers, parse_body},
    jobs::{JobSpec, prepare_spec},
};
use crate::models::{Error, JobCreate, ParamError, Problem, WorkerOptions};
use crate::{
    features::{Paging, PagingResult},
    models::AppState,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{Map, Value, value::RawValue};
use std::{collections::HashMap, sync::Arc};

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/schedules/{id}",
            get(get_by_id)
                .put(upsert)
                .delete(delete)
                .layer(DefaultBodyLimit::max(state.max_body_size)),
        )
        .route("/schedules/{id}/pause", post(pause))
        .route("/schedules/{id}/resume", post(resume))
        .route("/schedules", get(get_all))
        .with_state(state)
}

/// A schedule in JSON: the job of every run (as for `POST /api/v1/jobs`) and when it runs.
/// Its id is in the path.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleSpec {
    meta: Map<String, Value>,
    headers: Option<HashMap<String, String>>,
    body: Option<Box<RawValue>>,
    /// A cron expression of 5 fields (or 6 with seconds first), or `interval`, not both.
    cron: Option<String>,
    /// Seconds.
    interval: Option<u32>,
    /// The IANA zone of `cron` (`Europe/Berlin`), default UTC.
    tz: Option<String>,
    /// RFC 3339 time (`2026-12-31T23:59:59Z`): the schedule ends.
    until: Option<String>,
}

/// `cron` or `interval`, one of them.
fn parse_rule(cron: Option<&str>, interval: Option<u32>) -> Result<ScheduleRule, ParamError> {
    match (cron, interval) {
        (Some(_), Some(_)) => Err(ParamError::new(
            "cron",
            "cannot be set together with interval",
        )),
        (Some(cron), None) => ScheduleRule::parse_cron(cron),
        (None, Some(0)) => Err(ParamError::new(
            "interval",
            "must be a positive number of seconds",
        )),
        (None, Some(interval)) => Ok(ScheduleRule::Interval(interval)),
        (None, None) => Err(ParamError::new("cron", "cron or interval is required")),
    }
}

/// Validates a schedule with its job: every invalid field is reported, not only the first.
fn prepare_schedule(
    options: &WorkerOptions,
    schedule_id: &str,
    spec: ScheduleSpec,
    now: DateTime<Utc>,
) -> Result<JobCreate, Vec<ParamError>> {
    let mut errors = Vec::new();
    if !is_key(schedule_id) {
        errors.push(ParamError::new("id", KEY_RULE));
    }
    let rule = parse_rule(spec.cron.as_deref(), spec.interval)
        .map_err(|err| errors.push(err))
        .ok();
    let tz = spec
        .tz
        .as_deref()
        .map_or(Ok(Tz::UTC), str::parse::<Tz>)
        .unwrap_or_else(|_| {
            errors.push(ParamError::new("tz", TZ_RULE));
            Tz::UTC
        });
    let until = spec.until.as_deref().and_then(|until| {
        let until = JobSchedule::parse_time(until);
        if until.is_none() {
            errors.push(ParamError::new("until", TIME_RULE));
        }
        until
    });
    let schedule = rule.map(|rule| JobSchedule { rule, tz, until });
    let job = prepare_spec(
        options,
        JobSpec {
            meta: spec.meta,
            headers: spec.headers,
            body: spec.body,
            key: None,
        },
        now,
    )
    .map_err(|found| errors.extend(found))
    .ok();
    let (Some(mut job), Some(schedule), true) = (job, schedule, errors.is_empty()) else {
        return Err(errors);
    };
    // a schedule runs at least once
    if schedule.next(job.at.unwrap_or(now)).is_none() {
        let parameter = if until.is_some() { "until" } else { "cron" };
        return Err(vec![ParamError::new(
            parameter,
            "the schedule has no run before it ends",
        )]);
    }
    job.schedule = Some(schedule);
    // the key of the schedule's job is the id of the schedule
    job.key = Some(schedule_id.to_owned());
    Ok(job)
}

/// Declares a schedule: created with its first run, or changed (it keeps its id, the waiting run
/// is cancelled and replaced by one with the new job and rule; a paused schedule is resumed).
/// `200` with the run that is next (`id`, `schedule_id`, `key`) and the same in the headers.
async fn upsert(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
    body: Bytes,
) -> Result<impl IntoResponse, Problem> {
    let spec: ScheduleSpec = parse_body(&body, "schedule")?;
    let job = prepare_schedule(&state.worker_options, &schedule_id, spec, Utc::now())
        .map_err(Error::InvalidParams)?;
    let run = super::db::upsert(&state.pool, job, &state.instance_id).await?;
    let mut headers = job_headers(&run);
    // the schedule is what was declared
    if let Ok(location) = HeaderValue::from_str(&format!("/api/v1/schedules/{schedule_id}")) {
        headers.insert("location", location);
    }
    Ok((headers, Json(run)))
}

async fn get_all(
    State(state): State<Arc<AppState>>,
    Query(pagination): Query<Paging>,
) -> Result<impl IntoResponse, Problem> {
    let result = super::db::get_all(
        &state.pool,
        pagination.limit.unwrap_or(10),
        pagination.offset.unwrap_or(0),
    )
    .await?;
    Ok(Json(PagingResult {
        limit: pagination.limit.unwrap_or(10),
        offset: pagination.offset.unwrap_or(0),
        data: result,
    }))
}

async fn get_by_id(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
) -> Result<Response, Problem> {
    let schedule = super::db::get_by_id(&state.pool, &schedule_id)
        .await?
        .ok_or(Error::ScheduleNotFound(schedule_id))?;
    Ok(Json(schedule).into_response())
}

/// Pauses the schedule and cancels its pending run (`PUT` is kept for compatibility).
async fn pause(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
) -> Result<Response, Problem> {
    let found = super::db::pause(&state.pool, &schedule_id, &state.instance_id).await?;
    Ok(found_status(found).into_response())
}

fn found_status(found: bool) -> StatusCode {
    if found {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

/// Resumes the schedule: its next run is scheduled when none is pending.
async fn resume(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
) -> Result<Response, Problem> {
    let found = super::db::resume(
        &state.pool,
        &schedule_id,
        chrono::Utc::now(),
        &state.instance_id,
    )
    .await?;
    Ok(found_status(found).into_response())
}

async fn delete(
    State(state): State<Arc<AppState>>,
    Path(schedule_id): Path<String>,
) -> Result<Response, Problem> {
    let rows = super::db::delete(&state.pool, &schedule_id).await?;
    match rows {
        0 => Ok(StatusCode::NOT_FOUND.into_response()),
        _ => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepare(json: &str) -> Result<JobCreate, Vec<ParamError>> {
        prepare_as("nightly", json)
    }

    fn prepare_as(schedule_id: &str, json: &str) -> Result<JobCreate, Vec<ParamError>> {
        prepare_schedule(
            &WorkerOptions::for_tests(),
            schedule_id,
            serde_json::from_str(json).expect("spec"),
            Utc::now(),
        )
    }

    fn parameters(errors: &[ParamError]) -> Vec<&str> {
        errors.iter().map(|error| error.parameter).collect()
    }

    const META: &str = r#""meta":{"sink":"http","method":"POST","url":"https://example.com"}"#;

    #[test]
    fn a_schedule_comes_with_the_job_of_its_runs() {
        let job = prepare(&format!(
            r#"{{{META},"cron":"*/5 * * * *","until":"2100-01-01T00:00:00Z"}}"#
        ))
        .expect("valid schedule");
        assert!(matches!(
            job.schedule,
            Some(JobSchedule {
                rule: ScheduleRule::Cron(_),
                ..
            })
        ));
        assert_eq!(
            job.schedule.as_ref().and_then(|s| s.until),
            JobSchedule::from_unix_secs(4_102_444_800)
        );
        assert_eq!(
            JobSchedule::parse_time("2100-01-01T00:00:00Z"),
            JobSchedule::parse_time("4102444800"),
            "the same time as RFC 3339 and as unix seconds"
        );
        assert_eq!(job.key.as_deref(), Some("nightly"));

        let job = prepare(&format!(r#"{{{META},"interval":300}}"#)).expect("valid schedule");
        assert_eq!(
            job.schedule,
            Some(JobSchedule {
                rule: ScheduleRule::Interval(300),
                tz: Tz::UTC,
                until: None
            })
        );
        assert_eq!(
            job.key.as_deref(),
            Some("nightly"),
            "the id of the schedule"
        );
    }

    #[test]
    fn every_invalid_field_is_reported() {
        let errors = prepare(
            r#"{"meta":{"sink":"http","method":"POST","url":"ftp://example.com"},
                "cron":"* * * * *","interval":60,"until":"soon"}"#,
        )
        .expect_err("invalid schedule");
        assert_eq!(parameters(&errors), ["cron", "until", "meta.sink"]);
    }

    #[test]
    fn a_schedule_is_cron_or_interval() {
        for (schedule, parameter) in [
            ("", "cron"),
            (r#","cron":"nope""#, "cron"),
            (r#","cron":"300""#, "cron"),
            (r#","interval":0"#, "interval"),
        ] {
            let errors = prepare(&format!("{{{META}{schedule}}}")).expect_err("invalid schedule");
            assert_eq!(parameters(&errors), [parameter], "{schedule}");
        }
    }

    #[test]
    fn a_cron_schedule_has_a_time_zone() {
        let job = prepare(&format!(
            r#"{{{META},"cron":"0 9 * * *","tz":"Europe/Berlin"}}"#
        ))
        .expect("valid schedule");
        assert_eq!(job.schedule.map(|s| s.tz), Some(chrono_tz::Europe::Berlin));
        let job = prepare(&format!(r#"{{{META},"cron":"0 9 * * *"}}"#)).expect("valid schedule");
        assert_eq!(job.schedule.map(|s| s.tz), Some(Tz::UTC));

        let errors = prepare(&format!(
            r#"{{{META},"cron":"0 9 * * *","tz":"Mars/Base"}}"#
        ))
        .expect_err("no such zone");
        assert_eq!(parameters(&errors), ["tz"]);
        assert!(errors[0].detail.contains("an IANA"));
    }

    #[test]
    fn the_id_is_a_key() {
        let errors =
            prepare_as("a\u{1}b", &format!(r#"{{{META},"interval":60}}"#)).expect_err("invalid id");
        assert_eq!(parameters(&errors), ["id"]);
    }

    #[test]
    fn a_schedule_runs_at_least_once() {
        let errors = prepare(&format!(
            r#"{{{META},"interval":3600,"until":"1970-01-01T00:00:01Z"}}"#
        ))
        .expect_err("no run before until");
        assert_eq!(parameters(&errors), ["until"]);
    }
}
