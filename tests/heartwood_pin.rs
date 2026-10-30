//! The workspace pins one heartwood commit, everywhere:
//! - **every workspace manifest** (each member the root's `members` lists)
//!   names heartwood only as a git dependency on its canonical URL, at one
//!   full commit, and has no `[patch]` that could redirect it. A manifest
//!   may name none;
//! - **the lockfile** has exactly the heartwood packages below, each once,
//!   each from the canonical URL at that commit, resolved to that commit. A
//!   package missing, added, or locked from another source or commit fails.
//!
//! A bump that missed a manifest would put two heartwoods in the graph,
//! whose types don't match (the bindings pass the library's `radicle`
//! types).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const HEARTWOOD: &str = "https://github.com/solardev-xyz/heartwood";

/// The heartwood packages the graph has: every one the lockfile resolves
/// from heartwood, at this pin.
const PACKAGES: [&str; 16] = [
    "radicle",
    "radicle-cob",
    "radicle-core",
    "radicle-crypto",
    "radicle-dag",
    "radicle-fetch",
    "radicle-git-metadata",
    "radicle-git-ref-format",
    "radicle-localtime",
    "radicle-log",
    "radicle-node",
    "radicle-oid",
    "radicle-protocol",
    "radicle-signals",
    "radicle-surf",
    "radicle-windows",
];

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The workspace's members, as the root manifest's `members` lists them.
fn members(root: &str) -> Vec<String> {
    let line = root
        .lines()
        .find(|line| line.trim_start().starts_with("members"))
        .expect("the root manifest lists its members");
    let (_, list) = line.split_once('[').expect("members = [ … ]");
    let (list, _) = list.split_once(']').expect("members = [ … ] on one line");
    list.split(',')
        .map(|member| member.trim().trim_matches('"').to_owned())
        .filter(|member| !member.is_empty())
        .collect()
}

/// The commits `manifest`'s heartwood dependencies name, refusing any other
/// form: a git dependency elsewhere, a heartwood line without a full `rev`,
/// or a `[patch]` section.
fn manifest_revs(name: &str, manifest: &str) -> Vec<String> {
    let mut revs = Vec::new();
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        assert!(
            !line.starts_with("[patch"),
            "{name} has a [patch] section: {line}"
        );
        if !(line.contains("git =") || line.contains("heartwood")) {
            continue;
        }
        assert!(
            line.contains(&format!("git = \"{HEARTWOOD}\"")),
            "{name}: a git dependency that isn't heartwood's canonical URL: {line}"
        );
        let (_, rest) = line
            .split_once("rev = \"")
            .unwrap_or_else(|| panic!("{name}: a heartwood dependency without a rev: {line}"));
        let rev = rest.split('"').next().unwrap_or_default();
        assert!(
            rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()),
            "{name}: not a full commit: {rev}"
        );
        revs.push(rev.to_owned());
    }
    revs
}

/// The lockfile's packages, by record: name to every (version, source) it
/// is locked at. A record without a source (a workspace member) has `None`.
fn locked(lock: &str) -> BTreeMap<String, Vec<(String, Option<String>)>> {
    let mut packages: BTreeMap<String, Vec<(String, Option<String>)>> = BTreeMap::new();
    for record in lock.split("[[package]]").skip(1) {
        let field = |key: &str| {
            record.lines().find_map(|line| {
                line.strip_prefix(key)
                    .and_then(|rest| rest.strip_prefix(" = \""))
                    .and_then(|rest| rest.strip_suffix('"'))
                    .map(str::to_owned)
            })
        };
        let name = field("name").expect("a package record has a name");
        let version = field("version").expect("a package record has a version");
        packages
            .entry(name)
            .or_default()
            .push((version, field("source")));
    }
    packages
}

/// What every heartwood package's record must have as its source.
fn source(rev: &str) -> String {
    format!("git+{HEARTWOOD}?rev={rev}#{rev}")
}

/// Checks the workspace at `root`: every member's manifest, then the
/// lockfile, against the one commit they name.
fn check(root: &Path) {
    let manifest = read(&root.join("Cargo.toml"));
    let mut revs = BTreeSet::new();
    for member in members(&manifest) {
        let path = root.join(&member).join("Cargo.toml");
        revs.extend(manifest_revs(&member, &read(&path)));
    }
    assert_eq!(revs.len(), 1, "the manifests name {revs:?}");
    let rev = revs.pop_first().unwrap_or_default();

    let packages = locked(&read(&root.join("Cargo.lock")));
    let expected = source(&rev);
    let heartwood: BTreeSet<&str> = packages
        .iter()
        .filter(|(name, records)| {
            name.starts_with("radicle")
                || records
                    .iter()
                    .any(|(_, source)| source.as_deref().is_some_and(|s| s.contains("heartwood")))
        })
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        heartwood,
        PACKAGES.into_iter().collect::<BTreeSet<_>>(),
        "the lockfile's heartwood packages"
    );
    for name in PACKAGES {
        let records = &packages[name];
        assert_eq!(records.len(), 1, "{name} is locked {} times", records.len());
        let (version, source) = &records[0];
        assert_eq!(
            source.as_deref(),
            Some(expected.as_str()),
            "{name} {version} isn't locked from heartwood at {rev}"
        );
    }
}

#[test]
fn one_heartwood_commit_throughout() {
    check(Path::new(env!("CARGO_MANIFEST_DIR")));
}
