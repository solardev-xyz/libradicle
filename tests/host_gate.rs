//! The host's repository gate, and repairs and removals with the node's own
//! key (freedom-core design 0008, libradicle item 9a):
//! - `Options::repositories` is the gate the node consults: a repository
//!   the host quarantines there isn't served, while another of the same
//!   node is;
//! - `Embedded::repair_step` re-signs the node's own namespace with the key
//!   the node runs as, and writes no key file;
//! - `Embedded::remove_quarantined` checks the record's `qid` before it
//!   touches anything, then removes the repository, its seeding policy and
//!   its record.

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::time::Duration;

use libradicle::{Embedded, HostKey, Options};
use radicle::git::raw as git2;
use radicle::node::gate::{Quarantine as Shut, RepoGate, RepoState};
use radicle::node::Handle as _;
use radicle::prelude::{NodeId, RepoId};
use radicle::storage::git::reconcile::driver::{Progress, Stop};
use radicle::storage::git::reconcile::quarantine::{
    Kind, Qid, Quarantine, Removal, QUARANTINE_DB_FILE,
};
use radicle::storage::git::reconcile::state::State;

fn key(byte: u8) -> HostKey {
    let mut bytes = vec![byte; 32];
    HostKey::from_bytes(&mut bytes).expect("32 bytes")
}

/// A home whose node dials no one by itself: the test network, a static
/// peer set, and no preferred seeds (one would bring the shipped seed book
/// after it). Under TMPDIR, unless that is too deep for a Unix socket path.
fn home(name: &str) -> PathBuf {
    let name = format!("{name}-{}", std::process::id());
    let mut home = std::env::temp_dir().join(&name);
    if home.join("node/control.sock").as_os_str().len() > 100 {
        home = Path::new("/tmp").join(&name);
    }
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.json"),
        r#"{"preferredSeeds":[],"node":{"alias":"gate","network":"test","connect":[],"peers":{"type":"static"}}}"#,
    )
    .unwrap();
    home
}

fn start(
    home: &Path,
    byte: u8,
    listen: Vec<SocketAddr>,
    repositories: Option<RepoGate>,
) -> Embedded {
    Embedded::start_with_key(
        Options {
            home: home.to_path_buf(),
            alias: "gate".into(),
            listen,
            control_socket: true,
            routing: Some(common::routing()),
            isolated: false,
            repositories,
        },
        key(byte),
    )
    .expect("start")
}

/// A working copy with one commit on `master`, under TMPDIR.
fn working_copy(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{name}-work-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let repo = git2::Repository::init(&dir).unwrap();
    std::fs::write(dir.join("README.md"), "# Gate\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Gate", "gate@example.invalid").unwrap();
    repo.commit(
        Some("refs/heads/master"),
        &signature,
        &signature,
        "First",
        &tree,
        &[],
    )
    .unwrap();
    dir
}

fn import(node: &mut Embedded, name: &str) -> RepoId {
    let work = working_copy(name);
    let rid = node
        .import_repo(&work, name, "A gate test.", "master")
        .expect("import");
    let _ = std::fs::remove_dir_all(&work);
    rid
}

fn storage_of(home: &Path, rid: RepoId) -> PathBuf {
    home.join("storage").join(rid.canonical())
}

/// The node's control socket in `home`.
fn control(home: &Path) -> radicle::node::Node {
    radicle::node::Node::new(home.join("node/control.sock"))
}

/// Fetches `rid` from `from` through `home`'s node, as a clone does.
fn fetch(home: &Path, rid: RepoId, from: NodeId) -> radicle::node::FetchResult {
    let mut node = control(home);
    node.seed(rid, radicle::node::policy::Scope::All)
        .expect("seed");
    node.fetch(rid, from, Duration::from_secs(20), None)
        .expect("a fetch's answer")
}

#[test]
// Both nodes have static peer sets and no seeds, so neither dials anyone
// but the other: nothing reaches the network.
fn a_repository_the_host_quarantines_isnt_served_and_another_is() {
    let gate = RepoGate::new();
    let seed_home = home("gate-seed");
    let mut seed = start(
        &seed_home,
        21,
        vec![([127, 0, 0, 1], 0).into()],
        Some(gate.clone()),
    );
    let open = import(&mut seed, "open");
    let shut = import(&mut seed, "shut");
    gate.close(
        shut,
        RepoState::Quarantined(Shut::Namespace),
        Duration::from_secs(5),
    )
    .expect("the gate closes");
    let addr = control(&seed_home)
        .listen_addrs()
        .expect("listen addresses")
        .into_iter()
        .next()
        .expect("a listen address");
    let nid = NodeId::from_str(&seed.identity().nid).unwrap();

    let peer_home = home("gate-peer");
    let peer = start(&peer_home, 22, vec![], None);
    let connected = control(&peer_home)
        .connect(
            nid,
            addr.into(),
            radicle::node::ConnectOptions {
                persistent: false,
                timeout: Duration::from_secs(10),
            },
        )
        .expect("a connect's answer");
    assert!(
        matches!(connected, radicle::node::ConnectResult::Connected),
        "{connected:?}"
    );

    // The seed serves the repository its host left open.
    let served = fetch(&peer_home, open, nid);
    assert!(served.is_success(), "{served:?}");
    assert!(storage_of(&peer_home, open).is_dir());
    // And not the one its host quarantined: the node refuses the upload.
    let refused = fetch(&peer_home, shut, nid);
    assert!(!refused.is_success(), "a quarantined repository was served");
    assert!(!storage_of(&peer_home, shut).exists());

    peer.shutdown().expect("clean shutdown");
    seed.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&peer_home);
    let _ = std::fs::remove_dir_all(&seed_home);
}

#[test]
fn an_online_repair_resigns_with_the_nodes_key_and_writes_no_key_file() {
    let gate = RepoGate::new();
    let home = home("gate-repair");
    let mut node = start(&home, 23, vec![], Some(gate.clone()));
    let rid = import(&mut node, "repaired");
    let nid = node.identity().nid;

    // A kill between a ref's update and its signing, as an uncertain write
    // can leave.
    let repo = git2::Repository::open_bare(storage_of(&home, rid)).unwrap();
    let head = repo
        .refname_to_id(&format!("refs/namespaces/{nid}/refs/heads/master"))
        .unwrap();
    repo.reference(
        &format!("refs/namespaces/{nid}/refs/heads/unsigned"),
        head,
        true,
        "test",
    )
    .unwrap();
    let sigrefs = format!("refs/namespaces/{nid}/refs/rad/sigrefs");
    let before = repo.refname_to_id(&sigrefs).unwrap();

    gate.close(rid, RepoState::Repairing, Duration::from_secs(5))
        .expect("the gate closes");
    let mut state = State::open(home.join("cobs").join("cache.db")).unwrap();
    let mut quarantine = Quarantine::open(home.join("node").join(QUARANTINE_DB_FILE)).unwrap();
    let mut repair = node.online_repair(rid);
    let mut units = 0;
    loop {
        units += 1;
        assert!(units < 1_000, "the repair didn't end");
        match node
            .repair_step(
                &mut repair,
                &mut state,
                &mut quarantine,
                &Stop::default(),
                &(),
            )
            .expect("a unit")
        {
            Progress::More => {}
            Progress::Done => break,
            other => panic!("the repair ended {other:?}"),
        }
    }
    gate.open(rid);

    assert!(repair.did().resigned);
    assert!(repair.did().cache);
    assert_ne!(repo.refname_to_id(&sigrefs).unwrap(), before);
    assert_eq!(quarantine.count().unwrap(), 0);
    assert!(!home.join("keys").join("radicle").exists());

    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_removal_checks_its_qid_then_removes_the_repository_and_its_record() {
    let gate = RepoGate::new();
    let home = home("gate-remove");
    let mut node = start(&home, 24, vec![], Some(gate.clone()));
    let rid = import(&mut node, "removed");
    let mut quarantine = Quarantine::open(home.join("node").join(QUARANTINE_DB_FILE)).unwrap();
    let record = quarantine
        .quarantine(
            rid,
            Kind::Namespace,
            "test",
            None,
            "a test",
            Default::default(),
        )
        .unwrap();
    gate.close(
        rid,
        RepoState::Quarantined(Shut::Namespace),
        Duration::from_secs(5),
    )
    .expect("the gate closes");

    // Another record's ID: nothing is touched.
    let other = Qid::from_str(&format!("{:032x}", 1)).unwrap();
    assert_ne!(other, record.qid);
    assert_eq!(
        node.remove_quarantined(rid, other, &mut quarantine)
            .unwrap(),
        Removal::NotQuarantined
    );
    assert!(storage_of(&home, rid).is_dir());
    assert!(quarantine.get(&rid).unwrap().is_some());
    assert!(node
        .list_seeded_repos()
        .unwrap()
        .iter()
        .any(|repo| repo.rid == rid));

    assert_eq!(
        node.remove_quarantined(rid, record.qid, &mut quarantine)
            .unwrap(),
        Removal::Removed
    );
    assert!(!storage_of(&home, rid).exists());
    assert!(quarantine.get(&rid).unwrap().is_none());
    assert!(!node
        .list_seeded_repos()
        .unwrap()
        .iter()
        .any(|repo| repo.rid == rid));
    // Done once: the record is gone, so a delayed duplicate does nothing.
    assert_eq!(
        node.remove_quarantined(rid, record.qid, &mut quarantine)
            .unwrap(),
        Removal::NotQuarantined
    );

    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}
