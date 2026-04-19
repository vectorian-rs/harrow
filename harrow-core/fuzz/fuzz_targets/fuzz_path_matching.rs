#![no_main]

use harrow_core::path::PathPattern;
use libfuzzer_sys::fuzz_target;

fn normalize(input: &[u8]) -> String {
    let text = String::from_utf8_lossy(input);
    if text.is_empty() {
        "/".to_string()
    } else if text.starts_with('/') {
        text.into_owned()
    } else {
        format!("/{text}")
    }
}

fuzz_target!(|data: &[u8]| {
    let (pattern_bytes, path_bytes) = if let Some((pattern, path)) = data.split_first() {
        let split = usize::from(*pattern) % (path.len() + 1);
        path.split_at(split)
    } else {
        (&[][..], &[][..])
    };

    let pattern = normalize(pattern_bytes);
    let path = normalize(path_bytes);

    let parsed = PathPattern::parse(&pattern);
    let matched = parsed.match_path(&path);
    let matches = parsed.matches(&path);

    assert_eq!(
        matched.is_some(),
        matches,
        "match_path/matches disagreement for pattern={pattern:?} path={path:?}",
    );

    let _ = parsed.as_str();
    let _ = parsed.as_arc_str();
    let _ = parsed.to_string();
});
