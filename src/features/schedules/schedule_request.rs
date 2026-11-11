use super::{JobSchedule, ScheduleRule};
use crate::handlers::{
    http::{KEY_RULE, TIME_RULE, TZ_RULE, is_key},
    job_request::JobRequest,
};
use crate::models::{JobCreate, ParamError, WorkerOptions};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::Deserialize;

/// When a schedule runs: the body of `PUT /api/v1/schedules/{id}` has these next to the fields of
/// the job of every run ([`JobRequest`]). The id is in the path.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct ScheduleRequest {
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
pub(super) fn prepare_schedule(
    options: &WorkerOptions,
    schedule_id: &str,
    mut job: JobRequest,
    spec: ScheduleRequest,
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
    // the key of the schedule's job is the id of the schedule
    job.key = None;
    let job = job
        .validate(options, now)
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
            serde_json::from_str(json).expect("job"),
            serde_json::from_str(json).expect("spec"),
            Utc::now(),
        )
    }

    fn parameters(errors: &[ParamError]) -> Vec<&str> {
        errors.iter().map(|error| error.parameter).collect()
    }

    const META: &str = r#""sink":"http","method":"POST","url":"https://example.com""#;

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
            r#"{"sink":"http","method":"POST","url":"ftp://example.com",
                "cron":"* * * * *","interval":60,"until":"soon"}"#,
        )
        .expect_err("invalid schedule");
        assert_eq!(parameters(&errors), ["cron", "until", "url"]);
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
