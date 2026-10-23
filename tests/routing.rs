//! `Options::routing`: the node's routing, set in memory over `config.json`,
//! which is never written; and the gate the node dials through.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libradicle::{Embedded, HostKey, Options, Routing};
use radicle::node::config::AddressConfig;
use radicle::node::Handle as _;

fn key() -> HostKey {
    let mut bytes = vec![11u8; 32];
    HostKey::from_bytes(&mut bytes).expect("32 bytes")
}

/// A peer's node ID, for a `connect` entry.
fn peer() -> radicle::node::NodeId {
    use radicle::crypto::Signer as _;

    let secret = radicle::crypto::SigningKey::from_seed(radicle::crypto::Seed::new([3; 32]));
    *secret.public_key()
}

/// A home whose `config.json` routes through a proxy, forwards onion and
/// I2P names, advertises an external address, and connects to one peer by
/// name.
/// Under TMPDIR, unless that is too deep for a Unix socket path.
fn home(name: &str) -> PathBuf {
    let name = format!("{name}-{}", std::process::id());
    let mut home = std::env::temp_dir().join(&name);
    if home.join("node/control.sock").as_os_str().len() > 100 {
        home = Path::new("/tmp").join(&name);
    }
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.json"),
        format!(
            r#"{{"preferredSeeds":[],"node":{{"alias":"routing","network":"test","peers":{{"type":"static"}},"proxy":"127.0.0.1:9050","onion":{{"mode":"forward"}},"i2p":{{"mode":"forward"}},"externalAddresses":["198.51.100.1:8776"],"connect":["{}@seed.test:8776"]}}}}"#,
            peer()
        ),
    )
    .unwrap();
    home
}

fn start(home: &Path, routing: Option<Routing>) -> Embedded {
    Embedded::start_with_key(
        Options {
            home: home.to_str().expect("utf-8 temp dir").into(),
            alias: "routing".into(),
            listen: vec![],
            control_socket: true,
            routing,
        },
        key(),
    )
    .expect("start")
}

/// The node's configuration, as it runs.
fn running_config(home: &Path) -> radicle::node::Config {
    radicle::node::Node::new(home.join("node/control.sock"))
        .config()
        .expect("config")
}

/// A gate that refuses everything, and records the names it was asked for.
#[derive(Debug, Default)]
struct Recording(Mutex<Vec<String>>);

impl radicle::node::dial::Gate for Recording {
    fn allows(&self, _: &radicle::node::Address, _: &std::net::SocketAddr) -> bool {
        false
    }

    fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
        self.0.lock().unwrap().push(format!("{host}:{port}"));
        Err(std::io::ErrorKind::NotFound.into())
    }
}

#[test]
fn routing_replaces_config_jsons_routing_in_memory() {
    let home = home("radrouting");
    let written = std::fs::read(home.join("config.json")).unwrap();
    let gate = Arc::new(Recording::default());
    let node = start(
        &home,
        Some(Routing {
            proxy: None,
            onion: AddressConfig::Drop,
            gate: gate.clone(),
        }),
    );

    let config = running_config(&home);
    assert_eq!(config.proxy, None);
    assert_eq!(config.onion, AddressConfig::Drop);
    assert_eq!(config.i2p, AddressConfig::Drop);
    assert!(config.external_addresses.is_empty());
    // `config.json` is as it was.
    assert_eq!(std::fs::read(home.join("config.json")).unwrap(), written);

    // The node dials its `connect` peer through the gate: the name is
    // resolved there, and nowhere else.
    let until = Instant::now() + Duration::from_secs(10);
    while gate.0.lock().unwrap().is_empty() {
        assert!(Instant::now() < until, "the gate was never asked");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(gate
        .0
        .lock()
        .unwrap()
        .iter()
        .all(|name| name == "seed.test:8776"));

    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn without_routing_config_json_routes_as_before() {
    let home = home("radnorouting");
    let node = start(&home, None);

    let config = running_config(&home);
    assert_eq!(config.proxy, Some(([127, 0, 0, 1], 9050).into()));
    assert_eq!(config.onion, AddressConfig::Forward);
    assert_eq!(config.i2p, AddressConfig::Forward);
    assert_eq!(config.external_addresses.len(), 1);

    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}
