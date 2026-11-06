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

use libradicle::{
    json, Bounds, CancelToken, Embedded, FetchPolicy, Network, Options, Progress,
    BINDING_MAX_PER_PAGE, BINDING_READ_BUDGET,
};

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

/// The text of a caught panic, for the error payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}

/// Run `f`, answering a panic with an `{"error": ...}` payload instead.
///
/// A panic must not reach napi: `compute` runs on a libuv worker inside an
/// `extern "C"` callback, where an unwinding panic aborts the process, and
/// the process is the host application (the whole browser, for Freedom).
fn guarded(f: impl FnOnce() -> String) -> String {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(json) => json,
        Err(payload) => err_json(format!(
            "libradicle panicked: {}",
            panic_message(payload.as_ref())
        )),
    }
}

/// One blocking call scheduled on the libuv thread pool (myotis pattern).
pub struct BlockingJson {
    run: Option<Box<dyn FnOnce() -> String + Send>>,
}

impl Task for BlockingJson {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        Ok(self.output())
    }

    fn resolve(&mut self, _env: Env, output: String) -> Result<String> {
        Ok(output)
    }
}

impl BlockingJson {
    /// The task's result: its work, run once, with a panic in it answered
    /// as an error payload ([`guarded`]).
    fn output(&mut self) -> String {
        match self.run.take() {
            Some(f) => guarded(f),
            None => r#"{"error":"task already ran"}"#.to_string(),
        }
    }
}

fn blocking(f: impl FnOnce() -> String + Send + 'static) -> AsyncTask<BlockingJson> {
    AsyncTask::new(BlockingJson {
        run: Some(Box::new(f)),
    })
}

/// Run `f` on the `NODE` slot, its guard held. A panic in `f` is caught
/// here, while the guard is still held, so it does not poison `NODE` for
/// every later call. `start`, `shutdown` and every call on the node go
/// through this.
fn with_slot(f: impl FnOnce(&mut Option<Embedded>) -> String) -> String {
    match NODE.lock() {
        Ok(mut guard) => guarded(|| f(&mut guard)),
        Err(_) => err_json("node lock poisoned"),
    }
}

/// Run `f` on the started node ([`with_slot`]).
fn with_node(f: impl FnOnce(&mut Embedded) -> String) -> String {
    with_slot(|slot| match slot.as_mut() {
        Some(node) => f(node),
        None => err_json("node not started"),
    })
}

/// Start a node into `NODE` with `start`, answering `{"did": ...}` or an
/// error payload. A panic in `start` is caught while the guard is held
/// ([`with_slot`]), so a failed start never poisons `NODE` for the next one.
fn start_node(start: impl FnOnce() -> std::result::Result<Embedded, libradicle::Error>) -> String {
    with_slot(|slot| {
        if slot.is_some() {
            return err_json("node already started");
        }
        match start() {
            Ok(node) => {
                let did = node.did();
                *slot = Some(node);
                json::to_string(|w| json::did(w, &did))
            }
            Err(e) => err_json(e),
        }
    })
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

/// The options `start` gives the node: `config.json`'s routing. In this
/// crate's tests, the hermetic guard's instead, checked before the node
/// starts, so that a test through `start` never reaches the network
/// (`tests/common/hermetic.rs`), as in the UniFFI binding.
fn node_options(home: String, alias: String) -> Options {
    let opts = Options {
        home: home.into(),
        alias,
        listen: vec![],
        control_socket: true,
        routing: None,
        seeding_policy: None,
        isolated: false,
        repositories: None,
    };
    #[cfg(test)]
    let opts = {
        let opts = Options {
            routing: Some(libradicle::Routing {
                proxy: None,
                onion: radicle::node::config::AddressConfig::Drop,
                gate: hermetic::gate(),
            }),
            ..opts
        };
        hermetic::check(
            &opts.home,
            opts.isolated,
            &opts.listen,
            Some((None, &radicle::node::config::AddressConfig::Drop)),
        );
        opts
    };
    opts
}

/// Start the embedded node with a profile at `home`. Resolves to
/// `{"did": "..."}` once the node thread is up.
#[napi]
pub fn start(home: String, alias: String) -> AsyncTask<BlockingJson> {
    blocking(move || start_json(home, alias))
}

fn start_json(home: String, alias: String) -> String {
    start_node(|| Embedded::start(node_options(home, alias)))
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
        with_node(
            |node| match node.list_repos_bounded(&Bounds::for_binding()) {
                Ok(repos) => json::to_string(|w| json::repos(w, &repos)),
                Err(e) => err_json(e),
            },
        )
    })
}

/// Explicit allow-seeding policies, including repositories still awaiting a
/// successful first fetch.
#[napi]
pub fn list_seeded_repos() -> AsyncTask<BlockingJson> {
    blocking(|| {
        with_node(
            |node| match node.list_seeded_repos_bounded(&Bounds::for_binding()) {
                Ok(repos) => json::to_string(|w| json::seeded_repos(w, &repos)),
                Err(e) => err_json(e),
            },
        )
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
        with_node(
            |node| match node.issues_bounded(rid, &Bounds::for_binding()) {
                Ok(issues) => json::to_string(|w| json::issues(w, &issues)),
                Err(e) => err_json(e),
            },
        )
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
        with_node(
            |node| match node.issue_bounded(rid, &issue_id, &Bounds::for_binding()) {
                Ok((id, issue)) => json::to_string(|w| json::issue(w, &id, &issue)),
                Err(e) => err_json(e),
            },
        )
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
        with_node(
            |node| match node.patches_bounded(rid, &Bounds::for_binding()) {
                Ok(patches) => json::to_string(|w| json::patches(w, &patches)),
                Err(e) => err_json(e),
            },
        )
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
        with_node(
            |node| match node.patch_bounded(rid, &patch_id, &Bounds::for_binding()) {
                Ok((id, patch)) => json::to_string(|w| json::patch(w, &id, &patch)),
                Err(e) => err_json(e),
            },
        )
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
        with_node(
            |node| match node.repo_info_bounded(rid, &Bounds::for_binding()) {
                Ok(info) => json::to_string(|w| json::repo_info(w, &info)),
                Err(e) => err_json(e),
            },
        )
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
        with_node(
            |node| match node.commit_bounded(rid, &revision, &Bounds::for_binding()) {
                Ok(detail) => json::to_string(|w| json::commit(w, &detail)),
                Err(e) => err_json(e),
            },
        )
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
        with_node(
            |node| match node.list_files_bounded(rid, &Bounds::for_binding()) {
                Ok(files) => json::to_string(|w| json::files(w, &files)),
                Err(e) => err_json(e),
            },
        )
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
        with_node(
            |node| match node.tree_bounded(rid, &path, &Bounds::for_binding()) {
                Ok(entries) => json::to_string(|w| json::tree(w, &entries)),
                Err(e) => err_json(e),
            },
        )
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
        with_node(|node| {
            match node.tree_at_bounded(rid, &revision, &path, &Bounds::for_binding()) {
                Ok((entries, commit)) => json::to_string(|w| json::tree_at(w, &entries, &commit)),
                Err(e) => err_json(e),
            }
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
        with_node(
            |node| match node.read_blob_capped(rid, &path, BINDING_READ_BUDGET) {
                Ok(b) => json::to_string(|w| json::blob(w, &path, &b)),
                Err(e) => err_json(e),
            },
        )
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
        with_node(|node| {
            match node.read_blob_at_capped(rid, &revision, &path, BINDING_READ_BUDGET) {
                Ok(b) => json::to_string(|w| json::blob(w, &path, &b)),
                Err(e) => err_json(e),
            }
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
        with_node(
            |node| match node.remotes_bounded(rid, &Bounds::for_binding()) {
                Ok(remotes) => json::to_string(|w| json::remotes(w, &remotes)),
                Err(e) => err_json(e),
            },
        )
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
        with_node(
            |node| match node.repo_stats_bounded(rid, &revision, &Bounds::for_binding()) {
                Ok(stats) => json::to_string(|w| json::repo_stats(w, &stats)),
                Err(e) => err_json(e),
            },
        )
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

/// Stop the node, join its thread, and wait for it to let go of its home,
/// within `timeoutMs` for all of it ([`libradicle::shutdown_slot`]), or
/// [`libradicle::SHUTDOWN_DEADLINE`] (30 s) if none is passed. A host passes
/// a deadline below its own quit watchdog's.
///
/// Resolves to `{"ok": true}` once the node has stopped and its home is
/// free. Past the deadline, to `{"error": "...", "stopping": true}`: the
/// node stays, asked to stop, and another `shutdown` waits again; a start
/// before then is `HomeLocked`. Any other error is final, and the node gone.
#[napi]
pub fn shutdown(timeout_ms: Option<u32>) -> AsyncTask<BlockingJson> {
    blocking(move || shutdown_json(timeout_ms))
}

fn shutdown_json(timeout_ms: Option<u32>) -> String {
    let timeout = timeout_ms.map_or(libradicle::SHUTDOWN_DEADLINE, |ms| {
        Duration::from_millis(u64::from(ms))
    });
    // The node stays in the slot while it stops, so the guard is held
    // throughout: a panic is caught inside it ([`with_slot`]).
    with_slot(|slot| match libradicle::shutdown_slot(slot, timeout) {
        Ok(libradicle::Slot::Stopped) => json::to_string(json::ok),
        Ok(libradicle::Slot::Empty) => err_json("node not started"),
        Err(e @ libradicle::Error::ShutdownTimeout(_)) => json::to_string(|w| json::stopping(w, e)),
        Err(e) => err_json(e),
    })
}

/// The guard every test node gets: loopback only, and loudly. `start`
/// takes it too, in this crate's tests ([`node_options`]).
#[cfg(test)]
#[path = "../../tests/common/hermetic.rs"]
mod hermetic;

#[cfg(test)]
mod tests {
    use super::{
        guarded, page_size, shutdown_json, start_json, start_node, with_node, BlockingJson,
        BINDING_MAX_PER_PAGE, NODE,
    };

    #[test]
    fn a_panic_becomes_an_error_payload() {
        let json: serde_json::Value =
            serde_json::from_str(&guarded(|| panic!("boom"))).expect("JSON");
        assert_eq!(json["error"], "libradicle panicked: boom");

        let json: serde_json::Value =
            serde_json::from_str(&guarded(|| panic!("{}", String::from("owned")))).unwrap();
        assert_eq!(json["error"], "libradicle panicked: owned");

        assert_eq!(guarded(|| "{}".to_string()), "{}");

        // Every task runs through it (`compute`, on the libuv worker).
        let mut task = BlockingJson {
            run: Some(Box::new(|| -> String { panic!("in a task") })),
        };
        let json: serde_json::Value = serde_json::from_str(&task.output()).unwrap();
        assert_eq!(json["error"], "libradicle panicked: in a task");
    }

    fn error_of(json: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(json).expect("JSON");
        value["error"].as_str().unwrap_or_default().to_string()
    }

    fn temp_home(name: &str) -> std::path::PathBuf {
        let mut home = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        if home.join("node/control.sock").as_os_str().len() > 100 {
            home = std::path::Path::new("/tmp").join(format!("{name}-{}", std::process::id()));
        }
        let _ = std::fs::remove_dir_all(&home);
        home
    }

    /// One test drives `NODE` end to end, so no two tests race the global.
    #[test]
    fn a_failed_start_leaves_the_node_slot_usable() {
        // A panic inside the startup lock is answered and does not poison it.
        let failed = start_node(|| panic!("startup panic"));
        assert_eq!(error_of(&failed), "libradicle panicked: startup panic");
        assert!(NODE.lock().is_ok(), "NODE is not poisoned");

        // An alias Radicle refuses is an error, not a panic, and nothing is
        // written for it.
        let home = temp_home("lr-napi-start");
        let home_str = home.to_str().expect("utf-8 temp dir").to_owned();
        let invalid = start_json(home_str.clone(), "invalid alias".into());
        assert!(error_of(&invalid).contains("alias"), "{invalid}");
        assert!(NODE.lock().is_ok(), "NODE is not poisoned");
        assert!(!home.join("config.json").exists());

        // The next, valid start works, and so do calls on the node. Its home
        // dials nobody (a fresh profile's defaults would dial the seeds).
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.json"),
            r#"{"preferredSeeds":[],"node":{"alias":"ValidAlias","network":"test","connect":[],"peers":{"type":"static"}}}"#,
        )
        .unwrap();
        let started: serde_json::Value =
            serde_json::from_str(&start_json(home_str.clone(), "ValidAlias".into())).unwrap();
        assert!(
            started["did"].as_str().unwrap().starts_with("did:key:"),
            "{started}"
        );
        assert_eq!(with_node(|node| node.identity().alias), "ValidAlias");

        // A panic in a call on the node is answered too, and leaves the node
        // usable.
        let failed = with_node(|_| panic!("call panic"));
        assert_eq!(error_of(&failed), "libradicle panicked: call panic");
        assert!(NODE.lock().is_ok(), "NODE is not poisoned");
        assert_eq!(with_node(|node| node.identity().alias), "ValidAlias");

        assert_eq!(shutdown_json(None), r#"{"ok":true}"#);
        assert!(NODE.lock().unwrap().is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// R-18: `commits` never walks a page larger than the cap, whatever
    /// page size the host asks for (a `u32`).
    #[test]
    fn page_size_is_capped() {
        assert_eq!(page_size(0), 0);
        assert_eq!(page_size(30), 30);
        assert_eq!(page_size(u32::MAX), BINDING_MAX_PER_PAGE);
    }
}
