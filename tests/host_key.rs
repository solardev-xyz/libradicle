//! `Embedded::start_with_key`: the node runs as a key the host hands in,
//! writes nothing secret, and leaves the profile's own key alone, so a
//! plain `Embedded::start` afterwards is the device identity again.

use libradicle::{Embedded, HostKey, Options};

/// Freedom's Radicle key for `abandon ×11 about` (SLIP-0010 Ed25519 at
/// m/44'/73404'/0'/0'/0'), and its DID, as desktop's `derivation.js` /
/// `formats.js` compute them.
const SEED_HEX: &str = "b262e62fc6a558fd045ca68dd7000e30a135f678bc0816935e13ebe6a97e14bd";
const DID: &str = "did:key:z6Mkgb93MjdiDEUrHVCY2X4EfaSwzoFCorViqqPnjoQX8gAn";

fn key() -> HostKey {
    let mut bytes: Vec<u8> = (0..32)
        .map(|i| u8::from_str_radix(&SEED_HEX[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    let key = HostKey::from_bytes(&mut bytes).expect("32 bytes");
    assert!(bytes.iter().all(|b| *b == 0), "the caller's copy is zeroed");
    key
}

fn opts(home: &str) -> Options {
    Options { home: home.into(), alias: "host-key-test".into(), listen: vec![] }
}

#[test]
fn host_key_runs_as_that_key_and_keeps_the_profiles_own() {
    let home = format!("/tmp/radhk-{}", std::process::id());
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("RAD_SOCKET", format!("/tmp/radhk-{}.sock", std::process::id()));
    let secret_file = std::path::Path::new(&home).join("keys/radicle");

    // A fresh home: the profile is created, but no key file is written.
    let node = Embedded::start_with_key(opts(&home), key()).expect("start with a host key");
    assert_eq!(node.did(), DID);
    node.shutdown().expect("clean shutdown");
    assert!(!secret_file.exists(), "no secret key file");
    let keys = std::path::Path::new(&home).join("keys");
    assert!(!keys.exists() || std::fs::read_dir(&keys).unwrap().next().is_none(), "keys/ stays empty");

    // Without a host key the node makes (and keeps) its own.
    let node = Embedded::start(opts(&home)).expect("start with the profile's key");
    let device = node.did();
    assert_ne!(device, DID);
    node.shutdown().expect("clean shutdown");
    let device_key = std::fs::read(&secret_file).expect("device key written");

    // The host key again: its identity, the device key file untouched.
    let node = Embedded::start_with_key(opts(&home), key()).expect("start with a host key again");
    assert_eq!(node.did(), DID);
    node.shutdown().expect("clean shutdown");
    assert_eq!(std::fs::read(&secret_file).unwrap(), device_key);

    // And back to the device's own.
    let node = Embedded::start(opts(&home)).expect("start with the profile's key again");
    assert_eq!(node.did(), device);
    node.shutdown().expect("clean shutdown");

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn host_key_must_be_32_bytes_and_is_zeroed_regardless() {
    let mut short = vec![7u8; 31];
    assert!(HostKey::from_bytes(&mut short).is_none());
    assert!(short.iter().all(|b| *b == 0));
    assert_eq!(format!("{:?}", key()), "HostKey(..)");
}
