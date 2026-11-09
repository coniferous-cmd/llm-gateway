//! Virtual-key authentication, model resolution, and round-robin routing.
//!
//! Routes are immutable after construction. The only mutable state is the
//! per-token round-robin counter, which is why requests can share a [`Map`]
//! across threads without locking.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use time::OffsetDateTime;

use crate::config::{Entry, Model, Target};

/// Claude Code's 1M-context marker on a model id. The base id serves the
/// suffixed variant, with the marker kept on the upstream name.
const LONG_CONTEXT_SUFFIX: &str = "[1m]";

#[derive(Clone, Debug)]
struct ModelTarget {
    target: Target,
    model: Model,
}

/// The immutable entry list of one virtual key plus its routing state.
pub struct Route {
    entries: Vec<Entry>,
    by_model: HashMap<String, ModelTarget>,
    next: AtomicU64,
}

/// All configured routes, keyed by the exact virtual token.
pub struct Map {
    routes: HashMap<String, Arc<Route>>,
}

impl Map {
    pub fn new(keys: HashMap<String, Vec<Entry>>) -> Self {
        let mut routes = HashMap::with_capacity(keys.len());
        for (token, entries) in keys {
            if entries.is_empty() {
                continue;
            }
            let mut by_model = HashMap::new();
            let entries: Vec<Entry> = entries
                .into_iter()
                .inspect(|entry| {
                    // The owned clone keeps startup state immutable.
                    for model in &entry.models {
                        by_model.insert(
                            model.id.clone(),
                            ModelTarget {
                                target: entry.target.clone(),
                                model: model.clone(),
                            },
                        );
                    }
                })
                .collect();
            routes.insert(
                token,
                Arc::new(Route {
                    entries,
                    by_model,
                    next: AtomicU64::new(0),
                }),
            );
        }
        Self { routes }
    }

    /// Authenticate the `Authorization` header. A token whose every entry has
    /// expired authenticates as unknown. Only [`Route::round_robin`] advances
    /// the counter, so model-matched and rejected requests never shift it.
    pub fn select(&self, authorization: &str) -> Option<&Arc<Route>> {
        let mut fields = authorization.split_whitespace();
        let scheme = fields.next()?;
        let token = fields.next()?;
        if fields.next().is_some() || !scheme.eq_ignore_ascii_case("Bearer") {
            return None;
        }
        let route = self.routes.get(token)?;
        route.has_live().then_some(route)
    }
}

impl Route {
    /// Whether any entry of this token is not past its expire time.
    pub fn has_live(&self) -> bool {
        self.entries.iter().any(|entry| !expired(&entry.target))
    }

    /// Whether the token declares model ids for model routing.
    pub fn has_models(&self) -> bool {
        !self.by_model.is_empty()
    }

    /// The model ids this token declares across its live entries, in
    /// declaration order. A token without `models` yields an empty list.
    pub fn model_ids(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut ids = Vec::new();
        for entry in &self.entries {
            if expired(&entry.target) {
                continue;
            }
            for model in &entry.models {
                if seen.insert(model.id.as_str()) {
                    ids.push(model.id.clone());
                }
            }
        }
        ids
    }

    /// The target the next model-less request would receive, without
    /// advancing the round-robin counter. Used to choose the rules for a
    /// locally answered request.
    pub fn peek(&self) -> Target {
        let live: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|entry| !expired(&entry.target))
            .collect();
        debug_assert!(!live.is_empty(), "select() rejects fully expired routes");
        let next = self.next.load(Ordering::Relaxed);
        live[(next % live.len() as u64) as usize].target.clone()
    }

    /// Map a caller's model id onto the entry declaring it. A verbatim id
    /// wins; otherwise the id without `[1m]` is tried and the suffix is
    /// appended to the entry's upstream name.
    pub fn resolve(&self, id: &str) -> Option<(Target, Model)> {
        if let Some(found) = self.by_model.get(id) {
            if expired(&found.target) {
                return None;
            }
            return Some((found.target.clone(), found.model.clone()));
        }
        let base = id.strip_suffix(LONG_CONTEXT_SUFFIX)?;
        let found = self.by_model.get(base)?;
        if expired(&found.target) {
            return None;
        }
        Some((
            found.target.clone(),
            Model {
                id: id.to_string(),
                name: format!("{}{LONG_CONTEXT_SUFFIX}", found.model.name),
            },
        ))
    }

    /// Advance the counter for requests without a model match, cycling only
    /// through entries that are not past their expire time.
    pub fn round_robin(&self) -> Target {
        let live: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|entry| !expired(&entry.target))
            .collect();
        debug_assert!(!live.is_empty(), "select() rejects fully expired routes");
        let previous = self.next.fetch_add(1, Ordering::Relaxed);
        live[(previous % live.len() as u64) as usize].target.clone()
    }
}

fn expired(target: &Target) -> bool {
    target
        .expire
        .is_some_and(|expire| OffsetDateTime::now_utc() >= expire)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use time::Duration;

    use super::*;

    fn target(base: &str) -> Target {
        Target {
            base_url: base.to_string(),
            ..Target::default()
        }
    }

    fn entry(base: &str, models: &[(&str, &str)]) -> Entry {
        Entry {
            target: target(base),
            models: models
                .iter()
                .map(|(id, name)| Model {
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                })
                .collect(),
        }
    }

    fn build_map(keys: &[(&str, Vec<Entry>)]) -> Map {
        Map::new(
            keys.iter()
                .map(|(token, entries)| ((*token).to_string(), entries.clone()))
                .collect(),
        )
    }

    #[test]
    fn authentication_requires_exactly_one_bearer_token() {
        let routes = build_map(&[
            ("dev", vec![entry("https://a", &[])]),
            ("upper", vec![entry("https://b", &[])]),
        ]);
        for header in [
            "Bearer dev",
            "bearer dev",
            "BEARER dev",
            "  Bearer   dev  ",
            "Bearer upper",
        ] {
            assert!(routes.select(header).is_some(), "{header}");
        }
        for header in [
            "",
            "Basic dev",
            "Bearer",
            "Bearer unknown",
            "Bearer dev extra",
            "Bearer ",
            "Bearer dev\tx",
        ] {
            assert!(routes.select(header).is_none(), "{header}");
        }
    }

    #[test]
    fn round_robin_cycles_in_order_per_token() {
        let routes = build_map(&[
            (
                "dev",
                vec![
                    entry("https://1", &[]),
                    entry("https://2", &[]),
                    entry("https://3", &[]),
                ],
            ),
            (
                "other",
                vec![entry("https://x", &[]), entry("https://y", &[])],
            ),
        ]);
        let dev = routes.select("Bearer dev").unwrap();
        for expected in ["https://1", "https://2", "https://3", "https://1"] {
            assert_eq!(dev.round_robin().base_url, expected);
        }
        let other = routes.select("Bearer other").unwrap();
        for expected in ["https://x", "https://y", "https://x"] {
            assert_eq!(other.round_robin().base_url, expected);
        }
        // The dev counter is independent and continues where it left off.
        assert_eq!(dev.round_robin().base_url, "https://2");
    }

    #[test]
    fn model_resolution_does_not_advance_round_robin() {
        let routes = build_map(&[(
            "dev",
            vec![
                entry("https://1", &[("m", "first")]),
                entry("https://2", &[]),
            ],
        )]);
        let route = routes.select("Bearer dev").unwrap();
        assert!(route.has_models());
        let (target, model) = route.resolve("m").unwrap();
        assert_eq!(target.base_url, "https://1");
        assert_eq!(
            model,
            Model {
                id: "m".into(),
                name: "first".into()
            }
        );
        assert!(route.resolve("nope").is_none());
        // No model request still starts at the first entry.
        assert_eq!(route.round_robin().base_url, "https://1");
    }

    #[test]
    fn long_context_suffix_falls_back_and_keeps_the_marker() {
        let routes = build_map(&[(
            "dev",
            vec![
                entry("https://1", &[("claude-haiku-5-5", "deepseek-v4.1-flash")]),
                entry("https://2", &[("explicit[1m]", "verbatim")]),
            ],
        )]);
        let route = routes.select("Bearer dev").unwrap();
        let (_, model) = route.resolve("claude-haiku-5-5[1m]").unwrap();
        assert_eq!(model.id, "claude-haiku-5-5[1m]");
        assert_eq!(model.name, "deepseek-v4.1-flash[1m]");
        let (_, model) = route.resolve("explicit[1m]").unwrap();
        assert_eq!(model.name, "verbatim");
        for id in ["unknown[1m]", "[1m]", "claude-haiku-5-5[1M]"] {
            assert!(route.resolve(id).is_none(), "{id}");
        }
    }

    #[test]
    fn expired_entries_are_skipped_everywhere() {
        let future = OffsetDateTime::now_utc() + Duration::hours(1);
        let past = OffsetDateTime::now_utc() - Duration::hours(1);
        let mut live = entry("https://live", &[("m", "live")]);
        live.target.expire = Some(future);
        let mut dead = entry("https://dead", &[]);
        dead.target.expire = Some(past);
        let routes = build_map(&[("dev", vec![dead.clone(), live.clone()])]);
        let route = routes.select("Bearer dev").unwrap();
        assert_eq!(route.round_robin().base_url, "https://live");
        assert_eq!(route.round_robin().base_url, "https://live");
        assert!(route.resolve("m").is_some());

        let mut expired_model = entry("https://dead-model", &[("gone", "gone")]);
        expired_model.target.expire = Some(past);
        let routes = build_map(&[("dev", vec![expired_model])]);
        assert!(routes.select("Bearer dev").is_none());
        let route = routes.routes.get("dev").unwrap();
        assert!(route.resolve("gone").is_none());
        assert!(route.resolve("gone[1m]").is_none());
    }

    #[test]
    fn legacy_tokens_have_no_models() {
        let routes = build_map(&[("dev", vec![entry("https://1", &[])])]);
        let route = routes.select("Bearer dev").unwrap();
        assert!(!route.has_models());
        assert!(route.resolve("anything").is_none());
    }

    #[test]
    fn model_ids_lists_live_declarations_in_order_once() {
        let future = OffsetDateTime::now_utc() + Duration::hours(1);
        let past = OffsetDateTime::now_utc() - Duration::hours(1);
        let mut live = entry("https://live", &[("a", "real-a"), ("b", "real-b")]);
        live.target.expire = Some(future);
        let mut dead = entry("https://dead", &[("gone", "gone")]);
        dead.target.expire = Some(past);
        let routes = build_map(&[("dev", vec![dead, live, entry("https://c", &[("b", "b")])])]);
        let route = routes.select("Bearer dev").unwrap();
        assert_eq!(route.model_ids(), vec!["a", "b"]);
    }

    #[test]
    fn legacy_tokens_list_no_models() {
        let routes = build_map(&[("dev", vec![entry("https://1", &[])])]);
        let route = routes.select("Bearer dev").unwrap();
        assert!(route.model_ids().is_empty());
    }

    #[test]
    fn peek_does_not_advance_round_robin() {
        let routes = build_map(&[(
            "dev",
            vec![entry("https://1", &[]), entry("https://2", &[])],
        )]);
        let route = routes.select("Bearer dev").unwrap();
        assert_eq!(route.peek().base_url, "https://1");
        assert_eq!(route.peek().base_url, "https://1");
        assert_eq!(route.round_robin().base_url, "https://1");
        assert_eq!(route.peek().base_url, "https://2");
        assert_eq!(route.round_robin().base_url, "https://2");
    }

    #[test]
    fn targets_survive_resolution_and_round_robin() {
        use crate::config::RewriteRule;
        let mut target = target("https://1");
        target.peer = "peer".into();
        target.api_key = "sk-real".into();
        target.rewrite = vec![RewriteRule {
            source: "/api".into(),
            destination: "/v1".into(),
        }];
        let routes = Map::new(HashMap::from([(
            "dev".to_string(),
            vec![Entry {
                target: target.clone(),
                models: vec![],
            }],
        )]));
        let route = routes.select("Bearer dev").unwrap();
        assert_eq!(route.round_robin(), target);
    }

    #[test]
    fn concurrent_selection_distributes_evenly() {
        let routes = Arc::new(build_map(&[(
            "dev",
            vec![
                entry("https://1", &[]),
                entry("https://2", &[]),
                entry("https://3", &[]),
            ],
        )]));
        let counts = [
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        ];
        std::thread::scope(|scope| {
            for _ in 0..300 {
                scope.spawn(|| {
                    let route = routes.select("Bearer dev").unwrap();
                    let index = route
                        .round_robin()
                        .base_url
                        .trim_start_matches("https://")
                        .parse::<usize>()
                        .unwrap()
                        - 1;
                    counts[index].fetch_add(1, Ordering::Relaxed);
                });
            }
        });
        assert_eq!(
            counts
                .iter()
                .map(|c| c.load(Ordering::Relaxed))
                .collect::<Vec<_>>(),
            vec![100, 100, 100]
        );
    }
}
