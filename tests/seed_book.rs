//! The seed book (`seeds::effective` and `seeds::install`): a profile's
//! preferred seeds, then the shipped seeds, which are seeds of the main
//! network. A profile on another network gets its preferred seeds alone.
//!
//! Every node here dials through a gate that refuses every target and
//! resolves no name, so a dial fails before any socket or DNS query.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use libradicle::{Embedded, HostKey, Options, Routing};
use radicle::node::address::Store as _;
use radicle::node::config::AddressConfig;

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

/// A preferred seed at a loopback address nothing listens on.
const PREFERRED: &str = "z6MktwupdmLXVVqTzCw4i46r4uGyosGXRnR3XjN4Zq7oMMsw@127.0.0.1:1";

/// A home on `network` with a static peer set and one preferred seed.
fn home(name: &str, network: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("lr-seeds-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.json"),
        format!(
            r#"{{"preferredSeeds":["{PREFERRED}"],"node":{{"alias":"seeds","network":"{network}","connect":[],"peers":{{"type":"static"}}}}}}"#
        ),
    )
    .unwrap();
    home
}

fn start(home: &Path) -> Embedded {
    let mut bytes = vec![41u8; 32];
    Embedded::start_with_key(
        Options {
            home: home.to_path_buf(),
            alias: "seeds".into(),
            listen: vec![],
            control_socket: false,
            routing: Some(Routing {
                proxy: None,
                onion: AddressConfig::Drop,
                gate: Arc::new(Refusing),
            }),
            seeding_policy: None,
            isolated: false,
        },
        HostKey::from_bytes(&mut bytes).expect("32 bytes"),
    )
    .expect("start")
}

/// The node IDs in `home`'s address book.
fn address_book(home: &Path) -> BTreeSet<String> {
    let book = radicle::profile::Home::new(home)
        .unwrap()
        .addresses(Default::default())
        .unwrap();
    book.entries()
        .unwrap()
        .map(|entry| entry.node.to_string())
        .collect()
}

/// Every seed the node's seed book holds, by the dials `connect_seed_book`
/// tries: one per candidate, each refused by the gate.
fn seed_book(node: &Embedded) -> usize {
    node.connect_seed_book(Duration::from_secs(10))
        .expect("a seed book report")
        .attempted
}

#[test]
fn a_test_network_profile_with_a_preferred_seed_gets_that_seed_alone() {
    let home = home("test", "test");
    let node = start(&home);
    assert_eq!(seed_book(&node), 1);
    let book = address_book(&home);
    assert!(
        book.is_empty() || book == [PREFERRED.split('@').next().unwrap().to_owned()].into(),
        "the address book holds {book:?}"
    );
    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_main_network_profile_with_a_preferred_seed_gets_the_shipped_seeds_after_it() {
    let home = home("main", "main");
    let node = start(&home);
    // The preferred seed, then the fourteen shipped ones.
    assert_eq!(seed_book(&node), 15);
    assert_eq!(address_book(&home).len(), 14);
    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}
