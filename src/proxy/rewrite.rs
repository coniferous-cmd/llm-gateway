//! Escape-preserving path-prefix rewriting.

use crate::config::RewriteRule;

/// Apply at most one rule to the escaped path. Returns `None` when no rule
/// matches or the match would leave the path unchanged, so callers keep the
/// original URI representation. The query string is copied verbatim,
/// including an empty `?`.
pub fn rewrite_path(path_and_query: &str, rules: &[RewriteRule]) -> Option<String> {
    if rules.is_empty() {
        return None;
    }
    let (path, query) = match path_and_query.find('?') {
        Some(index) => (&path_and_query[..index], &path_and_query[index..]),
        None => (path_and_query, ""),
    };
    for rule in rules {
        if !path.starts_with(&rule.source) {
            continue;
        }
        let suffix = &path[rule.source.len()..];
        if !suffix.is_empty() && !rule.source.ends_with('/') && !suffix.starts_with('/') {
            continue;
        }
        let mut escaped = format!("{}{suffix}", rule.destination);
        if rule.destination.is_empty() && !escaped.starts_with('/') {
            // Prefix deletion must still yield an absolute HTTP path.
            escaped.insert(0, '/');
        }
        if escaped == path {
            return None;
        }
        return Some(format!("{escaped}{query}"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(source: &str, destination: &str) -> RewriteRule {
        RewriteRule {
            source: source.into(),
            destination: destination.into(),
        }
    }

    const QUERY: &str = "?next=/api&x=a%2Fb&x=2";

    #[test]
    fn rewrite_table() {
        let standard = vec![rule("/api", "/v1")];
        let remove = vec![rule("/api", "")];
        let cases: &[(&str, &str, &str, &[RewriteRule])] = &[
            ("delete exact", "/api", "/", &remove),
            ("delete trailing slash", "/api/", "/", &remove),
            (
                "delete descendant",
                "/api/v1/messages",
                "/v1/messages",
                &remove,
            ),
            ("delete once", "/api/api/models", "/api/models", &remove),
            ("delete boundary", "/apix/models", "/apix/models", &remove),
            (
                "delete not at start",
                "/other/api/models",
                "/other/api/models",
                &remove,
            ),
            (
                "delete encoded suffix",
                "/api/a%2Fb%20c/%25/%2f",
                "/a%2Fb%20c/%25/%2f",
                &remove,
            ),
            (
                "delete encoded boundary",
                "/api%2Fmodels",
                "/api%2Fmodels",
                &remove,
            ),
            ("delete no cleaning", "/api/../a//b", "/../a//b", &remove),
            ("exact", "/api", "/v1", &standard),
            ("trailing slash", "/api/", "/v1/", &standard),
            (
                "descendant",
                "/api/chat/completions",
                "/v1/chat/completions",
                &standard,
            ),
            ("not segment", "/apix/models", "/apix/models", &standard),
            (
                "not at start",
                "/other/api/models",
                "/other/api/models",
                &standard,
            ),
            ("case sensitive", "/API/models", "/API/models", &standard),
            (
                "not global replace",
                "/api/api/models",
                "/v1/api/models",
                &standard,
            ),
            ("no cleaning", "/api/../a//b", "/v1/../a//b", &standard),
            (
                "encoded suffix",
                "/api/a%2Fb%20c/%25/%2f",
                "/v1/a%2Fb%20c/%25/%2f",
                &standard,
            ),
            (
                "encoded boundary",
                "/api%2Fmodels",
                "/api%2Fmodels",
                &standard,
            ),
            (
                "encoded source differs",
                "/%61pi/models",
                "/%61pi/models",
                &standard,
            ),
        ];
        for (name, path, want, rules) in cases {
            let input = format!("{path}{QUERY}");
            match rewrite_path(&input, rules) {
                Some(rewritten) => assert_eq!(rewritten, format!("{want}{QUERY}"), "{name}"),
                None => assert_eq!(*path, *want, "{name}: unchanged"),
            }
        }
    }

    #[test]
    fn first_match_wins_and_rules_do_not_chain() {
        assert_eq!(
            rewrite_path(
                "/api/admin/m",
                &[rule("/api", "/v1"), rule("/api/admin", "/admin")]
            )
            .as_deref(),
            Some("/v1/admin/m")
        );
        assert_eq!(
            rewrite_path(
                "/api/admin/m",
                &[rule("/api/admin", "/admin"), rule("/api", "/v1")]
            )
            .as_deref(),
            Some("/admin/m")
        );
        assert_eq!(
            rewrite_path("/api/models", &[rule("/api", "/v1"), rule("/v1", "/v2")]).as_deref(),
            Some("/v1/models")
        );
        assert_eq!(
            rewrite_path(
                "/openai/models",
                &[rule("/api", "/v1"), rule("/openai", "/v1")]
            )
            .as_deref(),
            Some("/v1/models")
        );
    }

    #[test]
    fn root_trailing_and_destination_forms() {
        assert_eq!(
            rewrite_path("/models", &[rule("/", "/v1/")]).as_deref(),
            Some("/v1/models")
        );
        assert_eq!(
            rewrite_path("/", &[rule("/", "/v1/")]).as_deref(),
            Some("/v1/")
        );
        assert_eq!(
            rewrite_path("/api/models", &[rule("/api/", "/v1/")]).as_deref(),
            Some("/v1/models")
        );
        assert_eq!(
            rewrite_path("/api/", &[rule("/api/", "/v1/")]).as_deref(),
            Some("/v1/")
        );
        assert_eq!(
            rewrite_path("/api", &[rule("/api/", "/v1/")]).as_deref(),
            None
        );
        assert_eq!(
            rewrite_path("/api/models", &[rule("/api", "/v1/")]).as_deref(),
            Some("/v1//models")
        );
        assert_eq!(
            rewrite_path("/api/models", &[rule("/api/", "/")]).as_deref(),
            Some("/models")
        );
        assert_eq!(
            rewrite_path("/api/models", &[rule("/api", "/v%20x")]).as_deref(),
            Some("/v%20x/models")
        );
        assert_eq!(
            rewrite_path("/a%2Fb/models", &[rule("/a%2Fb", "/v1")]).as_deref(),
            Some("/v1/models")
        );
    }

    #[test]
    fn empty_query_is_preserved() {
        assert_eq!(
            rewrite_path("/api/models?", &[rule("/api", "/v1")]).as_deref(),
            Some("/v1/models?")
        );
    }

    #[test]
    fn unmatched_or_unchanged_returns_none() {
        assert_eq!(rewrite_path("/api/models", &[]), None);
        assert_eq!(rewrite_path("/other", &[rule("/api", "/v1")]), None);
        assert_eq!(rewrite_path("/api", &[rule("/api", "/api")]), None);
    }
}
