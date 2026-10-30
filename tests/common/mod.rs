//! What the integration tests share. Every test node is started with
//! [`routing`], or with [`routing_with`] around a test gate of its own, so
//! that it reaches loopback only ([`hermetic`]).

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
