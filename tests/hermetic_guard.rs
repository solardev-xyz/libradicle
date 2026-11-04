//! The hermetic guard (`tests/common/hermetic.rs`) itself: a test node that
//! would reach off this host ends its test process with 101 and a message
//! naming what it tried, before any DNS query or socket. Each case in a
//! child process of its own, since the guard ends the process.
//!
//! - **The gate:** a node on a fresh profile, whose default configuration
//!   (the main network, a dynamic peer set) dials Radicle's bootstrap nodes
//!   by name.
//! - **The options**, which the gate is never asked about: an off-host SOCKS
//!   proxy, an off-host onion proxy, a listener on every interface, and a
//!   node with no routing of its own whose `config.json` names an off-host
//!   proxy, or a peer by name with no proxy.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use libradicle::{HostKey, Options, Routing};
use radicle::node::config::AddressConfig;

const CHILD: &str = "LIBRADICLE_HERMETIC_GUARD_CHILD";
const HOME: &str = "LIBRADICLE_HERMETIC_GUARD_HOME";

fn key() -> HostKey {
    let mut bytes = vec![31u8; 32];
    HostKey::from_bytes(&mut bytes).expect("32 bytes")
}

fn options(home: &Path, listen: Vec<SocketAddr>, routing: Option<Routing>) -> Options {
    Options {
        home: home.into(),
        alias: "guard".into(),
        listen,
        control_socket: false,
        routing,
        isolated: false,
        repositories: None,
    }
}

/// A home whose `config.json` routes as `node`, a JSON object's members,
/// says, with static peers and no seeds.
fn configured(home: &Path, node: &str) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("config.json"),
        format!(
            r#"{{"preferredSeeds":[],"node":{{"alias":"guard","network":"test","peers":{{"type":"static"}}{node}}}}}"#
        ),
    )
    .unwrap();
}

/// In the child: the case `LIBRADICLE_HERMETIC_GUARD_CHILD` names, on the
/// home `LIBRADICLE_HERMETIC_GUARD_HOME`.
#[test]
fn child() {
    let (Ok(case), Ok(home)) = (std::env::var(CHILD), std::env::var(HOME)) else {
        return;
    };
    let home = Path::new(&home);
    let off_host: SocketAddr = ([198, 51, 100, 7], 9050).into();
    let opts = match case.as_str() {
        "bootstrap" => options(home, vec![], Some(common::routing())),
        "proxy" => options(
            home,
            vec![],
            Some(Routing {
                proxy: Some(off_host),
                ..common::routing()
            }),
        ),
        "onion" => options(
            home,
            vec![],
            Some(Routing {
                onion: AddressConfig::Proxy { address: off_host },
                ..common::routing()
            }),
        ),
        "listen" => options(
            home,
            vec![([0, 0, 0, 0], 0).into()],
            Some(common::routing()),
        ),
        "config-proxy" => {
            configured(home, r#","proxy":"198.51.100.7:9050""#);
            options(home, vec![], None)
        }
        "config-name" => {
            configured(
                home,
                r#","connect":["z6MktwupdmLXVVqTzCw4i46r4uGyosGXRnR3XjN4Zq7oMMsw@named.test:8776"]"#,
            );
            options(home, vec![], None)
        }
        other => panic!("no case {other}"),
    };
    let node = common::start_with_key(opts, key()).expect("start");
    // The guard ends the process long before this.
    std::thread::sleep(Duration::from_secs(60));
    drop(node);
}

/// Runs `case` in a child, and what it wrote to stderr, once it ended with
/// 101.
fn ends_loudly(case: &str) -> String {
    let home = std::env::temp_dir().join(format!("lr-guard-{case}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CHILD, case)
        .env(HOME, &home)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&home);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(101), "{case}: {stderr}");
    assert!(stderr.contains("off this host"), "{case}: {stderr}");
    stderr
}

#[test]
fn a_test_node_that_would_leave_the_host_fails_its_test_loudly() {
    let stderr = ends_loudly("bootstrap");
    assert!(
        stderr.contains("a test node tried to reach the name")
            && stderr.contains("radicle.network, through DNS, off this host"),
        "{stderr}"
    );
}

#[test]
fn an_off_host_proxy_is_refused_before_the_node_starts() {
    let stderr = ends_loudly("proxy");
    assert!(stderr.contains("the proxy 198.51.100.7:9050"), "{stderr}");
    let stderr = ends_loudly("config-proxy");
    assert!(stderr.contains("the proxy 198.51.100.7:9050"), "{stderr}");
}

#[test]
fn an_off_host_onion_proxy_is_refused_before_the_node_starts() {
    let stderr = ends_loudly("onion");
    assert!(
        stderr.contains("the onion proxy 198.51.100.7:9050"),
        "{stderr}"
    );
}

#[test]
fn a_listener_off_loopback_is_refused_before_the_node_starts() {
    let stderr = ends_loudly("listen");
    assert!(stderr.contains("a listener on 0.0.0.0:0"), "{stderr}");
}

#[test]
fn a_name_with_no_proxy_is_refused_for_a_node_without_routing() {
    let stderr = ends_loudly("config-name");
    assert!(
        stderr.contains("the name in ") && stderr.contains("named.test:8776, through DNS"),
        "{stderr}"
    );
}
