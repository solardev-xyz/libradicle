//! `Embedded::shutdown_within` past its deadline: the node stays the
//! caller's, and a retry joins it. These tests have a binary of their own,
//! so that they can count the node threads in the process.

use std::time::{Duration, Instant};

use libradicle::{Embedded, Error, HostKey, Options};

/// One test at a time, so that a test's count of node threads sees only its
/// own node.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
fn a_timed_out_shutdown_keeps_the_node_and_can_be_retried() {
    let _serial = serial();
    let home = home("radtimeout");
    // A node stops within a millisecond or so, so a retry that didn't wait
    // could find it gone by chance: several rounds make that a miss.
    for _ in 0..5 {
        let mut node = start(&home);
        // No time at all: the call returns at once, without waiting for the
        // node or for the request to reach it.
        let started = Instant::now();
        match node.shutdown_within(Duration::ZERO) {
            Err(Error::ShutdownTimeout(timeout)) => assert_eq!(timeout, Duration::ZERO),
            other => panic!("expected a timeout, got {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a zero timeout took {:?}",
            started.elapsed()
        );
        // The node is still the caller's: waiting again joins it, so its
        // thread is gone when the call returns, and its home is free at once.
        node.shutdown_within(Duration::from_secs(10))
            .expect("stops on the retry");
        assert!(!node.node_alive());
        #[cfg(target_os = "linux")]
        assert_eq!(
            node_threads(),
            0,
            "the retry returned before the node's thread ended"
        );
    }
    let again = start(&home);
    assert!(again.node_alive());
    again.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}

/// While the stop request is held up (its connect to the control socket
/// can't complete), each call still returns at its deadline, the node keeps
/// running and stays the caller's, and once the request gets through a retry
/// joins it.
///
/// Linux only: there, a connect to a Unix socket whose backlog is full blocks.
#[cfg(target_os = "linux")]
#[test]
fn a_held_up_stop_request_doesnt_hold_up_the_call() {
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let _serial = serial();
    let home = home("radheld");
    let mut node = start(&home);

    // The node's control socket is swapped for one that never accepts, and
    // its backlog is filled, so the next connect, the request's, blocks.
    let socket = std::path::Path::new(&home).join("node/control.sock");
    std::fs::remove_file(&socket).unwrap();
    let held = UnixListener::bind(&socket).unwrap();
    // The held socket goes when the test releases it, or after 5 s, so that a
    // call that waits on the request returns late rather than never.
    let (release, released) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _ = released.recv_timeout(Duration::from_secs(5));
        drop(held);
    });
    let queued = Arc::new(AtomicUsize::new(0));
    let filler = std::thread::spawn({
        let (socket, queued) = (socket.clone(), queued.clone());
        move || {
            let mut streams = Vec::new();
            while let Ok(stream) = UnixStream::connect(&socket) {
                streams.push(stream);
                queued.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    let mut last = usize::MAX;
    while queued.load(Ordering::SeqCst) != last {
        last = queued.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(200));
    }

    for timeout in [Duration::ZERO, Duration::from_millis(300)] {
        let started = Instant::now();
        match node.shutdown_within(timeout) {
            Err(Error::ShutdownTimeout(t)) => assert_eq!(t, timeout),
            other => panic!("expected a timeout, got {other:?}"),
        }
        assert!(
            started.elapsed() < timeout + Duration::from_secs(1),
            "a {timeout:?} timeout took {:?}",
            started.elapsed()
        );
        // Not asked yet: the request is still connecting.
        assert!(node.node_alive());
    }

    // The request gets through once the held socket goes, and the retry
    // joins the node.
    release.send(()).unwrap();
    holder.join().unwrap();
    filler.join().unwrap();
    node.shutdown_within(Duration::from_secs(10))
        .expect("stops once its request gets through");
    assert!(!node.node_alive());
    assert_eq!(node_threads(), 0);
    let _ = std::fs::remove_dir_all(&home);
}

/// This process's threads named as libradicle names the node's thread.
#[cfg(target_os = "linux")]
fn node_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
        .filter(|name| name.trim_end() == "radicle-node")
        .count()
}
