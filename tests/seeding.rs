//! `Options::seeding_policy`: the node's default seeding policy, forced in
//! memory over the profile's, while `config.json` stays as it was written.
//!
//! Every node here is a test-network node with no seeds, no `connect` peers
//! and a static peer set, and it dials through a gate that refuses every
//! target and resolves no name: it reaches no one, loopback included.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use libradicle::{Embedded, HostKey, Options};
use radicle::node::config::DefaultSeedingPolicy;
use radicle::node::routing::Store as _;
use radicle::node::Handle as _;
use radicle::prelude::{NodeId, RepoId};

/// A gate that refuses every target and resolves no name.
#[derive(Debug)]
struct Refusing;

impl radicle::node::dial::Gate for Refusing {
    fn allows(&self, _: &radicle::node::Address, _: &std::net::SocketAddr) -> bool {
        false
    }

    fn resolve(&self, _: &str, _: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
        Err(std::io::ErrorKind::PermissionDenied.into())
    }
}

/// A scratch directory under TMPDIR, unless that is too deep for the node's
/// Unix socket path.
fn scratch(name: &str) -> PathBuf {
    let name = format!("{name}-{}", std::process::id());
    let mut dir = std::env::temp_dir().join(&name);
    if dir.join("home/node/control.sock").as_os_str().len() > 100 {
        dir = Path::new("/tmp").join(&name);
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A home whose `config.json` seeds every repository by default.
fn home(dir: &Path) -> PathBuf {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.json"),
        r#"{"preferredSeeds":[],"node":{"alias":"seeding","network":"test","connect":[],"peers":{"type":"static"},"seedingPolicy":{"default":"allow","scope":"all"}}}"#,
    )
    .unwrap();
    home
}

/// A working repository with one commit on `main`.
fn work(dir: &Path) -> PathBuf {
    let work = dir.join("work");
    let repo = git2::Repository::init(&work).unwrap();
    std::fs::write(work.join("README"), b"seeding\n").unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Test", "test@example.com").unwrap();
    repo.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "seeding",
        &tree,
        &[],
    )
    .unwrap();
    repo.set_head("refs/heads/main").unwrap();
    work
}

fn start(home: &Path, seeding_policy: Option<DefaultSeedingPolicy>) -> Embedded {
    let mut bytes = vec![43u8; 32];
    common::start_with_key(
        Options {
            home: home.to_path_buf(),
            alias: "seeding".into(),
            listen: vec![],
            control_socket: true,
            routing: Some(common::routing_with(Some(Arc::new(Refusing)))),
            seeding_policy,
            isolated: false,
            repositories: None,
        },
        HostKey::from_bytes(&mut bytes).expect("32 bytes"),
    )
    .expect("start")
}

/// The running node's default seeding policy, as it answers over its control
/// socket. The answer comes from the node's service, so the node has also
/// built its inventory by then.
fn running_policy(home: &Path) -> DefaultSeedingPolicy {
    radicle::node::Node::new(home.join("node/control.sock"))
        .config()
        .expect("the node answers")
        .seeding_policy
}

/// Whether the node seeds `rid`, as it says in its routing table: at start,
/// it puts there every repository in storage that it seeds, and no other.
fn inventoried(home: &Path, rid: RepoId, nid: NodeId) -> bool {
    radicle::profile::Home::new(home.to_path_buf())
        .expect("home")
        .routing(Default::default())
        .expect("routing table")
        .get(&rid)
        .expect("routing entry")
        .contains(&nid)
}

fn listed(node: &Embedded, rid: RepoId) -> bool {
    node.list_repos()
        .expect("list_repos")
        .iter()
        .any(|repo| repo.rid == rid)
}

#[test]
fn a_forced_block_default_wins_over_config_jsons_allow() {
    let dir = scratch("radseeding");
    let home = home(&dir);
    let written = std::fs::read(home.join("config.json")).unwrap();

    // A repository in storage with no seeding policy of its own: imported,
    // which seeds it explicitly, then unseeded, which deletes that policy.
    let mut node = start(&home, Some(DefaultSeedingPolicy::Block));
    let nid: NodeId = node.identity().nid.parse().unwrap();
    let rid = node
        .import_repo(&work(&dir), "seeding", "", "main")
        .expect("import");
    assert!(node.unseed_repo(rid).expect("unseed"));
    node.shutdown().expect("clean shutdown");

    // Forced: the node runs with `block`, and seeds the repository not.
    let node = start(&home, Some(DefaultSeedingPolicy::Block));
    assert_eq!(running_policy(&home), DefaultSeedingPolicy::Block);
    assert!(!inventoried(&home, rid, nid));
    assert!(!listed(&node, rid));
    node.shutdown().expect("clean shutdown");
    // `config.json` is as it was.
    assert_eq!(std::fs::read(home.join("config.json")).unwrap(), written);

    // Not forced: `config.json`'s `allow` applies, and the node seeds it.
    let node = start(&home, None);
    assert!(running_policy(&home).is_allow());
    assert!(inventoried(&home, rid, nid));
    assert!(listed(&node, rid));
    node.shutdown().expect("clean shutdown");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_forced_block_default_wins_over_the_deprecated_policy_fields() {
    let dir = scratch("radseedlegacy");
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    // heartwood lets `node.policy` and `node.scope` override `seedingPolicy`.
    std::fs::write(
        home.join("config.json"),
        r#"{"preferredSeeds":[],"node":{"alias":"seeding","network":"test","connect":[],"peers":{"type":"static"},"seedingPolicy":{"default":"block"},"policy":"allow","scope":"all"}}"#,
    )
    .unwrap();

    let node = start(&home, None);
    assert!(running_policy(&home).is_allow());
    node.shutdown().expect("clean shutdown");

    let node = start(&home, Some(DefaultSeedingPolicy::Block));
    assert_eq!(running_policy(&home), DefaultSeedingPolicy::Block);
    node.shutdown().expect("clean shutdown");

    let _ = std::fs::remove_dir_all(&dir);
}
