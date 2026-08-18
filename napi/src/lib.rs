//! Node.js binding over `libradicle`, in the style of Freedom Browser's
//! myotis-node addon: every export returns a JSON string, blocking work
//! runs on the libuv thread pool via `AsyncTask`.
//!
//! One embedded node per process, held in a global slot. The Freedom main
//! process owns exactly one Radicle profile at a time, so a singleton is
//! the honest shape — a second `start` without `shutdown` is an error.

use std::sync::Mutex;
use std::time::Duration;

use napi::bindgen_prelude::AsyncTask;
use napi::{Env, Result, Task};
use napi_derive::napi;

use libradicle::{Embedded, Network, Options};

static NODE: Mutex<Option<Embedded>> = Mutex::new(None);

fn err_json(e: impl std::fmt::Display) -> String {
    serde_json::json!({ "error": e.to_string() }).to_string()
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
        }) {
            Ok(node) => {
                let did = node.did();
                *guard = Some(node);
                serde_json::json!({ "did": did }).to_string()
            }
            Err(e) => err_json(e),
        }
    })
}

/// Connect to the profile's preferred seeds. Resolves to `{"connected": n}`.
#[napi]
pub fn connect_seeds(timeout_ms: u32) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let mut network = match network() {
            Ok(network) => network,
            Err(e) => return err_json(e),
        };
        match network.connect_preferred_seeds(Duration::from_millis(timeout_ms.into())) {
            Ok(n) => serde_json::json!({ "connected": n }).to_string(),
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
            Ok(()) => r#"{"ok":true}"#.to_string(),
            Err(e) => err_json(e),
        }
    })
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
            Ok(unseeded) => serde_json::json!({ "unseeded": unseeded }).to_string(),
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
            serde_json::json!({
                "did": identity.did,
                "nid": identity.nid,
                "alias": identity.alias,
            })
            .to_string()
        })
    })
}

/// Repositories the local node is currently seeding.
#[napi]
pub fn list_repos() -> AsyncTask<BlockingJson> {
    blocking(|| {
        with_node(|node| match node.list_repos() {
            Ok(repos) => serde_json::json!(repos
                .into_iter()
                .map(|repo| serde_json::json!({
                    "rid": repo.rid.to_string(),
                    "name": repo.name,
                    "description": repo.description,
                }))
                .collect::<Vec<_>>())
            .to_string(),
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
                Ok(id) => serde_json::json!({ "id": id }).to_string(),
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
                Ok(id) => serde_json::json!({ "id": id }).to_string(),
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
            Ok(id) => serde_json::json!({ "id": id, "state": state }).to_string(),
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
            Ok(id) => serde_json::json!({ "id": id }).to_string(),
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
                Ok(rid) => serde_json::json!({ "rid": rid.to_string() }).to_string(),
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
            Ok(info) => serde_json::json!({
                "rid": info.rid.to_string(),
                "name": info.name,
                "description": info.description,
                "defaultBranch": info.default_branch,
                "head": info.head,
                "issuesOpen": info.issues_open,
                "patchesOpen": info.patches_open,
            })
            .to_string(),
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
            Ok(files) => serde_json::json!(files).to_string(),
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
            Ok(entries) => serde_json::json!({
                "entries": entries.iter().map(|e| serde_json::json!({
                    "name": e.name,
                    "path": e.path,
                    "kind": e.kind,
                })).collect::<Vec<_>>(),
            })
            .to_string(),
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
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        with_node(|node| match node.read_blob(rid, &path) {
            Ok(b) if b.binary => serde_json::json!({
                "binary": true,
                "name": name,
            })
            .to_string(),
            Ok(b) => serde_json::json!({
                "binary": false,
                "name": name,
                "content": String::from_utf8_lossy(&b.content),
            })
            .to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Node status: `{"connectedPeers": n}`.
#[napi]
pub fn status() -> AsyncTask<BlockingJson> {
    blocking(|| {
        with_node(|node| match node.connected_peers() {
            Ok(n) => serde_json::json!({ "connectedPeers": n }).to_string(),
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
            Ok(n) => serde_json::json!({ "seeding": n }).to_string(),
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
                Ok(()) => r#"{"ok":true}"#.to_string(),
                Err(e) => err_json(e),
            },
            None => err_json("node not started"),
        }
    })
}
