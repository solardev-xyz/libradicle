//! UniFFI (Swift / iOS) binding over `libradicle`.
//!
//! This mirrors the napi desktop binding one-to-one: every export returns a
//! JSON string with the exact shapes the Freedom `window.radicle` provider
//! already parses, so the iOS webview consumes the same payloads as desktop.
//! It follows the Myotis pattern — plain `#[uniffi::export]` free functions,
//! `uniffi::setup_scaffolding!()` at the crate root — with one difference from
//! napi: calls are **synchronous** (UniFFI has no libuv thread pool), so the
//! Swift side dispatches them off the main thread. Progress is delivered via a
//! foreign callback interface instead of a napi ThreadsafeFunction.
//!
//! One embedded node per process, held in a global slot, exactly as on
//! desktop: the app owns a single Radicle profile at a time.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use libradicle::{json, CancelToken, Embedded, FetchPolicy, Network, Options, Progress};

uniffi::setup_scaffolding!();

static NODE: Mutex<Option<Embedded>> = Mutex::new(None);

/// In-flight clone cancellation tokens, keyed by full RID.
static CANCELS: LazyLock<Mutex<HashMap<String, CancelToken>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Foreign progress sink for `cloneRepoWithProgress`. Swift implements this;
/// it receives one JSON object per event (`{phase, ...}`).
#[uniffi::export(callback_interface)]
pub trait ProgressListener: Send + Sync {
    fn on_progress(&self, event: String);
}

fn err_json(e: impl std::fmt::Display) -> String {
    json::to_string(|w| json::error(w, e))
}

fn with_node(f: impl FnOnce(&mut Embedded) -> String) -> String {
    match NODE.lock() {
        Ok(mut guard) => match guard.as_mut() {
            Some(node) => f(node),
            None => err_json("node not started"),
        },
        Err(_) => err_json("node lock poisoned"),
    }
}

fn network() -> std::result::Result<Network, String> {
    match NODE.lock() {
        Ok(guard) => guard
            .as_ref()
            .map(Embedded::network)
            .ok_or_else(|| "node not started".to_string()),
        Err(_) => Err("node lock poisoned".to_string()),
    }
}

fn parse_rid(rid: &str) -> std::result::Result<radicle::identity::RepoId, String> {
    rid.parse().map_err(|e| format!("invalid RID {rid:?}: {e}"))
}

/// Start the embedded node with a profile at `home`. `{"did": "..."}` on success.
#[uniffi::export]
pub fn start(home: String, alias: String) -> String {
    let mut guard = match NODE.lock() {
        Ok(g) => g,
        Err(_) => return err_json("node lock poisoned"),
    };
    if guard.is_some() {
        return err_json("node already started");
    }
    match Embedded::start(Options {
        home: home.into(),
        alias,
        listen: vec![],
        control_socket: true,
        routing: None,
    }) {
        Ok(node) => {
            let did = node.did();
            *guard = Some(node);
            json::to_string(|w| json::did(w, &did))
        }
        Err(e) => err_json(e),
    }
}

/// Start the embedded node as `secret_key`, an identity the host keeps
/// (a 32-byte Ed25519 secret seed), instead of the profile's own key file.
/// Nothing secret is written to `home`. `{"did": "..."}` on success.
///
/// Only the Rust-side copy of `secret_key` is zeroed before this returns.
/// The foreign array the host passed in (Kotlin `ByteArray`, Swift `Data`)
/// and the buffer UniFFI copied it through are *not* wiped here — the host
/// must clear its own array after the call and should keep the key out of
/// long-lived managed objects.
#[uniffi::export]
pub fn start_with_key(home: String, alias: String, secret_key: Vec<u8>) -> String {
    let mut secret_key = secret_key;
    let key = libradicle::HostKey::from_bytes(&mut secret_key);
    let Some(key) = key else {
        return err_json("secret key must be 32 bytes");
    };
    let mut guard = match NODE.lock() {
        Ok(g) => g,
        Err(_) => return err_json("node lock poisoned"),
    };
    if guard.is_some() {
        return err_json("node already started");
    }
    match Embedded::start_with_key(
        Options {
            home: home.into(),
            alias,
            listen: vec![],
            control_socket: true,
            routing: None,
        },
        key,
    ) {
        Ok(node) => {
            let did = node.did();
            *guard = Some(node);
            json::to_string(|w| json::did(w, &did))
        }
        Err(e) => err_json(e),
    }
}

/// Concurrently bootstrap from the effective seed book. The response retains
/// `connected` and adds attempt/readiness diagnostics.
#[uniffi::export]
pub fn connect_seeds(timeout_ms: u32) -> String {
    let mut network = match network() {
        Ok(network) => network,
        Err(e) => return err_json(e),
    };
    match network.connect_seed_book(Duration::from_millis(timeout_ms.into())) {
        Ok(report) => json::to_string(|w| json::seed_report(w, &report)),
        Err(e) => err_json(e),
    }
}

/// Seed + fetch a repository into storage. `{"ok": true}`.
#[uniffi::export]
pub fn clone_repo(rid: String, timeout_ms: u32) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    let mut network = match network() {
        Ok(network) => network,
        Err(e) => return err_json(e),
    };
    match network.clone_repo(rid, Duration::from_millis(timeout_ms.into())) {
        Ok(()) => json::to_string(json::ok),
        Err(e) => err_json(e),
    }
}

/// Seed + fetch a repository, pushing one JSON progress event per phase to
/// `on_progress`. `{"ok":true}`, `{"cancelled":true}`, or `{"error":...}`.
/// Cancel mid-flight with `cancelClone(rid)`.
#[uniffi::export]
pub fn clone_repo_with_progress(
    rid: String,
    timeout_ms: u32,
    on_progress: Box<dyn ProgressListener>,
) -> String {
    let parsed = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    let mut network = match network() {
        Ok(network) => network,
        Err(e) => return err_json(e),
    };

    let cancel = CancelToken::new();
    if let Ok(mut map) = CANCELS.lock() {
        map.insert(rid.clone(), cancel.clone());
    }

    let policy = FetchPolicy::from_timeout(Duration::from_millis(timeout_ms.into()));
    let mut emit =
        |p: Progress| on_progress.on_progress(json::to_string(|w| json::progress(w, &p)));
    let result = network.clone_repo_with(parsed, &policy, &cancel, &mut emit);

    if let Ok(mut map) = CANCELS.lock() {
        map.remove(&rid);
    }
    match result {
        Ok(()) => json::to_string(json::ok),
        Err(libradicle::Error::Cancelled) => json::to_string(|w| json::cancelled(w, true)),
        Err(e) => err_json(e),
    }
}

/// Request cancellation of an in-flight clone for `rid`. `{"cancelled": bool}`.
#[uniffi::export]
pub fn cancel_clone(rid: String) -> String {
    let found = CANCELS
        .lock()
        .ok()
        .and_then(|map| map.get(&rid).map(|t| t.cancel()))
        .is_some();
    json::to_string(|w| json::cancelled(w, found))
}

/// Stop seeding a repository. `{"unseeded": bool}`.
#[uniffi::export]
pub fn unseed_repo(rid: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.unseed_repo(rid) {
        Ok(unseeded) => json::to_string(|w| json::unseeded(w, unseeded)),
        Err(e) => err_json(e),
    })
}

/// The public profile identity used by the browser provider.
#[uniffi::export]
pub fn identity() -> String {
    with_node(|node| {
        let identity = node.identity();
        json::to_string(|w| json::identity(w, &identity))
    })
}

/// Repositories the local node is currently seeding.
#[uniffi::export]
pub fn list_repos() -> String {
    with_node(|node| match node.list_repos() {
        Ok(repos) => json::to_string(|w| json::repos(w, &repos)),
        Err(e) => err_json(e),
    })
}

/// Explicit allow-seeding policies, including repositories still awaiting a
/// successful first fetch.
#[uniffi::export]
pub fn list_seeded_repos() -> String {
    with_node(|node| match node.list_seeded_repos() {
        Ok(repos) => json::to_string(|w| json::seeded_repos(w, &repos)),
        Err(e) => err_json(e),
    })
}

/// All issues in a repository, in radicle-httpd-compatible JSON shape.
#[uniffi::export]
pub fn issues(rid: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.issues(rid) {
        Ok(issues) => json::to_string(|w| json::issues(w, &issues)),
        Err(e) => err_json(e),
    })
}

/// One issue in radicle-httpd-compatible JSON shape.
#[uniffi::export]
pub fn issue(rid: String, issue_id: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.issue(rid, &issue_id) {
        Ok((id, issue)) => json::to_string(|w| json::issue(w, &id, &issue)),
        Err(e) => err_json(e),
    })
}

/// All patches in a repository, in radicle-httpd-compatible JSON shape.
#[uniffi::export]
pub fn patches(rid: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.patches(rid) {
        Ok(patches) => json::to_string(|w| json::patches(w, &patches)),
        Err(e) => err_json(e),
    })
}

/// One patch in radicle-httpd-compatible JSON shape.
#[uniffi::export]
pub fn patch(rid: String, patch_id: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.patch(rid, &patch_id) {
        Ok((id, patch)) => json::to_string(|w| json::patch(w, &id, &patch)),
        Err(e) => err_json(e),
    })
}

/// Create an issue directly in the COB store. `labels_json` is a JSON string array.
#[uniffi::export]
pub fn create_issue(
    rid: String,
    title: String,
    description: String,
    labels_json: String,
) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    let labels = match serde_json::from_str::<Vec<String>>(&labels_json) {
        Ok(labels) => labels,
        Err(e) => return err_json(format!("invalid labels: {e}")),
    };
    with_node(
        |node| match node.create_issue(rid, &title, &description, labels) {
            Ok(id) => json::to_string(|w| json::id(w, id)),
            Err(e) => err_json(e),
        },
    )
}

/// Comment on an issue.
#[uniffi::export]
pub fn comment_issue(
    rid: String,
    issue_id: String,
    body: String,
    reply_to: Option<String>,
) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(
        |node| match node.comment_issue(rid, &issue_id, &body, reply_to.as_deref()) {
            Ok(id) => json::to_string(|w| json::id(w, id)),
            Err(e) => err_json(e),
        },
    )
}

/// Transition an issue state.
#[uniffi::export]
pub fn edit_issue_state(rid: String, issue_id: String, state: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.edit_issue_state(rid, &issue_id, &state) {
        Ok(id) => json::to_string(|w| json::id_state(w, id, &state)),
        Err(e) => err_json(e),
    })
}

/// Comment on a patch revision.
#[uniffi::export]
pub fn comment_patch(rid: String, revision_id: String, body: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.comment_patch(rid, &revision_id, &body) {
        Ok(id) => json::to_string(|w| json::id(w, id)),
        Err(e) => err_json(e),
    })
}

/// Import a working Git repository into native Radicle storage. Desktop-only:
/// heartwood shells out to `git` to push a working copy, so this is unavailable
/// on a no-spawn build.
#[uniffi::export]
pub fn import_repo(
    path: String,
    name: String,
    description: String,
    default_branch: String,
) -> String {
    with_node(|node| {
        match node.import_repo(
            std::path::Path::new(&path),
            &name,
            &description,
            &default_branch,
        ) {
            Ok(rid) => json::to_string(|w| json::rid(w, &rid)),
            Err(e) => err_json(e),
        }
    })
}

/// Identity payload + head + COB counts, straight from storage.
#[uniffi::export]
pub fn repo_info(rid: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.repo_info(rid) {
        Ok(info) => json::to_string(|w| json::repo_info(w, &info)),
        Err(e) => err_json(e),
    })
}

/// Paginated commit history reachable from a validated full commit id.
#[uniffi::export]
pub fn commits(rid: String, parent: String, page: u32, per_page: u32) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(
        |node| match node.commits(rid, &parent, page as usize, per_page as usize) {
            Ok(commits) => json::to_string(|w| json::commits(w, &commits)),
            Err(e) => err_json(e),
        },
    )
}

/// Commit metadata plus its structured first-parent diff.
#[uniffi::export]
pub fn commit(rid: String, revision: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.commit(rid, &revision) {
        Ok(detail) => json::to_string(|w| json::commit(w, &detail)),
        Err(e) => err_json(e),
    })
}

/// File paths at the head of the default branch, as a JSON array.
#[uniffi::export]
pub fn list_files(rid: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.list_files(rid) {
        Ok(files) => json::to_string(|w| json::files(w, &files)),
        Err(e) => err_json(e),
    })
}

/// Tree entries at the head of the default branch, httpd-shaped.
#[uniffi::export]
pub fn tree(rid: String, path: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.tree(rid, &path) {
        Ok(entries) => json::to_string(|w| json::tree(w, &entries)),
        Err(e) => err_json(e),
    })
}

/// Tree entries at a validated full commit id, including commit metadata.
#[uniffi::export]
pub fn tree_at(rid: String, revision: String, path: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.tree_at(rid, &revision, &path) {
        Ok((entries, commit)) => json::to_string(|w| json::tree_at(w, &entries, &commit)),
        Err(e) => err_json(e),
    })
}

/// Blob at the head of the default branch, httpd-shaped. Binary blobs omit content.
#[uniffi::export]
pub fn blob(rid: String, path: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.read_blob(rid, &path) {
        Ok(b) => json::to_string(|w| json::blob(w, &path, &b)),
        Err(e) => err_json(e),
    })
}

/// Blob at a validated full commit id.
#[uniffi::export]
pub fn blob_at(rid: String, revision: String, path: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.read_blob_at(rid, &revision, &path) {
        Ok(b) => json::to_string(|w| json::blob(w, &path, &b)),
        Err(e) => err_json(e),
    })
}

/// Signed remotes and their branch heads.
#[uniffi::export]
pub fn remotes(rid: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.remotes(rid) {
        Ok(remotes) => json::to_string(|w| json::remotes(w, &remotes)),
        Err(e) => err_json(e),
    })
}

/// Repository history statistics reachable from a validated full commit id.
#[uniffi::export]
pub fn repo_stats(rid: String, revision: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.repo_stats(rid, &revision) {
        Ok(stats) => json::to_string(|w| json::repo_stats(w, &stats)),
        Err(e) => err_json(e),
    })
}

/// Node status: `{"connectedPeers": n}`.
#[uniffi::export]
pub fn status() -> String {
    with_node(|node| match node.connected_peers() {
        Ok(n) => json::to_string(|w| json::connected_peers(w, n)),
        Err(e) => err_json(e),
    })
}

/// Number of known seeders for a repo: `{"seeding": n}`.
#[uniffi::export]
pub fn seeders(rid: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.seeders(rid) {
        Ok(n) => json::to_string(|w| json::seeding(w, n)),
        Err(e) => err_json(e),
    })
}

/// Gracefully stop the node and join its thread. `{"ok": true}`.
#[uniffi::export]
pub fn shutdown() -> String {
    let node = match NODE.lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => return err_json("node lock poisoned"),
    };
    match node {
        Some(node) => match node.shutdown() {
            Ok(()) => json::to_string(json::ok),
            Err(e) => err_json(e),
        },
        None => err_json("node not started"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exported surface is callable and returns the documented sentinels
    /// before a node is started (host-side smoke — device tests run on macOS).
    #[test]
    fn exports_return_not_started_sentinels() {
        assert!(identity().contains("node not started"));
        assert!(list_repos().contains("node not started"));
        assert!(status().contains("node not started"));
        assert!(shutdown().contains("node not started"));
        // Argument validation happens before the node check.
        assert!(clone_repo("not-a-rid".into(), 1000).contains("invalid RID"));
        // Cancelling an unknown clone is a well-formed negative.
        assert_eq!(cancel_clone("whatever".into()), r#"{"cancelled":false}"#);
    }

    #[test]
    fn progress_json_is_flat_with_phase() {
        let p = Progress::Connecting {
            nid: "z6Mk".into(),
            addr: "1.2.3.4:8776".into(),
            index: 1,
            total: 3,
        };
        let v: serde_json::Value =
            serde_json::from_str(&json::to_string(|w| json::progress(w, &p))).unwrap();
        assert_eq!(v["phase"], "connecting");
        assert_eq!(v["nid"], "z6Mk");
        assert_eq!(v["total"], 3);
    }
}
