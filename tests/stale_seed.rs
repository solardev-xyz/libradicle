//! `connect_seed_book` after the node's transport restarts (libradicle #37).
//! The restart cuts the seed's connection, and the node's redial of it fails
//! while the transport is down. Heartwood then keeps the seed's session,
//! disconnected, while it waits out a reconnection delay that doubles with
//! each failed dial. Once the transport is back, a pass dials the seed again
//! at once, rather than leaving it to that delay and answering `attempted: 0`.
//!
//! The transport is a relay on loopback, between the node and its one seed.
//! Its restart cuts every connection through it; while it's down, it closes
//! each new connection at once, as a Tor client that can't reach the seed
//! does.

mod common;

use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libradicle::{Embedded, HostKey, Options};

/// What a relay shares with its thread.
#[derive(Default)]
struct Shared {
    down: AtomicBool,
    stop: AtomicBool,
    /// Connections relayed to the seed.
    relayed: AtomicUsize,
    /// Connections closed at once, while down.
    refused: AtomicUsize,
    /// Every socket of every relayed connection, to cut them.
    open: Mutex<Vec<TcpStream>>,
}

/// A relay on loopback to `target`.
struct Relay {
    addr: SocketAddr,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Relay {
    fn start(target: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Shared::default());
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if shared.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(inbound) = stream else { continue };
                    if shared.down.load(Ordering::SeqCst) {
                        shared.refused.fetch_add(1, Ordering::SeqCst);
                        let _ = inbound.shutdown(Shutdown::Both);
                        continue;
                    }
                    let Ok(outbound) = TcpStream::connect(target) else {
                        let _ = inbound.shutdown(Shutdown::Both);
                        continue;
                    };
                    shared.relayed.fetch_add(1, Ordering::SeqCst);
                    let mut open = shared.open.lock().unwrap();
                    open.push(inbound.try_clone().unwrap());
                    open.push(outbound.try_clone().unwrap());
                    drop(open);
                    pipe(inbound.try_clone().unwrap(), outbound.try_clone().unwrap());
                    pipe(outbound, inbound);
                }
            })
        };
        Relay {
            addr,
            shared,
            thread: Some(thread),
        }
    }

    /// The transport stops: every connection through it is cut, and each new
    /// one is closed at once until [`Relay::up`].
    fn down(&self) {
        self.shared.down.store(true, Ordering::SeqCst);
        for stream in self.shared.open.lock().unwrap().drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    /// The transport is back.
    fn up(&self) {
        self.shared.down.store(false, Ordering::SeqCst);
    }

    fn relayed(&self) -> usize {
        self.shared.relayed.load(Ordering::SeqCst)
    }

    fn refused(&self) -> usize {
        self.shared.refused.load(Ordering::SeqCst)
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.down();
        self.shared.stop.store(true, Ordering::SeqCst);
        // Wakes the accept.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Copies `from` to `to` on a thread of its own, and closes both at the end
/// of either.
fn pipe(mut from: TcpStream, mut to: TcpStream) {
    std::thread::spawn(move || {
        let _ = io::copy(&mut from, &mut to);
        let _ = to.shutdown(Shutdown::Both);
        let _ = from.shutdown(Shutdown::Both);
    });
}

/// A free port on loopback, for the seed to listen on.
fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// A home on the test network with a static peer set, and `seed` as its one
/// preferred seed, if any.
fn home(name: &str, seed: Option<&str>) -> PathBuf {
    let home = std::env::temp_dir().join(format!("lr-stale-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let seeds = seed.map(|seed| format!(r#""{seed}""#)).unwrap_or_default();
    std::fs::write(
        home.join("config.json"),
        format!(
            r#"{{"preferredSeeds":[{seeds}],"node":{{"alias":"{name}","network":"test","connect":[],"peers":{{"type":"static"}}}}}}"#
        ),
    )
    .unwrap();
    home
}

fn start(home: &Path, alias: &str, key: u8, listen: Vec<SocketAddr>) -> Embedded {
    let mut bytes = vec![key; 32];
    common::start_with_key(
        Options {
            home: home.to_path_buf(),
            alias: alias.into(),
            listen,
            control_socket: false,
            routing: Some(common::routing()),
            seeding_policy: None,
            isolated: false,
            repositories: None,
        },
        HostKey::from_bytes(&mut bytes).expect("32 bytes"),
    )
    .expect("start")
}

/// Waits up to `within` for `done`, or panics naming `what`.
fn wait_for(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < within, "no {what} within {within:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_seed_cut_off_by_a_transport_restart_is_dialled_again_at_once() {
    let listen = free_port();
    let seed_home = home("seed", None);
    let seed = start(&seed_home, "seed", 61, vec![listen]);
    let relay = Relay::start(listen);
    let node_home = home(
        "node",
        Some(&format!("{}@{}", seed.identity().nid, relay.addr)),
    );
    let node = start(&node_home, "node", 62, vec![]);

    let report = node
        .connect_seed_book(Duration::from_secs(10))
        .expect("a seed book report");
    assert_eq!((report.attempted, report.connected), (1, 1), "{report:?}");
    assert_eq!(relay.relayed(), 1);

    // The transport stops. The seed's connection is cut, and the node's
    // redial of it, at heartwood's first reconnection delay (4 s), fails.
    // Heartwood doubles the delay, to 8 s: the seed's session waits out that
    // delay, disconnected.
    relay.down();
    wait_for("cut", Duration::from_secs(10), || {
        node.connected_peers().expect("peers") == 0
    });
    wait_for("failed redial", Duration::from_secs(30), || {
        relay.refused() >= 1
    });
    // The failure reaches the node within milliseconds, and its next redial
    // is 8 s away.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(node.connected_peers().expect("peers"), 0);

    // The transport is back, and a pass dials the seed at once. Heartwood's
    // own redial would wait at least 3 s (its least delay).
    relay.up();
    let asked = Instant::now();
    let report = node
        .connect_seed_book(Duration::from_secs(5))
        .expect("a seed book report");
    let took = asked.elapsed();
    assert_eq!(report.attempted, 1, "the seed is dialled again: {report:?}");
    assert_eq!(report.connected, 1, "{report:?}");
    assert!(report.target_reached, "{report:?}");
    assert!(report.failures.is_empty(), "{report:?}");
    assert!(took < Duration::from_secs(3), "the pass took {took:?}");
    // Connected in fact, through the relay, and not only reported so.
    assert_eq!(relay.relayed(), 2);
    assert_eq!(node.connected_peers().expect("peers"), 1);

    node.shutdown().expect("node shutdown");
    seed.shutdown().expect("seed shutdown");
    drop(relay);
    let _ = std::fs::remove_dir_all(&node_home);
    let _ = std::fs::remove_dir_all(&seed_home);
}
