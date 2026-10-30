//! The JSON every host gets back from an [`Embedded`](crate::Embedded) call,
//! in one place: the Node add-on (`napi`), the UniFFI binding (`uniffi`) and
//! any other host write exactly these bytes.
//!
//! Each function writes one result into any [`io::Write`], so a host can
//! stream it, or bound what it writes; [`to_string`] collects one into a
//! `String`, as the bindings return them. The shapes are the add-on's, byte
//! for byte: the radicle-httpd-compatible ones for COBs, and flat objects for
//! the rest. An error is `{"error": "<message>"}` ([`error`]).

use std::fmt::Display;
use std::io;

use radicle::cob;
use radicle::identity::RepoId;
use serde::ser::{SerializeMap as _, Serializer};
use serde::Serialize;
use serde_json::{json, Value};

use crate::{
    Blob, CommitDetail, CommitInfo, Identity, Progress, RemoteInfo, RepoInfo, RepoStats,
    SeedConnectReport, SeededRepoInfo, TreeEntry,
};

/// What every writer here returns.
pub type Written = serde_json::Result<()>;

fn put(w: &mut impl io::Write, value: &Value) -> Written {
    serde_json::to_writer(w, value)
}

// The repository-controlled results (trees, COBs, remotes, repositories)
// are written field by field as they are serialized, never built as a
// `Value` first: a bounded read counts them through `meter::Count`, and a
// copy of the whole result would be the very allocation the budget is
// there to prevent (review of libradicle #15). The keys and their order
// are the `json!` shapes these replaced, byte for byte.

/// A value written through its `Display`, with no `String` built.
struct Shown<T>(T);

impl<T: Display> Serialize for Shown<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

/// An array of what the iterator `F` makes, each written as it comes.
struct Seq<F>(F);

impl<F, I> Serialize for Seq<F>
where
    F: Fn() -> I,
    I: Iterator,
    I::Item: Serialize,
{
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq((self.0)())
    }
}

/// An object of its entries, written in order.
macro_rules! object {
    ($s:expr, { $($key:literal : $value:expr),* $(,)? }) => {{
        let mut map = $s.serialize_map(None)?;
        $(map.serialize_entry($key, &$value)?;)*
        map.end()
    }};
}

/// One result, collected into a `String`. A write that fails is written as
/// an [`error`] instead.
pub fn to_string(write: impl FnOnce(&mut Vec<u8>) -> Written) -> String {
    let mut out = Vec::new();
    let failed = match write(&mut out) {
        Ok(()) => match String::from_utf8(out) {
            Ok(text) => return text,
            Err(e) => e.to_string(),
        },
        Err(e) => e.to_string(),
    };
    let mut out = Vec::new();
    match error(&mut out, failed) {
        Ok(()) => String::from_utf8(out).unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// `{"error": "<message>"}`.
pub fn error(w: &mut impl io::Write, e: impl Display) -> Written {
    put(w, &json!({ "error": e.to_string() }))
}

/// `{"error": "<e>", "stopping": true}`: a node asked to stop that hasn't
/// yet, or hasn't let go of its home, within the deadline. Another shutdown
/// waits again.
pub fn stopping(w: &mut impl io::Write, e: impl Display) -> Written {
    put(w, &json!({ "error": e.to_string(), "stopping": true }))
}

/// `{"ok": true}`.
pub fn ok(w: &mut impl io::Write) -> Written {
    w.write_all(br#"{"ok":true}"#)
        .map_err(serde_json::Error::io)
}

/// `{"cancelled": bool}`.
pub fn cancelled(w: &mut impl io::Write, cancelled: bool) -> Written {
    put(w, &json!({ "cancelled": cancelled }))
}

/// `{"did": "<did>"}`.
pub fn did(w: &mut impl io::Write, did: &str) -> Written {
    put(w, &json!({ "did": did }))
}

/// A seed bootstrap's report.
pub fn seed_report(w: &mut impl io::Write, report: &SeedConnectReport) -> Written {
    put(
        w,
        &json!({
            "attempted": report.attempted,
            "connected": report.connected,
            "target": report.target,
            "targetReached": report.target_reached,
            "elapsedMs": report.elapsed.as_millis(),
            "failures": report.failures.iter().map(|failure| json!({
                "nid": failure.nid,
                "addr": failure.addr,
                "reason": failure.reason,
            })).collect::<Vec<_>>(),
        }),
    )
}

/// A [`Progress`] event as a flat `{ "phase": ..., ... }` object.
pub fn progress(w: &mut impl io::Write, p: &Progress) -> Written {
    let mut v = match p {
        Progress::Resolving { candidates } => json!({ "candidates": candidates }),
        Progress::Connecting {
            nid,
            addr,
            index,
            total,
        } => {
            json!({ "nid": nid, "addr": addr, "index": index, "total": total })
        }
        Progress::Fetching { nid, index, total } => {
            json!({ "nid": nid, "index": index, "total": total })
        }
        Progress::PeerFailed {
            nid,
            index,
            total,
            reason,
        } => {
            json!({ "nid": nid, "index": index, "total": total, "reason": reason })
        }
        Progress::Failed { reason } => json!({ "reason": reason }),
        Progress::Done | Progress::Cancelled => json!({}),
    };
    v["phase"] = Value::from(p.phase());
    put(w, &v)
}

/// `{"unseeded": bool}`.
pub fn unseeded(w: &mut impl io::Write, unseeded: bool) -> Written {
    put(w, &json!({ "unseeded": unseeded }))
}

/// The public profile identity.
pub fn identity(w: &mut impl io::Write, identity: &Identity) -> Written {
    put(
        w,
        &json!({
            "did": identity.did,
            "nid": identity.nid,
            "alias": identity.alias,
        }),
    )
}

/// `{"rid","name","description"}`: a listed repository, or a seeded one.
struct RepoJson<'a> {
    rid: &'a RepoId,
    name: Option<&'a str>,
    description: Option<&'a str>,
}

impl Serialize for RepoJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        object!(s, {
            "rid": Shown(self.rid),
            "name": self.name,
            "description": self.description,
        })
    }
}

impl<'a> From<&'a RepoInfo> for RepoJson<'a> {
    fn from(repo: &'a RepoInfo) -> Self {
        Self {
            rid: &repo.rid,
            name: Some(&repo.name),
            description: Some(&repo.description),
        }
    }
}

impl<'a> From<&'a SeededRepoInfo> for RepoJson<'a> {
    fn from(repo: &'a SeededRepoInfo) -> Self {
        Self {
            rid: &repo.rid,
            name: repo.name.as_deref(),
            description: repo.description.as_deref(),
        }
    }
}

/// The repositories in local storage.
pub fn repos(w: &mut impl io::Write, repos: &[RepoInfo]) -> Written {
    serde_json::to_writer(w, &Seq(|| repos.iter().map(RepoJson::from)))
}

/// One repository, as an item of [`repos`].
pub fn repo(w: &mut impl io::Write, repo: &RepoInfo) -> Written {
    serde_json::to_writer(w, &RepoJson::from(repo))
}

/// The repositories with an explicit allow-seeding policy.
pub fn seeded_repos(w: &mut impl io::Write, repos: &[SeededRepoInfo]) -> Written {
    serde_json::to_writer(w, &Seq(|| repos.iter().map(RepoJson::from)))
}

/// One seeded repository, as an item of [`seeded_repos`].
pub fn seeded_repo(w: &mut impl io::Write, repo: &SeededRepoInfo) -> Written {
    serde_json::to_writer(w, &RepoJson::from(repo))
}

/// A thread comment: `{"id","author","body","timestamp","replyTo"}`.
struct CommentJson<'a, L>(&'a cob::EntryId, &'a cob::thread::Comment<L>);

impl<L> Serialize for CommentJson<'_, L> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let (id, comment) = (self.0, self.1);
        object!(s, {
            "id": Shown(id),
            "author": cob::common::Author::new(comment.author()),
            "body": comment.body(),
            "timestamp": comment.timestamp(),
            "replyTo": comment.reply_to().map(Shown),
        })
    }
}

struct IssueJson<'a>(&'a cob::ObjectId, &'a cob::issue::Issue);

impl Serialize for IssueJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let (id, issue) = (self.0, self.1);
        object!(s, {
            "id": Shown(id),
            "author": issue.author(),
            "title": issue.title(),
            "state": issue.state(),
            "labels": Seq(|| issue.labels().map(Shown)),
            "assignees": Seq(|| issue.assignees().map(Shown)),
            "discussion": Seq(|| issue.comments().map(|(id, c)| CommentJson(id, c))),
        })
    }
}

struct ReviewJson<'a>(&'a cob::patch::Review);

impl Serialize for ReviewJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let review = self.0;
        object!(s, {
            "id": Shown(review.id()),
            "author": review.author(),
            "verdict": review.verdict().map(Shown),
            "summary": review.summary(),
            "timestamp": review.timestamp(),
        })
    }
}

struct RevisionJson<'a>(cob::patch::RevisionId, &'a cob::patch::Revision);

impl Serialize for RevisionJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let (id, revision) = (self.0, self.1);
        object!(s, {
            "id": Shown(id),
            "author": revision.author(),
            "description": revision.description(),
            "base": Shown(revision.base()),
            "oid": Shown(revision.head()),
            "timestamp": revision.timestamp(),
            "discussions": Seq(|| revision.discussion().comments().map(|(id, c)| CommentJson(id, c))),
            "reviews": Seq(|| revision.reviews().map(|(_, review)| ReviewJson(review))),
        })
    }
}

struct MergeJson<'a>(&'a cob::ActorId, &'a cob::patch::Merge);

impl Serialize for MergeJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let (author, merge) = (self.0, self.1);
        object!(s, {
            "author": cob::common::Author::new(author),
            "revision": Shown(merge.revision),
            "commit": Shown(merge.commit),
            "timestamp": merge.timestamp,
        })
    }
}

struct PatchJson<'a>(&'a cob::ObjectId, &'a cob::patch::Patch);

impl Serialize for PatchJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let (id, patch) = (self.0, self.1);
        object!(s, {
            "id": Shown(id),
            "author": patch.author(),
            "title": patch.title(),
            "state": patch.state(),
            "target": patch.target(),
            "labels": Seq(|| patch.labels().map(Shown)),
            "assignees": Seq(|| patch.assignees().map(Shown)),
            "revisions": Seq(|| patch.revisions().map(|(id, revision)| RevisionJson(id, revision))),
            "merges": Seq(|| patch.merges().map(|(author, merge)| MergeJson(author, merge))),
        })
    }
}

/// All issues of a repository, radicle-httpd-shaped.
pub fn issues(w: &mut impl io::Write, issues: &[(cob::ObjectId, cob::issue::Issue)]) -> Written {
    serde_json::to_writer(
        w,
        &Seq(|| issues.iter().map(|(id, issue)| IssueJson(id, issue))),
    )
}

/// One issue, radicle-httpd-shaped.
pub fn issue(w: &mut impl io::Write, id: &cob::ObjectId, issue: &cob::issue::Issue) -> Written {
    serde_json::to_writer(w, &IssueJson(id, issue))
}

/// All patches of a repository, radicle-httpd-shaped.
pub fn patches(w: &mut impl io::Write, patches: &[(cob::ObjectId, cob::patch::Patch)]) -> Written {
    serde_json::to_writer(
        w,
        &Seq(|| patches.iter().map(|(id, patch)| PatchJson(id, patch))),
    )
}

/// One patch, radicle-httpd-shaped.
pub fn patch(w: &mut impl io::Write, id: &cob::ObjectId, patch: &cob::patch::Patch) -> Written {
    serde_json::to_writer(w, &PatchJson(id, patch))
}

/// `{"id": <id>}`: what a COB write made.
pub fn id(w: &mut impl io::Write, id: impl Serialize) -> Written {
    put(w, &json!({ "id": id }))
}

/// `{"id": <id>, "state": "<state>"}`: an issue's new state.
pub fn id_state(w: &mut impl io::Write, id: impl Serialize, state: &str) -> Written {
    put(w, &json!({ "id": id, "state": state }))
}

/// `{"rid": "<rid>"}`: an imported repository.
pub fn rid(w: &mut impl io::Write, rid: &RepoId) -> Written {
    put(w, &json!({ "rid": rid.to_string() }))
}

/// A repository's identity payload, head and COB counts.
pub fn repo_info(w: &mut impl io::Write, info: &RepoInfo) -> Written {
    put(
        w,
        &json!({
            "rid": info.rid.to_string(),
            "name": info.name,
            "description": info.description,
            "defaultBranch": info.default_branch,
            "head": info.head,
            "delegates": info.delegates,
            "threshold": info.threshold,
            "visibility": info.visibility,
            "issuesOpen": info.issues_open,
            "patchesOpen": info.patches_open,
        }),
    )
}

/// A page of commit history, each commit as radicle-surf serializes it.
pub fn commits(w: &mut impl io::Write, commits: &[radicle_surf::Commit]) -> Written {
    serde_json::to_writer(w, commits)
}

/// A commit's metadata and its diff against its first parent.
pub fn commit(w: &mut impl io::Write, detail: &CommitDetail) -> Written {
    put(
        w,
        &json!({
            "commit": detail.commit,
            "diff": detail.diff,
        }),
    )
}

/// File paths, as an array.
pub fn files(w: &mut impl io::Write, files: &[String]) -> Written {
    serde_json::to_writer(w, files)
}

struct EntryJson<'a>(&'a TreeEntry);

impl Serialize for EntryJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let e = self.0;
        object!(s, {
            "name": e.name,
            "path": e.path,
            "kind": e.kind,
        })
    }
}

/// One tree entry, as an item of [`tree`]'s `entries`.
pub fn tree_entry(w: &mut impl io::Write, entry: &TreeEntry) -> Written {
    serde_json::to_writer(w, &EntryJson(entry))
}

struct SignatureJson<'a>(&'a crate::CommitSignature);

impl Serialize for SignatureJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let signature = self.0;
        object!(s, {
            "name": signature.name,
            "email": signature.email,
            "time": signature.time,
        })
    }
}

struct LastCommitJson<'a>(&'a CommitInfo);

impl Serialize for LastCommitJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let commit = self.0;
        object!(s, {
            "id": commit.id,
            "summary": commit.summary,
            "description": commit.description,
            "author": SignatureJson(&commit.author),
            "committer": SignatureJson(&commit.committer),
        })
    }
}

struct TreeJson<'a>(&'a [TreeEntry], Option<&'a CommitInfo>);

impl Serialize for TreeJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let entries = Seq(|| self.0.iter().map(EntryJson));
        match self.1 {
            None => object!(s, { "entries": entries }),
            Some(commit) => object!(s, {
                "entries": entries,
                "lastCommit": LastCommitJson(commit),
            }),
        }
    }
}

/// A tree's entries, httpd-shaped: `{"entries":[{"name","path","kind"}]}`.
pub fn tree(w: &mut impl io::Write, tree: &[TreeEntry]) -> Written {
    serde_json::to_writer(w, &TreeJson(tree, None))
}

/// A tree's entries at a revision, with that revision's commit metadata.
pub fn tree_at(w: &mut impl io::Write, tree: &[TreeEntry], commit: &CommitInfo) -> Written {
    serde_json::to_writer(w, &TreeJson(tree, Some(commit)))
}

/// A blob read at `path`, httpd-shaped: `{"binary":bool,"content":"...",
/// "name":"..."}`, named for the path's last component. A binary blob has no
/// content (the viewer renders a placeholder for it).
pub fn blob(w: &mut impl io::Write, path: &str, blob: &Blob) -> Written {
    let name = path.rsplit('/').next().unwrap_or(path);
    if blob.binary {
        put(
            w,
            &json!({
                "binary": true,
                "name": name,
            }),
        )
    } else {
        put(
            w,
            &json!({
                "binary": false,
                "name": name,
                "content": String::from_utf8_lossy(&blob.content),
            }),
        )
    }
}

struct RemoteJson<'a>(&'a RemoteInfo);

impl Serialize for RemoteJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let remote = self.0;
        object!(s, {
            "id": remote.id,
            "did": remote.did,
            "delegate": remote.delegate,
            "heads": remote.heads,
        })
    }
}

/// Signed remotes and their branch heads.
pub fn remotes(w: &mut impl io::Write, remotes: &[RemoteInfo]) -> Written {
    serde_json::to_writer(w, &Seq(|| remotes.iter().map(RemoteJson)))
}

/// One remote, as an item of [`remotes`].
pub fn remote(w: &mut impl io::Write, remote: &RemoteInfo) -> Written {
    serde_json::to_writer(w, &RemoteJson(remote))
}

/// A repository's history statistics.
pub fn repo_stats(w: &mut impl io::Write, stats: &RepoStats) -> Written {
    put(
        w,
        &json!({
            "commits": stats.commits,
            "branches": stats.branches,
            "contributors": stats.contributors,
        }),
    )
}

/// `{"connectedPeers": n}`.
pub fn connected_peers(w: &mut impl io::Write, peers: usize) -> Written {
    put(w, &json!({ "connectedPeers": peers }))
}

/// `{"seeding": n}`: a repository's known seeders.
pub fn seeding(w: &mut impl io::Write, seeders: usize) -> Written {
    put(w, &json!({ "seeding": seeders }))
}

#[cfg(test)]
mod tests;
