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

/// Minimal fingerprint of a watched file inside `~/Library/Application Support/Ebb/`.
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

/// Which watched files in `~/Library/Application Support/Ebb/` changed on disk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirWatchEvent {
    pub claude_changed: bool,
    pub agy_changed: bool,
    pub config_changed: bool,
}

impl DirWatchEvent {
    pub const fn any(self) -> bool {
        self.claude_changed || self.agy_changed || self.config_changed
    }

    pub const fn any_cache_changed(self) -> bool {
        self.claude_changed || self.agy_changed
    }
}

/// Zero-CPU macOS `kqueue` watcher monitoring Claude cache, Agy cache, and `config.json`
/// inside `~/Library/Application Support/Ebb/` via a single parent-directory `EVFILT_VNODE` fd.
///
/// Atomic `tmp + rename` replacements are always caught even when a target file did not exist
/// at startup; unrelated writes in the same directory (e.g. `usage-cache.json`) are ignored.
pub struct AppDirWatcher {
    stop_flag: std::sync::Arc<AtomicBool>,
    wake_write_fd: Mutex<Option<OwnedFd>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl AppDirWatcher {
    /// Start watching `claude_cache_path`, `agy_cache_path`, and `config_path` inside `base_dir`
    /// (creating `base_dir` with `0700` if needed).
    pub fn start<F>(
        base_dir: PathBuf,
        claude_cache_path: PathBuf,
        agy_cache_path: PathBuf,
        config_path: PathBuf,
        on_change: F,
    ) -> io::Result<Self>
    where
        F: Fn(DirWatchEvent) + Send + 'static,
    {
        ensure_private_dir(&base_dir)?;

        let dir_cstr = CString::new(base_dir.as_os_str().as_bytes()).map_err(|_| {
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
        let watched = WatchedFiles {
            claude_path: claude_cache_path,
            agy_path: agy_cache_path,
            config_path,
        };
        let initial_stamps = WatchedStamps {
            claude: read_file_stamp(&watched.claude_path),
            agy: read_file_stamp(&watched.agy_path),
            config: read_file_stamp(&watched.config_path),
        };

        let handle = thread::Builder::new()
            .name("ebb-dir-watcher".into())
            .spawn(move || {
                watch_loop(
                    kq_fd,
                    dir_fd,
                    wake_read_fd,
                    watched,
                    initial_stamps,
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
        }
        if let Some(handle) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AppDirWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct WatchedFiles {
    claude_path: PathBuf,
    agy_path: PathBuf,
    config_path: PathBuf,
}

struct WatchedStamps {
    claude: Option<FileStamp>,
    agy: Option<FileStamp>,
    config: Option<FileStamp>,
}

fn watch_loop<F>(
    kq_fd: OwnedFd,
    _dir_fd: OwnedFd,
    wake_read_fd: OwnedFd,
    watched: WatchedFiles,
    mut stamps: WatchedStamps,
    stop_flag: std::sync::Arc<AtomicBool>,
    on_change: F,
) where
    F: Fn(DirWatchEvent),
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
            let mut change = DirWatchEvent::default();

            let claude_now = read_file_stamp(&watched.claude_path);
            if claude_now.is_some() && claude_now != stamps.claude {
                stamps.claude = claude_now;
                change.claude_changed = true;
            }

            let agy_now = read_file_stamp(&watched.agy_path);
            if agy_now.is_some() && agy_now != stamps.agy {
                stamps.agy = agy_now;
                change.agy_changed = true;
            }

            let config_now = read_file_stamp(&watched.config_path);
            if config_now.is_some() && config_now != stamps.config {
                stamps.config = config_now;
                change.config_changed = true;
            }

            if change.any() {
                on_change(change);
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
