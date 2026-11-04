//! `Options::seeding`: the node's default seeding policy, set in memory over
//! `config.json`'s, which stays as it was written. Every node here dials
//! through a gate that refuses every target, and has no peers or seeds.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use libradicle::{Embedded, HostKey, Options, Routing};
use radicle::node::config::{AddressConfig, DefaultSeedingPolicy};
use radicle::node::Handle as _;
use radicle::prelude::RepoId;

/// A gate that refuses every target and resolves no name.
#[derive(Debug)]
struct Refusing;

impl radicle::node::dial::Gate for Refusing {
    fn allows(&self, _: &radicle::node::Address, _: &std::net::SocketAddr) -> bool {
        false
    }

    fn resolve(&self, _: &str, _: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
        Err(std::io::ErrorKind::PermissionDenied.into())
    }
}

/// A home whose `config.json` seeds every repository by default.
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
        r#"{"preferredSeeds":[],"node":{"alias":"seeding","network":"test","connect":[],"peers":{"type":"static"},"seedingPolicy":{"default":"allow","scope":"all"}}}"#,
    )
    .unwrap();
    home
}

fn start(home: &Path, seeding: Option<DefaultSeedingPolicy>) -> Embedded {
    let mut bytes = vec![43u8; 32];
    Embedded::start_with_key(
        Options {
            home: home.to_path_buf(),
            alias: "seeding".into(),
            listen: vec![],
            control_socket: true,
            routing: Some(Routing {
                proxy: None,
                onion: AddressConfig::Drop,
                gate: Arc::new(Refusing),
            }),
            seeding,
            isolated: false,
        },
        HostKey::from_bytes(&mut bytes).expect("32 bytes"),
    )
    .expect("start")
}

fn control(home: &Path) -> radicle::node::Node {
    radicle::node::Node::new(home.join("node/control.sock"))
}

/// Whether the running node would seed a repository it has no policy for.
fn seeds_unknown(home: &Path) -> bool {
    let rid: RepoId = "rad:z3gqcJUoA1n9HaHKufZs5FCSGazv5".parse().unwrap();
    control(home)
        .seeds_for(rid, [])
        .map(|_| ())
        .expect("the node answers");
    control(home)
        .config()
        .expect("config")
        .seeding_policy
        .is_allow()
}

#[test]
fn seeding_replaces_config_jsons_default_in_memory() {
    let home = home("radseeding");
    let written = std::fs::read(home.join("config.json")).unwrap();

    let node = start(&home, Some(DefaultSeedingPolicy::Block));
    assert_eq!(
        control(&home).config().expect("config").seeding_policy,
        DefaultSeedingPolicy::Block
    );
    assert!(!seeds_unknown(&home));
    node.shutdown().expect("clean shutdown");
    // `config.json` is as it was.
    assert_eq!(std::fs::read(home.join("config.json")).unwrap(), written);

    // Without it, `config.json`'s default applies.
    let node = start(&home, None);
    assert!(seeds_unknown(&home));
    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}
