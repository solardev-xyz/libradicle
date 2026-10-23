//! `Embedded::blob_header` and `Embedded::read_blob_at_capped`: a file's
//! size comes from its object header, so a read can refuse a file that is
//! too large before loading any of it.

use std::path::{Path, PathBuf};

use libradicle::{BlobHeader, Embedded, Error, HostKey, Options};

const SMALL: &[u8] = b"hello\n";
const BIG: usize = 1000;

fn key() -> HostKey {
    let mut bytes = vec![9u8; 32];
    HostKey::from_bytes(&mut bytes).expect("32 bytes")
}

/// A scratch directory under TMPDIR, unless that is too deep for the node's
/// Unix socket path.
fn scratch(name: &str) -> PathBuf {
    let name = format!("{name}-{}", std::process::id());
    let mut dir = std::env::temp_dir().join(&name);
    if dir.join("home/node/control.sock").as_os_str().len() > 100 {
        dir = Path::new("/tmp").join(&name);
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Fixture {
    dir: PathBuf,
    node: Embedded,
    rid: radicle::identity::RepoId,
    revision: String,
    big: git2::Oid,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A node that dials nobody, with one imported repository: `small.txt`,
/// `big.txt` (1000 bytes), `dir/inner.txt` and a submodule `sub`, in one
/// commit on `main`.
fn fixture(name: &str) -> Fixture {
    let dir = scratch(name);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.json"),
        r#"{"preferredSeeds":[],"node":{"alias":"blobs","network":"test","connect":[],"peers":{"type":"static"}}}"#,
    )
    .unwrap();
    let work = dir.join("work");
    let repo = git2::Repository::init(&work).unwrap();
    std::fs::write(work.join("small.txt"), SMALL).unwrap();
    std::fs::write(work.join("big.txt"), vec![b'x'; BIG]).unwrap();
    std::fs::create_dir_all(work.join("dir")).unwrap();
    std::fs::write(work.join("dir/inner.txt"), b"inner\n").unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let files = repo.find_tree(index.write_tree().unwrap()).unwrap();
    // A submodule: a gitlink to a commit this repository doesn't have.
    let mut builder = repo.treebuilder(Some(&files)).unwrap();
    builder
        .insert("sub", git2::Oid::from_bytes(&[7; 20]).unwrap(), 0o160000)
        .unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Test", "test@example.com").unwrap();
    let commit = repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "files",
            &tree,
            &[],
        )
        .unwrap();
    repo.set_head("refs/heads/main").unwrap();
    let big = tree.get_name("big.txt").unwrap().id();

    let mut node = Embedded::start_with_key(
        Options {
            home: home.to_str().expect("utf-8 temp dir").into(),
            alias: "blobs".into(),
            listen: vec![],
            control_socket: true,
            routing: None,
        },
        key(),
    )
    .expect("start");
    let rid = node
        .import_repo(&work, "blobs", "", "main")
        .expect("import");
    Fixture {
        dir,
        node,
        rid,
        revision: commit.to_string(),
        big,
    }
}

#[test]
fn a_header_gives_the_oid_and_size_and_the_cap_is_inclusive() {
    let f = fixture("radblobs");
    let header = f
        .node
        .blob_header(f.rid, &f.revision, "small.txt")
        .expect("header");
    let oid = git2::Oid::hash_object(git2::ObjectType::Blob, SMALL).unwrap();
    assert_eq!(
        header,
        BlobHeader {
            oid: oid.to_string(),
            size: SMALL.len() as u64,
        }
    );
    let blob = f
        .node
        .read_blob_at_capped(f.rid, &f.revision, "small.txt", SMALL.len() as u64)
        .expect("at the cap");
    assert_eq!(blob.content, SMALL);
    assert!(!blob.binary);
    match f
        .node
        .read_blob_at_capped(f.rid, &f.revision, "small.txt", SMALL.len() as u64 - 1)
    {
        Err(Error::TooLarge { size, limit }) => {
            assert_eq!((size, limit), (SMALL.len() as u64, SMALL.len() as u64 - 1))
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
    // The uncapped read is as before.
    let uncapped = f
        .node
        .read_blob_at(f.rid, &f.revision, "small.txt")
        .expect("uncapped");
    assert_eq!(uncapped.content, SMALL);
}

#[test]
fn a_directory_or_a_submodule_is_not_a_blob() {
    let f = fixture("radblobdir");
    for not_a_file in ["dir", "sub"] {
        assert!(matches!(
            f.node.blob_header(f.rid, &f.revision, not_a_file),
            Err(Error::NotABlob(ref path)) if path == not_a_file
        ));
        assert!(matches!(
            f.node.read_blob_at_capped(f.rid, &f.revision, not_a_file, u64::MAX),
            Err(Error::NotABlob(ref path)) if path == not_a_file
        ));
    }
    assert_eq!(
        f.node
            .blob_header(f.rid, &f.revision, "dir/inner.txt")
            .expect("a file under it")
            .size,
        6
    );
    assert!(f
        .node
        .blob_header(f.rid, &f.revision, "missing.txt")
        .is_err());
}

#[test]
fn a_file_over_the_cap_is_refused_before_it_is_loaded() {
    let f = fixture("radblobload");
    // `big.txt`'s stored object keeps its header (a 1000-byte blob) but loses
    // its content: only a read that loads it can tell.
    let object = loose_object(&f.dir.join("home/storage"), f.big);
    truncate_loose_object(&object, BIG);

    let header = f
        .node
        .blob_header(f.rid, &f.revision, "big.txt")
        .expect("the header alone is intact");
    assert_eq!(header.size, BIG as u64);
    match f
        .node
        .read_blob_at_capped(f.rid, &f.revision, "big.txt", BIG as u64 - 1)
    {
        Err(Error::TooLarge { size, limit }) => {
            assert_eq!((size, limit), (BIG as u64, BIG as u64 - 1))
        }
        other => panic!("expected TooLarge without a load, got {other:?}"),
    }
    // Within the cap, the object is loaded, and the damage shows.
    let loaded = f
        .node
        .read_blob_at_capped(f.rid, &f.revision, "big.txt", BIG as u64);
    assert!(loaded.is_err(), "the damaged object loaded: {loaded:?}");
}

/// The loose object for `oid` under `storage`, in whichever repository has it.
fn loose_object(storage: &Path, oid: git2::Oid) -> PathBuf {
    let hex = oid.to_string();
    for repo in std::fs::read_dir(storage).unwrap() {
        let path = repo
            .unwrap()
            .path()
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        if path.is_file() {
            return path;
        }
    }
    panic!("no loose object {hex} under {storage:?}");
}

/// Rewrites a loose object as a blob header claiming `size` bytes, followed
/// by ten: a valid zlib stream of one stored block.
fn truncate_loose_object(path: &Path, size: usize) {
    let mut data = format!("blob {size}\0").into_bytes();
    data.extend_from_slice(b"xxxxxxxxxx");
    let len = data.len() as u16;
    let mut stream = vec![0x78, 0x01, 0x01];
    stream.extend_from_slice(&len.to_le_bytes());
    stream.extend_from_slice(&(!len).to_le_bytes());
    stream.extend_from_slice(&data);
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &data {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    stream.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    // Git leaves objects read-only, and Windows won't replace a read-only file.
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions).unwrap();
    std::fs::write(path, stream).unwrap();
}
