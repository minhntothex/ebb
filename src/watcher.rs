use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::SystemTime;

/// Minimal fingerprint of the watched Claude cache file.
/// Including `ino` guarantees consecutive atomic `tmp + rename` writes with identical
/// byte length and timestamp granularity are always detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    ino: u64,
    len: u64,
    modified: Option<SystemTime>,
}

fn read_file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = fs::metadata(path).ok()?;
    Some(FileStamp {
        ino: meta.ino(),
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

/// Zero-CPU macOS `kqueue` watcher for `claude-statusline-cache.json`.
///
/// Watches the parent directory with `EVFILT_VNODE` (`O_EVTONLY`) so atomic `tmp + rename`
/// replacements are always caught even when the target file did not exist at startup.
/// Unrelated writes in the same directory (e.g. `config.json`, `usage-cache.json`) are filtered
/// out via `FileStamp`.
pub struct ClaudeCacheWatcher {
    stop_flag: std::sync::Arc<AtomicBool>,
    wake_write_fd: Mutex<Option<OwnedFd>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl ClaudeCacheWatcher {
    /// Start watching `cache_path` (creating its parent directory with `0700` if needed).
    /// Invokes `on_change` on the watcher thread whenever `cache_path` is created or replaced.
    pub fn start<F>(cache_path: PathBuf, on_change: F) -> io::Result<Self>
    where
        F: Fn() + Send + 'static,
    {
        let parent_dir = cache_path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "claude cache path has no parent directory",
            )
        })?;
        ensure_private_dir(parent_dir)?;

        let dir_cstr = CString::new(parent_dir.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory path contains interior NUL byte",
            )
        })?;

        let dir_fd = unsafe {
            let fd = libc::open(dir_cstr.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            OwnedFd::from_raw_fd(fd)
        };

        let kq_fd = unsafe {
            let fd = libc::kqueue();
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            set_cloexec(fd)?;
            OwnedFd::from_raw_fd(fd)
        };

        let (wake_read_fd, wake_write_fd) = create_cloexec_pipe()?;

        // Register both the parent directory vnode watch and the shutdown wakeup pipe.
        let changes = [
            libc::kevent {
                ident: dir_fd.as_raw_fd() as usize,
                filter: libc::EVFILT_VNODE,
                flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
                fflags: libc::NOTE_WRITE
                    | libc::NOTE_RENAME
                    | libc::NOTE_EXTEND
                    | libc::NOTE_ATTRIB,
                data: 0,
                udata: std::ptr::null_mut(),
            },
            libc::kevent {
                ident: wake_read_fd.as_raw_fd() as usize,
                filter: libc::EVFILT_READ,
                flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            },
        ];

        let reg_res = unsafe {
            libc::kevent(
                kq_fd.as_raw_fd(),
                changes.as_ptr(),
                changes.len() as i32,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if reg_res < 0 {
            return Err(io::Error::last_os_error());
        }

        let stop_flag = std::sync::Arc::new(AtomicBool::new(false));
        let worker_stop = std::sync::Arc::clone(&stop_flag);
        let initial_stamp = read_file_stamp(&cache_path);

        let handle = thread::Builder::new()
            .name("ebb-claude-watcher".into())
            .spawn(move || {
                watch_loop(
                    kq_fd,
                    dir_fd,
                    wake_read_fd,
                    cache_path,
                    initial_stamp,
                    worker_stop,
                    on_change,
                );
            })?;

        Ok(Self {
            stop_flag,
            wake_write_fd: Mutex::new(Some(wake_write_fd)),
            worker: Mutex::new(Some(handle)),
        })
    }

    /// Wake the `kqueue` thread immediately and wait for clean termination.
    pub fn shutdown(&self) {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(write_fd) = self
            .wake_write_fd
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let byte: [u8; 1] = [1];
            unsafe {
                let _ = libc::write(write_fd.as_raw_fd(), byte.as_ptr().cast(), 1);
            }
            // Dropping `write_fd` also closes the write end cleanly.
        }
        if let Some(handle) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ClaudeCacheWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn watch_loop<F>(
    kq_fd: OwnedFd,
    _dir_fd: OwnedFd,
    wake_read_fd: OwnedFd,
    cache_path: PathBuf,
    mut last_stamp: Option<FileStamp>,
    stop_flag: std::sync::Arc<AtomicBool>,
    on_change: F,
) where
    F: Fn(),
{
    let mut events = [unsafe { std::mem::zeroed::<libc::kevent>() }; 4];

    loop {
        if stop_flag.load(Ordering::Acquire) {
            return;
        }

        let n = unsafe {
            libc::kevent(
                kq_fd.as_raw_fd(),
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as i32,
                std::ptr::null(),
            )
        };

        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }

        if stop_flag.load(Ordering::Acquire) {
            return;
        }

        let wake_ident = wake_read_fd.as_raw_fd() as usize;
        let mut saw_dir_event = false;
        for ev in &events[..(n as usize)] {
            if ev.ident == wake_ident {
                return;
            }
            if ev.filter == libc::EVFILT_VNODE {
                saw_dir_event = true;
            }
        }

        if saw_dir_event {
            let current_stamp = read_file_stamp(&cache_path);
            if current_stamp.is_some() && current_stamp != last_stamp {
                last_stamp = current_stamp;
                on_change();
            }
        }
    }
}

fn create_cloexec_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    set_cloexec(read_fd.as_raw_fd())?;
    set_cloexec(write_fd.as_raw_fd())?;
    Ok((read_fd, write_fd))
}

fn set_cloexec(fd: libc::c_int) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(dir)
}
