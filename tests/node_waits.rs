//! The calls that wait on the node's reactor for an answer
//! (`connected_peers`, `unseed_repo`) wait within a bound (freedom-core
//! #340). A wedged reactor answers neither: each call returns
//! `NodeDeadline` at its bound, and `waiting_on_node` names it. At most
//! `NODE_WAIT_LIMIT` calls wait at once, within their bounds or past them,
//! even for callers that all arrive together: a call past those isn't sent,
//! and fails at once. When the reactor comes back, it answers the calls left
//! waiting, and they end.
//!
//! The reactor is wedged through the node's dial gate, which it asks on its
//! own thread before every dial: this test's gate doesn't return until the
//! test lets it.

mod common;

use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::time::{Duration, Instant};

use libradicle::{Embedded, Error, HostKey, Options, NODE_WAIT_LIMIT};
use radicle::identity::RepoId;

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

fn start(home: &std::path::Path, gate: Arc<Wedge>) -> Embedded {
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
            repositories: None,
        },
        HostKey::from_bytes(&mut bytes).expect("32 bytes"),
    )
    .expect("start")
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
    let mut node = start(&home, Arc::clone(&gate));
    let rid = RepoId::from_str("rad:z3gqcJUoA1n9HaHKufZs5FCSGazv5").unwrap();
    let bound = Duration::from_millis(300);

    // The node answers while its reactor runs.
    assert_eq!(node.connected_peers_within(bound).expect("an answer"), 0);
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
    let took = asked.elapsed();
    assert!(took < Duration::from_secs(3), "two calls took {took:?}");
    let waiting = node.waiting_on_node();
    assert_eq!(
        waiting
            .iter()
            .map(|wait| (wait.call, wait.overdue))
            .collect::<Vec<_>>(),
        [("sessions", true), ("unseed", true)],
        "{waiting:?}"
    );

    // Callers that all arrive at once, more than may wait: as many as there's
    // room for are sent, and wait to their bound; the rest aren't sent, and
    // fail at once. No more threads wait on the node than the limit, nor
    // more calls.
    let room = NODE_WAIT_LIMIT - 2;
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
    dial.join()
        .expect("the dial's thread")
        .expect("a seed book report");

    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}
