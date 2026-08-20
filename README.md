# libradicle

An **embeddable Radicle facade**: one Rust crate that runs a full
[Radicle](https://radicle.xyz) node in-process — profile, storage, and the
heartwood node runtime on a background thread — and exposes the small API a
host application actually needs. No `radicle-node` daemon, no
`radicle-httpd`, no localhost REST API, and no Radicle CLI on the client path.

Built as the shared core for [Freedom Browser](https://github.com/solardev-xyz/freedom-browser):
the desktop app embeds it via the napi-rs binding in [`napi/`](napi/), and
the iOS app will embed the same crate via UniFFI.

## Where this repo lives

| Channel | Address |
|---|---|
| Radicle (canonical) | `rad:z2SzCC9zYnP17QRPZUhrP2RTEwZHj` |
| GitHub (mirror + releases) | `solardev-xyz/libradicle` |

## API sketch

```rust
let node = Embedded::start(Options {
    home: "/path/to/radicle-home".into(),
    alias: "my-app".into(),
    listen: vec![],            // outbound-only
})?;
let bootstrap = node.connect_seed_book(Duration::from_secs(15))?;
assert!(bootstrap.target_reached); // four peers ready; dials run concurrently
node.clone_repo(rid, Duration::from_secs(120))?;   // seed + fetch, bare storage only

// …or stream progress and support cancellation:
let cancel = CancelToken::new();                   // cancel.cancel() from another thread
node.clone_repo_with(rid, &FetchPolicy::default(), &cancel, &mut |p| {
    eprintln!("{}: {:?}", p.phase(), p);           // resolving/connecting/fetching/…
})?;

let repos = node.list_repos()?;                    // currently seeded repositories
let info  = node.repo_info(rid)?;                  // payload, head, COB counts
let tree  = node.tree(rid, "src")?;                // browse at head
let blob  = node.read_blob(rid, "README.md")?;
let old   = node.tree_at(rid, &info.head, "src")?; // browse a pinned commit
let peers = node.remotes(rid)?;                    // signed remote branch heads
let stats = node.repo_stats(rid, &info.head)?;      // commits/branches/contributors
let issue = node.create_issue(rid, "Title", "Body", vec![])?;
node.shutdown()?;
```

COB mutations announce the updated refs before returning. The napi binding
also runs connect and fetch operations through a cloned network handle, so
long network requests do not block repository reads or status calls.

The napi binding (`napi/src/lib.rs`) exposes node lifecycle, identity,
repository import/list/seed/unseed, storage reads, and issue/patch mutations
to Node.js / Electron as JSON-string-returning async functions;
`napi/smoke.mjs` is a live-network end-to-end test.

Cold starts merge explicit user seeds with a shipped, independents-first
14-node DNS seed book. Six dials run concurrently with a five-second per-seed
timeout and stop pulling new candidates when four connections stand. Successful
outcomes are retained by Heartwood's local `node.db`, so later starts promote
the seeds that work from that device's network position. Setting
`preferredSeeds` to an empty list remains an explicit opt-out for isolated
profiles.

`cloneRepoWithProgress(rid, timeoutMs, cb)` pushes one JSON progress event
per phase to a `(event: string) => void` callback (via a napi
ThreadsafeFunction) and can be cancelled mid-flight with `cancelClone(rid)`;
`napi/progress-smoke.mjs` exercises both. Event phases: `resolving`,
`connecting`, `fetching`, `peer-failed`, `done`, `failed`, `cancelled`.

`treeAt`, `blobAt`, `remotes`, and `repoStats` provide the revision-pinned
repository-browser surface. Run `node napi/repository-smoke.mjs` after a
release build to verify historical reads and signed remote/stat shapes against
a temporary two-commit repository.

## Building

The workspace pins Rust 1.94 because the Heartwood revision used by the
embedded runtime does not compile with older Rust releases.

```bash
cargo build --release -p libradicle-napi
# → target/release/liblibradicle_napi.{so,dylib}  (rename to libradicle.node)

# Cross-compile the Windows ARM64 addon from a Windows MSVC host:
rustup target add aarch64-pc-windows-msvc
cargo build --release -p libradicle-napi --features no-spawn \
  --target aarch64-pc-windows-msvc
# → target/aarch64-pc-windows-msvc/release/libradicle_napi.dll
```

Tagged releases publish napi addons for macOS, Linux, and Windows on both x64
and ARM64. The Windows assets are named `libradicle-win-x64.node` and
`libradicle-win-arm64.node`.

Depends on the `freedom/embed` branch of
[`solardev-xyz/heartwood`](https://github.com/solardev-xyz/heartwood) — a
small patch-set over upstream heartwood (`Profile::load_from`, `no-serve`,
`no-gc`) intended for upstreaming. Cargo fetches it automatically. To hack
on heartwood locally, add to `.cargo/config.toml`:

```toml
[patch."https://github.com/solardev-xyz/heartwood"]
radicle         = { path = "../heartwood/crates/radicle" }
radicle-node    = { path = "../heartwood/crates/radicle-node" }
radicle-signals = { path = "../heartwood/crates/radicle-signals" }
```

## Features

- `no-spawn` — the node runtime starts no child processes: it serves inbound
  fetches in-process and skips `git gc`. Required on iOS and used by Freedom's
  self-contained desktop release addons.
- `import_repo` is desktop-only: heartwood currently invokes the system
  `git` executable to push a working copy into storage. Omit this method from
  iOS bindings until that push is implemented in-process.

## License

MIT OR Apache-2.0
