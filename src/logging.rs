//! JSON-line operational logging with slog-compatible field names.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// A process-wide logger writing one JSON object per line.
pub struct JsonLogger {
    out: Mutex<Box<dyn Write + Send>>,
}

impl JsonLogger {
    pub fn new(writer: Box<dyn Write + Send>) -> Arc<Self> {
        Arc::new(Self {
            out: Mutex::new(writer),
        })
    }

    pub fn stdout() -> Arc<Self> {
        Arc::new(Self {
            out: Mutex::new(Box::new(io::stdout())),
        })
    }

    /// Append to `path`, creating it with mode 0600 where supported.
    pub fn file(path: &Path) -> io::Result<Arc<Self>> {
        let file = open_append(path)?;
        Ok(Arc::new(Self {
            out: Mutex::new(Box::new(file)),
        }))
    }

    pub fn info(&self, msg: &str, fields: Map<String, Value>) {
        self.emit("INFO", msg, fields);
    }

    pub fn warn(&self, msg: &str, fields: Map<String, Value>) {
        self.emit("WARN", msg, fields);
    }

    pub fn error(&self, msg: &str, fields: Map<String, Value>) {
        self.emit("ERROR", msg, fields);
    }

    fn emit(&self, level: &str, msg: &str, fields: Map<String, Value>) {
        let mut line = String::with_capacity(160);
        line.push('{');
        write!(line, "\"time\":").ok();
        line.push_str(&serde_json::to_string(&now_rfc3339()).unwrap_or_default());
        write!(
            line,
            ",\"level\":{}",
            serde_json::to_string(level).unwrap_or_default()
        )
        .ok();
        write!(
            line,
            ",\"msg\":{}",
            serde_json::to_string(msg).unwrap_or_default()
        )
        .ok();
        for (key, value) in fields {
            write!(
                line,
                ",{}:{}",
                serde_json::to_string(&key).unwrap_or_default(),
                value
            )
            .ok();
        }
        line.push('}');
        line.push('\n');
        if let Ok(mut out) = self.out.lock() {
            let _ = out.write_all(line.as_bytes());
            let _ = out.flush();
        }
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn open_append(path: &Path) -> io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Start a field map for an event.
pub fn fields() -> Map<String, Value> {
    Map::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_one_json_line_with_core_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.log");
        let logger = JsonLogger::file(&path).unwrap();
        let mut fields = fields();
        fields.insert("method".into(), "GET".into());
        fields.insert("status".into(), 401.into());
        logger.info("request", fields);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        let value: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["level"], "INFO");
        assert_eq!(value["msg"], "request");
        assert_eq!(value["method"], "GET");
        assert_eq!(value["status"], 401);
        assert!(value["time"].as_str().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn log_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.log");
        let _ = JsonLogger::file(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
