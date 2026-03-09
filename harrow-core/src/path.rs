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
}
