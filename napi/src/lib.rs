//! Node.js binding over `libradicle`, in the style of Freedom Browser's
//! myotis-node addon: every export returns a JSON string, blocking work
//! runs on the libuv thread pool via `AsyncTask`.
//!
//! One embedded node per process, held in a global slot. The Freedom main
//! process owns exactly one Radicle profile at a time, so a singleton is
//! the honest shape — a second `start` without `shutdown` is an error.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use napi::bindgen_prelude::AsyncTask;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{Env, Result, Task};
use napi_derive::napi;

use libradicle::{json, CancelToken, Embedded, FetchPolicy, Network, Options, Progress};

static NODE: Mutex<Option<Embedded>> = Mutex::new(None);

/// In-flight clone cancellation tokens, keyed by full RID. Registered when
/// a progress clone starts and removed when it finishes; `cancel_clone`
/// flips the matching token.
static CANCELS: LazyLock<Mutex<HashMap<String, CancelToken>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A JS `(event: string) => void` progress callback. Non-callee-handled:
/// JS receives the JSON string directly, no error tuple.
type ProgressCb = ThreadsafeFunction<String, (), String, napi::Status, false>;

fn err_json(e: impl std::fmt::Display) -> String {
    json::to_string(|w| json::error(w, e))
}

/// One blocking call scheduled on the libuv thread pool (myotis pattern).
pub struct BlockingJson {
    run: Option<Box<dyn FnOnce() -> String + Send>>,
}

impl Task for BlockingJson {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        match self.run.take() {
            Some(f) => Ok(f()),
            None => Ok(r#"{"error":"task already ran"}"#.to_string()),
        }
    }

    fn resolve(&mut self, _env: Env, output: String) -> Result<String> {
        Ok(output)
    }
}

fn blocking(f: impl FnOnce() -> String + Send + 'static) -> AsyncTask<BlockingJson> {
    AsyncTask::new(BlockingJson {
        run: Some(Box::new(f)),
    })
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

/// Start the embedded node with a profile at `home`. Resolves to
/// `{"did": "..."}` once the node thread is up.
#[napi]
pub fn start(home: String, alias: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
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
        }) {
            Ok(node) => {
                let did = node.did();
                *guard = Some(node);
                json::to_string(|w| json::did(w, &did))
            }
            Err(e) => err_json(e),
        }
    })
}

/// Concurrently bootstrap from the effective seed book. The response retains
/// `connected` and adds attempt/readiness diagnostics.
#[napi]
pub fn connect_seeds(timeout_ms: u32) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let mut network = match network() {
            Ok(network) => network,
            Err(e) => return err_json(e),
        };
        match network.connect_seed_book(Duration::from_millis(timeout_ms.into())) {
            Ok(report) => json::to_string(|w| json::seed_report(w, &report)),
            Err(e) => err_json(e),
        }
    })
}

/// Seed + fetch a repository into storage. Resolves to `{"ok": true}`.
#[napi]
pub fn clone_repo(rid: String, timeout_ms: u32) -> AsyncTask<BlockingJson> {
    blocking(move || {
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
    })
}

/// Seed + fetch a repository, pushing progress events to `on_progress`
/// (a `(event: string) => void` callback receiving one JSON object per
/// event: `{phase, ...}`). Resolves to `{"ok":true}`, `{"cancelled":true}`,
/// or `{"error":...}`. Cancel mid-flight with `cancelClone(rid)`.
#[napi]
pub fn clone_repo_with_progress(
    rid: String,
    timeout_ms: u32,
    on_progress: ProgressCb,
) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let parsed = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        let mut network = match network() {
            Ok(network) => network,
            Err(e) => return err_json(e),
        };

        // Register a cancel token under the RID for the duration.
        let cancel = CancelToken::new();
        if let Ok(mut map) = CANCELS.lock() {
            map.insert(rid.clone(), cancel.clone());
        }

        let policy = FetchPolicy::from_timeout(Duration::from_millis(timeout_ms.into()));
        let mut emit = |p: Progress| {
            on_progress.call(
                json::to_string(|w| json::progress(w, &p)),
                ThreadsafeFunctionCallMode::NonBlocking,
            );
        };
        let result = network.clone_repo_with(parsed, &policy, &cancel, &mut emit);

        if let Ok(mut map) = CANCELS.lock() {
            map.remove(&rid);
        }
        match result {
            Ok(()) => json::to_string(json::ok),
            Err(libradicle::Error::Cancelled) => json::to_string(|w| json::cancelled(w, true)),
            Err(e) => err_json(e),
        }
    })
}

/// Request cancellation of an in-flight `cloneRepoWithProgress` for `rid`.
/// Returns `{"cancelled": bool}` — false if no clone was running.
#[napi]
pub fn cancel_clone(rid: String) -> String {
    let found = CANCELS
        .lock()
        .ok()
        .and_then(|map| map.get(&rid).map(|t| t.cancel()))
        .is_some();
    json::to_string(|w| json::cancelled(w, found))
}

/// Stop seeding a repository. Resolves to `{"unseeded": bool}`.
#[napi]
pub fn unseed_repo(rid: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.unseed_repo(rid) {
            Ok(unseeded) => json::to_string(|w| json::unseeded(w, unseeded)),
            Err(e) => err_json(e),
        })
    })
}

/// The public profile identity used by the browser provider.
#[napi]
pub fn identity() -> AsyncTask<BlockingJson> {
    blocking(|| {
        with_node(|node| {
            let identity = node.identity();
            json::to_string(|w| json::identity(w, &identity))
        })
    })
}

/// Repositories the local node is currently seeding.
#[napi]
pub fn list_repos() -> AsyncTask<BlockingJson> {
    blocking(|| {
        with_node(|node| match node.list_repos() {
            Ok(repos) => json::to_string(|w| json::repos(w, &repos)),
            Err(e) => err_json(e),
        })
    })
}

/// Explicit allow-seeding policies, including repositories still awaiting a
/// successful first fetch.
#[napi]
pub fn list_seeded_repos() -> AsyncTask<BlockingJson> {
    blocking(|| {
        with_node(|node| match node.list_seeded_repos() {
            Ok(repos) => json::to_string(|w| json::seeded_repos(w, &repos)),
            Err(e) => err_json(e),
        })
    })
}

/// All issues in a repository, in radicle-httpd-compatible JSON shape.
#[napi]
pub fn issues(rid: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.issues(rid) {
            Ok(issues) => json::to_string(|w| json::issues(w, &issues)),
            Err(e) => err_json(e),
        })
    })
}

/// One issue in radicle-httpd-compatible JSON shape.
#[napi]
pub fn issue(rid: String, issue_id: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.issue(rid, &issue_id) {
            Ok((id, issue)) => json::to_string(|w| json::issue(w, &id, &issue)),
            Err(e) => err_json(e),
        })
    })
}

/// All patches in a repository, in radicle-httpd-compatible JSON shape.
#[napi]
pub fn patches(rid: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.patches(rid) {
            Ok(patches) => json::to_string(|w| json::patches(w, &patches)),
            Err(e) => err_json(e),
        })
    })
}

/// One patch in radicle-httpd-compatible JSON shape.
#[napi]
pub fn patch(rid: String, patch_id: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.patch(rid, &patch_id) {
            Ok((id, patch)) => json::to_string(|w| json::patch(w, &id, &patch)),
            Err(e) => err_json(e),
        })
    })
}

/// Create an issue directly in the COB store.
#[napi]
pub fn create_issue(
    rid: String,
    title: String,
    description: String,
    labels_json: String,
) -> AsyncTask<BlockingJson> {
    blocking(move || {
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
    })
}

/// Comment on an issue.
#[napi]
pub fn comment_issue(
    rid: String,
    issue_id: String,
    body: String,
    reply_to: Option<String>,
) -> AsyncTask<BlockingJson> {
    blocking(move || {
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
    })
}

/// Transition an issue state.
#[napi]
pub fn edit_issue_state(rid: String, issue_id: String, state: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.edit_issue_state(rid, &issue_id, &state) {
            Ok(id) => json::to_string(|w| json::id_state(w, id, &state)),
            Err(e) => err_json(e),
        })
    })
}

/// Comment on a patch revision.
#[napi]
pub fn comment_patch(rid: String, revision_id: String, body: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.comment_patch(rid, &revision_id, &body) {
            Ok(id) => json::to_string(|w| json::id(w, id)),
            Err(e) => err_json(e),
        })
    })
}

/// Import a working Git repository into native Radicle storage.
#[napi]
pub fn import_repo(
    path: String,
    name: String,
    description: String,
    default_branch: String,
) -> AsyncTask<BlockingJson> {
    blocking(move || {
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
    })
}

/// Identity payload + head + COB counts, straight from storage.
#[napi]
pub fn repo_info(rid: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.repo_info(rid) {
            Ok(info) => json::to_string(|w| json::repo_info(w, &info)),
            Err(e) => err_json(e),
        })
    })
}

/// Paginated commit history reachable from a validated full commit id.
#[napi]
pub fn commits(rid: String, parent: String, page: u32, per_page: u32) -> AsyncTask<BlockingJson> {
    blocking(move || {
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
    })
}

/// Commit metadata plus its structured first-parent diff.
#[napi]
pub fn commit(rid: String, revision: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.commit(rid, &revision) {
            Ok(detail) => json::to_string(|w| json::commit(w, &detail)),
            Err(e) => err_json(e),
        })
    })
}

/// File paths at the head of the default branch, as a JSON array.
#[napi]
pub fn list_files(rid: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.list_files(rid) {
            Ok(files) => json::to_string(|w| json::files(w, &files)),
            Err(e) => err_json(e),
        })
    })
}

/// Tree entries at the head of the default branch, httpd-shaped:
/// `{"entries":[{"name","path","kind"}]}`.
#[napi]
pub fn tree(rid: String, path: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.tree(rid, &path) {
            Ok(entries) => json::to_string(|w| json::tree(w, &entries)),
            Err(e) => err_json(e),
        })
    })
}

/// Tree entries at a validated full commit id, including commit metadata.
#[napi]
pub fn tree_at(rid: String, revision: String, path: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.tree_at(rid, &revision, &path) {
            Ok((entries, commit)) => json::to_string(|w| json::tree_at(w, &entries, &commit)),
            Err(e) => err_json(e),
        })
    })
}

/// Blob at the head of the default branch, httpd-shaped:
/// `{"binary":bool,"content":"...","name":"..."}`. Binary blobs omit
/// content (the viewer renders a placeholder for them).
#[napi]
pub fn blob(rid: String, path: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.read_blob(rid, &path) {
            Ok(b) => json::to_string(|w| json::blob(w, &path, &b)),
            Err(e) => err_json(e),
        })
    })
}

/// Blob at a validated full commit id.
#[napi]
pub fn blob_at(rid: String, revision: String, path: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.read_blob_at(rid, &revision, &path) {
            Ok(b) => json::to_string(|w| json::blob(w, &path, &b)),
            Err(e) => err_json(e),
        })
    })
}

/// Signed remotes and their branch heads.
#[napi]
pub fn remotes(rid: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.remotes(rid) {
            Ok(remotes) => json::to_string(|w| json::remotes(w, &remotes)),
            Err(e) => err_json(e),
        })
    })
}

/// Repository history statistics reachable from a validated full commit id.
#[napi]
pub fn repo_stats(rid: String, revision: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.repo_stats(rid, &revision) {
            Ok(stats) => json::to_string(|w| json::repo_stats(w, &stats)),
            Err(e) => err_json(e),
        })
    })
}

/// Node status: `{"connectedPeers": n}`.
#[napi]
pub fn status() -> AsyncTask<BlockingJson> {
    blocking(|| {
        with_node(|node| match node.connected_peers() {
            Ok(n) => json::to_string(|w| json::connected_peers(w, n)),
            Err(e) => err_json(e),
        })
    })
}

/// Number of known seeders for a repo: `{"seeding": n}`.
#[napi]
pub fn seeders(rid: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.seeders(rid) {
            Ok(n) => json::to_string(|w| json::seeding(w, n)),
            Err(e) => err_json(e),
        })
    })
}

/// Gracefully stop the node and join its thread. Resolves to `{"ok": true}`.
#[napi]
pub fn shutdown() -> AsyncTask<BlockingJson> {
    blocking(|| {
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
    })
}
