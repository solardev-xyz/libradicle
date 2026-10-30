//! A stopped node lets go of its home before `shutdown` returns, even while
//! a name lookup it started still runs: heartwood's runtime holds the home
//! until every lookup has returned (heartwood #16, Security R-16), so
//! `shutdown` waits for it, and a node can start on the home again at once.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use libradicle::{Embedded, Error, HostKey, Options, Routing};

fn key() -> HostKey {
    let mut bytes = vec![13u8; 32];
    HostKey::from_bytes(&mut bytes).expect("32 bytes")
}

/// A peer's node ID, for a `connect` entry.
fn peer() -> radicle::node::NodeId {
    use radicle::crypto::Signer as _;

    let secret = radicle::crypto::SigningKey::from_seed(radicle::crypto::Seed::new([5; 32]));
    *secret.public_key()
}

/// A home whose node connects to one peer by name, and to no one else.
fn home(name: &str) -> PathBuf {
    let home = Path::new("/tmp").join(format!("radrel-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.json"),
        format!(
            r#"{{"preferredSeeds":[],"node":{{"alias":"release","network":"test","peers":{{"type":"static"}},"connect":["{}@held.test:8776"]}}}}"#,
            peer()
        ),
    )
    .unwrap();
    home
}

/// A gate whose name lookups wait until it is released.
#[derive(Debug, Default)]
struct Held {
    state: Mutex<(usize, bool)>,
    changed: Condvar,
}

impl Held {
    /// Waits until a lookup is held.
    fn wait_for_a_lookup(&self) {
        let mut state = self.state.lock().unwrap();
        while state.0 == 0 {
            state = self.changed.wait(state).unwrap();
        }
    }

    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

impl radicle::node::dial::Gate for Held {
    fn allows(&self, _: &radicle::node::Address, _: &std::net::SocketAddr) -> bool {
        false
    }

    fn resolve(&self, _: &str, _: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
        let mut state = self.state.lock().unwrap();
        state.0 += 1;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).unwrap();
        }
        Ok(vec![])
    }
}

fn start(home: &Path, gate: Option<Arc<Held>>) -> Result<Embedded, Error> {
    Embedded::start_with_key(
        Options {
            home: home.to_str().expect("utf-8 temp dir").into(),
            alias: "release".into(),
            listen: vec![],
            control_socket: true,
            routing: gate.map(|gate| Routing {
                proxy: None,
                onion: radicle::node::config::AddressConfig::Drop,
                gate: gate as Arc<dyn radicle::node::dial::Gate>,
            }),
            isolated: false,
        },
        key(),
    )
}

#[test]
fn a_restart_right_after_shutdown_finds_the_home_free() {
    let home = home("restart");
    let gate = Arc::new(Held::default());
    let node = start(&home, Some(gate.clone())).expect("start");
    gate.wait_for_a_lookup();
    // The lookup ends well past the second the node's stop waits for one.
    let lookup = {
        let gate = gate.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1_500));
            gate.release();
        })
    };

    let started = Instant::now();
    node.shutdown().expect("clean shutdown");
    assert!(
        started.elapsed() >= Duration::from_millis(1_400),
        "shutdown returned while the lookup held the home"
    );
    lookup.join().unwrap();
    let node = start(&home, None).expect("a node starts on the home again at once");
    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn shutdown_within_waits_for_the_home_within_its_deadline() {
    let home = home("within");
    let gate = Arc::new(Held::default());
    let mut node = start(&home, Some(gate.clone())).expect("start");
    gate.wait_for_a_lookup();

    // The node's thread ends, but the held lookup keeps the home.
    let result = node.shutdown_within(Duration::from_millis(2_000));
    assert!(
        matches!(result, Err(Error::ShutdownTimeout(_))),
        "{result:?}"
    );
    assert!(!node.node_alive());
    assert!(matches!(
        start(&home, None),
        Err(Error::Runtime(radicle_node::runtime::Error::HomeLocked(_)))
    ));

    // Once the lookup ends, another call finds the home let go of.
    gate.release();
    node.shutdown_within(Duration::from_secs(10))
        .expect("the home is let go of");
    // A node starts on the home again, and the stopped one stays done,
    // though the home is held once more, by the new node.
    let next = start(&home, None).expect("a node starts on the home again");
    node.shutdown_within(Duration::ZERO)
        .expect("the stopped node stays done");
    drop(node);
    next.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}
