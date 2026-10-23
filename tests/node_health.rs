//! `Embedded::node_alive` and `Embedded::shutdown_within`: a host can tell a
//! running node from a stopped one, and bound how long a shutdown waits.

use std::time::{Duration, Instant};

use libradicle::{Embedded, Error, HostKey, Options};
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
    Embedded::start_with_key(
        Options {
            home: home.into(),
            alias: "health".into(),
            listen: vec![],
        },
        key(),
    )
    .expect("start")
}

#[test]
fn a_running_node_is_alive_and_stops_within_its_timeout() {
    let home = home("radhealth");
    let node = start(&home);
    assert!(node.node_alive());
    let started = Instant::now();
    node.shutdown_within(Duration::from_secs(10))
        .expect("stops in time");
    assert!(started.elapsed() < Duration::from_secs(10));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_shutdown_that_cannot_wait_is_a_timeout() {
    let home = home("radtimeout");
    let node = start(&home);
    // No time at all: the node is asked to stop, and the call returns at
    // once rather than wait for its thread to end.
    match node.shutdown_within(Duration::ZERO) {
        Err(Error::ShutdownTimeout(timeout)) => assert_eq!(timeout, Duration::ZERO),
        other => panic!("expected a timeout, got {other:?}"),
    }
    // The thread was asked to stop, and finishes on its own: the home can
    // be started again once it has.
    let until = Instant::now() + Duration::from_secs(10);
    let again = loop {
        match Embedded::start_with_key(
            Options {
                home: home.clone().into(),
                alias: "health".into(),
                listen: vec![],
            },
            key(),
        ) {
            Ok(node) => break node,
            Err(_) if Instant::now() < until => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("the timed-out node never let go of its home: {e}"),
        }
    };
    assert!(again.node_alive());
    again.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_node_stopped_from_outside_is_not_alive_and_shuts_down_cleanly() {
    let home = home("radoutside");
    let node = start(&home);
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
    // Already stopped: joined for its own result, at once, even with no time.
    node.shutdown_within(Duration::ZERO)
        .expect("a node that stopped cleanly");
    let _ = std::fs::remove_dir_all(&home);
}
