//! Billing usage events for a control plane (ADR 015).
//!
//! A budget flush turns each billable key's positive spend delta into one
//! [`UsageEvent`] in the Lab's `usage.recorded` envelope. This module owns the
//! pure parts: the delta, the event shape, event ids and timestamps. Nothing
//! here does I/O.

use rand::Rng;
use serde::Serialize;

/// The `product` every LUMEN event carries.
pub const PRODUCT: &str = "lumen";
/// The only event type LUMEN reports.
pub const EVENT_TYPE: &str = "usage.recorded";

/// The live billing policy: present only when `[usage_events]` is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingPolicy {
    /// Gateway name copied into every event (`data.source`).
    pub source: String,
}

/// The raw unit counts of one billed window (spec section 4.3). The Lab
/// prices them; LUMEN never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct UsageUnits {
    /// Settled requests.
    pub requests: i64,
    /// Input (prompt) tokens.
    pub tokens_in: i64,
    /// Output (completion) tokens.
    pub tokens_out: i64,
    /// Tokens (in plus out) of the requests whose counts were local
    /// estimates (ADR 003), so the Lab can tell exact from estimated.
    pub tokens_estimated: i64,
}

/// One billable key's spend since its last billed flush, in micro-USD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageDelta {
    /// The LUMEN key id.
    pub key_id: String,
    /// The key's group `account_ref` (the Lab account id).
    pub account_ref: String,
    /// The key's `external_ref` (the Lab key id), if any.
    pub external_ref: Option<String>,
    /// Spend since the watermark; always positive.
    pub cost_micro: i64,
    /// Unit counts since the last billed flush (informational).
    pub units: UsageUnits,
    /// Previous billed flush (or boot), unix ms.
    pub window_start_ms: i64,
    /// This flush, unix ms.
    pub window_end_ms: i64,
    /// The group (lease) id.
    pub group_id: String,
    /// The lease's spend at flush time.
    pub group_spent_micro: i64,
    /// The lease's cap at flush time; `None` = unlimited.
    pub group_budget_max_micro: Option<i64>,
}

/// A `usage.recorded` event in the Lab envelope.
#[derive(Debug, Serialize)]
pub struct UsageEvent {
    /// Idempotency key (UUIDv7), stable across delivery attempts.
    pub id: String,
    /// Always [`EVENT_TYPE`].
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Flush time, RFC 3339 UTC with milliseconds.
    pub occurred_at: String,
    /// The Lab account id.
    pub account_id: String,
    /// The Lab key id, or `null`.
    pub api_key_id: Option<String>,
    /// Always [`PRODUCT`].
    pub product: &'static str,
    /// LUMEN-specific payload.
    pub data: UsageEventData,
}

/// The `data` object of a LUMEN usage event.
#[derive(Debug, Serialize)]
pub struct UsageEventData {
    /// Spend in micro-USD; the Lab converts to credits.
    pub cost_micro_usd: i64,
    /// Settled requests in the window.
    pub requests: i64,
    /// Settled tokens in the window.
    pub tokens: i64,
    /// The billed window.
    pub window: EventWindow,
    /// Gateway name.
    pub source: String,
    /// The LUMEN key id.
    pub key_id: String,
    /// The lease at flush time.
    pub group: GroupSnapshot,
}

/// A billed window, RFC 3339 bounds.
#[derive(Debug, Serialize)]
pub struct EventWindow {
    /// Window start.
    pub start: String,
    /// Window end.
    pub end: String,
}

/// The lease (budget group) at flush time, so the Lab can top up without polling.
#[derive(Debug, Serialize)]
pub struct GroupSnapshot {
    /// Group id.
    pub id: String,
    /// Pool spend, micro-USD.
    pub spent_micro: i64,
    /// Pool cap, micro-USD; `null` = unlimited.
    pub budget_max_micro: Option<i64>,
}

impl UsageEvent {
    /// Build the event for one delta. `id` is minted once by the caller and
    /// stored with the event, so retries reuse it.
    #[must_use]
    pub fn from_delta(id: String, delta: &UsageDelta, source: &str) -> Self {
        Self {
            id,
            kind: EVENT_TYPE,
            occurred_at: rfc3339_ms(delta.window_end_ms),
            account_id: delta.account_ref.clone(),
            api_key_id: delta.external_ref.clone(),
            product: PRODUCT,
            data: UsageEventData {
                cost_micro_usd: delta.cost_micro,
                requests: delta.units.requests,
                tokens: delta.units.tokens_in + delta.units.tokens_out,
                window: EventWindow {
                    start: rfc3339_ms(delta.window_start_ms),
                    end: rfc3339_ms(delta.window_end_ms),
                },
                source: source.to_owned(),
                key_id: delta.key_id.clone(),
                group: GroupSnapshot {
                    id: delta.group_id.clone(),
                    spent_micro: delta.group_spent_micro,
                    budget_max_micro: delta.group_budget_max_micro,
                },
            },
        }
    }
}

/// A random UUIDv7 (RFC 9562) for `unix_ms`, lower-case hyphenated.
#[must_use]
pub fn uuid_v7(unix_ms: i64) -> String {
    use std::fmt::Write;
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let ms = u64::try_from(unix_ms).unwrap_or(0) & 0xFFFF_FFFF_FFFF;
    bytes[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    bytes[6] = (bytes[6] & 0x0F) | 0x70;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    let mut out = String::with_capacity(36);
    for (i, b) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Whether `s` is a hyphenated UUID (any version), as the Lab requires for
/// `account_id`.
#[must_use]
pub fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// `unix_ms` as RFC 3339 UTC with milliseconds (`2001-09-09T01:46:40.000Z`).
/// Civil-from-days after Howard Hinnant; no date crate needed.
#[must_use]
#[allow(clippy::many_single_char_names)]
pub fn rfc3339_ms(unix_ms: i64) -> String {
    let ms = unix_ms.rem_euclid(1000);
    let secs = unix_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (h, m, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{ms:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta() -> UsageDelta {
        UsageDelta {
            key_id: "k1".to_owned(),
            account_ref: "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61".to_owned(),
            external_ref: Some("lab-key-9".to_owned()),
            cost_micro: 1834,
            units: UsageUnits {
                requests: 412,
                tokens_in: 90_000,
                tokens_out: 6_120,
                tokens_estimated: 0,
            },
            window_start_ms: 1_000_000_000_000,
            window_end_ms: 1_000_000_010_000,
            group_id: "g1".to_owned(),
            group_spent_micro: 8_123_400,
            group_budget_max_micro: Some(10_000_000),
        }
    }

    #[test]
    fn uuid_v7_has_the_version_and_variant_bits() {
        // 0x0123_4567_89ab ms: the first 48 bits are the timestamp, big-endian.
        let id = uuid_v7(0x0123_4567_89ab);
        assert!(is_uuid(&id), "{id}");
        assert_eq!(&id[0..8], "01234567");
        assert_eq!(&id[9..13], "89ab");
        assert_eq!(&id[14..15], "7");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"), "{id}");
        assert_ne!(uuid_v7(1), uuid_v7(1), "random tail must differ");
    }

    #[test]
    fn is_uuid_rejects_near_misses() {
        assert!(is_uuid("0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61"));
        assert!(!is_uuid("0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b6"));
        assert!(!is_uuid("0192f3c1_7c2e-7b1a-9f00-3c9d2e4a5b61"));
        assert!(!is_uuid("0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5bzz"));
        assert!(!is_uuid(""));
    }

    #[test]
    fn rfc3339_ms_formats_known_instants() {
        assert_eq!(rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_ms(1_000_000_000_000), "2001-09-09T01:46:40.000Z");
        assert_eq!(rfc3339_ms(951_782_400_007), "2000-02-29T00:00:00.007Z");
    }

    #[test]
    fn event_serializes_to_the_contract_shape() {
        let event = UsageEvent::from_delta(uuid_v7(1), &delta(), "eu-1");
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v["type"], "usage.recorded");
        assert_eq!(v["product"], "lumen");
        assert_eq!(v["account_id"], "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61");
        assert_eq!(v["api_key_id"], "lab-key-9");
        assert_eq!(v["occurred_at"], "2001-09-09T01:46:50.000Z");
        assert_eq!(v["data"]["cost_micro_usd"], 1834);
        assert_eq!(v["data"]["window"]["start"], "2001-09-09T01:46:40.000Z");
        assert_eq!(v["data"]["source"], "eu-1");
        assert_eq!(v["data"]["group"]["budget_max_micro"], 10_000_000);
    }

    #[test]
    fn events_validate_against_the_vendored_contract() {
        let schema: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/lab-events.schema.json"
        )))
        .unwrap();
        let validator = jsonschema::options()
            .should_validate_formats(true)
            .build(&schema)
            .unwrap();
        let mut unlimited = delta();
        unlimited.group_budget_max_micro = None;
        unlimited.external_ref = None;
        for d in [delta(), unlimited] {
            let v = serde_json::to_value(UsageEvent::from_delta(uuid_v7(5), &d, "eu-1")).unwrap();
            let errors: Vec<String> = validator.iter_errors(&v).map(|e| e.to_string()).collect();
            assert!(errors.is_empty(), "{errors:?}");
        }

        // Negative assertions: account_id must be a valid UUID
        let mut invalid_uuid =
            serde_json::to_value(UsageEvent::from_delta(uuid_v7(5), &delta(), "eu-1")).unwrap();
        invalid_uuid["account_id"] = serde_json::json!("not-a-uuid");
        let errors: Vec<String> = validator
            .iter_errors(&invalid_uuid)
            .map(|e| e.to_string())
            .collect();
        assert!(
            !errors.is_empty(),
            "invalid account_id should fail validation"
        );

        // cost_micro_usd must be >= 1
        let mut zero_cost =
            serde_json::to_value(UsageEvent::from_delta(uuid_v7(5), &delta(), "eu-1")).unwrap();
        zero_cost["data"]["cost_micro_usd"] = serde_json::json!(0);
        let errors: Vec<String> = validator
            .iter_errors(&zero_cost)
            .map(|e| e.to_string())
            .collect();
        assert!(
            !errors.is_empty(),
            "zero cost_micro_usd should fail validation"
        );
    }
}
