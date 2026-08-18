# libradicle

An **embeddable Radicle facade**: one Rust crate that runs a full
[Radicle](https://radicle.xyz) node in-process — profile, storage, and the
heartwood node runtime on a background thread — and exposes the small API a
host application actually needs. No `radicle-node` daemon, no
`radicle-httpd`, no localhost REST API, no child processes on the client
path.

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
let mut node = Embedded::start(Options {
    home: "/path/to/radicle-home".into(),
    alias: "my-app".into(),
    listen: vec![],            // outbound-only
})?;
node.connect_preferred_seeds(Duration::from_secs(15))?;
node.clone_repo(rid, Duration::from_secs(120))?;   // seed + fetch, bare storage only
let info  = node.repo_info(rid)?;                  // payload, head, COB counts
let tree  = node.tree(rid, "src")?;                // browse at head
let blob  = node.read_blob(rid, "README.md")?;
node.shutdown()?;
```

The napi binding (`napi/src/lib.rs`) exposes the same surface to Node.js /
Electron as JSON-string-returning async functions; `napi/smoke.mjs` is a
live-network end-to-end test.

## Building

```bash
cargo build --release -p libradicle-napi
# → target/release/liblibradicle_napi.{so,dylib}  (rename to libradicle.node)
```

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

- `no-spawn` — zero child processes: declines to serve inbound fetches and
  skips `git gc`. Required on iOS (which forbids spawning); leave **off**
  on desktop, where the node serves and publishes normally. Publishing
  from a `no-spawn` build is not possible until in-process upload-pack
  lands (follow-up work).

## License

MIT OR Apache-2.0
