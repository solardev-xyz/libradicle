//! Live-network smoke test for the libradicle facade.
//!
//! Usage: cargo run --example smoke -- <home-dir> <rid>
//!
//! Initializes a fresh profile under <home-dir>, starts an in-process
//! node, connects to the preferred seeds, clones <rid> into storage
//! (no working copy), and prints payload, COB counts, and the file list.

use std::time::Duration;

use libradicle::{Embedded, Options};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = std::env::args().skip(1);
    let home = args.next().expect("usage: smoke <home-dir> <rid>");
    let rid: radicle::identity::RepoId = args
        .next()
        .expect("usage: smoke <home-dir> <rid>")
        .parse()
        .expect("invalid RID");

    let node = Embedded::start(Options {
        home: home.into(),
        alias: "libradicle-smoke".into(),
        listen: vec![],
    })?;
    println!("node started, DID: {}", node.did());

    let n = node.connect_preferred_seeds(Duration::from_secs(15))?;
    println!("connected to {n} preferred seed(s)");

    println!("cloning {rid}…");
    node.clone_repo(rid, Duration::from_secs(120))?;
    println!("clone complete");

    let info = node.repo_info(rid)?;
    println!("name:            {}", info.name);
    println!("description:     {}", info.description);
    println!("default branch:  {}", info.default_branch);
    println!("head:            {}", info.head);
    println!("open issues:     {}", info.issues_open);
    println!("open patches:    {}", info.patches_open);

    let files = node.list_files(rid)?;
    println!("files at head:   {}", files.len());
    for f in files.iter().take(10) {
        println!("  {f}");
    }

    node.shutdown()?;
    println!("node shut down cleanly");
    Ok(())
}
