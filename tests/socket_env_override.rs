//! The embedded node must honor `RAD_SOCKET` so hosts whose profile
//! home lives at a long path (iOS/simulator sandboxes) can redirect the
//! control socket somewhere short. A home long enough to blow SUN_LEN
//! on its own proves the override is actually what gets bound.

use libradicle::{Embedded, Options};

#[test]
fn rad_socket_env_redirects_control_socket_away_from_long_home() {
    // ~150 bytes — over the ~104-byte sun_path cap if the socket were
    // derived from the home.
    let home = format!("/tmp/radsock-{}-{}", std::process::id(), "x".repeat(120));
    let socket = format!("/tmp/radsock-{}.sock", std::process::id());
    std::env::set_var("RAD_SOCKET", &socket);

    let node = Embedded::start(Options {
        home: home.clone().into(),
        alias: "sock-test".into(),
        listen: vec![],
    })
    .expect("start must bind the control socket at RAD_SOCKET, not under home");
    node.shutdown().expect("clean shutdown");

    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&home);
}
