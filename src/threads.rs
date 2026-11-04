//! How many threads this process has, for the checked initializers
//! ([`crate::isolate_git_config_single_threaded`] and
//! [`crate::remove_git_config_env_single_threaded`]). `None` means the count
//! couldn't be determined, and they take it as a refusal.

/// The process's threads, the caller's among them, or `None` where they
/// can't be counted.
pub(crate) fn count() -> Option<usize> {
    imp::count()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    /// `Threads:` in `/proc/self/status`: the kernel's count of the threads in
    /// this process. No `/proc`, or no such line, is `None`.
    pub(super) fn count() -> Option<usize> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        status
            .lines()
            .find_map(|line| line.strip_prefix("Threads:"))?
            .trim()
            .parse()
            .ok()
    }
}

#[cfg(target_vendor = "apple")]
mod imp {
    extern "C" {
        // <mach/mach_port.h>, which the libc crate doesn't declare.
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }

    /// `task_threads` on this process's own task. It hands back a send right
    /// for each thread, in an array the kernel allocates in this task: both
    /// are released before the count is returned. A failed call is `None`.
    pub(super) fn count() -> Option<usize> {
        // libc points at the mach2 crate for this, which isn't in the graph.
        #[allow(deprecated)]
        // SAFETY: reads the task's own port name, which libSystem sets before
        // `main`, and nothing else writes.
        let task = unsafe { libc::mach_task_self() };
        let mut threads: libc::thread_act_array_t = std::ptr::null_mut();
        let mut n: libc::mach_msg_type_number_t = 0;
        // SAFETY: both out-pointers are valid for writes. On success the
        // kernel writes an array of `n` thread ports, allocated in this task,
        // to `threads`.
        if unsafe { libc::task_threads(task, &mut threads, &mut n) } != libc::KERN_SUCCESS {
            return None;
        }
        let n = n as usize;
        if n == 0 || threads.is_null() {
            return None;
        }
        // SAFETY: on success, `threads` points at `n` ports, allocated and
        // written by the kernel (above), and read only until it is freed below.
        let ports = unsafe { std::slice::from_raw_parts(threads, n) };
        for &thread in ports {
            // SAFETY: releases the send right `task_threads` gave for this
            // thread, once.
            unsafe { mach_port_deallocate(task, thread) };
        }
        // SAFETY: frees the array `task_threads` allocated, `n` ports long,
        // once; nothing reads it after.
        unsafe {
            libc::vm_deallocate(
                task,
                threads as libc::vm_address_t,
                (n * std::mem::size_of::<libc::thread_act_t>()) as libc::vm_size_t,
            )
        };
        Some(n)
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Wdk::System::Threading::{
        NtQueryInformationThread, ThreadQuerySetWin32StartAddress,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_NO_MORE_FILES, HANDLE, HMODULE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::ProcessStatus::{K32GetModuleInformation, MODULEINFO};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessIdOfThread, OpenThread,
        THREAD_QUERY_INFORMATION,
    };

    /// A Toolhelp32 snapshot of the system's threads, filtered by this
    /// process's ID, less the threads that start in ntdll: the system thread
    /// pool's workers, the loader's among them. A failed snapshot, a walk
    /// that ends on anything but `ERROR_NO_MORE_FILES`, an entry too short to
    /// carry its owner, or a thread of this process whose start can't be
    /// read, is `None`.
    pub(super) fn count() -> Option<usize> {
        let ntdll = ntdll()?;
        // SAFETY: no pointer arguments; the handle is closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let threads = walk(snapshot, ntdll);
        // SAFETY: the snapshot's handle, from above, closed once.
        unsafe { CloseHandle(snapshot) };
        threads
    }

    /// The address range ntdll's image occupies in this process.
    fn ntdll() -> Option<std::ops::Range<usize>> {
        let name: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
        // SAFETY: a NUL-terminated UTF-16 name, alive for the call. The
        // module is never unloaded, so its handle needs no release.
        let module: HMODULE = unsafe { GetModuleHandleW(name.as_ptr()) };
        if module.is_null() {
            return None;
        }
        let mut info = MODULEINFO {
            lpBaseOfDll: std::ptr::null_mut(),
            SizeOfImage: 0,
            EntryPoint: std::ptr::null_mut(),
        };
        // SAFETY: no arguments; the pseudo-handle needs no release.
        let this = unsafe { GetCurrentProcess() };
        // SAFETY: this process's pseudo-handle, a module of it, and an
        // out-parameter of the size given.
        let ok = unsafe {
            K32GetModuleInformation(
                this,
                module,
                &mut info,
                std::mem::size_of::<MODULEINFO>() as u32,
            )
        };
        if ok == 0 {
            return None;
        }
        let base = info.lpBaseOfDll as usize;
        Some(base..base.checked_add(info.SizeOfImage as usize)?)
    }

    fn walk(snapshot: HANDLE, ntdll: std::ops::Range<usize>) -> Option<usize> {
        const SIZE: u32 = std::mem::size_of::<THREADENTRY32>() as u32;
        // An entry carries its owner only if it reaches past that field.
        const OWNED: u32 = (std::mem::offset_of!(THREADENTRY32, th32OwnerProcessID)
            + std::mem::size_of::<u32>()) as u32;
        // SAFETY: no arguments.
        let process = unsafe { GetCurrentProcessId() };
        let mut entry = THREADENTRY32 {
            dwSize: SIZE,
            ..Default::default()
        };
        let mut n = 0;
        // SAFETY: a live snapshot handle, and an entry whose `dwSize` is its
        // size, as both calls require.
        let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        while more {
            if entry.dwSize < OWNED {
                return None;
            }
            if entry.th32OwnerProcessID == process
                && !ntdll.contains(&start(entry.th32ThreadID, process)?)
            {
                n += 1;
            }
            entry.dwSize = SIZE;
            // SAFETY: as above.
            more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }
        // SAFETY: no arguments; it reads the failed call's error, just above.
        (unsafe { GetLastError() } == ERROR_NO_MORE_FILES).then_some(n)
    }

    /// Where thread `id`, of this process, started: its Win32 start address.
    /// `None` if it can't be opened or queried, or is no longer this
    /// process's.
    fn start(id: u32, process: u32) -> Option<usize> {
        // SAFETY: no pointer arguments; the handle is closed below.
        let thread = unsafe { OpenThread(THREAD_QUERY_INFORMATION, 0, id) };
        if thread.is_null() {
            return None;
        }
        let start = query(thread, process);
        // SAFETY: the thread's handle, from above, closed once.
        unsafe { CloseHandle(thread) };
        start
    }

    fn query(thread: HANDLE, process: u32) -> Option<usize> {
        // SAFETY: a live thread handle.
        if unsafe { GetProcessIdOfThread(thread) } != process {
            return None;
        }
        let mut start: usize = 0;
        // SAFETY: a live thread handle, opened with THREAD_QUERY_INFORMATION,
        // and an out-parameter of exactly the class's size, a pointer.
        let status = unsafe {
            NtQueryInformationThread(
                thread,
                ThreadQuerySetWin32StartAddress,
                (&raw mut start).cast(),
                std::mem::size_of::<usize>() as u32,
                std::ptr::null_mut(),
            )
        };
        (status >= 0).then_some(start)
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
mod imp {
    /// No count here: the checked initializers refuse.
    pub(super) fn count() -> Option<usize> {
        None
    }
}
