//! The dial gate every test node gets, so that a test never reaches the
//! network: loopback only, and loudly.
//!
//! - **An address off this host,** to dial or to take into the address
//!   book, ends the test process at once with a message naming it, before
//!   any socket: `exit(101)`, as a failed test ends.
//! - **A name to resolve** ends it the same way, before any DNS query,
//!   unless the test's own gate answers names ([`wrapping`]) without DNS.
//!   Every address such a gate answers must be loopback too.
//!
//! A test node gets it through `Options::routing`. An isolated node's gate
//! refuses every target already, whatever this one says. The tests that
//! can't take a gate (those of `routing: None` itself, and the bindings'
//! own `start`) write a `config.json` that dials no one: a static peer set,
//! no preferred seeds (one would bring the shipped seed book after it), and
//! no `connect` peers but loopback ones or names a test gate answers.
//!
//! Shared by the integration tests (`tests/common`), the crate's unit tests
//! and UniFFI's, through `#[path]`.

// Each includer uses some of it.
#![allow(dead_code)]

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

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
