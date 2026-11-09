//! Opt-in per-call conversation records.
//!
//! Each authenticated call produces one JSON file under
//! `<directory>/YYYY-MM-DD/HHMMSS_<request-id>.json` (UTC). Files are written
//! as temporary siblings and published with a hard link, so an existing record
//! is never overwritten. Diagnostics use fixed categories and never include
//! paths or raw errors, both of which may carry secrets.

pub mod capture;
pub mod record;
pub mod store;

pub use capture::Capture;
pub use record::{Body, Record, format_go_time};
pub use store::Store;

use std::sync::Mutex;

use time::OffsetDateTime;

use crate::config::Model;

const CATEGORIES: &[&str] = &[
    "preparation_failed",
    "upstream_transport",
    "upstream_read",
    "upstream_encoding",
    "upstream_decode",
    "client_write",
    "canceled",
    "aborted",
    "unsupported_hijack",
];

/// Per-request capture state shared with body wrappers.
pub struct Call {
    pub request: Capture,
    pub response: Capture,
    id: String,
    started_at: OffsetDateTime,
    method: String,
    path: String,
    category: Mutex<Option<&'static str>>,
}

impl Call {
    pub fn new(
        id: String,
        started_at: OffsetDateTime,
        method: String,
        path: String,
        request_limit: usize,
        request_expected: Option<u64>,
        response_limit: usize,
    ) -> Self {
        Self {
            request: Capture::new(request_limit, request_expected),
            response: Capture::new(response_limit, None),
            id,
            started_at,
            method,
            path,
            category: Mutex::new(None),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Record a fixed error category. Categories stay sticky except that the
    /// two provisional ones may be upgraded by later observations.
    pub fn fail(&self, category: &'static str) {
        if !CATEGORIES.contains(&category) {
            return;
        }
        let mut current = self
            .category
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if current.is_none()
            || matches!(*current, Some("preparation_failed" | "upstream_transport"))
        {
            *current = Some(category);
        }
    }

    pub fn finish(
        &self,
        ended: OffsetDateTime,
        status: u16,
        upstream: &str,
        model: Option<&Model>,
    ) -> Record {
        let category = *self
            .category
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut response = self.response.snapshot();
        let normal = matches!(
            category,
            None | Some("preparation_failed" | "upstream_transport")
        );
        // A normal handler return establishes completion; upstream read
        // failures and client aborts are explicitly observed.
        response.complete = normal;
        Record {
            schema_version: 1,
            request_id: self.id.clone(),
            started_at: self.started_at,
            ended_at: ended,
            duration_ms: (ended - self.started_at).whole_milliseconds() as i64,
            method: self.method.clone(),
            path: self.path.clone(),
            upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
            model_id: model.map(|model| model.id.clone()),
            model_name: model.map(|model| model.name.clone()),
            status: (status != 0).then_some(status),
            response_complete: normal,
            error: category.map(str::to_string),
            request: self.request.snapshot(),
            response,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn make_call() -> Call {
        Call::new(
            "abc".into(),
            datetime!(2026-10-26 14:30:25 UTC),
            "POST".into(),
            "/v1/chat".into(),
            1 << 20,
            Some(3),
            1 << 20,
        )
    }

    #[test]
    fn category_precedence_matches_go() {
        let call = make_call();
        call.fail("upstream_read");
        call.fail("client_write");
        assert_eq!(
            call.finish(datetime!(2026-10-26 14:30:25.1 UTC), 200, "https://x", None)
                .error
                .as_deref(),
            Some("upstream_read")
        );

        let call = make_call();
        call.fail("preparation_failed");
        call.fail("upstream_transport");
        assert_eq!(
            call.finish(datetime!(2026-10-26 14:30:25.1 UTC), 502, "https://x", None)
                .error
                .as_deref(),
            Some("upstream_transport")
        );

        let call = make_call();
        call.fail("not-a-category");
        assert!(
            call.finish(datetime!(2026-10-26 14:30:25.1 UTC), 200, "", None)
                .error
                .is_none()
        );
    }

    #[test]
    fn finish_overrides_response_completion_rules() {
        let call = make_call();
        call.request.observe(b"abc", true, false);
        call.response.observe(b"hi", true, false);
        let record = call.finish(datetime!(2026-10-26 14:30:25.15 UTC), 200, "", None);
        assert!(record.response_complete);
        assert!(record.response.complete);
        assert!(record.request.complete);
        assert_eq!(record.duration_ms, 150);
        assert_eq!(record.status, Some(200));
        assert!(record.upstream.is_none());

        let call = make_call();
        call.response.observe(b"hi", true, false);
        call.fail("upstream_read");
        let record = call.finish(datetime!(2026-10-26 14:30:25.2 UTC), 200, "", None);
        assert!(!record.response_complete);
        assert!(!record.response.complete);
    }
}
