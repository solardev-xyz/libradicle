//! The calls that wait on the node's reactor for an answer wait within a
//! bound: `connected_peers` and `unseed_repo` (freedom-core #340), and
//! `seeders`, a COB change's announcement and `remove_quarantined`'s unseed
//! (freedom-core #363). A wedged reactor answers none: each call returns
//! `NodeDeadline` at its bound, and `waiting_on_node` names it. A COB change
//! stands in storage either way, and a removal cut off at its unseed keeps
//! its record, and completes when run again. At most
//! `NODE_WAIT_LIMIT` calls wait at once, within their bounds or past them,
//! even for callers that all arrive together: a call past those isn't sent,
//! and fails at once. When the reactor comes back, it answers the calls left
//! waiting, and they end.
//!
//! The reactor is wedged through the node's dial gate, which it asks on its
//! own thread before every dial: this test's gate doesn't return until the
//! test lets it.

mod common;

use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::time::{Duration, Instant};

use libradicle::{Embedded, Error, HostKey, Options, NODE_WAIT_LIMIT};
use radicle::git::raw as git2;
use radicle::identity::RepoId;
use radicle::node::gate::{Quarantine as Shut, RepoGate, RepoState};
use radicle::storage::git::reconcile::quarantine::{Kind, Quarantine, Removal, QUARANTINE_DB_FILE};

/// A dial gate that holds the reactor in its first ask until it's let go,
/// then refuses every dial.
#[derive(Debug, Default)]
struct Wedge {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl Wedge {
    /// Waits until the reactor is held.
    fn wait_held(&self, within: Duration) {
        let (state, timeout) = self
            .changed
            .wait_timeout_while(self.state.lock().unwrap(), within, |(held, _)| !*held)
            .unwrap();
        assert!(
            state.0,
            "the reactor wasn't held within {within:?}: {timeout:?}"
        );
    }

    fn let_go(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

impl radicle::node::dial::Gate for Wedge {
    fn allows(&self, _: &radicle::node::Address, _: &std::net::SocketAddr) -> bool {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        let _released = self
            .changed
            .wait_while(state, |(_, let_go)| !*let_go)
            .unwrap();
        false
    }

    fn resolve(&self, _: &str, _: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
        Err(std::io::ErrorKind::PermissionDenied.into())
    }
}

/// A home on the test network, with a static peer set and one preferred
/// seed at a loopback address, for the gate to be asked about.
fn home() -> PathBuf {
    let home = std::env::temp_dir().join(format!("lr-waits-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.json"),
        r#"{"preferredSeeds":["z6MktwupdmLXVVqTzCw4i46r4uGyosGXRnR3XjN4Zq7oMMsw@127.0.0.1:1"],"node":{"alias":"waits","network":"test","connect":[],"peers":{"type":"static"}}}"#,
    )
    .unwrap();
    home
}

fn start(home: &Path, gate: Arc<Wedge>, repositories: RepoGate) -> Embedded {
    let mut bytes = vec![71u8; 32];
    common::start_with_key(
        Options {
            home: home.to_path_buf(),
            alias: "waits".into(),
            listen: vec![],
            control_socket: false,
            routing: Some(common::routing_with(Some(gate))),
            seeding_policy: None,
            isolated: false,
            repositories: Some(repositories),
        },
        HostKey::from_bytes(&mut bytes).expect("32 bytes"),
    )
    .expect("start")
}

/// A repository named `name` imported into `node`'s storage, from a working
/// copy with one commit on `master`.
fn import(node: &mut Embedded, name: &str) -> RepoId {
    let work = std::env::temp_dir().join(format!("lr-waits-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let repo = git2::Repository::init(&work).unwrap();
    std::fs::write(work.join("README.md"), "# Waits\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Waits", "waits@example.invalid").unwrap();
    repo.commit(
        Some("refs/heads/master"),
        &signature,
        &signature,
        "First",
        &tree,
        &[],
    )
    .unwrap();
    let rid = node
        .import_repo(&work, name, "A waits test.", "master")
        .expect("import");
    let _ = std::fs::remove_dir_all(&work);
    rid
}

/// The titles of the issues in `rid`, read from storage.
fn issue_titles(node: &Embedded, rid: RepoId) -> Vec<String> {
    let mut titles = node
        .issues(rid)
        .expect("the issues")
        .into_iter()
        .map(|(_, issue)| issue.title().to_string())
        .collect::<Vec<_>>();
    titles.sort();
    titles
}

/// The threads waiting on the node for a caller: on Linux, by their name;
/// elsewhere, the calls the node lists.
fn helpers(node: &Embedded) -> usize {
    #[cfg(target_os = "linux")]
    {
        let _ = node;
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
            .filter(|name| name.trim_end() == "radicle-wait")
            .count()
    }
    #[cfg(not(target_os = "linux"))]
    {
        node.waiting_on_node().len()
    }
}

/// Waits up to `within` for `done`, or panics naming `what`.
fn wait_for(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < within, "no {what} within {within:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_wedged_reactor_answers_no_call_past_its_bound() {
    let home = home();
    let gate = Arc::new(Wedge::default());
    let repositories = RepoGate::new();
    let mut node = start(&home, Arc::clone(&gate), repositories.clone());
    let rid = RepoId::from_str("rad:z3gqcJUoA1n9HaHKufZs5FCSGazv5").unwrap();
    let bound = Duration::from_millis(300);

    // A repository to write to, and one quarantined and shut, to remove.
    let written = import(&mut node, "written");
    let removed = import(&mut node, "removed");
    let mut quarantine = Quarantine::open(home.join("node").join(QUARANTINE_DB_FILE)).unwrap();
    let record = quarantine
        .quarantine(
            removed,
            Kind::Namespace,
            "test",
            None,
            "a test",
            Default::default(),
        )
        .unwrap();
    repositories
        .close(
            removed,
            RepoState::Quarantined(Shut::Namespace),
            Duration::from_secs(5),
        )
        .expect("the gate closes");
    let removed_storage = home.join("storage").join(removed.canonical());
    assert!(removed_storage.is_dir());

    // The node answers while its reactor runs.
    assert_eq!(node.connected_peers_within(bound).expect("an answer"), 0);
    let seeders = node.seeders_within(written, bound).expect("an answer");
    assert!(node.waiting_on_node().is_empty());

    // A dial of the seed reaches the gate on the reactor's thread, which it
    // holds from then on.
    let dial = {
        let mut network = node.network();
        std::thread::spawn(move || network.connect_seed_book(Duration::from_secs(1)))
    };
    gate.wait_held(Duration::from_secs(10));

    // Each call returns at its bound, and is named as waiting, overdue.
    let asked = Instant::now();
    let peers = node.connected_peers_within(bound);
    assert!(
        matches!(peers, Err(Error::NodeDeadline { call: "sessions", within }) if within == bound),
        "{peers:?}"
    );
    let unseed = node.unseed_repo_within(rid, bound);
    assert!(
        matches!(unseed, Err(Error::NodeDeadline { call: "unseed", within }) if within == bound),
        "{unseed:?}"
    );
    let seeding = node.seeders_within(written, bound);
    assert!(
        matches!(seeding, Err(Error::NodeDeadline { call: "seeds_for", within }) if within == bound),
        "{seeding:?}"
    );
    // A COB change is committed, then announced: the announcement is sent,
    // and not answered.
    let created = node.create_issue_within(written, "Wedged", "Its description.", vec![], bound);
    assert!(
        matches!(created, Err(Error::NodeDeadline { call: "announce_refs_for", within }) if within == bound),
        "{created:?}"
    );
    assert!(created.unwrap_err().is_announce());
    assert!(!seeding.unwrap_err().is_announce());
    // A removal's unseed is sent, and not answered: nothing after it runs.
    let removal = node.remove_quarantined_within(removed, record.qid, &mut quarantine, bound);
    assert!(
        matches!(removal, Err(Error::NodeDeadline { call: "unseed", within }) if within == bound),
        "{removal:?}"
    );
    let took = asked.elapsed();
    assert!(took < Duration::from_secs(6), "five calls took {took:?}");
    assert_eq!(issue_titles(&node, written), ["Wedged"]);
    assert!(quarantine.get(&removed).unwrap().is_some());
    assert!(removed_storage.is_dir());
    let waiting = node.waiting_on_node();
    assert_eq!(
        waiting
            .iter()
            .map(|wait| (wait.call, wait.overdue))
            .collect::<Vec<_>>(),
        [
            ("sessions", true),
            ("unseed", true),
            ("seeds_for", true),
            ("announce_refs_for", true),
            ("unseed", true)
        ],
        "{waiting:?}"
    );

    // Callers that all arrive at once, more than may wait: as many as there's
    // room for are sent, and wait to their bound; the rest aren't sent, and
    // fail at once. No more threads wait on the node than the limit, nor
    // more calls.
    let room = NODE_WAIT_LIMIT - waiting.len();
    let callers = NODE_WAIT_LIMIT + 4;
    let burst = Duration::from_secs(2);
    let start = Barrier::new(callers + 1);
    let answers = std::thread::scope(|scope| {
        let calls = (0..callers)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    let asked = Instant::now();
                    let answer = node.connected_peers_within(burst);
                    (answer, asked.elapsed())
                })
            })
            .collect::<Vec<_>>();
        start.wait();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(node.waiting_on_node().len(), NODE_WAIT_LIMIT);
        assert_eq!(helpers(&node), NODE_WAIT_LIMIT);
        calls
            .into_iter()
            .map(|call| call.join().expect("a caller"))
            .collect::<Vec<_>>()
    });
    let sent = answers
        .iter()
        .filter(|(answer, took)| {
            matches!(answer, Err(Error::NodeDeadline { call: "sessions", within }) if *within == burst)
                && *took >= burst
        })
        .count();
    let refused = answers
        .iter()
        .filter(|(answer, took)| {
            matches!(answer, Err(Error::NodeBusy { call: "sessions", waiting }) if *waiting == NODE_WAIT_LIMIT)
                && *took < Duration::from_secs(1)
        })
        .count();
    assert_eq!((sent, refused), (room, callers - room), "{answers:?}");

    // Every place is taken, by calls past their bounds: a call isn't sent.
    let asked = Instant::now();
    let busy = node.connected_peers_within(Duration::from_secs(5));
    assert!(
        matches!(busy, Err(Error::NodeBusy { call: "sessions", waiting }) if waiting == NODE_WAIT_LIMIT),
        "{busy:?}"
    );
    assert!(asked.elapsed() < Duration::from_secs(1));
    let asked = Instant::now();
    let seeding = node.seeders_within(written, Duration::from_secs(5));
    assert!(
        matches!(seeding, Err(Error::NodeBusy { call: "seeds_for", waiting }) if waiting == NODE_WAIT_LIMIT),
        "{seeding:?}"
    );
    // A COB change still stands, unannounced.
    let created = node.create_issue_within(
        written,
        "Unannounced",
        "Its description.",
        vec![],
        Duration::from_secs(5),
    );
    assert!(
        matches!(created, Err(Error::NodeBusy { call: "announce_refs_for", waiting }) if waiting == NODE_WAIT_LIMIT),
        "{created:?}"
    );
    assert!(created.unwrap_err().is_announce());
    assert_eq!(issue_titles(&node, written), ["Unannounced", "Wedged"]);
    // A removal touches nothing.
    let removal = node.remove_quarantined_within(
        removed,
        record.qid,
        &mut quarantine,
        Duration::from_secs(5),
    );
    assert!(
        matches!(removal, Err(Error::NodeBusy { call: "unseed", waiting }) if waiting == NODE_WAIT_LIMIT),
        "{removal:?}"
    );
    assert!(quarantine.get(&removed).unwrap().is_some());
    assert!(removed_storage.is_dir());
    let took = asked.elapsed();
    assert!(
        took < Duration::from_secs(2),
        "three refusals took {took:?}"
    );
    let waiting = node.waiting_on_node();
    assert_eq!(waiting.len(), NODE_WAIT_LIMIT);
    assert!(waiting.iter().all(|wait| wait.overdue), "{waiting:?}");
    assert_eq!(helpers(&node), NODE_WAIT_LIMIT);

    // The reactor comes back: it answers the calls left waiting, which end,
    // and answers new calls again.
    gate.let_go();
    wait_for("end of the waiting calls", Duration::from_secs(10), || {
        node.waiting_on_node().is_empty() && helpers(&node) == 0
    });
    assert_eq!(
        node.connected_peers_within(Duration::from_secs(10))
            .expect("an answer"),
        0
    );
    assert!(!node
        .unseed_repo_within(rid, Duration::from_secs(10))
        .expect("an answer"));
    assert_eq!(
        node.seeders_within(written, Duration::from_secs(10))
            .expect("an answer"),
        seeders
    );
    let (issue, _) = node
        .issues(written)
        .expect("the issues")
        .into_iter()
        .find(|(_, issue)| issue.title() == "Wedged")
        .expect("the wedged write's issue");
    node.comment_issue_within(
        written,
        &issue.to_string(),
        "Announced.",
        None,
        Duration::from_secs(10),
    )
    .expect("a comment, announced");
    // The removal, run again, completes.
    assert_eq!(
        node.remove_quarantined_within(
            removed,
            record.qid,
            &mut quarantine,
            Duration::from_secs(10)
        )
        .expect("a removal"),
        Removal::Removed
    );
    assert!(quarantine.get(&removed).unwrap().is_none());
    assert!(!removed_storage.exists());
    assert!(node.waiting_on_node().is_empty());
    dial.join()
        .expect("the dial's thread")
        .expect("a seed book report");

    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}
