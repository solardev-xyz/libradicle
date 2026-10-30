//! `Options::isolated`: a node that reaches no one (freedom-core design 0008,
//! R-12 part 3). It dials no one, explicitly or not, takes no bootstrap node
//! or shipped seed into its address book, and listens on no address.

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use libradicle::{Embedded, HostKey, Options};
use radicle::node::address::Store as _;
use radicle::node::config::PeerConfig;
use radicle::node::Handle as _;

fn key() -> HostKey {
    let mut bytes = vec![12u8; 32];
    HostKey::from_bytes(&mut bytes).expect("32 bytes")
}

/// A peer's node ID, for a `connect` entry.
fn peer() -> radicle::node::NodeId {
    use radicle::crypto::Signer as _;

    let secret = radicle::crypto::SigningKey::from_seed(radicle::crypto::Seed::new([4; 32]));
    *secret.public_key()
}

/// A home whose `config.json` has a dynamic peer set on `network`, and
/// connects to `connect`, if any. Its one preferred seed is `seed`, which
/// gives it the shipped seed book after it, and it has none otherwise. Under
/// TMPDIR, unless that is too deep for a Unix socket path.
fn home(name: &str, network: &str, connect: Option<String>, seed: Option<String>) -> PathBuf {
    let name = format!("{name}-{}", std::process::id());
    let mut home = std::env::temp_dir().join(&name);
    if home.join("node/control.sock").as_os_str().len() > 100 {
        home = Path::new("/tmp").join(&name);
    }
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let connect = connect
        .map(|connect| format!(r#","connect":["{connect}"]"#))
        .unwrap_or_default();
    let seeds = seed.map(|seed| format!(r#""{seed}""#)).unwrap_or_default();
    std::fs::write(
        home.join("config.json"),
        format!(
            r#"{{"preferredSeeds":[{seeds}],"node":{{"alias":"isolated","network":"{network}","peers":{{"type":"dynamic"}}{connect}}}}}"#
        ),
    )
    .unwrap();
    home
}

fn start(home: &Path, isolated: bool, listen: Vec<SocketAddr>) -> Embedded {
    Embedded::start_with_key(
        Options {
            home: home.to_str().expect("utf-8 temp dir").into(),
            alias: "isolated".into(),
            listen,
            control_socket: true,
            routing: None,
            isolated,
        },
        key(),
    )
    .expect("start")
}

/// The node's control socket client.
fn control(home: &Path) -> radicle::node::Node {
    radicle::node::Node::new(home.join("node/control.sock"))
}

/// A local peer that only counts the connections made to it.
struct Peer(TcpListener);

impl Peer {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        Self(listener)
    }

    fn addr(&self) -> SocketAddr {
        self.0.local_addr().unwrap()
    }

    /// Whether a connection is made to it within `within`.
    fn dialled(&self, within: Duration) -> bool {
        let until = Instant::now() + within;
        while Instant::now() < until {
            match self.0.accept() {
                Ok(_) => return true,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(e) => panic!("{e}"),
            }
        }
        false
    }
}

#[test]
// R-12 part 3: an isolated node never dials its `connect` peer, which the
// same node not isolated dials at once, and an explicit connect to it fails
// before any socket. Its running configuration has a static peer set with no
// `connect` peers.
fn an_isolated_node_dials_no_one() {
    let peer_node = Peer::new();
    let home = home(
        "radisolated",
        "test",
        Some(format!("{}@{}", peer(), peer_node.addr())),
        None,
    );

    // Not isolated, it dials its peer.
    let node = start(&home, false, vec![]);
    assert!(
        peer_node.dialled(Duration::from_secs(10)),
        "the node didn't dial its peer"
    );
    node.shutdown().expect("clean shutdown");

    let node = start(&home, true, vec![]);
    let config = control(&home).config().expect("config");
    assert!(matches!(config.peers, PeerConfig::Static));
    assert!(config.connect.is_empty());
    assert!(
        !peer_node.dialled(Duration::from_secs(2)),
        "an isolated node dialled its peer"
    );
    let result = control(&home)
        .connect(
            peer(),
            peer_node.addr().into(),
            radicle::node::ConnectOptions {
                persistent: false,
                timeout: Duration::from_secs(5),
            },
        )
        .expect("a connect's answer");
    assert!(
        !matches!(result, radicle::node::ConnectResult::Connected),
        "{result:?}"
    );
    assert!(
        !peer_node.dialled(Duration::from_millis(500)),
        "an explicit connect reached the peer"
    );

    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
// R-12 part 3: on the main network, with a preferred seed and so the shipped
// seed book, an isolated node takes neither of Radicle's bootstrap nodes,
// Iris and Rosa, nor the shipped seeds, into its address book, and listens on
// no address it was given. Its seed book is still there, and connecting to
// it reaches no seed: every dial fails, the preferred seed's among them.
fn an_isolated_node_takes_no_bootstrap_nodes_and_listens_nowhere() {
    let seed = Peer::new();
    let home = home(
        "radisolatedmain",
        "main",
        None,
        Some(format!("{}@{}", peer(), seed.addr())),
    );
    let node = start(&home, true, vec![([127, 0, 0, 1], 0).into()]);

    let addresses = radicle::profile::Home::new(&home)
        .unwrap()
        .addresses(Default::default())
        .unwrap();
    assert!(
        addresses.is_empty().unwrap(),
        "an isolated node's address book holds {} node(s)",
        addresses.len().unwrap()
    );
    assert!(control(&home)
        .listen_addrs()
        .expect("listen addresses")
        .is_empty());

    let report = node
        .connect_seed_book(Duration::from_secs(10))
        .expect("a seed book report");
    assert!(report.attempted > 0, "{report:?}");
    assert_eq!(report.connected, 0, "{report:?}");
    assert!(!report.target_reached, "{report:?}");
    assert_eq!(report.failures.len(), report.attempted, "{report:?}");
    assert!(
        !seed.dialled(Duration::from_millis(500)),
        "an isolated node dialled its preferred seed"
    );

    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}
