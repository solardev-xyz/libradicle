//! The hermetic gate (`tests/common/hermetic.rs`) itself: a test node that
//! would reach off this host ends its test process with 101 and a message
//! naming what it tried, before any DNS query or socket. In a child process
//! of its own, since the gate ends the process.

mod common;

use std::process::Command;
use std::time::Duration;

use libradicle::{Embedded, HostKey, Options};

const CHILD: &str = "LIBRADICLE_HERMETIC_GUARD_CHILD";

/// In the child: a node on a fresh profile, whose default configuration (the
/// main network, a dynamic peer set) dials Radicle's bootstrap nodes by
/// name, through the hermetic gate.
#[test]
fn child() {
    let Ok(home) = std::env::var(CHILD) else {
        return;
    };
    let mut bytes = vec![31u8; 32];
    let node = Embedded::start_with_key(
        Options {
            home: home.into(),
            alias: "guard".into(),
            listen: vec![],
            control_socket: false,
            routing: Some(common::routing()),
            isolated: false,
            repositories: None,
        },
        HostKey::from_bytes(&mut bytes).expect("32 bytes"),
    )
    .expect("start");
    // The gate ends the process long before this.
    std::thread::sleep(Duration::from_secs(60));
    drop(node);
}

#[test]
fn a_test_node_that_would_leave_the_host_fails_its_test_loudly() {
    let home = std::env::temp_dir().join(format!("lr-guard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CHILD, &home)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(101), "{stderr}");
    assert!(
        stderr.contains("a test node tried to reach the name")
            && stderr.contains("radicle.network, through DNS, off this host"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(&home);
}
