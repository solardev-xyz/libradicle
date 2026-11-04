//! The guard every test node gets, so that a test never reaches the
//! network: loopback only, and loudly. It has two parts, since a node opens
//! sockets the dial gate is never asked about.
//!
//! - **The dial gate** ([`gate`], [`wrapping`]), given in
//!   `Options::routing`:
//!   - an address off this host, to dial or to take into the address book,
//!     ends the test process at once with a message naming it, before any
//!     socket: `exit(101)`, as a failed test ends;
//!   - a name to resolve ends it the same way, before any DNS query, unless
//!     the test's own gate answers names ([`wrapping`]) without DNS. Every
//!     address such a gate answers must be loopback too.
//! - **The options, checked before the node starts** ([`check`]): what the
//!   node connects to without asking its gate. That is the SOCKS proxy every
//!   dial goes through, the onion proxy, and the addresses it listens on.
//!   For a node with no routing of its own (`routing: None`, whose gate
//!   resolves through the system and asks the proxy to resolve), it is its
//!   home's `config.json`: a static peer set, no preferred seeds (one would
//!   bring the shipped seed book after it), proxies at loopback, and
//!   `connect` peers at loopback addresses, or named only through a loopback
//!   proxy.
//!
//! An isolated node's own gate refuses every target, whatever this one says,
//! and it listens nowhere.
//!
//! Shared by the integration tests (`tests/common`), whose `start` and
//! `start_with_key` check every node, and, through `#[path]`, by the crate's
//! unit tests and UniFFI's, which check theirs.

// Each includer uses some of it.
#![allow(dead_code)]

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use radicle::node::config::AddressConfig;
use radicle::node::dial::Gate;
use radicle::node::Address;

/// The gate, with a test's own gate inside it, if any.
#[derive(Debug)]
pub struct Hermetic(Option<Arc<dyn Gate>>);

/// A gate that admits loopback addresses and resolves no name.
pub fn gate() -> Arc<dyn Gate> {
    Arc::new(Hermetic(None))
}

/// `inner`, a test's own gate, held to loopback: what it refuses stays
/// refused, and what it resolves names to must be loopback.
pub fn wrapping(inner: Arc<dyn Gate>) -> Arc<dyn Gate> {
    Arc::new(Hermetic(Some(inner)))
}

fn off_host(what: std::fmt::Arguments<'_>) -> ! {
    use std::io::Write as _;
    // Straight to stderr: the test harness captures `eprintln!`, and the
    // process ends before it would print what it captured.
    let _ = writeln!(
        std::io::stderr(),
        "a test node tried to reach {what}, off this host: tests must reach loopback only \
         (tests/common/hermetic.rs)"
    );
    std::process::exit(101)
}

impl Gate for Hermetic {
    fn allows(&self, target: &Address, addr: &SocketAddr) -> bool {
        if !addr.ip().to_canonical().is_loopback() {
            off_host(format_args!("{target} at {addr}"));
        }
        self.0
            .as_ref()
            .is_none_or(|inner| inner.allows(target, addr))
    }

    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        let Some(inner) = &self.0 else {
            off_host(format_args!("the name {host}, through DNS"));
        };
        let addrs = inner.resolve(host, port)?;
        if let Some(addr) = addrs
            .iter()
            .find(|addr| !addr.ip().to_canonical().is_loopback())
        {
            off_host(format_args!("{host} at {addr}"));
        }
        Ok(addrs)
    }

    fn resolve_timeout(&self) -> std::time::Duration {
        self.0
            .as_ref()
            .map_or(radicle::node::dial::RESOLVE_TIMEOUT, |inner| {
                inner.resolve_timeout()
            })
    }

    fn proxy_resolves_names(&self) -> bool {
        // A proxy handed a name would resolve it off this host.
        false
    }

    fn isolates_streams(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|inner| inner.isolates_streams())
    }
}

fn loopback(addr: &SocketAddr) -> bool {
    addr.ip().to_canonical().is_loopback()
}

/// Checks what a test node connects to without asking its gate, before it
/// starts on `home`: with `routing`, its proxy and its onion route; without,
/// its home's `config.json`; and either way the addresses it listens on.
/// Anything that could reach off this host ends the test process.
pub fn check(
    home: &Path,
    isolated: bool,
    listen: &[SocketAddr],
    routing: Option<(Option<SocketAddr>, &AddressConfig)>,
) {
    if isolated {
        // It dials no one and listens nowhere, whatever it is given.
        return;
    }
    if let Some(addr) = listen.iter().find(|addr| !loopback(addr)) {
        off_host(format_args!("a listener on {addr}"));
    }
    match routing {
        Some((proxy, onion)) => {
            if let Some(proxy) = proxy.filter(|proxy| !loopback(proxy)) {
                off_host(format_args!("the proxy {proxy}"));
            }
            if let AddressConfig::Proxy { address } = onion {
                if !loopback(address) {
                    off_host(format_args!("the onion proxy {address}"));
                }
            }
        }
        None => check_config(home),
    }
}

/// `routing: None`: the node routes as `home`'s `config.json` says, through
/// heartwood's own gate.
fn check_config(home: &Path) {
    let path = home.join("config.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        off_host(format_args!(
            "the network through a fresh profile's default configuration (no {})",
            path.display()
        ));
    };
    let config: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let node = &config["node"];
    if config["preferredSeeds"]
        .as_array()
        .is_none_or(|seeds| !seeds.is_empty())
    {
        off_host(format_args!(
            "the seed book: {} doesn't set preferredSeeds to []",
            path.display()
        ));
    }
    if node["peers"]["type"] != "static" {
        off_host(format_args!(
            "the network's peers: {} has no static peer set",
            path.display()
        ));
    }
    let proxy = node["proxy"].as_str().map(|proxy| {
        proxy
            .parse::<SocketAddr>()
            .unwrap_or_else(|_| off_host(format_args!("the proxy {proxy}")))
    });
    if let Some(proxy) = proxy.filter(|proxy| !loopback(proxy)) {
        off_host(format_args!("the proxy {proxy}"));
    }
    for route in ["onion", "i2p"] {
        let mode = &node[route]["mode"];
        if mode == "proxy" {
            let address = node[route]["address"].as_str().unwrap_or_default();
            if address
                .parse::<SocketAddr>()
                .map_or(true, |address| !loopback(&address))
            {
                off_host(format_args!("the {route} proxy {address}"));
            }
        } else if mode == "forward" && proxy.is_none() {
            off_host(format_args!(
                "{route} names, forwarded with no proxy: through DNS"
            ));
        }
    }
    for peer in node["connect"].as_array().into_iter().flatten() {
        let peer = peer.as_str().unwrap_or_default();
        let host = peer.rsplit_once('@').map_or(peer, |(_, addr)| addr);
        let host = host.rsplit_once(':').map_or(host, |(host, _)| host);
        let host = host.trim_start_matches('[').trim_end_matches(']');
        match host.parse::<IpAddr>() {
            Ok(ip) if ip.to_canonical().is_loopback() => {}
            Ok(_) => off_host(format_args!("the peer {peer}")),
            // A name goes to the proxy to resolve, or else to DNS.
            Err(_) if proxy.is_some() => {}
            Err(_) => off_host(format_args!("the name in {peer}, through DNS")),
        }
    }
}
