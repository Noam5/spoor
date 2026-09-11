//! Whole-filesystem fanotify watcher.
//!
//! The point of running privileged: ONE FAN_MARK_FILESYSTEM mark covers an
//! entire mount, including directories that do not exist yet. FSearch runs
//! unprivileged, so it must mark each directory individually (230k marks on
//! this machine) and silently misses trees created after its last scan.
//!
//! FAN_UNLIMITED_QUEUE also requires CAP_SYS_ADMIN. Without it the queue caps
//! at 16384 events and overflows are dropped with no way to notice.

use std::io;
use std::os::unix::io::RawFd;

pub const FAN_CLOEXEC: u32 = 0x0000_0001;
pub const FAN_CLASS_NOTIF: u32 = 0x0000_0000;
pub const FAN_UNLIMITED_QUEUE: u32 = 0x0000_0010;
pub const FAN_UNLIMITED_MARKS: u32 = 0x0000_0020;
pub const FAN_REPORT_DIR_FID: u32 = 0x0000_0400;
pub const FAN_REPORT_NAME: u32 = 0x0000_0800;
pub const FAN_REPORT_DFID_NAME: u32 = FAN_REPORT_DIR_FID | FAN_REPORT_NAME;

pub const FAN_MARK_ADD: u32 = 0x0000_0001;
pub const FAN_MARK_FILESYSTEM: u32 = 0x0000_0100;

pub const FAN_CREATE: u64 = 0x0000_0100;
pub const FAN_DELETE: u64 = 0x0000_0200;
pub const FAN_MOVED_FROM: u64 = 0x0000_0040;
pub const FAN_MOVED_TO: u64 = 0x0000_0080;
pub const FAN_ONDIR: u64 = 0x4000_0000;
pub const FAN_Q_OVERFLOW: u64 = 0x0000_4000;

const FAN_EVENT_INFO_TYPE_DFID_NAME: u8 = 2;
/// Kernel limit on a file handle's payload (MAX_HANDLE_SZ).
const MAX_HANDLE_SZ: usize = 128;

#[repr(C)]
struct EventMetadata {
    event_len: u32,
    vers: u8,
    reserved: u8,
    metadata_len: u16,
    mask: u64,
    fd: i32,
    pid: i32,
}

#[repr(C)]
struct InfoHeader {
    info_type: u8,
    pad: u8,
    len: u16,
}

#[repr(C)]
struct FileHandle {
    handle_bytes: u32,
    handle_type: i32,
    // f_handle[] follows
}

extern "C" {
    fn fanotify_init(flags: u32, event_f_flags: u32) -> i32;
    fn fanotify_mark(fd: i32, flags: u32, mask: u64, dirfd: i32, path: *const i8) -> i32;
    fn open_by_handle_at(mount_fd: i32, handle: *const FileHandle, flags: i32) -> i32;
}

#[derive(Debug)]
pub struct Event {
    pub mask: u64,
    /// Inode of the directory containing the change.
    pub parent_ino: u64,
    /// Full path of that directory, resolved via the file handle.
    pub parent_path: Option<String>,
    pub name: String,
}

impl Event {
    pub fn is_dir(&self) -> bool {
        self.mask & FAN_ONDIR != 0
    }
    pub fn is_create(&self) -> bool {
        self.mask & (FAN_CREATE | FAN_MOVED_TO) != 0
    }
    pub fn is_delete(&self) -> bool {
        self.mask & (FAN_DELETE | FAN_MOVED_FROM) != 0
    }
}

pub static HANDLE_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub struct Watcher {
    fd: RawFd,
    mount_fd: RawFd,
    buf: Vec<u8>,
}

impl Watcher {
    /// Mark an entire filesystem. `mount` may be any path on it.
    pub fn new(mount: &str) -> io::Result<Self> {
        let flags = FAN_CLOEXEC
            | FAN_CLASS_NOTIF
            | FAN_REPORT_DFID_NAME
            | FAN_UNLIMITED_QUEUE
            | FAN_UNLIMITED_MARKS;
        let fd = unsafe { fanotify_init(flags, libc::O_RDONLY as u32) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let cpath = std::ffi::CString::new(mount).unwrap();
        let mask = FAN_CREATE | FAN_DELETE | FAN_MOVED_FROM | FAN_MOVED_TO | FAN_ONDIR;
        let rc = unsafe {
            fanotify_mark(
                fd,
                FAN_MARK_ADD | FAN_MARK_FILESYSTEM,
                mask,
                libc::AT_FDCWD,
                cpath.as_ptr(),
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }

        // Needed by open_by_handle_at to resolve handles back to paths.
        let mount_fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
        if mount_fd < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }

        Ok(Watcher {
            fd,
            mount_fd,
            buf: vec![0u8; 256 * 1024],
        })
    }

    /// Wait up to `ms` for events. Returns true if some are ready.
    pub fn wait(&self, ms: i32) -> bool {
        let mut pfd = libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut pfd, 1, ms) > 0 }
    }

    /// Block until events arrive, then decode the batch.
    ///
    /// The buffer holds variable-length kernel records that are only 4-byte
    /// aligned, while the metadata struct needs 8: every struct is therefore
    /// copied out with read_unaligned rather than referenced in place (a
    /// misaligned reference is undefined behaviour), and every offset is
    /// checked against its record's end first. This runs as root; a malformed
    /// record is skipped, never read past.
    pub fn read_events(&mut self) -> io::Result<Vec<Event>> {
        const META: usize = std::mem::size_of::<EventMetadata>();
        const HDR: usize = std::mem::size_of::<InfoHeader>();
        let n = unsafe {
            libc::read(
                self.fd,
                self.buf.as_mut_ptr() as *mut libc::c_void,
                self.buf.len(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let total = n as usize;
        let mut out = Vec::new();
        let mut off = 0usize;

        while off + META <= total {
            let meta: EventMetadata = unsafe {
                std::ptr::read_unaligned(self.buf.as_ptr().add(off) as *const EventMetadata)
            };
            let ev_len = meta.event_len as usize;
            if ev_len < META || off + ev_len > total {
                break;
            }
            let end = off + ev_len;

            if meta.mask & FAN_Q_OVERFLOW != 0 {
                out.push(Event {
                    mask: meta.mask,
                    parent_ino: 0,
                    parent_path: None,
                    name: String::new(),
                });
                off = end;
                continue;
            }

            // Info records follow the fixed metadata header.
            let mut ioff = off + (meta.metadata_len as usize).max(META);
            while ioff + HDR <= end {
                let hdr: InfoHeader = unsafe {
                    std::ptr::read_unaligned(self.buf.as_ptr().add(ioff) as *const InfoHeader)
                };
                let hlen = hdr.len as usize;
                if hlen < HDR || ioff + hlen > end {
                    break;
                }
                if hdr.info_type == FAN_EVENT_INFO_TYPE_DFID_NAME {
                    if let Some(ev) = self.decode_dfid_name(meta.mask, ioff, ioff + hlen) {
                        out.push(ev);
                    }
                }
                ioff += hlen;
            }
            off = end;
        }
        Ok(out)
    }

    /// Decode one DFID_NAME info record occupying buf[start..end]: header (4),
    /// fsid (8), struct file_handle (8 + handle_bytes), then a NUL-terminated
    /// name. Returns None for anything that does not fit.
    fn decode_dfid_name(&self, mask: u64, start: usize, end: usize) -> Option<Event> {
        let fh_off = start + 4 + 8;
        if fh_off + 8 > end {
            return None;
        }
        let fh: FileHandle = unsafe {
            std::ptr::read_unaligned(self.buf.as_ptr().add(fh_off) as *const FileHandle)
        };
        let handle_bytes = fh.handle_bytes as usize;
        let name_off = fh_off + 8 + handle_bytes;
        if handle_bytes > MAX_HANDLE_SZ || name_off >= end {
            return None;
        }
        let nul = self.buf[name_off..end].iter().position(|&b| b == 0)?;
        let name = String::from_utf8_lossy(&self.buf[name_off..name_off + nul]).into_owned();
        if name.is_empty() || name == "." {
            return None;
        }
        let (ino, path) = self.resolve_handle(&fh, fh_off);
        Some(Event {
            mask,
            parent_ino: ino,
            parent_path: path,
            name,
        })
    }

    /// Turn a directory file handle into (inode, path). `fh` is a copy of the
    /// handle's header; its payload starts at buf[fh_off + 8] and has already
    /// been bounds-checked by the caller.
    ///
    /// FILEID_INO32_GEN (handle_type 1, used by ext4) encodes the inode in the
    /// first 4 bytes of the payload, so the common case needs no syscall.
    /// open_by_handle_at is the fallback for other handle types; it is
    /// unverified beyond ext4 (it returned EBADF in early testing).
    fn resolve_handle(&self, fh: &FileHandle, fh_off: usize) -> (u64, Option<String>) {
        const FILEID_INO32_GEN: i32 = 1;
        if fh.handle_type == FILEID_INO32_GEN && fh.handle_bytes >= 8 {
            let b = &self.buf[fh_off + 8..fh_off + 12];
            let ino = u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as u64;
            return (ino, None);
        }
        // The kernel needs the whole handle, header and payload, contiguous:
        // pass a raw pointer into the buffer, never a reference.
        let dfd = unsafe {
            open_by_handle_at(
                self.mount_fd,
                self.buf.as_ptr().add(fh_off) as *const FileHandle,
                libc::O_PATH | libc::O_NONBLOCK,
            )
        };
        if dfd < 0 {
            let n = HANDLE_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 3 {
                eprintln!(
                    "    open_by_handle_at failed: {} (handle_bytes={}, type={})",
                    std::io::Error::last_os_error(),
                    fh.handle_bytes,
                    fh.handle_type
                );
            }
            return (0, None);
        }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let empty = b"\0";
        let ino = if unsafe {
            libc::fstatat(
                dfd,
                empty.as_ptr() as *const libc::c_char,
                &mut st,
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            st.st_ino
        } else {
            0
        };
        let path = std::fs::read_link(format!("/proc/self/fd/{}", dfd))
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        unsafe { libc::close(dfd) };
        (ino, path)
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
            libc::close(self.mount_fd);
        }
    }
}
