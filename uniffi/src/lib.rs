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

use libradicle::{
    json, Bounds, CancelToken, Embedded, FetchPolicy, Network, Options, Progress,
    BINDING_MAX_PER_PAGE, BINDING_READ_BUDGET,
};

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

/// The page size a `commits` call gets: what it asked for, up to
/// [`BINDING_MAX_PER_PAGE`]. Every read here is bounded too (R-18): walks by
/// [`Bounds::for_binding`], blobs by [`BINDING_READ_BUDGET`].
fn page_size(per_page: u32) -> usize {
    (per_page as usize).min(BINDING_MAX_PER_PAGE)
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
        isolated: false,
        repositories: None,
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
            isolated: false,
            repositories: None,
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
    with_node(
        |node| match node.list_repos_bounded(&Bounds::for_binding()) {
            Ok(repos) => json::to_string(|w| json::repos(w, &repos)),
            Err(e) => err_json(e),
        },
    )
}

/// Explicit allow-seeding policies, including repositories still awaiting a
/// successful first fetch.
#[uniffi::export]
pub fn list_seeded_repos() -> String {
    with_node(
        |node| match node.list_seeded_repos_bounded(&Bounds::for_binding()) {
            Ok(repos) => json::to_string(|w| json::seeded_repos(w, &repos)),
            Err(e) => err_json(e),
        },
    )
}

/// All issues in a repository, in radicle-httpd-compatible JSON shape.
#[uniffi::export]
pub fn issues(rid: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(
        |node| match node.issues_bounded(rid, &Bounds::for_binding()) {
            Ok(issues) => json::to_string(|w| json::issues(w, &issues)),
            Err(e) => err_json(e),
        },
    )
}

/// One issue in radicle-httpd-compatible JSON shape.
#[uniffi::export]
pub fn issue(rid: String, issue_id: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(
        |node| match node.issue_bounded(rid, &issue_id, &Bounds::for_binding()) {
            Ok((id, issue)) => json::to_string(|w| json::issue(w, &id, &issue)),
            Err(e) => err_json(e),
        },
    )
}

/// All patches in a repository, in radicle-httpd-compatible JSON shape.
#[uniffi::export]
pub fn patches(rid: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(
        |node| match node.patches_bounded(rid, &Bounds::for_binding()) {
            Ok(patches) => json::to_string(|w| json::patches(w, &patches)),
            Err(e) => err_json(e),
        },
    )
}

/// One patch in radicle-httpd-compatible JSON shape.
#[uniffi::export]
pub fn patch(rid: String, patch_id: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(
        |node| match node.patch_bounded(rid, &patch_id, &Bounds::for_binding()) {
            Ok((id, patch)) => json::to_string(|w| json::patch(w, &id, &patch)),
            Err(e) => err_json(e),
        },
    )
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
    with_node(
        |node| match node.repo_info_bounded(rid, &Bounds::for_binding()) {
            Ok(info) => json::to_string(|w| json::repo_info(w, &info)),
            Err(e) => err_json(e),
        },
    )
}

/// Paginated commit history reachable from a validated full commit id.
#[uniffi::export]
pub fn commits(rid: String, parent: String, page: u32, per_page: u32) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| {
        match node.commits_bounded(
            rid,
            &parent,
            page as usize,
            page_size(per_page),
            &Bounds::for_binding(),
        ) {
            Ok(commits) => json::to_string(|w| json::commits(w, &commits)),
            Err(e) => err_json(e),
        }
    })
}

/// Commit metadata plus its structured first-parent diff.
#[uniffi::export]
pub fn commit(rid: String, revision: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(
        |node| match node.commit_bounded(rid, &revision, &Bounds::for_binding()) {
            Ok(detail) => json::to_string(|w| json::commit(w, &detail)),
            Err(e) => err_json(e),
        },
    )
}

/// File paths at the head of the default branch, as a JSON array.
#[uniffi::export]
pub fn list_files(rid: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(
        |node| match node.list_files_bounded(rid, &Bounds::for_binding()) {
            Ok(files) => json::to_string(|w| json::files(w, &files)),
            Err(e) => err_json(e),
        },
    )
}

/// Tree entries at the head of the default branch, httpd-shaped.
#[uniffi::export]
pub fn tree(rid: String, path: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(
        |node| match node.tree_bounded(rid, &path, &Bounds::for_binding()) {
            Ok(entries) => json::to_string(|w| json::tree(w, &entries)),
            Err(e) => err_json(e),
        },
    )
}

/// Tree entries at a validated full commit id, including commit metadata.
#[uniffi::export]
pub fn tree_at(rid: String, revision: String, path: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(
        |node| match node.tree_at_bounded(rid, &revision, &path, &Bounds::for_binding()) {
            Ok((entries, commit)) => json::to_string(|w| json::tree_at(w, &entries, &commit)),
            Err(e) => err_json(e),
        },
    )
}

/// Blob at the head of the default branch, httpd-shaped. Binary blobs omit content.
#[uniffi::export]
pub fn blob(rid: String, path: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(
        |node| match node.read_blob_capped(rid, &path, BINDING_READ_BUDGET) {
            Ok(b) => json::to_string(|w| json::blob(w, &path, &b)),
            Err(e) => err_json(e),
        },
    )
}

/// Blob at a validated full commit id.
#[uniffi::export]
pub fn blob_at(rid: String, revision: String, path: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(
        |node| match node.read_blob_at_capped(rid, &revision, &path, BINDING_READ_BUDGET) {
            Ok(b) => json::to_string(|w| json::blob(w, &path, &b)),
            Err(e) => err_json(e),
        },
    )
}

/// Signed remotes and their branch heads.
#[uniffi::export]
pub fn remotes(rid: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(
        |node| match node.remotes_bounded(rid, &Bounds::for_binding()) {
            Ok(remotes) => json::to_string(|w| json::remotes(w, &remotes)),
            Err(e) => err_json(e),
        },
    )
}

/// Repository history statistics reachable from a validated full commit id.
#[uniffi::export]
pub fn repo_stats(rid: String, revision: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(
        |node| match node.repo_stats_bounded(rid, &revision, &Bounds::for_binding()) {
            Ok(stats) => json::to_string(|w| json::repo_stats(w, &stats)),
            Err(e) => err_json(e),
        },
    )
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

/// Stop the node, join its thread, and wait for it to let go of its home,
/// within `timeout_ms` for all of it ([`libradicle::shutdown_slot`]), or
/// [`libradicle::SHUTDOWN_DEADLINE`] (30 s) if none is passed. A host passes
/// a deadline below its own watchdog's.
///
/// `{"ok": true}` once the node has stopped and its home is free. Past the
/// deadline, `{"error": "...", "stopping": true}`: the node stays, asked to
/// stop, and another `shutdown` waits again; a start before then is
/// `HomeLocked`. Any other error is final, and the node gone.
#[uniffi::export]
pub fn shutdown(timeout_ms: Option<u32>) -> String {
    let timeout = timeout_ms.map_or(libradicle::SHUTDOWN_DEADLINE, |ms| {
        Duration::from_millis(u64::from(ms))
    });
    let mut guard = match NODE.lock() {
        Ok(guard) => guard,
        Err(_) => return err_json("node lock poisoned"),
    };
    match libradicle::shutdown_slot(&mut guard, timeout) {
        Ok(libradicle::Slot::Stopped) => json::to_string(json::ok),
        Ok(libradicle::Slot::Empty) => err_json("node not started"),
        Err(e @ libradicle::Error::ShutdownTimeout(_)) => json::to_string(|w| json::stopping(w, e)),
        Err(e) => err_json(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that use the global `NODE` take this, so they never interleave.
    static NODE_TESTS: Mutex<()> = Mutex::new(());

    fn node_tests() -> std::sync::MutexGuard<'static, ()> {
        NODE_TESTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The exported surface is callable and returns the documented sentinels
    /// before a node is started (host-side smoke — device tests run on macOS).
    #[test]
    fn exports_return_not_started_sentinels() {
        let _serial = node_tests();
        assert!(identity().contains("node not started"));
        assert!(list_repos().contains("node not started"));
        assert!(status().contains("node not started"));
        assert!(shutdown(None).contains("node not started"));
        // Argument validation happens before the node check.
        assert!(clone_repo("not-a-rid".into(), 1000).contains("invalid RID"));
        // Cancelling an unknown clone is a well-formed negative.
        assert_eq!(cancel_clone("whatever".into()), r#"{"cancelled":false}"#);
    }

    fn error_of(json: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(json).expect("JSON");
        value["error"].as_str().unwrap_or_default().to_string()
    }

    /// R-18: the reads a host calls are bounded. A started node (dialing
    /// nobody) with one repository: a file one byte over the binding budget,
    /// added in the last of more commits than a page may hold.
    #[test]
    fn binding_reads_are_bounded() {
        let _serial = node_tests();
        let name = format!("lr-uniffi-bounded-{}", std::process::id());
        let mut dir = std::env::temp_dir().join(&name);
        if dir.join("home/node/control.sock").as_os_str().len() > 100 {
            dir = std::path::Path::new("/tmp").join(&name);
        }
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.json"),
            r#"{"preferredSeeds":[],"node":{"alias":"bounded","network":"test","connect":[],"peers":{"type":"static"}}}"#,
        )
        .unwrap();

        let work = dir.join("work");
        let repo = git2::Repository::init(&work).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        let mut parent: Option<git2::Oid> = None;
        let commits = BINDING_MAX_PER_PAGE + 5;
        for i in 0..commits {
            let mut tree = repo.treebuilder(None).unwrap();
            let small = repo.blob(format!("commit {i}\n").as_bytes()).unwrap();
            tree.insert("small.txt", small, 0o100644).unwrap();
            if i + 1 == commits {
                let big = repo
                    .blob(&vec![b'x'; BINDING_READ_BUDGET as usize + 1])
                    .unwrap();
                tree.insert("big.txt", big, 0o100644).unwrap();
            }
            let tree = repo.find_tree(tree.write().unwrap()).unwrap();
            let parents: Vec<git2::Commit> = parent
                .map(|oid| repo.find_commit(oid).unwrap())
                .into_iter()
                .collect();
            let parents: Vec<&git2::Commit> = parents.iter().collect();
            parent = Some(
                repo.commit(
                    Some("refs/heads/main"),
                    &signature,
                    &signature,
                    "change",
                    &tree,
                    &parents,
                )
                .unwrap(),
            );
        }
        repo.set_head("refs/heads/main").unwrap();
        let head = parent.unwrap().to_string();

        let started = start(home.to_str().unwrap().into(), "bounded".into());
        assert!(started.contains("did:key:"), "{started}");
        let imported: serde_json::Value = serde_json::from_str(&import_repo(
            work.to_str().unwrap().into(),
            "bounded".into(),
            String::new(),
            "main".into(),
        ))
        .unwrap();
        let rid = imported["rid"].as_str().expect("rid").to_string();

        // A page size far over the cap returns a capped page.
        let page: serde_json::Value =
            serde_json::from_str(&super::commits(rid.clone(), head.clone(), 0, u32::MAX)).unwrap();
        assert_eq!(
            page.as_array().map(Vec::len),
            Some(BINDING_MAX_PER_PAGE),
            "{page}"
        );

        // A blob over the budget is refused before it is loaded, at head and
        // at a revision; a small one still reads.
        for refused in [
            blob(rid.clone(), "big.txt".into()),
            blob_at(rid.clone(), head.clone(), "big.txt".into()),
        ] {
            assert!(error_of(&refused).contains("over the limit"), "{refused}");
        }
        assert!(blob(rid.clone(), "small.txt".into()).contains("commit"));

        // The diff that adds it does not fit the budget either.
        let diff = commit(rid.clone(), head.clone());
        assert!(error_of(&diff).contains("over the limit"), "{diff}");

        // Walks within the budget answer as before.
        assert!(list_files(rid.clone()).contains("big.txt"));
        assert!(repo_stats(rid.clone(), head.clone()).contains("\"commits\""));
        assert!(error_of(&issues(rid.clone())).is_empty());
        // So do the reads bounded since (review of libradicle #15).
        assert!(tree(rid.clone(), String::new()).contains("big.txt"));
        assert!(tree_at(rid.clone(), head.clone(), String::new()).contains("lastCommit"));
        assert!(error_of(&remotes(rid.clone())).is_empty());
        assert!(list_seeded_repos().contains(&rid));
        assert!(repo_info(rid.clone()).contains("\"defaultBranch\""));
        let missing = issue(rid.clone(), "0000000000".into());
        assert!(error_of(&missing).contains("not found"), "{missing}");

        assert_eq!(shutdown(None), r#"{"ok":true}"#);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_size_is_capped() {
        assert_eq!(page_size(0), 0);
        assert_eq!(page_size(30), 30);
        assert_eq!(page_size(u32::MAX), BINDING_MAX_PER_PAGE);
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

    /// A gate whose name lookups wait until it is released.
    #[derive(Debug, Default)]
    struct Held {
        state: Mutex<(usize, bool)>,
        changed: std::sync::Condvar,
    }

    impl radicle::node::dial::Gate for Held {
        fn allows(&self, _: &radicle::node::Address, _: &std::net::SocketAddr) -> bool {
            false
        }

        fn resolve(&self, _: &str, _: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
            let mut state = self.state.lock().unwrap();
            state.0 += 1;
            self.changed.notify_all();
            while !state.1 {
                state = self.changed.wait(state).unwrap();
            }
            Ok(vec![])
        }
    }

    #[test]
    // Review of libradicle #19, round 1: a shutdown past its deadline, while
    // a name lookup keeps the node's home, keeps the node, says it is
    // stopping, and returns by the deadline. A start then finds the node
    // still there; another shutdown, once the lookup ends, finishes it, and
    // a node starts on the home at once.
    fn a_shutdown_past_its_deadline_keeps_the_node_for_a_retry() {
        let _serial = node_tests();
        let home = std::path::Path::new("/tmp").join(format!("radufi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let peer = {
            use radicle::crypto::Signer as _;
            *radicle::crypto::SigningKey::from_seed(radicle::crypto::Seed::new([6; 32]))
                .public_key()
        };
        std::fs::write(
            home.join("config.json"),
            format!(
                r#"{{"preferredSeeds":[],"node":{{"alias":"uniffi","network":"test","peers":{{"type":"static"}},"connect":["{peer}@held.test:8776"]}}}}"#
            ),
        )
        .unwrap();
        let gate = std::sync::Arc::new(Held::default());
        let node = Embedded::start(Options {
            home: home.to_str().unwrap().into(),
            alias: "uniffi".into(),
            listen: vec![],
            control_socket: true,
            routing: Some(libradicle::Routing {
                proxy: None,
                onion: radicle::node::config::AddressConfig::Drop,
                gate: gate.clone(),
            }),
            isolated: false,
            repositories: None,
        })
        .unwrap();
        *NODE.lock().unwrap() = Some(node);
        {
            let mut state = gate.state.lock().unwrap();
            while state.0 == 0 {
                state = gate.changed.wait(state).unwrap();
            }
        }

        let started = std::time::Instant::now();
        let stopping: serde_json::Value = serde_json::from_str(&shutdown(Some(1_500))).unwrap();
        assert_eq!(stopping["stopping"], true, "{stopping}");
        assert!(started.elapsed() < Duration::from_millis(2_500));
        assert!(NODE.lock().unwrap().is_some(), "the node is kept");
        let home_str = home.to_str().unwrap().to_owned();
        assert!(start(home_str.clone(), "uniffi".into()).contains("node already started"));

        {
            gate.state.lock().unwrap().1 = true;
            gate.changed.notify_all();
        }
        assert_eq!(shutdown(Some(10_000)), r#"{"ok":true}"#);
        assert!(NODE.lock().unwrap().is_none());
        assert!(start(home_str, "uniffi".into()).contains("did"));
        assert_eq!(shutdown(None), r#"{"ok":true}"#);
        let _ = std::fs::remove_dir_all(&home);
    }
}
