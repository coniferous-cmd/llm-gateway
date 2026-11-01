//! Filesystem publication for conversation records.

use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use time::OffsetDateTime;
use time::UtcOffset;
use time::macros::format_description;

use crate::config::Model;
use crate::logging::{JsonLogger, fields};

use super::{Call, Record};

pub struct Store {
    directory: PathBuf,
    limit: usize,
    logger: Arc<JsonLogger>,
}

impl Store {
    /// Create the store. Initialization failures are logged and are not fatal;
    /// calls continue even when capture cannot be persisted.
    pub fn new(directory: PathBuf, limit: usize, logger: Arc<JsonLogger>) -> Arc<Self> {
        let store = Arc::new(Self {
            directory,
            limit,
            logger,
        });
        if let Err(error) = create_dir(&store.directory) {
            store.report(None, "initialize", &error);
        }
        store
    }

    pub fn begin(&self, method: &str, path: &str, expected: Option<u64>) -> Option<Call> {
        let id = match random_id() {
            Ok(id) => id,
            Err(error) => {
                self.report(None, "generate_id", &error);
                return None;
            }
        };
        Some(Call::new(
            id,
            OffsetDateTime::now_utc(),
            method.to_string(),
            path.to_string(),
            self.limit,
            expected,
            self.limit,
        ))
    }

    pub fn finish(&self, call: &Call, status: u16, upstream: &str, model: Option<&Model>) {
        let record = call.finish(OffsetDateTime::now_utc(), status, upstream, model);
        if let Err(error) = self.save(&record) {
            self.report(Some(&record.request_id), "persist", &error);
        }
    }

    fn save(&self, record: &Record) -> io::Result<()> {
        let started = record.started_at.to_offset(UtcOffset::UTC);
        let day = started
            .format(format_description!("[year]-[month]-[day]"))
            .map_err(io::Error::other)?;
        let time = started
            .format(format_description!("[hour][minute][second]"))
            .map_err(io::Error::other)?;
        let directory = self.directory.join(day);
        create_dir(&directory)?;

        let (mut file, temp_path) = create_temp(&directory)?;
        let result = (|| {
            let mut bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
            bytes.push(b'\n');
            file.write_all(&bytes)?;
            file.flush()?;
            drop(file);

            let mut suffix = record.request_id.clone();
            for _ in 0..8 {
                let final_path = directory.join(format!("{time}_{suffix}.json"));
                match std::fs::hard_link(&temp_path, &final_path) {
                    Ok(()) => return Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        suffix = random_id()?;
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "conversation record collision",
            ))
        })();
        let _ = std::fs::remove_file(&temp_path);
        result
    }

    /// Diagnostics deliberately avoid raw errors and paths; both can carry
    /// secrets.
    fn report(&self, request_id: Option<&str>, operation: &str, error: &io::Error) {
        let category = match error.kind() {
            io::ErrorKind::PermissionDenied => "permission_denied",
            io::ErrorKind::AlreadyExists => "collision",
            io::ErrorKind::NotFound => "not_found",
            _ => "io_failure",
        };
        let mut fields = fields();
        fields.insert("request_id".into(), json!(request_id.unwrap_or("")));
        fields.insert("operation".into(), json!(operation));
        fields.insert("category".into(), json!(category));
        self.logger.error("conversation logging failed", fields);
    }
}

fn create_dir(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn create_temp(directory: &Path) -> io::Result<(std::fs::File, PathBuf)> {
    for _ in 0..8 {
        let id = random_id()?;
        let path = directory.join(format!(".conversation-{id}.tmp"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "temporary conversation file collision",
    ))
}

fn random_id() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::conversationlog::record::Body;
    use time::macros::datetime;

    fn test_store(directory: &Path) -> (Arc<Store>, PathBuf) {
        let log = directory.join("gateway.log");
        let logger = JsonLogger::file(&log).unwrap();
        let store = Store::new(directory.join("conversations"), 1 << 20, logger);
        (store, log)
    }

    #[test]
    fn concurrent_calls_produce_distinct_files() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _) = test_store(temp.path());
        let mut handles = Vec::new();
        for _ in 0..20 {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                let call = store.begin("POST", "/v1/chat", Some(3)).unwrap();
                call.request.observe(b"ask", true, false);
                call.response.observe(b"ans", true, false);
                store.finish(&call, 200, "https://up", None);
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let root = temp.path().join("conversations");
        let mut files = Vec::new();
        for day in std::fs::read_dir(&root).unwrap() {
            let day = day.unwrap().path();
            assert!(day.is_dir());
            for file in std::fs::read_dir(&day).unwrap() {
                files.push(file.unwrap().path());
            }
        }
        assert_eq!(files.len(), 20);
        let mut ids = HashSet::new();
        for file in &files {
            assert_eq!(file.extension().unwrap(), "json");
            let text = std::fs::read_to_string(file).unwrap();
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["schema_version"], 1);
            assert_eq!(value["method"], "POST");
            assert_eq!(value["path"], "/v1/chat");
            assert_eq!(value["status"], 200);
            ids.insert(value["request_id"].as_str().unwrap().to_string());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(file).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600);
            }
        }
        assert_eq!(ids.len(), 20);
        assert!(
            files
                .iter()
                .all(|f| !f.file_name().unwrap().to_string_lossy().contains(".tmp"))
        );
    }

    #[test]
    fn collision_regenerates_suffix_without_overwriting() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _) = test_store(temp.path());
        let started = datetime!(2026-10-26 14:30:25 UTC);
        let call = Call::new(
            "abc".into(),
            started,
            "POST".into(),
            "/x".into(),
            8,
            Some(0),
            8,
        );
        let record = call.finish(started, 200, "", None);
        let directory = temp.path().join("conversations").join("2026-10-26");
        create_dir(&directory).unwrap();
        let existing = directory.join("143025_abc.json");
        std::fs::write(&existing, "existing").unwrap();
        store.save(&record).unwrap();
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "existing");
        let names: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.iter().any(|name| name != "143025_abc.json"));
        let body = std::fs::read_to_string(
            directory.join(
                names
                    .iter()
                    .find(|name| *name != "143025_abc.json")
                    .unwrap(),
            ),
        )
        .unwrap();
        assert!(body.contains("\"request_id\":\"abc\""));
    }

    #[test]
    fn initialization_and_persistence_failures_are_logged_without_secrets() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("gateway.log");
        let logger = JsonLogger::file(&log).unwrap();
        let blocked = temp.path().join("blocked");
        std::fs::write(&blocked, "not a directory").unwrap();
        let store = Store::new(blocked, 8, logger);
        let call = store.begin("POST", "/x", Some(0)).unwrap();
        call.request.observe(b"secret-value", true, false);
        store.finish(&call, 200, "", None);
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("conversation logging failed"));
        assert!(!text.contains("secret-value"));
        assert!(!text.contains("blocked"));
    }

    #[test]
    fn record_body_shape_is_complete() {
        let body = Body {
            data: String::new(),
            encoding: "utf-8",
            observed_bytes: 0,
            captured_bytes: 0,
            truncated: false,
            complete: true,
        };
        assert_eq!(body.encoding, "utf-8");
    }
}
