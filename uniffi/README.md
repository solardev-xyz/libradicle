# libradicle-uniffi

The **iOS / Swift** binding over [`libradicle`](../), the mirror of the
desktop [`napi`](../napi) addon. Every function returns a JSON string with the
same shapes the Freedom `window.radicle` provider already parses, so the iOS
webview consumes the same payloads as desktop.

It follows the Myotis pattern: plain `#[uniffi::export]` free functions,
`uniffi::setup_scaffolding!()` at the crate root, UniFFI 0.32. Calls are
**synchronous** (UniFFI has no thread pool) — the Swift side dispatches them
off the main thread. Clone progress arrives through the `ProgressListener`
callback interface; cancel an in-flight clone with `cancelClone(rid)`.

## The iOS story

With the `no-spawn` feature the embedded node serves fetches **in-process**
(libgit2 + a hand-written non-delta packfile, no `git upload-pack`) and skips
`git gc`. That is what lets a phone **publish**: peers fetch its refs without
it ever spawning a child process. Enable `no-spawn` for device builds.

`importRepo` stays desktop-only (heartwood shells out to `git` to push a
working copy); a phone comments and files issues, which write COBs directly
into storage in-process.

## Build + generate the Swift bindings

```bash
# 1. Build the library (host, for the bindgen scan; add --features no-spawn
#    and an --target for device/simulator archives).
cargo build -p libradicle-uniffi

# 2. Generate the Swift glue from the built library.
cargo run -p uniffi-bindgen -- generate \
  --library target/debug/liblibradicle_uniffi.{so,dylib} \
  --language swift --out-dir generated/

# Produces:
#   libradicle_uniffi.swift          — the Swift API
#   libradicle_uniffiFFI.h           — C header
#   libradicle_uniffiFFI.modulemap   — module map
```

## Package for the app (`RadicleKit`, like `MyotisKit`)

Cross-compile the staticlib for each Apple target, bundle them into an
`.xcframework`, and wrap the generated Swift + module map in a local Swift
package the app depends on:

```bash
for target in aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios; do
  cargo build -p libradicle-uniffi --features no-spawn --release --target "$target"
done
xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/liblibradicle_uniffi.a \
    -headers generated/ \
  -library target/aarch64-apple-ios-sim/release/liblibradicle_uniffi.a \
    -headers generated/ \
  -output RadicleKit/libradicle_uniffi.xcframework
```

The version generating the bindings and the version linked into the app must
match — a skew is a checksum failure at load, by design.
