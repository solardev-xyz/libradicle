//! What the integration tests share. Every test node is started through
//! [`start`] or [`start_with_key`], which check its options, and with
//! [`routing`], or [`routing_with`] around a test gate of its own, so that it
//! reaches loopback only ([`hermetic`]).

#![allow(dead_code)]

use std::sync::Arc;

pub mod hermetic;

/// The routing of a test node: no proxy, onions dropped, and the hermetic
/// gate.
pub fn routing() -> libradicle::Routing {
    routing_with(None)
}

/// As [`routing`], with `gate`, a test's own, inside the hermetic gate.
pub fn routing_with(gate: Option<Arc<dyn radicle::node::dial::Gate>>) -> libradicle::Routing {
    libradicle::Routing {
        proxy: None,
        onion: radicle::node::config::AddressConfig::Drop,
        gate: match gate {
            Some(gate) => hermetic::wrapping(gate),
            None => hermetic::gate(),
        },
    }
}

/// Checks `opts` ([`hermetic::check`]), as every test node's are.
pub fn checked(opts: &libradicle::Options) {
    hermetic::check(
        &opts.home,
        opts.isolated,
        &opts.listen,
        opts.routing
            .as_ref()
            .map(|routing| (routing.proxy, &routing.onion)),
    );
}

/// `Embedded::start`, once `opts` are checked.
pub fn start(opts: libradicle::Options) -> Result<libradicle::Embedded, libradicle::Error> {
    checked(&opts);
    libradicle::Embedded::start(opts)
}

/// `Embedded::start_with_key`, once `opts` are checked.
pub fn start_with_key(
    opts: libradicle::Options,
    key: libradicle::HostKey,
) -> Result<libradicle::Embedded, libradicle::Error> {
    checked(&opts);
    libradicle::Embedded::start_with_key(opts, key)
}
