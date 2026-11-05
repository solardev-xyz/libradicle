//! `Embedded::node_alive` and `Embedded::shutdown_within`: a host can tell a
//! running node from a stopped one, and bound how long a shutdown waits.

mod common;

use std::time::{Duration, Instant};

use libradicle::{Asked, Embedded, HomeLock, HostKey, Options, ThreadEnd};
use radicle::node::Handle as _;

fn key() -> HostKey {
    let mut bytes = vec![7u8; 32];
    HostKey::from_bytes(&mut bytes).expect("32 bytes")
}

/// A home whose node dials nobody: the test network, static peers, no
/// seeds. Under TMPDIR, unless that is too deep for a Unix socket path.
fn home(name: &str) -> String {
    let name = format!("{name}-{}", std::process::id());
    let mut home = std::env::temp_dir().join(&name);
    if home.join("node/control.sock").as_os_str().len() > 100 {
        home = std::path::Path::new("/tmp").join(&name);
    }
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.json"),
        r#"{"preferredSeeds":[],"node":{"alias":"health","network":"test","connect":[],"peers":{"type":"static"}}}"#,
    )
    .unwrap();
    home.to_str().expect("utf-8 temp dir").to_owned()
}

fn start(home: &str) -> Embedded {
    common::start_with_key(
        Options {
            home: home.into(),
            alias: "health".into(),
            listen: vec![],
            control_socket: true,
            routing: Some(common::routing()),
            seeding_policy: None,
            isolated: false,
            repositories: None,
        },
        key(),
    )
    .expect("start")
}

#[test]
fn a_running_node_is_alive_and_stops_within_its_timeout() {
    let home = home("radhealth");
    let mut node = start(&home);
    assert!(node.node_alive());
    let started = Instant::now();
    // The clean stop: this call asked a running node, which stopped, and let
    // go of its home.
    let stopped = node.shutdown_within(Duration::from_secs(10));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(stopped.asked, Asked::ThisCall, "{stopped:?}");
    assert_eq!(stopped.thread, ThreadEnd::Stopped, "{stopped:?}");
    assert!(matches!(stopped.home, HomeLock::Released), "{stopped:?}");
    assert!(stopped.stopped_by_this_call() && stopped.is_done());
    stopped.result().expect("stops in time");
    assert!(!node.node_alive());
    // Joined: another call has nothing to wait for, and didn't ask.
    let again = node.shutdown_within(Duration::ZERO);
    assert_eq!(again.asked, Asked::Earlier, "{again:?}");
    assert_eq!(again.thread, ThreadEnd::Stopped, "{again:?}");
    assert!(
        again.is_done() && !again.stopped_by_this_call(),
        "{again:?}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_timeout_too_large_for_a_deadline_waits_for_the_node() {
    let home = home("radforever");
    let mut node = start(&home);
    let stopped = node.shutdown_within(Duration::MAX);
    assert!(
        stopped.stopped_by_this_call() && stopped.is_done(),
        "{stopped:?}"
    );
    assert!(!node.node_alive());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_node_stopped_from_outside_is_not_alive_and_shuts_down_cleanly() {
    let home = home("radoutside");
    let mut node = start(&home);
    assert!(node.node_alive());
    // Stopped through its control socket, as `rad node stop` would: the
    // embedder never asked, and learns of it from `node_alive`.
    radicle::node::Node::new(std::path::Path::new(&home).join("node/control.sock"))
        .shutdown()
        .expect("stop request");
    let until = Instant::now() + Duration::from_secs(10);
    while node.node_alive() {
        assert!(Instant::now() < until, "the node never stopped");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Already stopped: joined for its own result, at once, even with no
    // time. No stop call of the embedder's asked it.
    let stopped = node.shutdown_within(Duration::ZERO);
    assert_eq!(stopped.asked, Asked::Never, "{stopped:?}");
    assert_eq!(stopped.thread, ThreadEnd::Stopped, "{stopped:?}");
    assert!(matches!(stopped.home, HomeLock::Released), "{stopped:?}");
    assert!(stopped.is_done() && !stopped.stopped_by_this_call());
    stopped.result().expect("a node that stopped cleanly");
    // Nor does a later call.
    let again = node.shutdown_within(Duration::ZERO);
    assert_eq!(again.asked, Asked::Never, "{again:?}");
    let _ = std::fs::remove_dir_all(&home);
}
