//! `Embedded::shutdown_within` past its deadline, and dropping an
//! `Embedded`: the node stays the caller's, a retry joins it, and a drop
//! leaves no thread behind. These tests have a binary of their own, so that
//! they can count the node threads in the process.

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
    start_with(home, true)
}

fn start_with(home: &str, control_socket: bool) -> Embedded {
    Embedded::start_with_key(
        Options {
            home: home.into(),
            alias: "health".into(),
            listen: vec![],
            control_socket,
            routing: None,
            isolated: false,
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

/// Replaces the node's control socket at `home` with one that never accepts,
/// and fills its backlog, so that a connect to it blocks. Linux only: there, a
/// connect to a Unix socket whose backlog is full blocks. The socket is held
/// until the returned guard is dropped.
#[cfg(target_os = "linux")]
fn wedge_control_socket(home: &str) -> impl Drop {
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Wedged {
        release: Option<std::sync::mpsc::Sender<()>>,
        threads: Vec<std::thread::JoinHandle<()>>,
    }
    impl Drop for Wedged {
        fn drop(&mut self) {
            drop(self.release.take());
            for thread in self.threads.drain(..) {
                thread.join().ok();
            }
        }
    }

    let socket = std::path::Path::new(home).join("node/control.sock");
    std::fs::remove_file(&socket).unwrap();
    let held = UnixListener::bind(&socket).unwrap();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        // Until released, or 10 s at most, so a test that fails can't hang.
        let _ = released.recv_timeout(Duration::from_secs(10));
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
    Wedged {
        release: Some(release),
        threads: vec![holder, filler],
    }
}

/// A control socket that can't accept doesn't hold up a stop: the stop goes
/// to the node's reactor directly.
#[cfg(target_os = "linux")]
#[test]
fn a_wedged_control_socket_doesnt_hold_up_the_stop() {
    let _serial = serial();
    let home = home("radwedged");
    let mut node = start(&home);
    let wedged = wedge_control_socket(&home);

    let started = Instant::now();
    node.shutdown_within(Duration::from_secs(10))
        .expect("stops while its control socket is wedged");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(!node.node_alive());
    assert_eq!(node_threads(), 0);
    drop(wedged);
    let _ = std::fs::remove_dir_all(&home);
}

/// Review of libradicle #5, round 2: dropping a node whose shutdown timed out,
/// with its control socket wedged, leaves no thread behind.
#[cfg(target_os = "linux")]
#[test]
fn dropping_a_timed_out_node_leaves_no_thread_behind() {
    let _serial = serial();
    let home = home("raddropped");
    let mut node = start(&home);
    let wedged = wedge_control_socket(&home);

    assert!(matches!(
        node.shutdown_within(Duration::ZERO),
        Err(Error::ShutdownTimeout(_))
    ));
    let _ = node.shutdown_within(Duration::from_millis(300));
    drop(node);
    // Joined by `Drop`, with the control socket still wedged.
    assert_eq!((stop_helpers(), node_threads()), (0, 0));
    drop(wedged);
    let _ = std::fs::remove_dir_all(&home);
}

/// A node started without a control socket has none, stops through this
/// handle alone, and leaves no thread behind when it is dropped.
#[test]
fn a_node_without_a_control_socket_is_dropped_cleanly() {
    let _serial = serial();
    let home = home("radsocketless");
    let mut node = start_with(&home, false);
    assert!(node.node_alive());
    assert!(!std::path::Path::new(&home)
        .join("node/control.sock")
        .exists());

    assert!(matches!(
        node.shutdown_within(Duration::ZERO),
        Err(Error::ShutdownTimeout(_))
    ));
    drop(node);
    #[cfg(target_os = "linux")]
    assert_eq!((stop_helpers(), node_threads()), (0, 0));
    // Its home is free at once.
    start_with(&home, false).shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}

/// Dropping a node that runs, with no shutdown asked for, stops and joins it.
#[test]
fn dropping_a_running_node_stops_it() {
    let _serial = serial();
    let home = home("radrunning");
    let node = start(&home);
    assert!(node.node_alive());
    drop(node);
    #[cfg(target_os = "linux")]
    assert_eq!(node_threads(), 0);
    start(&home).shutdown().expect("its home is free at once");
    let _ = std::fs::remove_dir_all(&home);
}

/// This process's threads named `name` (as Linux keeps it, cut to 15 bytes).
#[cfg(target_os = "linux")]
fn threads_named(name: &str) -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
        .filter(|comm| comm.trim_end() == name)
        .count()
}

/// The node's thread, as libradicle names it.
#[cfg(target_os = "linux")]
fn node_threads() -> usize {
    threads_named("radicle-node")
}

/// Threads sending a stop request, as libradicle once named them.
#[cfg(target_os = "linux")]
fn stop_helpers() -> usize {
    threads_named("radicle-node-st")
}
