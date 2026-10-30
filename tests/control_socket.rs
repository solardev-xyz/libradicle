//! The control socket at start, now that heartwood's runtime guards it
//! (heartwood #16, Security R-16): a stale socket is replaced, a live one is
//! kept and the start refused, and a file that isn't a socket is left as it
//! is. libradicle no longer removes stale sockets of its own.

#![cfg(unix)]

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use libradicle::{Embedded, Error, Options};
use radicle::node::Handle as _;

/// A home whose node takes no peers, with its control socket's directory
/// made: under /tmp, so that the socket's path stays short.
fn home(name: &str) -> PathBuf {
    let home = Path::new("/tmp").join(format!("radctl-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("node")).unwrap();
    std::fs::write(
        home.join("config.json"),
        r#"{"preferredSeeds":[],"node":{"alias":"control","network":"test","connect":[],"peers":{"type":"static"}}}"#,
    )
    .unwrap();
    home
}

fn start(home: &Path) -> Result<Embedded, Error> {
    Embedded::start(Options {
        home: home.to_str().expect("utf-8 temp dir").into(),
        alias: "control".into(),
        listen: vec![],
        control_socket: true,
        routing: None,
        isolated: false,
        repositories: None,
    })
}

#[test]
fn a_stale_control_socket_is_replaced() {
    let home = home("stale");
    let socket = home.join("node/control.sock");
    // A socket that nothing listens on any more, as a killed node leaves.
    drop(UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());

    let node = start(&home).expect("start over a stale socket");
    radicle::node::Node::new(&socket)
        .config()
        .expect("the new node answers on the socket");
    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_live_control_socket_is_kept_and_the_start_refused() {
    let home = home("live");
    let socket = home.join("node/control.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    let result = start(&home);
    assert!(
        matches!(
            &result,
            Err(Error::Runtime(
                radicle_node::runtime::Error::AlreadyRunning(_)
            ))
        ),
        "{:?}",
        result.as_ref().err()
    );
    // The socket is still there, and still the listener's.
    UnixStream::connect(&socket).expect("the live socket is kept");
    drop(listener);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_file_that_isnt_a_socket_is_left_as_it_is() {
    let home = home("file");
    let socket = home.join("node/control.sock");
    std::fs::write(&socket, b"keep").unwrap();

    let result = start(&home);
    assert!(
        matches!(
            &result,
            Err(Error::Runtime(radicle_node::runtime::Error::ControlPath(
                ..
            )))
        ),
        "{:?}",
        result.as_ref().err()
    );
    assert_eq!(std::fs::read(&socket).unwrap(), b"keep");
    let _ = std::fs::remove_dir_all(&home);
}
