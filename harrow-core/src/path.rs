use std::fmt;
use std::sync::Arc;

/// A parsed path pattern such as `/users/:id` or `/files/*path`.
#[derive(Clone)]
pub struct PathPattern {
    raw: Arc<str>,
    segments: Vec<Segment>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Segment {
    /// Exact literal match, e.g. `users`.
    Literal(String),
    /// Named parameter, e.g. `:id`.
    Param(String),
    /// Wildcard glob capturing the rest, e.g. `*path`. Must be last.
    Glob(String),
}

/// The result of matching a request path against a pattern.
///
/// Stores captured parameters as a flat `Vec<(name, value)>` instead of a
/// `HashMap`. Most routes have 0–2 params; linear scan on a Vec of that
/// size is faster than hashing and avoids the ~112 B HashMap bucket
/// allocation entirely. For 0-param routes this is zero-alloc.
#[derive(Debug, Default)]
pub struct PathMatch {
    params: Vec<(String, String)>,
}

impl PathMatch {
    /// Get a captured parameter by name. Linear scan — O(n) where n is
    /// typically 0–2.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self {
            params: Vec::with_capacity(cap),
        }
    }

    pub(crate) fn push(&mut self, name: String, value: String) {
        self.params.push((name, value));
    }
}

impl PathPattern {
    /// Parse a pattern string like `/users/:id` or `/files/*path`.
    pub fn parse(pattern: &str) -> Self {
        let raw: Arc<str> = pattern.into();
        let segments = pattern
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| {
                if let Some(name) = s.strip_prefix(':') {
                    Segment::Param(name.to_string())
                } else if let Some(name) = s.strip_prefix('*') {
                    Segment::Glob(name.to_string())
                } else {
                    Segment::Literal(s.to_string())
                }
            })
            .collect();

        Self { raw, segments }
    }

    /// Attempt to match a request path against this pattern.
    /// Returns `Some(PathMatch)` with captured params on success, `None` on failure.
    ///
    /// Uses an iterator over path segments instead of collecting into a Vec,
    /// so the only heap allocations are the param name/value Strings themselves.
    #[cfg_attr(feature = "profiling", inline(never))]
    pub fn match_path(&self, path: &str) -> Option<PathMatch> {
        let trimmed = path.trim_start_matches('/');
        let mut path_segments = trimmed.split('/').filter(|s| !s.is_empty());
        let mut params = Vec::new();

        for seg in &self.segments {
            match seg {
                Segment::Literal(lit) => {
                    if path_segments.next()? != lit.as_str() {
                        return None;
                    }
                }
                Segment::Param(name) => {
                    let val = path_segments.next()?;
                    params.push((name.clone(), val.to_string()));
                }
                Segment::Glob(name) => {
                    // Collect remaining segments for the glob.
                    let rest: Vec<&str> = path_segments.collect();
                    params.push((name.clone(), rest.join("/")));
                    return Some(PathMatch { params });
                }
            }
        }

        // Reject if there are unconsumed path segments.
        if path_segments.next().is_some() {
            return None;
        }

        Some(PathMatch { params })
    }

    /// Returns `true` if the path matches this pattern, without capturing
    /// parameters. Used for 405 detection where we only care about existence.
    /// Zero-alloc.
    pub fn matches(&self, path: &str) -> bool {
        let trimmed = path.trim_start_matches('/');
        let mut path_segments = trimmed.split('/').filter(|s| !s.is_empty());

        for seg in &self.segments {
            match seg {
                Segment::Literal(lit) => match path_segments.next() {
                    Some(s) if s == lit.as_str() => {}
                    _ => return false,
                },
                Segment::Param(_) => {
                    if path_segments.next().is_none() {
                        return false;
                    }
                }
                Segment::Glob(_) => {
                    return true;
                }
            }
        }

        path_segments.next().is_none()
    }

    /// The raw pattern string as provided.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Return a cheap `Arc<str>` clone of the raw pattern.
    pub fn as_arc_str(&self) -> Arc<str> {
        Arc::clone(&self.raw)
    }
}

/// Translate harrow pattern syntax to matchit syntax: `:id` → `{id}`, `*path` → `{*path}`.
pub(crate) fn to_matchit_pattern(pattern: &str) -> String {
    pattern
        .split('/')
        .map(|seg| {
            if let Some(name) = seg.strip_prefix(':') {
                format!("{{{name}}}")
            } else if let Some(name) = seg.strip_prefix('*') {
                format!("{{*{name}}}")
            } else {
                seg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

impl fmt::Display for PathPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl fmt::Debug for PathPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PathPattern({:?})", self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn exact_match() {
        let p = PathPattern::parse("/users");
        assert!(p.match_path("/users").is_some());
        assert!(p.match_path("/users/").is_some());
        assert!(p.match_path("/other").is_none());
        assert!(p.match_path("/users/123").is_none());
    }

    #[test]
    fn param_match() {
        let p = PathPattern::parse("/users/:id");
        let m = p.match_path("/users/42").unwrap();
        assert_eq!(m.get("id"), Some("42"));
        assert!(p.match_path("/users").is_none());
        assert!(p.match_path("/users/42/extra").is_none());
    }

    #[test]
    fn multi_param() {
        let p = PathPattern::parse("/orgs/:org/repos/:repo");
        let m = p.match_path("/orgs/acme/repos/widget").unwrap();
        assert_eq!(m.get("org"), Some("acme"));
        assert_eq!(m.get("repo"), Some("widget"));
    }

    #[test]
    fn glob_match() {
        let p = PathPattern::parse("/files/*path");
        let m = p.match_path("/files/a/b/c.txt").unwrap();
        assert_eq!(m.get("path"), Some("a/b/c.txt"));
    }

    #[test]
    fn root_match() {
        let p = PathPattern::parse("/");
        assert!(p.match_path("/").is_some());
    }

    #[test]
    fn matches_without_capture() {
        let p = PathPattern::parse("/users/:id");
        assert!(p.matches("/users/42"));
        assert!(!p.matches("/users"));
        assert!(!p.matches("/other/42"));
    }

    #[test]
    fn matches_glob_without_capture() {
        let p = PathPattern::parse("/files/*path");
        assert!(p.matches("/files/a/b/c.txt"));
        assert!(p.matches("/files/x"));
    }

    // -----------------------------------------------------------------------
    // proptest strategies
    // -----------------------------------------------------------------------

    fn arb_literal() -> impl Strategy<Value = Segment> {
        "[a-z]{1,8}".prop_map(Segment::Literal)
    }

    fn arb_param() -> impl Strategy<Value = Segment> {
        "[a-z]{1,5}".prop_map(Segment::Param)
    }

    fn arb_glob() -> impl Strategy<Value = Segment> {
        "[a-z]{1,5}".prop_map(Segment::Glob)
    }

    fn arb_non_glob_segment() -> impl Strategy<Value = Segment> {
        prop_oneof![arb_literal(), arb_param(),]
    }

    /// Generate a valid pattern: 1-4 segments, glob only at end if present.
    /// Param/glob names are made unique by appending a positional suffix.
    fn arb_pattern() -> impl Strategy<Value = Vec<Segment>> {
        // 80% without glob, 20% with glob at end
        prop_oneof![
            4 => prop::collection::vec(arb_non_glob_segment(), 1..=4)
                .prop_map(uniquify_names),
            1 => (prop::collection::vec(arb_non_glob_segment(), 0..=3), arb_glob())
                .prop_map(|(mut segs, glob)| { segs.push(glob); uniquify_names(segs) }),
        ]
    }

    /// Make param/glob names unique by appending a positional index.
    fn uniquify_names(segments: Vec<Segment>) -> Vec<Segment> {
        segments
            .into_iter()
            .enumerate()
            .map(|(i, seg)| match seg {
                Segment::Param(name) => Segment::Param(format!("{name}{i}")),
                Segment::Glob(name) => Segment::Glob(format!("{name}{i}")),
                other => other,
            })
            .collect()
    }

    /// Build a PathPattern from a Vec<Segment>.
    fn pattern_from_segments(segments: &[Segment]) -> PathPattern {
        let raw: String = segments
            .iter()
            .map(|s| match s {
                Segment::Literal(l) => format!("/{l}"),
                Segment::Param(p) => format!("/:{p}"),
                Segment::Glob(g) => format!("/*{g}"),
            })
            .collect();
        let raw = if raw.is_empty() { "/".to_string() } else { raw };
        PathPattern::parse(&raw)
    }

    /// Generate a path that should match the given pattern.
    fn arb_path_for_pattern(segments: &[Segment]) -> BoxedStrategy<String> {
        let strategies: Vec<BoxedStrategy<String>> = segments
            .iter()
            .map(|seg| match seg {
                Segment::Literal(lit) => Just(lit.clone()).boxed(),
                Segment::Param(_) => "[a-z0-9]{1,8}".prop_map(|s| s).boxed(),
                Segment::Glob(_) => prop::collection::vec("[a-z0-9]{1,8}", 0..=3)
                    .prop_map(|parts| parts.join("/"))
                    .boxed(),
            })
            .collect();

        strategies
            .into_iter()
            .fold(Just(String::new()).boxed(), |acc, seg_strat| {
                (acc, seg_strat)
                    .prop_map(|(mut path, seg)| {
                        path.push('/');
                        path.push_str(&seg);
                        path
                    })
                    .boxed()
            })
    }

    /// Generate a random path for agreement tests.
    fn arb_random_path() -> impl Strategy<Value = String> {
        prop::collection::vec("[a-z0-9]{1,8}", 0..=5).prop_map(|parts| {
            if parts.is_empty() {
                "/".to_string()
            } else {
                format!("/{}", parts.join("/"))
            }
        })
    }

    // -----------------------------------------------------------------------
    // proptest properties
    // -----------------------------------------------------------------------

    proptest! {
        /// match_path and matches must always agree.
        #[test]
        fn proptest_match_path_matches_agreement(
            segments in arb_pattern(),
            path in arb_random_path(),
        ) {
            let pattern = pattern_from_segments(&segments);
            let has_match = pattern.match_path(&path).is_some();
            let matches = pattern.matches(&path);
            prop_assert_eq!(
                has_match, matches,
                "disagreement on pattern={} path={}", pattern.as_str(), path,
            );
        }

        /// For a pattern with params and a matching path, captured values
        /// correspond to the path segments at the param positions.
        #[test]
        fn proptest_param_capture_correctness(segments in arb_pattern()) {
            let pattern = pattern_from_segments(&segments);
            // Use a deterministic matching path
            let path_strategy = arb_path_for_pattern(&segments);
            proptest::test_runner::TestRunner::default()
                .run(&path_strategy, |path| {
                    if let Some(m) = pattern.match_path(&path) {
                        let path_parts: Vec<&str> = path
                            .trim_start_matches('/')
                            .split('/')
                            .filter(|s| !s.is_empty())
                            .collect();
                        let mut pi = 0;
                        for seg in &segments {
                            match seg {
                                Segment::Literal(_) => { pi += 1; }
                                Segment::Param(name) => {
                                    prop_assert_eq!(
                                        m.get(name).unwrap(),
                                        path_parts[pi],
                                        "param {} mismatch", name,
                                    );
                                    pi += 1;
                                }
                                Segment::Glob(name) => {
                                    let expected = path_parts[pi..].join("/");
                                    prop_assert_eq!(
                                        m.get(name).unwrap(),
                                        &expected,
                                        "glob {} mismatch", name,
                                    );
                                }
                            }
                        }
                    }
                    Ok(())
                })?;
        }

        /// A path whose first literal differs from the pattern never matches.
        #[test]
        fn proptest_non_matching_first_literal(
            segments in arb_pattern(),
        ) {
            // Only test patterns that start with a literal
            if let Some(Segment::Literal(lit)) = segments.first() {
                let pattern = pattern_from_segments(&segments);
                // Build a path that starts with a different literal
                let bad_first = format!("{}x", lit);
                let bad_path = format!("/{}", bad_first);
                prop_assert!(
                    pattern.match_path(&bad_path).is_none(),
                    "pattern={} should not match path={}",
                    pattern.as_str(),
                    bad_path,
                );
            }
        }

        /// For patterns ending in a glob, the glob value equals
        /// the joined remaining path segments.
        #[test]
        fn proptest_glob_captures_remainder(
            segments in (prop::collection::vec(arb_non_glob_segment(), 0..=2), arb_glob())
                .prop_map(|(mut segs, glob)| { segs.push(glob); uniquify_names(segs) }),
        ) {
            let pattern = pattern_from_segments(&segments);
            let path_strategy = arb_path_for_pattern(&segments);
            proptest::test_runner::TestRunner::default()
                .run(&path_strategy, |path| {
                    if let Some(m) = pattern.match_path(&path)
                        && let Some(Segment::Glob(name)) = segments.last()
                    {
                        let path_parts: Vec<&str> = path
                            .trim_start_matches('/')
                            .split('/')
                            .filter(|s| !s.is_empty())
                            .collect();
                        // Glob starts after all non-glob segments
                        let prefix_len = segments.len() - 1;
                        let expected = path_parts[prefix_len..].join("/");
                        prop_assert_eq!(
                            m.get(name).unwrap(),
                            &expected,
                            "glob remainder mismatch",
                        );
                    }
                    Ok(())
                })?;
        }
    }
}
