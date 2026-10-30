//! The workspace pins one heartwood commit: every manifest names the same
//! `rev`, and the lockfile resolves every heartwood package to it. A bump
//! that misses a manifest would put two heartwoods in the graph, whose
//! types don't match (the bindings pass the library's `radicle` types).

const MANIFESTS: [(&str, &str); 3] = [
    ("Cargo.toml", include_str!("../Cargo.toml")),
    ("napi/Cargo.toml", include_str!("../napi/Cargo.toml")),
    ("uniffi/Cargo.toml", include_str!("../uniffi/Cargo.toml")),
];
const LOCK: &str = include_str!("../Cargo.lock");
const HEARTWOOD: &str = "https://github.com/solardev-xyz/heartwood";

/// The `rev`s that `text`'s heartwood dependencies name, in order.
fn manifest_revs(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#') && line.contains(HEARTWOOD))
        .map(|line| {
            let (_, rest) = line
                .split_once("rev = \"")
                .unwrap_or_else(|| panic!("a heartwood dependency without a rev: {line}"));
            rest.split('"').next().unwrap_or_default().to_owned()
        })
        .collect()
}

/// The commits the lockfile resolves heartwood packages to.
fn locked_revs() -> Vec<String> {
    let prefix = format!("source = \"git+{HEARTWOOD}?rev=");
    LOCK.lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .map(|rest| {
            let (rev, resolved) = rest
                .trim_end_matches('"')
                .split_once('#')
                .unwrap_or_else(|| panic!("a locked heartwood source without its commit: {rest}"));
            assert!(
                rev.starts_with(resolved),
                "the lockfile resolves rev {rev} to {resolved}"
            );
            rev.to_owned()
        })
        .collect()
}

#[test]
fn one_heartwood_commit_throughout() {
    let mut revs = std::collections::BTreeSet::new();
    for (name, text) in MANIFESTS {
        let found = manifest_revs(text);
        assert!(!found.is_empty(), "{name} names no heartwood dependency");
        revs.extend(found);
    }
    assert_eq!(revs.len(), 1, "the manifests name {revs:?}");
    let rev = revs.pop_first().unwrap_or_default();
    assert_eq!(rev.len(), 40, "a full commit, not {rev}");

    let locked = locked_revs();
    assert!(!locked.is_empty(), "the lockfile has no heartwood package");
    for found in &locked {
        assert_eq!(found, &rev, "a heartwood package locked at another commit");
    }
}
