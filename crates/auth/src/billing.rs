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
/// The operation LUMEN reports: one event aggregates every capability of a
/// key over the billed window (spec section 4.3).
pub const OPERATION: &str = "gateway";

/// The live billing policy: present only when `[usage_events]` is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingPolicy {
    /// Gateway label for logs; not sent in events (the Lab identifies the
    /// instance from the `X-Lab-Instance-Id` header).
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

/// The `data` object of a LUMEN `usage.recorded` event (spec section 4.2).
/// Exactly the four properties the Lab-owned schema allows: `usageData`
/// forbids additional properties, so the key id and the window travel in
/// `description`.
#[derive(Debug, Serialize)]
pub struct UsageEventData {
    /// Always [`OPERATION`].
    pub operation: &'static str,
    /// Raw unit counts; the Lab prices them.
    pub units: UsageUnits,
    /// Settled cost delta in micro-USD, passed through and marked up by the Lab.
    pub provider_cost_micro_usd: i64,
    /// `key <lumen key id> <window start>..<window end>`.
    pub description: String,
}

impl UsageEvent {
    /// Build the event for one delta. `id` is minted once by the caller and
    /// stored with the event, so retries reuse it.
    #[must_use]
    pub fn from_delta(id: String, delta: &UsageDelta) -> Self {
        Self {
            id,
            kind: EVENT_TYPE,
            occurred_at: rfc3339_ms(delta.window_end_ms),
            account_id: delta.account_ref.clone(),
            api_key_id: delta.external_ref.clone(),
            product: PRODUCT,
            data: UsageEventData {
                operation: OPERATION,
                units: delta.units,
                provider_cost_micro_usd: delta.cost_micro,
                description: format!(
                    "key {} {}..{}",
                    delta.key_id,
                    rfc3339_ms(delta.window_start_ms),
                    rfc3339_ms(delta.window_end_ms)
                ),
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

/// The stored form of a group's `account_ref`: a UUID (a Lab account id) is
/// lowercased, so the exact matches of account-scoped admin calls (whose
/// `X-Lumen-Account-Ref` is read as lowercase) and of the usage filters
/// always find it; any other operator ref is kept exactly as written.
#[must_use]
pub fn canonical_account_ref(value: String) -> String {
    if is_uuid(&value) {
        value.to_ascii_lowercase()
    } else {
        value
    }
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
    fn a_uuid_account_ref_is_canonical_lowercase() {
        assert_eq!(
            canonical_account_ref("0192F3C1-7C2E-7B1A-9F00-3C9D2E4A5B61".to_owned()),
            "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61"
        );
        assert_eq!(canonical_account_ref("Team-A".to_owned()), "Team-A");
    }

    #[test]
    fn event_serializes_to_the_contract_shape() {
        let event = UsageEvent::from_delta(uuid_v7(1), &delta());
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v["type"], "usage.recorded");
        assert_eq!(v["product"], "lumen");
        assert_eq!(v["account_id"], "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61");
        assert_eq!(v["api_key_id"], "lab-key-9");
        assert_eq!(v["occurred_at"], "2001-09-09T01:46:50.000Z");
        assert_eq!(v["data"]["operation"], "gateway");
        assert_eq!(v["data"]["provider_cost_micro_usd"], 1834);
        assert_eq!(v["data"]["units"]["requests"], 412);
        assert_eq!(v["data"]["units"]["tokens_in"], 90_000);
        assert_eq!(v["data"]["units"]["tokens_out"], 6_120);
        assert_eq!(v["data"]["units"]["tokens_estimated"], 0);
        assert_eq!(
            v["data"]["description"],
            "key k1 2001-09-09T01:46:40.000Z..2001-09-09T01:46:50.000Z"
        );
        let data = v["data"].as_object().unwrap();
        assert_eq!(data.len(), 4, "nothing the schema forbids: {data:?}");
    }

    #[test]
    fn events_validate_against_the_vendored_contract() {
        let schema: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/vendor/lab/lab-events.schema.json"
        )))
        .unwrap();
        let validator = jsonschema::options()
            .should_validate_formats(true)
            .build(&schema)
            .unwrap();
        let mut keyless = delta();
        keyless.external_ref = None;
        for d in [delta(), keyless] {
            let v = serde_json::to_value(UsageEvent::from_delta(uuid_v7(5), &d)).unwrap();
            let errors: Vec<String> = validator.iter_errors(&v).map(|e| e.to_string()).collect();
            assert!(errors.is_empty(), "{errors:?}");
        }

        // Negative assertions, so the schema is really enforced.
        let valid = serde_json::to_value(UsageEvent::from_delta(uuid_v7(5), &delta())).unwrap();
        let mut invalid_uuid = valid.clone();
        invalid_uuid["account_id"] = serde_json::json!("not-a-uuid");
        assert!(validator.iter_errors(&invalid_uuid).next().is_some());
        let mut negative_cost = valid.clone();
        negative_cost["data"]["provider_cost_micro_usd"] = serde_json::json!(-1);
        assert!(validator.iter_errors(&negative_cost).next().is_some());
        let mut negative_unit = valid.clone();
        negative_unit["data"]["units"]["requests"] = serde_json::json!(-1);
        assert!(validator.iter_errors(&negative_unit).next().is_some());
        let mut extra = valid;
        extra["data"]["source"] = serde_json::json!("eu-1");
        assert!(
            validator.iter_errors(&extra).next().is_some(),
            "usageData forbids additional properties"
        );
    }
}
