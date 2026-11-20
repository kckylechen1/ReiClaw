use crate::cron::{CronJob, CronJobPatch, Schedule, deserialize_maybe_stringified};
use reiclaw_config::schema::Config;
use serde_json::{Value, json};

pub(crate) const CRON_TZ_DESCRIPTION: &str = "Optional explicit IANA timezone name, e.g. 'America/New_York'. If omitted, the schedule uses the runtime local timezone. For user-facing schedules, pass an explicit IANA timezone.";

pub(crate) const AT_DESCRIPTION: &str = "RFC3339 timestamp with explicit Z or offset, e.g. '2025-12-31T23:59:00Z' or '2025-12-31T18:59:00-05:00'.";

/// JSON Schema for the `delivery` object shared by `cron_add` and
/// `cron_update`. Announce delivery only reaches a configured
/// `[gateway.bridges.<name>]` (in-process channels are retired), so the
/// channel enum lists exactly those bridges. With none configured the
/// schema offers only `mode = "none"`, which still lets the model clear an
/// existing delivery; the run result lands in the job's session and run
/// history either way.
pub(crate) fn delivery_schema(config: &Config) -> Value {
    let mut bridges: Vec<&String> = config.gateway.bridges.keys().collect();
    bridges.sort();
    if bridges.is_empty() {
        return json!({
            "type": "object",
            "description": "No gateway bridges are configured, so job output cannot be announced to a channel; it stays in the job's session and run history. Set mode 'none' to clear an existing delivery.",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["none"],
                    "description": "'none' disables delivery"
                }
            }
        });
    }
    json!({
        "type": "object",
        "description": "Optional delivery config to also send job output to a configured gateway bridge after each run; the result always lands in the job's session and run history. With mode 'announce', channel and to are expected.",
        "properties": {
            "mode": {
                "type": "string",
                "enum": ["none", "announce"],
                "description": "'announce' sends output to the bridge named in channel; 'none' disables delivery"
            },
            "channel": {
                "type": "string",
                "enum": bridges,
                "description": "Configured gateway bridge to deliver output to"
            },
            "to": {
                "type": "string",
                "description": "Recipient on the bridge, e.g. a chat or user ID"
            },
            "thread_id": {
                "type": "string",
                "description": "Optional thread/conversation identifier, for bridges that route a reply into a thread distinct from `to`."
            },
            "best_effort": {
                "type": "boolean",
                "description": "If true, a delivery failure does not fail the job itself. Defaults to true."
            }
        }
    })
}

pub(crate) fn deserialize_schedule_arg(value: &Value) -> Result<Schedule, String> {
    reject_at_without_explicit_offset(value)?;
    deserialize_maybe_stringified::<Schedule>(value)
        .map_err(|err| format!("Invalid schedule: {err}"))
}

pub(crate) fn deserialize_patch_arg(value: &Value) -> Result<CronJobPatch, String> {
    if let Some(normalized) = normalize_maybe_stringified_json(value)
        && let Some(schedule) = normalized.get("schedule")
    {
        reject_at_without_explicit_offset(schedule)
            .map_err(|err| err.replacen("Invalid schedule", "Invalid patch payload", 1))?;
    }

    deserialize_maybe_stringified::<CronJobPatch>(value)
        .map_err(|err| format!("Invalid patch payload: {err}"))
}

pub(crate) fn cron_add_output(job: &CronJob) -> Value {
    let fields = timezone_confirmation_fields(job);
    json!({
        "id": job.id,
        "name": job.name,
        "job_type": job.job_type,
        "schedule": job.schedule,
        "next_run": reiclaw_api::time::format_rfc3339_z(job.next_run),
        "next_run_utc": reiclaw_api::time::format_rfc3339_z(job.next_run),
        "schedule_timezone": fields.schedule_timezone,
        "timezone_source": fields.timezone_source,
        "next_run_local": fields.next_run_local,
        "enabled": job.enabled,
        "allowed_tools": job.allowed_tools
    })
}

pub(crate) fn cron_job_output(job: &CronJob) -> serde_json::Result<Value> {
    let mut output = serde_json::to_value(job)?;
    if let Value::Object(ref mut object) = output {
        let fields = timezone_confirmation_fields(job);
        object.insert(
            "next_run_utc".to_string(),
            json!(reiclaw_api::time::format_rfc3339_z(job.next_run)),
        );
        object.insert("schedule_timezone".to_string(), fields.schedule_timezone);
        object.insert("timezone_source".to_string(), fields.timezone_source);
        object.insert("next_run_local".to_string(), fields.next_run_local);
    }
    Ok(output)
}

struct TimezoneConfirmationFields {
    schedule_timezone: Value,
    timezone_source: Value,
    next_run_local: Value,
}

fn timezone_confirmation_fields(job: &CronJob) -> TimezoneConfirmationFields {
    match &job.schedule {
        Schedule::Cron { tz: Some(tz), .. } => {
            let next_run_local = jiff::tz::TimeZone::get(tz).map_or(Value::Null, |timezone| {
                json!(reiclaw_api::time::format_rfc3339_zoned(
                    &job.next_run.to_zoned(timezone)
                ))
            });
            TimezoneConfirmationFields {
                schedule_timezone: json!(tz),
                timezone_source: json!("explicit"),
                next_run_local,
            }
        }
        Schedule::Cron { tz: None, .. } => TimezoneConfirmationFields {
            schedule_timezone: json!("runtime local timezone"),
            timezone_source: json!("runtime_local"),
            next_run_local: json!(reiclaw_api::time::format_rfc3339_zoned(
                &job.next_run.to_zoned(jiff::tz::TimeZone::system())
            )),
        },
        Schedule::At { .. } | Schedule::Every { .. } => TimezoneConfirmationFields {
            schedule_timezone: Value::Null,
            timezone_source: json!("not_applicable"),
            next_run_local: Value::Null,
        },
    }
}

fn reject_at_without_explicit_offset(value: &Value) -> Result<(), String> {
    let Some(normalized) = normalize_maybe_stringified_json(value) else {
        return Ok(());
    };

    if normalized.get("kind").and_then(Value::as_str) != Some("at") {
        return Ok(());
    }

    let Some(raw_at) = normalized.get("at").and_then(Value::as_str) else {
        return Ok(());
    };

    reiclaw_api::time::parse_rfc3339(raw_at)
        .map(|_| ())
        .map_err(|err| {
            format!(
                "Invalid schedule: 'at' must be an RFC3339 timestamp with explicit Z or offset, \
                 e.g. 2026-05-18T09:00:00Z or 2026-05-18T09:00:00-04:00; got '{raw_at}': {err}"
            )
        })
}

fn normalize_maybe_stringified_json(value: &Value) -> Option<Value> {
    match value {
        Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                serde_json::from_str(trimmed).ok()
            } else {
                None
            }
        }
        other => Some(other.clone()),
    }
}
