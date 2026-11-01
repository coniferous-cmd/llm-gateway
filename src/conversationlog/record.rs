//! Versioned conversation record and Go-compatible timestamp formatting.

use serde::{Serialize, Serializer};
use time::{OffsetDateTime, UtcOffset};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Body {
    pub data: String,
    pub encoding: &'static str,
    pub observed_bytes: i64,
    pub captured_bytes: usize,
    pub truncated: bool,
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub schema_version: u32,
    pub request_id: String,
    #[serde(serialize_with = "serialize_time")]
    pub started_at: OffsetDateTime,
    #[serde(serialize_with = "serialize_time")]
    pub ended_at: OffsetDateTime,
    pub duration_ms: i64,
    pub method: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub response_complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub request: Body,
    pub response: Body,
}

fn serialize_time<S: Serializer>(value: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format_go_time(*value))
}

/// Format like Go's `time.Time` JSON encoding: RFC3339 with UTC `Z` and the
/// minimal number of fractional digits (none when the fraction is zero).
pub fn format_go_time(value: OffsetDateTime) -> String {
    let value = value.to_offset(UtcOffset::UTC);
    let mut out = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        value.year(),
        value.month() as u8,
        value.day(),
        value.hour(),
        value.minute(),
        value.second()
    );
    let nanos = value.nanosecond();
    if nanos > 0 {
        let fraction = format!("{nanos:09}");
        out.push('.');
        out.push_str(fraction.trim_end_matches('0'));
    }
    out.push('Z');
    out
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use time::macros::datetime;

    use super::*;

    #[test]
    fn go_time_format_matches_rfc3339nano() {
        assert_eq!(
            format_go_time(datetime!(2026-10-26 14:30:25 UTC)),
            "2026-10-26T14:30:25Z"
        );
        assert_eq!(
            format_go_time(datetime!(2026-10-26 14:30:25.150 UTC)),
            "2026-10-26T14:30:25.15Z"
        );
        assert_eq!(
            format_go_time(datetime!(2026-10-26 14:30:25.001 UTC)),
            "2026-10-26T14:30:25.001Z"
        );
        assert_eq!(
            format_go_time(datetime!(2026-10-26 14:30:25.123456789 UTC)),
            "2026-10-26T14:30:25.123456789Z"
        );
        assert_eq!(
            format_go_time(datetime!(2026-10-26 16:30:25 +02:00)),
            "2026-10-26T14:30:25Z"
        );
    }

    #[test]
    fn record_serialization_uses_expected_field_names_and_omits_empty() {
        let body = Body {
            data: "Hello".into(),
            encoding: "utf-8",
            observed_bytes: 5,
            captured_bytes: 5,
            truncated: false,
            complete: true,
        };
        let record = Record {
            schema_version: 1,
            request_id: "abc".into(),
            started_at: datetime!(2026-10-26 14:30:25 UTC),
            ended_at: datetime!(2026-10-26 14:30:25.15 UTC),
            duration_ms: 150,
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            upstream: Some("https://api.example.com".into()),
            model_id: None,
            model_name: None,
            status: Some(200),
            response_complete: true,
            error: None,
            request: body.clone(),
            response: Body {
                data: "Hi".into(),
                observed_bytes: 2,
                captured_bytes: 2,
                ..body
            },
        };
        let value: Value = serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["started_at"], "2026-10-26T14:30:25Z");
        assert_eq!(value["ended_at"], "2026-10-26T14:30:25.15Z");
        assert_eq!(value["duration_ms"], 150);
        assert_eq!(value["status"], 200);
        assert!(value.get("model_id").is_none());
        assert!(value.get("error").is_none());
        assert_eq!(value["request"]["encoding"], "utf-8");
    }
}
