//! Offline diagnostics for native Codex quota recovery prerequisites.
//!
//! Schema fields and saved responses are observations, never authorization.
//! This module has no transport, state writer, recovery action or enable path.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const REVIEWED_CODEX_VERSION: &str = "0.153.4";
const REVIEWED_SOURCE_REVISION: &str = "3d2ee51ca2d5db578f328aa75e20aa22c0197c9a";

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Blocker {
    ConditionalFailedTurnContinuation,
    DurableSubmissionReplay,
    SessionOwnershipAndUserIntent,
    FreshSameAccountLimits,
}

#[derive(Debug, Serialize)]
struct Report {
    recovery_enabled: bool,
    reviewed_codex_version: &'static str,
    reviewed_source_revision: &'static str,
    blockers: Vec<Blocker>,
    schema: SchemaEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn: Option<TurnEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread: Option<ThreadEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rate_limits: Option<RateLimitEvidence>,
}

#[derive(Debug, Serialize)]
struct SchemaEvidence {
    turn_start_fields: Vec<String>,
    turn_steer_required_fields: Vec<String>,
    queue_start_fields: Vec<String>,
    account_id_field_present: bool,
    usage_limit_error_variant_present: bool,
    active_hold_variants: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SchemaDocument {
    title: String,
    #[serde(rename = "type")]
    kind: String,
    properties: BTreeMap<String, Value>,
    #[serde(default)]
    required: Vec<String>,
    #[serde(default)]
    definitions: BTreeMap<String, Value>,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("Cannot read {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("Invalid JSON data in {}", path.display()))
}

fn read_schema(dir: &Path, name: &str) -> Result<SchemaDocument> {
    let path = dir.join("v2").join(format!("{name}.json"));
    let schema: SchemaDocument = read_json(&path)?;
    if schema.title != name
        || schema.kind != "object"
        || schema
            .required
            .iter()
            .any(|field| !schema.properties.contains_key(field))
    {
        bail!("Unexpected native schema shape in {}", path.display());
    }
    Ok(schema)
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

impl SchemaEvidence {
    fn read(dir: &Path) -> Result<Self> {
        let start = read_schema(dir, "TurnStartParams")?;
        let steer = read_schema(dir, "TurnSteerParams")?;
        let queue = read_schema(dir, "ThreadQueueStartParams")?;
        let limits = read_schema(dir, "GetAccountRateLimitsResponse")?;
        let turn = read_schema(dir, "TurnCompletedNotification")?;
        let thread = read_schema(dir, "ThreadReadResponse")?;
        let codes = string_array(
            turn.definitions
                .get("CodexErrorInfo")
                .and_then(|value| value.pointer("/oneOf/0/enum")),
        );
        Ok(Self {
            turn_start_fields: start.properties.into_keys().collect(),
            turn_steer_required_fields: steer.required,
            queue_start_fields: queue.properties.into_keys().collect(),
            account_id_field_present: limits.properties.contains_key("accountId"),
            usage_limit_error_variant_present: codes
                .iter()
                .any(|code| code == "usageLimitExceeded"),
            active_hold_variants: string_array(
                thread
                    .definitions
                    .get("ThreadActiveFlag")
                    .and_then(|value| value.get("enum")),
            ),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Turn {
    id: String,
    status: String,
    #[serde(default)]
    error: Option<TurnError>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnError {
    #[serde(default)]
    codex_error_info: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct TurnNotification {
    method: String,
    params: TurnCompleted,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnCompleted {
    thread_id: String,
    turn: Turn,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TurnOutcome {
    QuotaFailed,
    OtherFailure,
    Interrupted,
    Completed,
    Unknown,
}

#[derive(Debug, Serialize)]
struct TurnEvidence {
    thread_id: String,
    turn_id: String,
    outcome: TurnOutcome,
}

fn nonempty(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("Native {field} is empty");
    }
    Ok(())
}

impl TurnEvidence {
    fn read(path: &Path) -> Result<Self> {
        let event: TurnNotification = read_json(path)?;
        if event.method != "turn/completed" {
            bail!("Expected a native turn/completed notification");
        }
        nonempty(&event.params.thread_id, "threadId")?;
        nonempty(&event.params.turn.id, "turn.id")?;
        let turn = event.params.turn;
        let outcome = match turn.status.as_str() {
            "failed"
                if turn
                    .error
                    .as_ref()
                    .and_then(|e| e.codex_error_info.as_ref())
                    .and_then(Value::as_str)
                    == Some("usageLimitExceeded") =>
            {
                TurnOutcome::QuotaFailed
            }
            "failed" => TurnOutcome::OtherFailure,
            "interrupted" => TurnOutcome::Interrupted,
            "completed" => TurnOutcome::Completed,
            _ => TurnOutcome::Unknown,
        };
        Ok(Self {
            thread_id: event.params.thread_id,
            turn_id: turn.id,
            outcome,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ThreadResponse {
    thread: NativeThread,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeThread {
    id: String,
    session_id: String,
    #[serde(default)]
    can_accept_direct_input: Option<bool>,
    status: NativeThreadStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeThreadStatus {
    #[serde(rename = "type")]
    kind: String,
    // None differs from a known empty activeFlags array.
    active_flags: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct ThreadEvidence {
    thread_id: String,
    session_id: String,
    status: String,
    active_flags: Option<Vec<String>>,
    can_accept_direct_input: Option<bool>,
    // No idle snapshot proves the absence of a durable user pause/cancel.
    pause_cancel_intent_verified: bool,
}

impl ThreadEvidence {
    fn read(path: &Path) -> Result<Self> {
        let response: ThreadResponse = read_json(path)?;
        let thread = response.thread;
        nonempty(&thread.id, "thread.id")?;
        nonempty(&thread.session_id, "thread.sessionId")?;
        Ok(Self {
            thread_id: thread.id,
            session_id: thread.session_id,
            status: thread.status.kind,
            active_flags: thread.status.active_flags,
            can_accept_direct_input: thread.can_accept_direct_input,
            pause_cancel_intent_verified: false,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitsResponse {
    account_id: Option<String>,
    rate_limits: NativeLimit,
    rate_limits_by_limit_id: Option<BTreeMap<String, NativeLimit>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeLimit {
    limit_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct RateLimitEvidence {
    account_id: Option<String>,
    reported_limit_ids: Vec<String>,
    freshness_verified: bool,
}

impl RateLimitEvidence {
    fn read(path: &Path) -> Result<Self> {
        let response: RateLimitsResponse = read_json(path)?;
        if let Some(account_id) = &response.account_id {
            nonempty(account_id, "accountId")?;
        }
        let mut ids: Vec<_> = response
            .rate_limits_by_limit_id
            .unwrap_or_default()
            .into_keys()
            .collect();
        if let Some(id) = response.rate_limits.limit_id {
            ids.push(id);
        }
        ids.sort();
        ids.dedup();
        Ok(Self {
            account_id: response.account_id,
            reported_limit_ids: ids,
            freshness_verified: false,
        })
    }
}

fn inspect(
    schema_dir: &Path,
    turn_event: Option<&Path>,
    thread_response: Option<&Path>,
    rate_limits_response: Option<&Path>,
) -> Result<Report> {
    Ok(Report {
        // Deliberately unconditional: field presence cannot establish semantics,
        // authenticated ownership, freshness or a conditional mutation contract.
        recovery_enabled: false,
        reviewed_codex_version: REVIEWED_CODEX_VERSION,
        reviewed_source_revision: REVIEWED_SOURCE_REVISION,
        blockers: vec![
            Blocker::ConditionalFailedTurnContinuation,
            Blocker::DurableSubmissionReplay,
            Blocker::SessionOwnershipAndUserIntent,
            Blocker::FreshSameAccountLimits,
        ],
        schema: SchemaEvidence::read(schema_dir)?,
        turn: turn_event.map(TurnEvidence::read).transpose()?,
        thread: thread_response.map(ThreadEvidence::read).transpose()?,
        rate_limits: rate_limits_response
            .map(RateLimitEvidence::read)
            .transpose()?,
    })
}

/// Print offline native-contract evidence without enabling or performing recovery.
///
/// Reads generated schemas and optional saved native payloads. It never starts
/// Codex, connects to a server, sends input or changes workmux/authorization state.
/// Invalid or missing files return an error before any report is printed.
pub fn run(
    schema_dir: &Path,
    turn_event: Option<&Path>,
    thread_response: Option<&Path>,
    rate_limits_response: Option<&Path>,
) -> Result<()> {
    let report = inspect(
        schema_dir,
        turn_event,
        thread_response,
        rate_limits_response,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schemas() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("v2")).unwrap();
        let fixture: Value = serde_json::from_str(include_str!(
            "../../resources/codex/quota-recovery/schema-projection.json"
        ))
        .unwrap();
        for (name, schema) in fixture["schemas"].as_object().unwrap() {
            fs::write(
                dir.path().join("v2").join(format!("{name}.json")),
                serde_json::to_vec(schema).unwrap(),
            )
            .unwrap();
        }
        dir
    }

    fn event(status: &str, code: Value, message: &str) -> Value {
        json!({"method":"turn/completed", "params": {"threadId":"thread-a", "turn": {
            "id":"turn-a", "status":status, "items":[],
            "error":{"codexErrorInfo":code,"message":message}
        }}})
    }

    #[test]
    fn only_native_failed_usage_limit_is_quota_not_text_or_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("event.json");
        for (status, code, expected) in [
            (
                "failed",
                json!("usageLimitExceeded"),
                TurnOutcome::QuotaFailed,
            ),
            (
                "failed",
                json!("rateLimitExceeded"),
                TurnOutcome::OtherFailure,
            ),
            (
                "failed",
                json!("serverOverloaded"),
                TurnOutcome::OtherFailure,
            ),
            (
                "failed",
                json!("sessionBudgetExceeded"),
                TurnOutcome::OtherFailure,
            ),
            (
                "failed",
                json!({"httpConnectionFailed":{"httpStatusCode":429}}),
                TurnOutcome::OtherFailure,
            ),
            ("failed", Value::Null, TurnOutcome::OtherFailure),
            (
                "interrupted",
                json!("usageLimitExceeded"),
                TurnOutcome::Interrupted,
            ),
            (
                "completed",
                json!("usageLimitExceeded"),
                TurnOutcome::Completed,
            ),
            (
                "future-status",
                json!("usageLimitExceeded"),
                TurnOutcome::Unknown,
            ),
        ] {
            fs::write(
                &path,
                serde_json::to_vec(&event(
                    status,
                    code,
                    "Quoted usageLimitExceeded: You've hit your usage limit.",
                ))
                .unwrap(),
            )
            .unwrap();
            let observed = TurnEvidence::read(&path).unwrap();
            assert_eq!(observed.outcome, expected);
            assert_eq!(observed.thread_id, "thread-a");
            assert_eq!(observed.turn_id, "turn-a");
        }
    }

    #[test]
    fn missing_identity_and_nonterminal_notifications_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("event.json");
        let mut value = event("failed", json!("usageLimitExceeded"), "limit reached");
        value["method"] = json!("error");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(TurnEvidence::read(&path).is_err());
        value["method"] = json!("turn/completed");
        value["params"]["threadId"] = json!("");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(TurnEvidence::read(&path).is_err());
    }

    #[test]
    fn native_fields_are_reported_without_inventing_a_recovery_contract() {
        let dir = schemas();
        let report = inspect(dir.path(), None, None, None).unwrap();
        assert!(!report.recovery_enabled);
        assert!(
            !report
                .schema
                .turn_start_fields
                .contains(&"expectedTurnId".into())
        );
        assert!(
            report
                .schema
                .turn_steer_required_fields
                .contains(&"expectedTurnId".into())
        );
        assert!(report.schema.account_id_field_present);
        assert!(report.schema.usage_limit_error_variant_present);
        assert!(
            report
                .schema
                .active_hold_variants
                .contains(&"waitingOnUserInput".into())
        );
        // Familiar field names in a future or altered schema cannot prove
        // conditional mutation semantics or silently enable this feature.
        let path = dir.path().join("v2/TurnStartParams.json");
        let mut schema: Value = read_json(&path).unwrap();
        for field in ["expectedTurnId", "idempotencyKey", "intentRevision"] {
            schema["properties"][field] = json!({"type":"string"});
        }
        fs::write(&path, serde_json::to_vec(&schema).unwrap()).unwrap();
        let future = inspect(dir.path(), None, None, None).unwrap();
        assert!(
            future
                .schema
                .turn_start_fields
                .contains(&"expectedTurnId".into())
        );
        assert!(!future.recovery_enabled);
        assert_eq!(
            serde_json::to_value(&report.blockers).unwrap(),
            serde_json::to_value(&future.blockers).unwrap()
        );
    }

    #[test]
    fn holds_unknown_account_and_saved_headroom_never_authorize_resume() {
        let dir = schemas();
        let thread = dir.path().join("thread.json");
        let rates = dir.path().join("rates.json");
        let turn = dir.path().join("turn.json");
        fs::write(
            &turn,
            serde_json::to_vec(&event("failed", json!("usageLimitExceeded"), "quota")).unwrap(),
        )
        .unwrap();
        for state in [
            json!({"type":"active","activeFlags":["waitingOnUserInput"]}),
            json!({"type":"active","activeFlags":["waitingOnApproval"]}),
            json!({"type":"active","activeFlags":["futureHold"]}),
            json!({"type":"idle"}),
            json!({"type":"notLoaded"}),
        ] {
            fs::write(
                &thread,
                serde_json::to_vec(&json!({"thread":{
                    "id":"thread-a", "sessionId":"session-a", "status":state,
                    "canAcceptDirectInput":true
                }}))
                .unwrap(),
            )
            .unwrap();
            for account in [
                Value::Null,
                json!("account-a"),
                json!("a-different-account"),
            ] {
                fs::write(
                    &rates,
                    serde_json::to_vec(&json!({"accountId":account, "rateLimits":{
                        "limitId":"codex", "primary":{"usedPercent":0,"resetsAt":0},
                        "secondary":{"usedPercent":0,"resetsAt":0}, "spendControlReached":false
                    }}))
                    .unwrap(),
                )
                .unwrap();
                let report = inspect(dir.path(), Some(&turn), Some(&thread), Some(&rates)).unwrap();
                assert!(!report.recovery_enabled);
                let evidence = report.thread.unwrap();
                assert_eq!(evidence.session_id, "session-a");
                assert_eq!(
                    serde_json::to_value(evidence.active_flags).unwrap(),
                    state.get("activeFlags").cloned().unwrap_or(Value::Null)
                );
                assert!(!evidence.pause_cancel_intent_verified);
                assert!(!report.rate_limits.unwrap().freshness_verified);
            }
        }
    }

    #[test]
    fn malformed_or_partial_schema_is_an_error_not_a_clean_check() {
        let dir = schemas();
        let path = dir.path().join("v2/TurnStartParams.json");
        fs::write(&path, "{}").unwrap();
        assert!(inspect(dir.path(), None, None, None).is_err());
        fs::remove_file(path).unwrap();
        assert!(inspect(dir.path(), None, None, None).is_err());
    }
}
