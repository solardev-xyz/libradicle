//! `process_init`: out of memory in libgit2 or SQLite ends the process with
//! exit 71, and is never returned as an error (freedom-core design 0010, "The
//! memory cap"). Without it, the same failures come back as errors that
//! `Error::is_out_of_memory` counts.
//!
//! Each case runs in a child process: this binary, run again with only its
//! `child` test and the case in `LIBRADICLE_OOM_CASE`. A case installs the
//! hooks or lowers `RLIMIT_DATA`, which are the child's own; this test runner
//! changes neither. The child test runs before anything in the child uses
//! libgit2 or SQLite, on the one thread that does, as `process_init`'s
//! contract asks.
//!
//! On Linux, a case lowers the child's `RLIMIT_DATA` to just above what it
//! uses, as the memory cap does, and then asks for more: the allocation the
//! system allocator refuses is a modest one. Elsewhere, a libgit2 case asks
//! for a size no system allocator can give; SQLite refuses such a size itself,
//! before its allocator, so its cases need the limit.

use std::process::{Command, Output};

use git2::{ObjectType, Odb};

/// The child's case. Without it, `child` passes and does nothing.
const CASE: &str = "LIBRADICLE_OOM_CASE";

/// What a child prints when the failure it forced came back to it.
const RETURNED: &str = "returned to the caller: ";

/// What a child prints when its case ran to the end.
const DONE: &str = "case done";

/// A size no system allocator gives.
const HUGE: usize = usize::MAX / 4;

/// Above the lowered `RLIMIT_DATA`'s headroom ([`lower_data_limit`]).
#[cfg(target_os = "linux")]
const OVER_THE_LIMIT: usize = 512 << 20;

#[test]
fn libgit2_allocation_failure_exits_71() {
    assert_exits_71("libgit2-huge", "libgit2");
}

#[cfg(target_os = "linux")]
#[test]
fn libgit2_allocation_failure_under_the_data_limit_exits_71() {
    assert_exits_71("libgit2-limit", "libgit2");
}

#[cfg(target_os = "linux")]
#[test]
fn sqlite_allocation_failure_under_the_data_limit_exits_71() {
    assert_exits_71("sqlite-limit", "SQLite");
}

/// The hooks are what turn these failures into an exit: without them, both
/// libraries return them, and `is_out_of_memory` counts them.
#[cfg(target_os = "linux")]
#[test]
fn without_the_hooks_the_failures_are_errors_the_backstop_counts() {
    assert_done("libgit2-limit-unhooked");
    assert_done("sqlite-limit-unhooked");
}

/// A second call returns the first one's result, and the hooks stay.
#[test]
fn a_second_call_returns_the_first_ones_result() {
    assert_exits_71("twice", "libgit2");
}

/// After SQLite is initialized, `process_init` returns its error and
/// installs nothing, libgit2's hook included.
#[test]
fn after_sqlite_is_initialized_it_changes_nothing() {
    assert_done("after-sqlite");
}

/// libgit2's global initialization allocates (its system directories, its
/// filters): with the hook set before it, a failure there exits 71 too, where
/// libgit2-sys would panic. The child has no memory left to give before its
/// first `process_init`.
#[cfg(target_os = "linux")]
#[test]
fn libgit2_initialization_failure_exits_71() {
    assert_exits_71("libgit2-init-exhausted", "libgit2");
}

/// libgit2's `grealloc`, of a buffer it holds (`git_buf_grow` after
/// `git_buf_set`) and of NULL (`git_buf_grow` on an empty buffer).
#[test]
fn libgit2_reallocation_failure_exits_71() {
    assert_exits_71("libgit2-realloc", "libgit2");
    assert_exits_71("libgit2-realloc-null", "libgit2");
}

/// SQLite's `xRealloc`, of a block it holds; and a reallocation of NULL,
/// which SQLite answers with an allocation (`sqlite3Realloc` calls
/// `sqlite3Malloc` for it), so that `xRealloc` never sees NULL.
#[cfg(target_os = "linux")]
#[test]
fn sqlite_reallocation_failure_under_the_data_limit_exits_71() {
    assert_exits_71("sqlite-realloc-limit", "SQLite");
    assert_exits_71("sqlite-realloc-null-limit", "SQLite");
}

#[test]
fn child() {
    let Ok(case) = std::env::var(CASE) else {
        return;
    };
    match case.as_str() {
        "libgit2-huge" => {
            init();
            let odb = mempack();
            returned(odb.writer(HUGE, ObjectType::Blob).map(drop));
        }
        #[cfg(target_os = "linux")]
        "libgit2-limit" => {
            init();
            let odb = mempack();
            lower_data_limit(HEADROOM);
            returned(odb.writer(OVER_THE_LIMIT, ObjectType::Blob).map(drop));
        }
        #[cfg(target_os = "linux")]
        "sqlite-limit" => {
            init();
            let db = sqlite::open(":memory:").unwrap();
            lower_data_limit(HEADROOM);
            returned(db.execute(randomblob(OVER_THE_LIMIT)));
        }
        #[cfg(target_os = "linux")]
        "libgit2-limit-unhooked" => {
            let odb = mempack();
            lower_data_limit(HEADROOM);
            let error = odb.writer(OVER_THE_LIMIT, ObjectType::Blob).err().unwrap();
            assert_eq!(error.class(), git2::ErrorClass::NoMemory, "{error}");
            assert!(libradicle::Error::Git(error).is_out_of_memory());
            println!("{DONE}");
        }
        #[cfg(target_os = "linux")]
        "sqlite-limit-unhooked" => {
            let db = sqlite::open(":memory:").unwrap();
            lower_data_limit(HEADROOM);
            let error = db.execute(randomblob(OVER_THE_LIMIT)).unwrap_err();
            assert_eq!(error.code, Some(7), "SQLITE_NOMEM: {error}");
            assert!(libradicle::Error::Sqlite(error).is_out_of_memory());
            println!("{DONE}");
        }
        #[cfg(target_os = "linux")]
        "libgit2-init-exhausted" => {
            lower_data_limit(0);
            exhaust_the_allocator();
            // SAFETY: first in the child, on the one thread that uses libgit2
            // or SQLite, before either is used.
            returned(unsafe { libradicle::process_init() }.map(drop));
        }
        "libgit2-realloc" => {
            init();
            let mut buf = empty_buf();
            // SAFETY: `buf` is a valid, empty `git_buf`, and the data is one
            // byte long.
            let set = unsafe { libgit2_sys::git_buf_set(&mut buf, b"x".as_ptr().cast(), 1) };
            assert_eq!(set, 0, "git_buf_set");
            // SAFETY: `buf` holds libgit2's allocation; growing it reallocates.
            returned(rc(unsafe { libgit2_sys::git_buf_grow(&mut buf, HUGE) }));
        }
        "libgit2-realloc-null" => {
            init();
            let mut buf = empty_buf();
            // SAFETY: `buf` is a valid, empty `git_buf`, whose NULL pointer
            // libgit2 reallocates (`git_buf_grow`, `src/libgit2/buf.c`).
            returned(rc(unsafe { libgit2_sys::git_buf_grow(&mut buf, HUGE) }));
        }
        #[cfg(target_os = "linux")]
        "sqlite-realloc-limit" => {
            init();
            // SAFETY: SQLite's allocator; the block is reallocated below.
            let block = unsafe { sqlite::ffi::sqlite3_malloc64(64) };
            assert!(!block.is_null());
            lower_data_limit(HEADROOM);
            // SAFETY: `block` is SQLite's, from `sqlite3_malloc64`.
            let grown = unsafe { sqlite::ffi::sqlite3_realloc64(block, OVER_THE_LIMIT as u64) };
            returned(if grown.is_null() { Err("NULL") } else { Ok(()) });
        }
        #[cfg(target_os = "linux")]
        "sqlite-realloc-null-limit" => {
            init();
            lower_data_limit(HEADROOM);
            // SAFETY: a reallocation of NULL is an allocation.
            let grown = unsafe {
                sqlite::ffi::sqlite3_realloc64(std::ptr::null_mut(), OVER_THE_LIMIT as u64)
            };
            returned(if grown.is_null() { Err("NULL") } else { Ok(()) });
        }
        "twice" => {
            init();
            init();
            // Both libraries work as before.
            let db = sqlite::open(":memory:").unwrap();
            db.execute("CREATE TABLE t (x BLOB); INSERT INTO t VALUES (randomblob(4096))")
                .unwrap();
            let odb = mempack();
            odb.write(ObjectType::Blob, &[7; 4096]).unwrap();
            returned(odb.writer(HUGE, ObjectType::Blob).map(drop));
        }
        "after-sqlite" => {
            let db = sqlite::open(":memory:").unwrap();
            db.execute("SELECT 1").unwrap();
            // SAFETY: the one thread that uses libgit2 or SQLite in this
            // child. SQLite is initialized: the call must refuse.
            match unsafe { libradicle::process_init() } {
                Err(libradicle::Error::ProcessInit(why)) => {
                    assert!(why.contains("SQLITE_MISUSE"), "{why}");
                }
                other => panic!("process_init after SQLite: {other:?}"),
            }
            // libgit2's hook wasn't installed either: its failure is an
            // error again.
            let error = mempack().writer(HUGE, ObjectType::Blob).err().unwrap();
            assert!(libradicle::Error::Git(error).is_out_of_memory());
            println!("{DONE}");
        }
        other => panic!("unknown case {other:?}"),
    }
}

fn init() {
    // SAFETY: first in the child, on the one thread that uses libgit2 or
    // SQLite, before either is used.
    unsafe { libradicle::process_init() }.expect("process_init");
}

/// An object database with an in-memory backend only, which has no write
/// stream of its own: libgit2 makes one with a buffer of the whole object's
/// size, from its allocator (`init_fake_wstream`, `src/libgit2/odb.c`).
fn mempack() -> Odb<'static> {
    let odb = Odb::new().unwrap();
    odb.add_new_mempack_backend(1).unwrap();
    odb
}

/// A statement whose evaluation asks SQLite's allocator for `bytes`, under
/// its default length limit of 10^9.
#[cfg(target_os = "linux")]
fn randomblob(bytes: usize) -> String {
    format!("SELECT randomblob({bytes})")
}

/// The failure came back, where the hooks should have ended the process.
fn returned<E: std::fmt::Debug>(result: Result<(), E>) {
    println!("{RETURNED}{result:?}");
}

/// An empty `git_buf` (`GIT_BUF_INIT`).
fn empty_buf() -> libgit2_sys::git_buf {
    libgit2_sys::git_buf {
        ptr: std::ptr::null_mut(),
        reserved: 0,
        size: 0,
    }
}

/// A libgit2 return code as a result.
fn rc(code: std::ffi::c_int) -> Result<(), std::ffi::c_int> {
    if code < 0 {
        Err(code)
    } else {
        Ok(())
    }
}

/// Takes every block the C library's allocator has left under the lowered
/// limit, so that its next allocation of any size fails. The blocks are never
/// freed: the child ends with its case. Nothing in it may allocate after
/// this but the code under test.
#[cfg(target_os = "linux")]
fn exhaust_the_allocator() {
    for size in [1 << 20, 1 << 16, 1 << 12, 1 << 8, 1] {
        // SAFETY: `malloc` takes any size; its blocks are leaked on purpose.
        // `black_box` keeps the compiler from assuming the allocation and
        // dropping it.
        while !std::hint::black_box(unsafe { libc::malloc(size) }).is_null() {}
    }
}

/// The headroom a case leaves itself under the lowered limit
/// ([`lower_data_limit`]).
#[cfg(target_os = "linux")]
const HEADROOM: u64 = 64 << 20;

/// Lowers this child's `RLIMIT_DATA`, soft and hard, to `headroom` above the
/// private writable memory it maps now (`VmData`, which the limit counts).
#[cfg(target_os = "linux")]
fn lower_data_limit(headroom: u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let kib: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix("VmData:"))
        .and_then(|value| value.trim().strip_suffix("kB"))
        .and_then(|kib| kib.trim().parse().ok())
        .expect("VmData in /proc/self/status");
    let cap = kib * 1024 + headroom;
    let limit = libc::rlimit {
        rlim_cur: cap,
        rlim_max: cap,
    };
    // SAFETY: `setrlimit` reads `limit`, a valid `rlimit`. It lowers this
    // child's own limit, which no unprivileged process can raise again.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_DATA, &limit) }, 0);
}

fn run(case: &str) -> Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["child", "--exact", "--nocapture", "--test-threads=1"])
        .env(CASE, case)
        .output()
        .unwrap()
}

fn describe(case: &str, out: &Output) -> String {
    format!(
        "case {case}: {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn assert_exits_71(case: &str, library: &str) {
    let out = run(case);
    let what = describe(case, &out);
    assert_eq!(
        out.status.code(),
        Some(libradicle::OUT_OF_MEMORY_EXIT),
        "{what}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains(RETURNED),
        "{what}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains(&format!("libradicle: {library} is out of memory: exit 71")),
        "{what}"
    );
}

fn assert_done(case: &str) {
    let out = run(case);
    let what = describe(case, &out);
    assert!(out.status.success(), "{what}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(DONE),
        "{what}"
    );
}
