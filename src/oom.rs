//! Out of memory in libgit2 and SQLite ends the process (freedom-core design
//! 0010, "The memory cap"; 0008's question 10).
//!
//! Under the service's memory cap an allocation fails, and nothing ends the
//! process by itself: Rust's allocator aborts, but libgit2 and SQLite would
//! answer the failure as their caller's error. [`crate::process_init`]
//! installs a hook in each that ends the process with exit 71 instead
//! ([`install`]), and [`crate::Error::is_out_of_memory`] is the backstop for
//! the out-of-memory errors neither hook sees ([`in_chain`]).

use std::error::Error as StdError;
use std::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

use sqlite::ffi as sqlite_ffi;

/// The exit code of a process that [`crate::process_init`] set up when
/// libgit2 or SQLite can't get memory: `out_of_memory` (freedom-core design
/// 0008, *Errors*; `fc_service::Exit::OutOfMemory`).
pub const OUT_OF_MEMORY_EXIT: i32 = 71;

const LIBGIT2_OUT_OF_MEMORY: &[u8] =
    b"libradicle: libgit2 is out of memory: exit 71 (out_of_memory)\n";
const SQLITE_OUT_OF_MEMORY: &[u8] =
    b"libradicle: SQLite is out of memory: exit 71 (out_of_memory)\n";

/// Installs both hooks, all or nothing: it checks libgit2's version and reads
/// SQLite's default methods, which SQLite refuses once it is initialized,
/// before it changes anything.
///
/// # Safety
///
/// [`crate::process_init`]'s: no other thread uses libgit2 or SQLite, and
/// SQLite hasn't been initialized.
pub(crate) unsafe fn install() -> Result<(), String> {
    libgit2_version()?;
    // SAFETY: this function's own contract.
    let defaults = unsafe { sqlite_defaults() }?;
    // SAFETY: as above; `defaults` are SQLite's own, from `GETMALLOC`.
    unsafe { install_sqlite(defaults) }?;
    // SAFETY: as above.
    unsafe { install_libgit2() }
}

/// What both hooks end with: a line on standard error, and `_exit(71)`.
///
/// `_exit`, not `exit`: it runs no `atexit` handler, destructor or flush,
/// which could need memory or a lock that the failing thread or another one
/// holds. Its message is static, so that nothing is formatted or allocated.
fn fail_stop(message: &'static [u8]) -> ! {
    #[cfg(unix)]
    let len = message.len();
    #[cfg(windows)]
    let len = message.len() as std::ffi::c_uint;
    // SAFETY: `write` reads `len` bytes from `message`, a static buffer of
    // that length, and touches no other memory. A short or failed write is
    // ignored: the exit code is what the host reads.
    let _ = unsafe { libc::write(2, message.as_ptr().cast(), len) };
    // SAFETY: `_exit` ends the process at once. Nothing runs after it, so no
    // invariant of this process can be observed broken.
    unsafe { libc::_exit(OUT_OF_MEMORY_EXIT) }
}

// libgit2 -------------------------------------------------------------------

/// libgit2's `git_allocator`, which libgit2-sys doesn't declare, as libgit2
/// 1.9.6 (libgit2-sys 0.18.7+1.9.6, which Cargo.toml pins) defines it in
/// `include/git2/sys/alloc.h`, lines 34 to 50: three members, in this order,
/// and no others:
///
/// - `void * GIT_CALLBACK(gmalloc)(size_t n, const char *file, int line);` (line 36)
/// - `void * GIT_CALLBACK(grealloc)(void *ptr, size_t size, const char *file, int line);` (line 43)
/// - `void GIT_CALLBACK(gfree)(void *ptr);` (line 49)
///
/// `GIT_CALLBACK(name)` is `(*name)`, or `(__cdecl *name)` under MSVC
/// (`include/git2/common.h`, lines 51 to 55): `extern "C"` in both cases.
/// [`libgit2_version`] refuses at run time any libgit2 but 1.9.6, the one
/// this was checked against.
///
/// `GIT_OPT_SET_ALLOCATOR` copies the struct (`git_allocator_setup`,
/// `src/util/alloc.c`), so it needn't outlive the call.
#[repr(C)]
struct GitAllocator {
    gmalloc: unsafe extern "C" fn(n: libc::size_t, file: *const c_char, line: c_int) -> *mut c_void,
    grealloc: unsafe extern "C" fn(
        ptr: *mut c_void,
        size: libc::size_t,
        file: *const c_char,
        line: c_int,
    ) -> *mut c_void,
    gfree: unsafe extern "C" fn(ptr: *mut c_void),
}

/// `GIT_OPT_SET_ALLOCATOR` is the 24th of `git_libgit2_opt_t`
/// (`include/git2/common.h`, line 238), counting from 0 at
/// `GIT_OPT_GET_MWINDOW_SIZE`.
const _: () = assert!(libgit2_sys::GIT_OPT_SET_ALLOCATOR as i64 == 23);

/// Refuses any libgit2 but 1.9.6, whose `git_allocator` [`GitAllocator`]
/// was checked against. libgit2-sys 0.18.7 vendors 1.9.6, but can link a
/// system libgit2 from 1.9.6 up to 1.10.0, whose layout no one has checked:
/// a mismatch would have libgit2 call through a wrong table.
fn libgit2_version() -> Result<(), String> {
    checked_version(git2::Version::get().libgit2_version())
}

fn checked_version(version: (u32, u32, u32)) -> Result<(), String> {
    match version {
        (1, 9, 6) => Ok(()),
        (major, minor, rev) => Err(format!(
            "libgit2 {major}.{minor}.{rev} is linked, and its allocator's layout is checked for 1.9.6 only"
        )),
    }
}

/// Routes libgit2's allocations through [`git_malloc`], [`git_realloc`] and
/// [`git_free`], from libgit2's global initialization on.
///
/// The allocator is set first, and libgit2 initialized after, so that the
/// initialization's own allocations (`git_sysdir_global_init`,
/// `git_filter_global_init` and the others) go through it too. libgit2
/// allows that order:
///
/// - `git_libgit2_opts` checks no initialization: it is a switch on its
///   option (`src/libgit2/settings.c`, lines 108 to 115), and
///   `GIT_OPT_SET_ALLOCATOR` only copies the struct into `git__allocator`
///   (lines 346 to 348; `git_allocator_setup`, `src/util/alloc.c`, lines 111
///   to 118);
/// - `git_libgit2_init`'s first step, `git_allocator_global_init`
///   (`src/libgit2/libgit2.c`, line 40), keeps an allocator set before it,
///   and installs the default only over the placeholder that fails every
///   allocation (`src/util/alloc.c`, lines 17 to 21 and 99 to 108).
///
/// In a process that used libgit2 before, the allocator is replaced after
/// its initialization: what libgit2 allocated with its default, the hooks
/// free with the same `free`.
///
/// # Safety
///
/// No other thread uses libgit2: every libgit2 allocation reads the table
/// `GIT_OPT_SET_ALLOCATOR` writes, without synchronization.
unsafe fn install_libgit2() -> Result<(), String> {
    let mut allocator = GitAllocator {
        gmalloc: git_malloc,
        grealloc: git_realloc,
        gfree: git_free,
    };
    // SAFETY: `GIT_OPT_SET_ALLOCATOR` takes one `git_allocator *`, which
    // `allocator` is in layout (`GitAllocator`'s docs), with every member
    // set; libgit2 copies it before the call returns, and needs no
    // initialization for it (this function's docs). No other thread uses
    // libgit2 (this function's contract).
    let rc = unsafe {
        libgit2_sys::git_libgit2_opts(
            libgit2_sys::GIT_OPT_SET_ALLOCATOR as c_int,
            &mut allocator as *mut GitAllocator,
        )
    };
    if rc < 0 {
        return Err(format!("libgit2 refused its allocator: code {rc}"));
    }
    // Once per process; a NULL from an allocation in it now ends the
    // process, where libgit2-sys would panic on the failed initialization.
    libgit2_sys::init();
    Ok(())
}

/// libgit2's `gmalloc`. The C library's `malloc`, as libgit2's default
/// allocator's is (`stdalloc__malloc`, `src/util/allocators/stdalloc.c`),
/// but a NULL for a size above zero ends the process. libgit2-sys builds
/// libgit2 without `GIT_DEBUG_STRICT_ALLOC` or `GIT_WIN32_LEAKCHECK`, so the
/// default it replaces is that one.
///
/// A NULL for zero bytes, which the C library may answer, is libgit2's to
/// handle, as it is with the default.
///
/// # Safety
///
/// Called by libgit2, as `malloc` would be.
unsafe extern "C" fn git_malloc(
    n: libc::size_t,
    _file: *const c_char,
    _line: c_int,
) -> *mut c_void {
    // SAFETY: `malloc` takes any size.
    let ptr = unsafe { libc::malloc(n) };
    if ptr.is_null() && n > 0 {
        fail_stop(LIBGIT2_OUT_OF_MEMORY);
    }
    ptr
}

/// libgit2's `grealloc`: the C library's `realloc`, as `stdalloc__realloc`
/// is, but a NULL for a size above zero ends the process.
///
/// # Safety
///
/// `ptr` is NULL or libgit2's, from `malloc` or `realloc`: this module's
/// hooks or, before they were installed, libgit2's default allocator, which
/// calls the same `malloc`.
unsafe extern "C" fn git_realloc(
    ptr: *mut c_void,
    size: libc::size_t,
    _file: *const c_char,
    _line: c_int,
) -> *mut c_void {
    // SAFETY: `ptr` came from the C library's `malloc` or `realloc`, or is
    // NULL (this function's contract).
    let new = unsafe { libc::realloc(ptr, size) };
    if new.is_null() && size > 0 {
        fail_stop(LIBGIT2_OUT_OF_MEMORY);
    }
    new
}

/// libgit2's `gfree`: the C library's `free`, as `stdalloc__free` is.
///
/// # Safety
///
/// As for [`git_realloc`]'s `ptr`.
unsafe extern "C" fn git_free(ptr: *mut c_void) {
    // SAFETY: as above; `free` takes NULL too.
    unsafe { libc::free(ptr) }
}

// SQLite --------------------------------------------------------------------

/// SQLite's default `xMalloc` and `xRealloc`, which the hooks call: set
/// once, before the hooks are installed, and read by every SQLite allocation
/// after.
struct SqliteDefaults {
    malloc: unsafe extern "C" fn(c_int) -> *mut c_void,
    realloc: unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void,
}

static SQLITE_DEFAULTS: OnceLock<SqliteDefaults> = OnceLock::new();

/// Reads SQLite's memory methods (`SQLITE_CONFIG_GETMALLOC`): its defaults,
/// since nothing in this process has replaced them. SQLite refuses it with
/// `SQLITE_MISUSE` once initialized, as it refuses `SQLITE_CONFIG_MALLOC`.
///
/// # Safety
///
/// No other thread uses SQLite: `sqlite3_config` isn't threadsafe.
unsafe fn sqlite_defaults() -> Result<sqlite_ffi::sqlite3_mem_methods, String> {
    let mut methods = sqlite_ffi::sqlite3_mem_methods {
        xMalloc: None,
        xFree: None,
        xRealloc: None,
        xSize: None,
        xRoundup: None,
        xInit: None,
        xShutdown: None,
        pAppData: std::ptr::null_mut(),
    };
    // SAFETY: `SQLITE_CONFIG_GETMALLOC` takes one `sqlite3_mem_methods *`,
    // which it fills; `methods` is one, valid for writes. No other thread
    // uses SQLite (this function's contract).
    let rc = unsafe {
        sqlite_ffi::sqlite3_config(
            sqlite_ffi::SQLITE_CONFIG_GETMALLOC,
            &mut methods as *mut sqlite_ffi::sqlite3_mem_methods,
        )
    };
    sqlite_ok("SQLITE_CONFIG_GETMALLOC", rc)?;
    Ok(methods)
}

/// Installs SQLite's default methods with [`sqlite_malloc`] and
/// [`sqlite_realloc`] in place of `xMalloc` and `xRealloc`
/// (`SQLITE_CONFIG_MALLOC`). `xFree`, `xSize`, `xRoundup`, `xInit`,
/// `xShutdown` and `pAppData` stay the defaults'.
///
/// # Safety
///
/// As for [`sqlite_defaults`], whose result `defaults` is.
unsafe fn install_sqlite(defaults: sqlite_ffi::sqlite3_mem_methods) -> Result<(), String> {
    let (Some(malloc), Some(realloc)) = (defaults.xMalloc, defaults.xRealloc) else {
        return Err("SQLite's default memory methods have no xMalloc or xRealloc".to_owned());
    };
    SQLITE_DEFAULTS
        .set(SqliteDefaults { malloc, realloc })
        .map_err(|_| "SQLite's default memory methods were already read".to_owned())?;
    let mut methods = sqlite_ffi::sqlite3_mem_methods {
        xMalloc: Some(sqlite_malloc),
        xRealloc: Some(sqlite_realloc),
        ..defaults
    };
    // SAFETY: `SQLITE_CONFIG_MALLOC` takes one `sqlite3_mem_methods *`, which
    // SQLite copies before the call returns; `methods` is SQLite's own
    // defaults with two members replaced by functions of the same
    // signatures. No other thread uses SQLite (this function's contract).
    let rc = unsafe {
        sqlite_ffi::sqlite3_config(
            sqlite_ffi::SQLITE_CONFIG_MALLOC,
            &mut methods as *mut sqlite_ffi::sqlite3_mem_methods,
        )
    };
    sqlite_ok("SQLITE_CONFIG_MALLOC", rc)
}

fn sqlite_ok(op: &str, rc: c_int) -> Result<(), String> {
    match rc {
        sqlite_ffi::SQLITE_OK => Ok(()),
        sqlite_ffi::SQLITE_MISUSE => Err(format!(
            "SQLite refused {op} (SQLITE_MISUSE): it is already initialized in this process, so its allocator can't be replaced"
        )),
        rc => Err(format!("SQLite refused {op}: code {rc}")),
    }
}

/// SQLite's `xMalloc`: its default's, but a NULL ends the process. SQLite
/// calls it only for sizes above zero and below 2^31 - 256: it refuses
/// larger ones itself, with `SQLITE_NOMEM`, which the backstop counts
/// ([`in_chain`]).
///
/// # Safety
///
/// Called by SQLite, as its default `xMalloc` would be.
unsafe extern "C" fn sqlite_malloc(n: c_int) -> *mut c_void {
    let Some(defaults) = SQLITE_DEFAULTS.get() else {
        // Unreachable: the defaults are set before the hooks are installed.
        // A NULL is SQLite's `SQLITE_NOMEM`, which the backstop counts.
        return std::ptr::null_mut();
    };
    // SAFETY: SQLite's own default `xMalloc`, called as SQLite calls it.
    let ptr = unsafe { (defaults.malloc)(n) };
    if ptr.is_null() && n > 0 {
        fail_stop(SQLITE_OUT_OF_MEMORY);
    }
    ptr
}

/// SQLite's `xRealloc`: its default's, but a NULL ends the process.
///
/// # Safety
///
/// Called by SQLite, as its default `xRealloc` would be: `ptr` is from its
/// `xMalloc` or `xRealloc`, which are the defaults' or call them.
unsafe extern "C" fn sqlite_realloc(ptr: *mut c_void, n: c_int) -> *mut c_void {
    let Some(defaults) = SQLITE_DEFAULTS.get() else {
        return std::ptr::null_mut();
    };
    // SAFETY: SQLite's own default `xRealloc`, called as SQLite calls it.
    let new = unsafe { (defaults.realloc)(ptr, n) };
    if new.is_null() && n > 0 {
        fail_stop(SQLITE_OUT_OF_MEMORY);
    }
    new
}

// The backstop --------------------------------------------------------------

/// How many errors [`in_chain`] looks at before it gives up: far more than
/// any chain heartwood and libradicle build, so only a `source()` that loops
/// reaches it.
const CHAIN_LIMIT: usize = 64;

/// Whether `error`, or an error it wraps, is libgit2's `GIT_ERROR_NOMEMORY`
/// or SQLite's `SQLITE_NOMEM` ([`crate::Error::is_out_of_memory`]).
///
/// It follows each error's [`hidden`] error where it has one, or else its
/// `source()`. A wrapper that is thiserror's `transparent` forwards its
/// `source()` to the error it wraps, skipping that error itself, so a
/// `source()` walk alone never sees a `git2::Error` or `sqlite::Error`
/// wrapped that way.
pub(crate) fn in_chain(error: &(dyn StdError + 'static)) -> bool {
    let mut next = Some(error);
    for _ in 0..CHAIN_LIMIT {
        let Some(error) = next else {
            return false;
        };
        if let Some(e) = error.downcast_ref::<git2::Error>() {
            if e.class() == git2::ErrorClass::NoMemory {
                return true;
            }
        }
        if let Some(e) = error.downcast_ref::<sqlite::Error>() {
            if sqlite_out_of_memory(e) {
                return true;
            }
        }
        next = hidden(error).or_else(|| error.source());
    }
    false
}

/// `SQLITE_NOMEM`, or the extended `SQLITE_IOERR_NOMEM`, which SQLite's API
/// mostly answers as `SQLITE_NOMEM` but which a connection with extended
/// result codes can report. The `sqlite` crate's `code` is
/// `sqlite3_errcode`'s, `SQLITE_NOMEM` too for a connection SQLite couldn't
/// allocate.
fn sqlite_out_of_memory(e: &sqlite::Error) -> bool {
    matches!(
        e.code,
        Some(code) if code == sqlite_ffi::SQLITE_NOMEM as isize
            || code == sqlite_ffi::SQLITE_IOERR_NOMEM as isize
    )
}

/// For each type: if `$error` is one, the error its listed variant wraps,
/// or `None` for its other variants.
macro_rules! hops {
    ($error:ident; $($ty:ty { $($variant:ident($e:ident) => $inner:expr),+ $(,)? }),+ $(,)?) => {
        $(
            if let Some(error) = $error.downcast_ref::<$ty>() {
                type Wrapper = $ty;
                #[allow(unreachable_patterns)]
                return match error {
                    $(Wrapper::$variant($e) => Some($inner as &(dyn StdError + 'static)),)+
                    _ => None,
                };
            }
        )+
    };
}

/// The error `error` wraps without making it its `source()`: libradicle's
/// own variants ([`crate::Error::wrapped`]), an `io::Error`'s inner error,
/// and the heartwood wrappers below, of the heartwood types this crate's
/// `Error` holds and the public types those hold. Their other variants,
/// and every other type, go on by `source()`.
///
/// heartwood wraps further errors this way in types it keeps private, or
/// in crates this one doesn't depend on, which no downcast reaches
/// ([`crate::Error::is_out_of_memory`] lists them).
fn hidden<'a>(error: &'a (dyn StdError + 'static)) -> Option<&'a (dyn StdError + 'static)> {
    if let Some(e) = error.downcast_ref::<crate::Error>() {
        return e.wrapped();
    }
    if let Some(e) = error.downcast_ref::<std::io::Error>() {
        return e.get_ref().map(|e| e as &(dyn StdError + 'static));
    }
    hops! { error;
        radicle::profile::Error {
            Io(e) => e, InitConfig(e) => e, LoadConfig(e) => e, Node(e) => e,
            Routing(e) => e, Keystore(e) => e, PolicyStore(e) => e,
            NotificationsStore(e) => e, DatabaseStore(e) => e, Repository(e) => e,
            CobsCache(e) => e, Storage(e) => e,
        },
        radicle::storage::Error { Refs(e) => e },
        radicle::storage::RepositoryError {
            Storage(e) => &**e, Store(e) => e, Doc(e) => e, Payload(e) => e,
            Git(e) => e, Quorum(e) => e, Refs(e) => &**e, FindObjects(e) => e,
        },
        radicle::storage::refs::Error { Git(e) => e, Read(e) => e, Write(e) => e },
        radicle::cob::store::Error {
            Identity(e) => e, Serialize(e) => e, SignRefs(e) => &**e, Git(e) => e,
        },
        radicle::cob::patch::Error { DefaultBranch(e) => e, TargetBranch(e) => e },
        radicle::cob::issue::cache::Error { Object(e) => e, Json(e) => e, Sql(e) => e },
        radicle::cob::patch::cache::Error { ObjectId(e) => e, Json(e) => e, Sql(e) => e },
        radicle::cob::error::Create { Evaluate(e) => &**e, CreateChange(e) => e, Io(e) => e },
        radicle::cob::error::Retrieve { Evaluate(e) => &**e, Git(e) => e, Io(e) => e },
        radicle::cob::error::Update {
            Evaluate(e) => &**e, CreateChange(e) => e, Git(e) => e, Io(e) => e,
        },
        radicle::storage::git::reconcile::driver::Error { Fsync(e) => e, Host(e) => e },
        radicle::storage::git::reconcile::quarantine::Error { Sql(e) => e },
        radicle::storage::git::reconcile::quarantine::RemoveError {
            Quarantine(e) => e, Policy(e) => e, Io(e) => e, Cache(e) => e,
        },
        radicle::storage::git::reconcile::state::Error { Sql(e) => e, Git(e) => e },
        radicle::storage::git::reconcile::cache::Error { Git(e) => e, Json(e) => e, State(e) => e },
        radicle::storage::git::reconcile::namespace::Error {
            Storage(e) => e, Repository(e) => e, Git(e) => e,
        },
        radicle_node::runtime::handle::Error { Io(e) => e },
        radicle_surf::Error {
            Branches(e) => e, Categories(e) => e, Commit(e) => e, Diff(e) => e,
            Directory(e) => e, File(e) => e, Git(e) => e, Glob(e) => e,
            Namespace(e) => e, RefFormat(e) => e, Revision(e) => &**e,
            ToCommit(e) => &**e, Tags(e) => e, Repo(e) => e, Oid(e) => e,
        },
        radicle_surf::fs::error::Directory { Git(e) => e, File(e) => e, Submodule(e) => e },
        radicle_surf::fs::error::File { Git(e) => e },
        radicle_surf::diff::git::error::Diff {
            Addition(e) => e, Deletion(e) => e, Git(e) => e, FileMode(e) => e,
            Hunk(e) => e, Line(e) => e,
        },
        radicle_surf::diff::git::error::Addition { Git(e) => e },
        radicle_surf::diff::git::error::Deletion { Git(e) => e },
        radicle_surf::diff::git::error::Hunk { Git(e) => e, Line(e) => e },
    }
    None
}

#[cfg(test)]
mod tests {
    use std::io;

    use git2::{ErrorClass, ErrorCode};
    use radicle::storage::git::reconcile;
    use radicle::storage::RepositoryError;

    use crate::Error;

    fn git(class: ErrorClass) -> git2::Error {
        git2::Error::new(ErrorCode::GenericError, class, "out of memory")
    }

    fn nomem() -> git2::Error {
        git(ErrorClass::NoMemory)
    }

    fn sqlite(code: Option<isize>) -> sqlite::Error {
        sqlite::Error {
            code,
            message: Some("out of memory".to_owned()),
        }
    }

    #[test]
    fn only_libgit2_1_9_6_is_accepted() {
        assert!(super::libgit2_version().is_ok(), "the vendored libgit2");
        assert!(super::checked_version((1, 9, 6)).is_ok());
        for other in [
            (1, 9, 5),
            (1, 9, 7),
            (1, 9, 0),
            (1, 10, 6),
            (1, 8, 6),
            (2, 9, 6),
        ] {
            assert!(super::checked_version(other).is_err(), "{other:?}");
        }
    }

    #[test]
    fn libgit2s_and_sqlites_own_errors() {
        assert!(Error::Git(nomem()).is_out_of_memory());
        for class in [ErrorClass::Zlib, ErrorClass::Os, ErrorClass::None] {
            assert!(!Error::Git(git(class)).is_out_of_memory(), "{class:?}");
        }
        // SQLITE_NOMEM, and the extended SQLITE_IOERR_NOMEM.
        assert!(Error::Sqlite(sqlite(Some(7))).is_out_of_memory());
        assert!(Error::Sqlite(sqlite(Some(3082))).is_out_of_memory());
        // SQLITE_BUSY, SQLITE_IOERR, SQLITE_TOOBIG, and no code at all.
        for code in [Some(5), Some(10), Some(18), None] {
            assert!(!Error::Sqlite(sqlite(code)).is_out_of_memory(), "{code:?}");
        }
    }

    /// Through `source()`, through an `io::Error`, and through heartwood's
    /// wrappers that aren't a `source()`: `transparent` ones and fields
    /// without `#[source]`.
    #[test]
    fn wrapped_by_heartwood_and_libradicle() {
        let found = [
            // `transparent`, which `source()` skips.
            Error::Repository(RepositoryError::Git(nomem())),
            Error::IssueCache(radicle::issue::cache::Error::Sql(sqlite(Some(7)))),
            // A `source()`: "git: {0}" with `#[from]`.
            Error::Storage(radicle::storage::Error::Git(nomem())),
            Error::Policy(radicle::node::policy::store::Error::Internal(sqlite(Some(
                7,
            )))),
            // Two `transparent` wrappers.
            Error::Storage(radicle::storage::Error::Refs(
                radicle::storage::refs::Error::Git(nomem()),
            )),
            // An `io::Error`'s inner error, which its `source()` skips.
            Error::Io(io::Error::other(nomem())),
            Error::Profile(radicle::profile::Error::Storage(
                radicle::storage::Error::Io(io::Error::other(nomem())),
            )),
            // A boxed field without `#[source]`.
            Error::Cob(radicle::cob::store::Error::SignRefs(Box::new(
                RepositoryError::Git(nomem()),
            ))),
            Error::from(reconcile::driver::Error::Host(nomem())),
            // A `source()`, then a boxed `transparent` one.
            Error::Cob(radicle::cob::store::Error::Retrieve(
                radicle::cob::error::Retrieve::Evaluate(Box::new(nomem())),
            )),
            // radicle-surf's public errors, nested.
            Error::Surf(radicle_surf::Error::Directory(
                radicle_surf::fs::error::Directory::File(radicle_surf::fs::error::File::Git(
                    nomem(),
                )),
            )),
        ];
        for error in found {
            assert!(error.is_out_of_memory(), "{error:?}");
        }

        let missed = [
            Error::Repository(RepositoryError::Git(git(ErrorClass::Odb))),
            Error::Io(io::Error::other("out of memory")),
            // Text, whatever it says.
            Error::NodeThread("out of memory".to_owned()),
            Error::Announce(Error::Git(nomem()).to_string()),
        ];
        for error in missed {
            assert!(!error.is_out_of_memory(), "{error:?}");
        }
    }
}
