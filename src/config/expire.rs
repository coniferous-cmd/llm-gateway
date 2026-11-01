//! Parsing for `target.expire`.

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::ConfigError;
use super::yaml::{self, Node};

const INVALID: &str = "expire must be an RFC3339 timestamp or Unix seconds";

/// An omitted, null, empty or zero expire never expires. Unix `0` is the
/// explicit "never" form, not 1970.
pub fn parse(node: &Node) -> Result<Option<OffsetDateTime>, ConfigError> {
    if yaml::is_null(node) {
        return Ok(None);
    }
    if let Some(seconds) = yaml::scalar_integer(node) {
        if seconds == 0 {
            return Ok(None);
        }
        return OffsetDateTime::from_unix_timestamp(seconds)
            .map(Some)
            .map_err(|_| ConfigError::at(yaml::line(node), INVALID));
    }
    let Some(text) = yaml::scalar_string(node) else {
        return Err(ConfigError::at(yaml::line(node), INVALID));
    };
    OffsetDateTime::parse(text, &Rfc3339)
        .map(Some)
        .map_err(|_| ConfigError::at(yaml::line(node), INVALID))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::yaml::load;

    fn one(source: &str) -> Result<Option<OffsetDateTime>, ConfigError> {
        let docs = load(source).expect("parse");
        let root = docs.into_iter().next().unwrap();
        parse(yaml::field(&root, "expire").unwrap())
    }

    #[test]
    fn accepts_unix_seconds_and_rfc3339() {
        assert_eq!(one("expire: 0").unwrap(), None);
        assert_eq!(one("expire: null").unwrap(), None);
        assert_eq!(
            one("expire: 1800000000").unwrap().unwrap().unix_timestamp(),
            1_800_000_000
        );
        assert_eq!(
            one("expire: '2026-11-01T00:00:00Z'")
                .unwrap()
                .unwrap()
                .unix_timestamp(),
            1_793_491_200
        );
    }

    #[test]
    fn rejects_other_forms_without_leaking_the_value() {
        let err = one("expire: secret-value").unwrap_err().to_string();
        assert!(err.contains("expire must be"), "{err}");
        assert!(!err.contains("secret-value"), "{err}");
    }
}
