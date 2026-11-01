//! Bounded, concurrency-safe body capture.

use std::sync::Mutex;

use base64::Engine as _;

use super::record::Body;

#[derive(Debug)]
struct State {
    data: Vec<u8>,
    observed: u64,
    expected: Option<u64>,
    eof: bool,
    failed: bool,
}

/// Retains at most `limit` bytes while counting everything observed.
#[derive(Debug)]
pub struct Capture {
    limit: usize,
    state: Mutex<State>,
}

impl Capture {
    pub fn new(limit: usize, expected: Option<u64>) -> Self {
        Self {
            limit,
            state: Mutex::new(State {
                data: Vec::new(),
                observed: 0,
                expected,
                eof: false,
                failed: false,
            }),
        }
    }

    /// Record bytes as they pass through. `eof` and `failed` describe the
    /// transfer event alongside this chunk.
    pub fn observe(&self, data: &[u8], eof: bool, failed: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.observed += data.len() as u64;
        let remaining = self.limit.saturating_sub(state.data.len());
        let take = remaining.min(data.len());
        state.data.extend_from_slice(&data[..take]);
        if eof {
            state.eof = true;
        }
        if failed {
            state.failed = true;
        }
    }

    pub fn observed_bytes(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .observed
    }

    pub fn snapshot(&self) -> Body {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let captured = state.data.len();
        let complete = !state.failed
            && (state.eof
                || state
                    .expected
                    .is_some_and(|expected| state.observed == expected));
        if std::str::from_utf8(&state.data).is_ok() {
            Body {
                data: String::from_utf8_lossy(&state.data).into_owned(),
                encoding: "utf-8",
                observed_bytes: state.observed as i64,
                captured_bytes: captured,
                truncated: state.observed > captured as u64,
                complete,
            }
        } else {
            Body {
                data: base64::engine::general_purpose::STANDARD.encode(&state.data),
                encoding: "base64",
                observed_bytes: state.observed as i64,
                captured_bytes: captured,
                truncated: state.observed > captured as u64,
                complete,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_table() {
        type Event = (&'static [u8], bool, bool);
        type Case = (
            &'static str,
            usize,
            Option<u64>,
            &'static [Event],
            &'static str,
            i64,
            usize,
            bool,
            bool,
        );
        let cases: &[Case] = &[
            ("empty", 8, Some(0), &[], "utf-8", 0, 0, false, true),
            (
                "exact",
                8,
                Some(3),
                &[(b"abc", true, false)],
                "utf-8",
                3,
                3,
                false,
                true,
            ),
            (
                "overflow",
                2,
                Some(4),
                &[(b"abcd", true, false)],
                "utf-8",
                4,
                2,
                true,
                true,
            ),
            (
                "unknown length",
                8,
                None,
                &[(b"abc", false, false)],
                "utf-8",
                3,
                3,
                false,
                false,
            ),
            (
                "unknown plus eof",
                8,
                None,
                &[(b"abc", false, false), (b"", true, false)],
                "utf-8",
                3,
                3,
                false,
                true,
            ),
            (
                "partial",
                8,
                Some(8),
                &[(b"ab", false, false)],
                "utf-8",
                2,
                2,
                false,
                false,
            ),
            (
                "binary",
                8,
                None,
                &[(&[0xff], false, false), (b"", true, false)],
                "base64",
                1,
                1,
                false,
                true,
            ),
        ];
        for (name, limit, expected, events, encoding, observed, captured, truncated, complete) in
            cases
        {
            let capture = Capture::new(*limit, *expected);
            for (data, eof, failed) in *events {
                capture.observe(data, *eof, *failed);
            }
            let body = capture.snapshot();
            assert_eq!(body.encoding, *encoding, "{name}");
            assert_eq!(body.observed_bytes, *observed, "{name}");
            assert_eq!(body.captured_bytes, *captured, "{name}");
            assert_eq!(body.truncated, *truncated, "{name}");
            assert_eq!(body.complete, *complete, "{name}");
        }
    }

    #[test]
    fn truncated_mid_utf8_uses_base64() {
        let capture = Capture::new(2, None);
        capture.observe("你".as_bytes(), true, false);
        let body = capture.snapshot();
        assert_eq!(body.encoding, "base64");
        assert_eq!(body.captured_bytes, 2);
        assert!(body.truncated);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&body.data)
                .unwrap(),
            "你".as_bytes()[..2]
        );
    }

    #[test]
    fn snapshot_is_safe_alongside_observation() {
        use std::sync::Arc;
        let capture = Arc::new(Capture::new(1 << 20, None));
        let observer = capture.clone();
        let handle = std::thread::spawn(move || {
            for _ in 0..10_000 {
                observer.observe(b"abc", false, false);
            }
            observer.observe(b"", true, false);
        });
        for _ in 0..100 {
            let _ = capture.snapshot();
        }
        handle.join().unwrap();
        assert_eq!(capture.snapshot().observed_bytes, 30_000);
    }
}
