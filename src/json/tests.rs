//! The shared writers' bytes, as the add-on has always written them: keys
//! in the order each object lists them, no whitespace.

use std::time::Duration;

use super::*;
use crate::{CommitSignature, SeedConnectFailure};

fn text(write: impl FnOnce(&mut Vec<u8>) -> Written) -> String {
    to_string(write)
}

#[test]
fn flat_results() {
    assert_eq!(
        text(|w| error(w, "node not started")),
        r#"{"error":"node not started"}"#
    );
    assert_eq!(text(ok), r#"{"ok":true}"#);
    assert_eq!(text(|w| cancelled(w, true)), r#"{"cancelled":true}"#);
    assert_eq!(text(|w| cancelled(w, false)), r#"{"cancelled":false}"#);
    assert_eq!(
        text(|w| did(w, "did:key:z6Mk")),
        r#"{"did":"did:key:z6Mk"}"#
    );
    assert_eq!(text(|w| unseeded(w, true)), r#"{"unseeded":true}"#);
    assert_eq!(text(|w| connected_peers(w, 3)), r#"{"connectedPeers":3}"#);
    assert_eq!(text(|w| seeding(w, 0)), r#"{"seeding":0}"#);
    assert_eq!(text(|w| id(w, "abc")), r#"{"id":"abc"}"#);
    assert_eq!(
        text(|w| id_state(w, "abc", "closed")),
        r#"{"id":"abc","state":"closed"}"#
    );
    assert_eq!(
        text(|w| files(w, &["a".into(), "b/c".into()])),
        r#"["a","b/c"]"#
    );
}

#[test]
fn identity_and_stats() {
    let me = Identity {
        did: "did:key:z6Mk".into(),
        nid: "z6Mk".into(),
        alias: "me".into(),
    };
    assert_eq!(
        text(|w| identity(w, &me)),
        r#"{"did":"did:key:z6Mk","nid":"z6Mk","alias":"me"}"#
    );
    let stats = RepoStats {
        commits: 3,
        branches: 1,
        contributors: 2,
    };
    assert_eq!(
        text(|w| repo_stats(w, &stats)),
        r#"{"commits":3,"branches":1,"contributors":2}"#
    );
}

#[test]
fn progress_is_flat_with_its_phase() {
    let p = Progress::Connecting {
        nid: "z6Mk".into(),
        addr: "1.2.3.4:8776".into(),
        index: 1,
        total: 3,
    };
    assert_eq!(
        text(|w| progress(w, &p)),
        r#"{"nid":"z6Mk","addr":"1.2.3.4:8776","index":1,"total":3,"phase":"connecting"}"#
    );
    assert_eq!(
        text(|w| progress(w, &Progress::Done)),
        r#"{"phase":"done"}"#
    );
}

#[test]
fn a_seed_report() {
    let report = SeedConnectReport {
        attempted: 2,
        connected: 1,
        target: 1,
        target_reached: true,
        elapsed: Duration::from_millis(1500),
        failures: vec![SeedConnectFailure {
            nid: "z6Mk".into(),
            addr: "1.2.3.4:8776".into(),
            reason: "refused".into(),
        }],
    };
    assert_eq!(
        text(|w| seed_report(w, &report)),
        concat!(
            r#"{"attempted":2,"connected":1,"target":1,"targetReached":true,"#,
            r#""elapsedMs":1500,"#,
            r#""failures":[{"nid":"z6Mk","addr":"1.2.3.4:8776","reason":"refused"}]}"#
        )
    );
}

#[test]
fn trees_and_blobs() {
    let entries = vec![TreeEntry {
        name: "README.md".into(),
        path: "README.md".into(),
        kind: "blob".into(),
    }];
    assert_eq!(
        text(|w| tree(w, &entries)),
        r#"{"entries":[{"name":"README.md","path":"README.md","kind":"blob"}]}"#
    );
    let signature = CommitSignature {
        name: "A".into(),
        email: "a@example.com".into(),
        time: 7,
    };
    let commit = CommitInfo {
        id: "c0".into(),
        summary: "s".into(),
        description: "d".into(),
        author: signature.clone(),
        committer: signature,
    };
    assert_eq!(
        text(|w| tree_at(w, &entries, &commit)),
        concat!(
            r#"{"entries":[{"name":"README.md","path":"README.md","kind":"blob"}],"#,
            r#""lastCommit":{"id":"c0","summary":"s","description":"d","#,
            r#""author":{"name":"A","email":"a@example.com","time":7},"#,
            r#""committer":{"name":"A","email":"a@example.com","time":7}}}"#
        )
    );
    let text_blob = Blob {
        binary: false,
        content: b"hello".to_vec(),
    };
    assert_eq!(
        text(|w| blob(w, "docs/notes.txt", &text_blob)),
        r#"{"binary":false,"name":"notes.txt","content":"hello"}"#
    );
    let binary = Blob {
        binary: true,
        content: vec![0, 1, 2],
    };
    assert_eq!(
        text(|w| blob(w, "image.png", &binary)),
        r#"{"binary":true,"name":"image.png"}"#
    );
}

#[test]
fn repos_and_remotes() {
    let seeded = vec![SeededRepoInfo {
        rid: "rad:z3gqcJUoA1n9HaHKufZs5FCSGazv5".parse().unwrap(),
        name: None,
        description: None,
    }];
    assert_eq!(
        text(|w| seeded_repos(w, &seeded)),
        r#"[{"rid":"rad:z3gqcJUoA1n9HaHKufZs5FCSGazv5","name":null,"description":null}]"#
    );
    let remotes_ = vec![RemoteInfo {
        id: "z6Mk".into(),
        did: "did:key:z6Mk".into(),
        delegate: true,
        heads: [("main".to_string(), "c0".to_string())]
            .into_iter()
            .collect(),
    }];
    assert_eq!(
        text(|w| remotes(w, &remotes_)),
        r#"[{"id":"z6Mk","did":"did:key:z6Mk","delegate":true,"heads":{"main":"c0"}}]"#
    );
}

/// A writer that refuses: the result becomes an error, never a partial
/// string.
#[test]
fn a_failed_write_is_an_error() {
    let failed = to_string(|_| Err(serde_json::Error::io(io::Error::other("refused"))));
    assert_eq!(failed, r#"{"error":"refused"}"#);
}
