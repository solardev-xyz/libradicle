//! The workspace pins one heartwood commit, and takes nothing else from git,
//! as cargo itself resolves it (`cargo metadata --locked --offline`), not as
//! the manifests happen to be spelled:
//! - **every dependency a workspace member declares with a git source**
//!   names heartwood's canonical URL at one full commit, whatever its name,
//!   alias, table or target;
//! - **every package in the graph with a git source** is one of the
//!   heartwood packages below, from that URL at that commit, resolved to
//!   it, each once; and every one of the 16 is there;
//! - **every other package** is from crates.io, or is a workspace member.
//!
//! A bump that missed a manifest would put two heartwoods in the graph,
//! whose types don't match (the bindings pass the library's `radicle`
//! types). `deny.toml` refuses any other git source in CI too (`[sources]`).

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use serde_json::Value;

const HEARTWOOD: &str = "https://github.com/solardev-xyz/heartwood";
const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// The heartwood packages the graph has, at this pin.
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

/// The workspace as cargo resolves it from its manifests and its lockfile,
/// without touching the network: a git source not already fetched fails.
fn metadata() -> Value {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata's JSON")
}

/// The commit a git source names: `git+<url>?rev=<rev>`, with `#<commit>`
/// for a resolved package. Anything else at `url` is `None`.
fn rev_of<'a>(source: &'a str, url: &str) -> Option<&'a str> {
    let rest = source.strip_prefix("git+")?.strip_prefix(url)?;
    rest.strip_prefix("?rev=")
}

fn full_commit(rev: &str) -> bool {
    rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Checks the resolved workspace `metadata`: the commit every heartwood
/// source names, or the reason it fails.
fn check(metadata: &Value) -> Result<String, String> {
    let members: BTreeSet<&str> = metadata["workspace_members"]
        .as_array()
        .ok_or("no workspace members")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let packages = metadata["packages"].as_array().ok_or("no packages")?;

    // What the members declare: every git dependency, whatever its name.
    let mut revs = BTreeSet::new();
    for package in packages {
        let id = package["id"].as_str().unwrap_or_default();
        if !members.contains(id) {
            continue;
        }
        let member = package["name"].as_str().unwrap_or_default();
        for dependency in package["dependencies"].as_array().into_iter().flatten() {
            let Some(source) = dependency["source"].as_str() else {
                continue;
            };
            if source == CRATES_IO {
                continue;
            }
            let name = dependency["rename"]
                .as_str()
                .or(dependency["name"].as_str())
                .unwrap_or_default();
            let rev = rev_of(source, HEARTWOOD)
                .filter(|rev| full_commit(rev))
                .ok_or_else(|| {
                    format!(
                        "{member} depends on {name} from {source}, not heartwood at a full commit"
                    )
                })?;
            revs.insert(rev.to_owned());
        }
    }
    if revs.len() != 1 {
        return Err(format!("the members' heartwood dependencies name {revs:?}"));
    }
    let rev = revs.pop_first().unwrap_or_default();

    // What the graph has: heartwood's packages at that commit, and nothing
    // else from git.
    let expected = format!("git+{HEARTWOOD}?rev={rev}#{rev}");
    let mut heartwood: BTreeMap<&str, usize> = BTreeMap::new();
    for package in packages {
        let name = package["name"].as_str().unwrap_or_default();
        let version = package["version"].as_str().unwrap_or_default();
        match package["source"].as_str() {
            None if members.contains(package["id"].as_str().unwrap_or_default()) => {}
            None => {
                return Err(format!(
                    "{name} {version} is a path package outside the workspace"
                ))
            }
            Some(CRATES_IO) => {}
            Some(source) if source == expected => *heartwood.entry(name).or_default() += 1,
            Some(source) => return Err(format!("{name} {version} is from {source}")),
        }
    }
    let found: BTreeSet<&str> = heartwood.keys().copied().collect();
    let wanted: BTreeSet<&str> = PACKAGES.into_iter().collect();
    if found != wanted {
        return Err(format!(
            "heartwood's packages: missing {:?}, unexpected {:?}",
            wanted.difference(&found).collect::<Vec<_>>(),
            found.difference(&wanted).collect::<Vec<_>>()
        ));
    }
    if let Some((name, n)) = heartwood.iter().find(|(_, n)| **n != 1) {
        return Err(format!("{name} is in the graph {n} times"));
    }
    Ok(rev)
}

#[test]
fn one_heartwood_commit_throughout() {
    let rev = check(&metadata()).unwrap_or_else(|e| panic!("{e}"));
    assert!(full_commit(&rev), "{rev}");
}

#[test]
fn rev_of_reads_only_heartwoods_url() {
    let rev = "6b2dc16fa657bbcb03c34eaee922d354fb1fe66e";
    assert_eq!(
        rev_of(&format!("git+{HEARTWOOD}?rev={rev}"), HEARTWOOD),
        Some(rev)
    );
    for other in [
        format!("git+{HEARTWOOD}-fork?rev={rev}"),
        format!("git+https://github.com/example/hw?rev={rev}"),
        format!("git+{HEARTWOOD}?branch=main"),
        format!("git+{HEARTWOOD}"),
        format!("registry+{HEARTWOOD}"),
    ] {
        assert!(
            rev_of(&other, HEARTWOOD).is_none_or(|rev| !full_commit(rev)),
            "{other}"
        );
    }
}
