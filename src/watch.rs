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
    fn fanotify_mark(fd: i32, flags: u32, mask: u64, dirfd: i32, path: *const libc::c_char) -> i32;
    fn open_by_handle_at(mount_fd: i32, handle: *const FileHandle, flags: i32) -> i32;
}

#[derive(Debug, Clone)]
pub struct Event {
    pub mask: u64,
    /// Inode of the directory containing the change.
    pub parent_ino: u64,
    /// Full path of that directory, resolved via the file handle.
    pub parent_path: Option<Vec<u8>>,
    /// Raw bytes, exactly as the kernel reported them.
    pub name: Vec<u8>,
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

pub static HANDLE_FAILURES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub struct Watcher {
    fd: RawFd,
    mount_fd: RawFd,
    /// st_dev of the watched tree. One filesystem mark on btrfs also covers
    /// every other subvolume, whose inode numbers repeat; events resolved to a
    /// different device are not ours.
    root_dev: u64,
    buf: Vec<u8>,
}

/// The mount point `path` sits on, from /proc/self/mountinfo.
fn mount_point_of(path: &[u8]) -> Option<Vec<u8>> {
    longest_mount(&std::fs::read("/proc/self/mountinfo").ok()?, path)
}

/// The longest mount point containing `path`. Field 5 of a mountinfo line is
/// the mount point, with space, tab, newline and backslash written as octal
/// escapes.
fn longest_mount(mountinfo: &[u8], path: &[u8]) -> Option<Vec<u8>> {
    let mut best: Option<Vec<u8>> = None;
    for line in mountinfo.split(|&b| b == b'\n') {
        let Some(field) = line.split(|&b| b == b' ').nth(4) else {
            continue;
        };
        let mp = unescape_mount(field);
        if under(path, &mp) && best.as_ref().is_none_or(|b| mp.len() > b.len()) {
            best = Some(mp);
        }
    }
    best
}

fn under(path: &[u8], mount: &[u8]) -> bool {
    mount == b"/"
        || path == mount
        || (path.starts_with(mount) && path.get(mount.len()) == Some(&b'/'))
}

fn unescape_mount(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        let octal = (field[i] == b'\\')
            .then(|| field.get(i + 1..i + 4))
            .flatten()
            .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)))
            .map(|d| (d[0] - b'0') * 64 + (d[1] - b'0') * 8 + (d[2] - b'0'));
        match octal {
            Some(b) => {
                out.push(b);
                i += 4;
            }
            None => {
                out.push(field[i]);
                i += 1;
            }
        }
    }
    out
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
            let e = io::Error::last_os_error();
            let hint = match e.raw_os_error() {
                Some(libc::EINVAL) => " -- spoor needs Linux 5.9 or newer (FAN_REPORT_DFID_NAME)",
                Some(libc::EPERM) => " -- spoor needs CAP_SYS_ADMIN; run it as root",
                _ => "",
            };
            return Err(io::Error::new(
                e.kind(),
                format!("fanotify_init: {}{}", e, hint),
            ));
        }

        let cpath = std::ffi::CString::new(mount).unwrap();
        let mask = FAN_CREATE | FAN_DELETE | FAN_MOVED_FROM | FAN_MOVED_TO | FAN_ONDIR;
        let mark = |c: &std::ffi::CString| unsafe {
            fanotify_mark(
                fd,
                FAN_MARK_ADD | FAN_MARK_FILESYSTEM,
                mask,
                libc::AT_FDCWD,
                c.as_ptr(),
            )
        };
        let mut rc = mark(&cpath);
        if rc < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EXDEV) {
            // A btrfs subvolume carries its own device number, and the kernel
            // refuses a filesystem mark on a path that is not on the
            // filesystem's own device. One mark covers the whole superblock,
            // subvolumes included, so mark the containing mount instead; the
            // device check in resolve_handle keeps only this tree's events.
            if let Some(mp) = mount_point_of(mount.as_bytes()) {
                if mp != cpath.as_bytes() {
                    if let Ok(c) = std::ffi::CString::new(mp) {
                        rc = mark(&c);
                        if rc >= 0 {
                            eprintln!(
                                "spoor: {} is a subvolume; watching its mount {}",
                                mount,
                                c.to_string_lossy()
                            );
                        }
                    }
                }
            }
        }
        if rc < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            let hint = match e.raw_os_error() {
                Some(libc::EXDEV) => {
                    " -- fanotify refuses a filesystem mark on this path (a btrfs \
                     subvolume), and its mount point could not be marked either"
                }
                Some(libc::ENODEV) | Some(libc::EOPNOTSUPP) => {
                    " -- this filesystem does not support fanotify file handles"
                }
                _ => "",
            };
            return Err(io::Error::new(
                e.kind(),
                format!("fanotify_mark {}: {}{}", mount, e, hint),
            ));
        }

        // Needed by open_by_handle_at to resolve handles back to paths.
        let mount_fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
        if mount_fd < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }

        let root_dev = std::fs::metadata(mount)
            .map(|m| std::os::unix::fs::MetadataExt::dev(&m))
            .unwrap_or(0);
        Ok(Watcher {
            fd,
            mount_fd,
            root_dev,
            buf: vec![0u8; 256 * 1024],
        })
    }

    /// Wait up to `ms` for events. Returns true if some are ready.
    pub fn wait(&self, ms: i32) -> bool {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe { libc::poll(&mut pfd, 1, ms) > 0 }
    }

    /// Block until events arrive, then decode the batch.
    pub fn read_events(&mut self) -> io::Result<Vec<Event>> {
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
        Ok(parse_events(&self.buf[..total], &|fh, fh_off| {
            self.resolve_handle(fh, fh_off)
        }))
    }

    /// Turn a directory file handle into (inode, path). `fh` is a copy of the
    /// handle's header; its payload starts at buf[fh_off + 8] and has already
    /// been bounds-checked by the caller.
    ///
    /// FILEID_INO32_GEN (handle_type 1, used by ext4) encodes the inode in the
    /// first 4 bytes of the payload, so the common case needs no syscall.
    /// open_by_handle_at is the fallback for other handle types; it is
    /// unverified beyond ext4 (it returned EBADF in early testing).
    fn resolve_handle(&self, fh: &FileHandle, fh_off: usize) -> (u64, Option<Vec<u8>>) {
        // Formats that carry the inode in the clear (generic ext2/3/4 and xfs
        // encoders, native byte order). The caller has already checked that
        // handle_bytes of payload lie inside the record.
        const FILEID_INO32_GEN: i32 = 1;
        const FILEID_INO32_GEN_PARENT: i32 = 2;
        const FILEID_INO64_GEN: i32 = 0x81;
        const FILEID_INO64_GEN_PARENT: i32 = 0x82;
        let p = fh_off + 8;
        let hb = fh.handle_bytes as usize;
        match fh.handle_type {
            FILEID_INO32_GEN | FILEID_INO32_GEN_PARENT if hb >= 8 => {
                let b = &self.buf[p..p + 4];
                return (u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as u64, None);
            }
            FILEID_INO64_GEN | FILEID_INO64_GEN_PARENT if hb >= 12 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&self.buf[p..p + 8]);
                return (u64::from_ne_bytes(b), None);
            }
            _ => {}
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
        let dev = st.st_dev;
        let path = std::fs::read_link(format!("/proc/self/fd/{}", dfd))
            .ok()
            .map(|p| std::os::unix::ffi::OsStringExt::into_vec(p.into_os_string()));
        unsafe { libc::close(dfd) };
        if ino == 0 || dev != self.root_dev {
            return (0, None); // another subvolume or filesystem: not indexed here
        }
        (ino, path)
    }
}

/// Maps a file handle (and its offset in the read buffer, for the
/// open_by_handle_at fallback) to an inode and, when known, its path.
type Resolve<'a> = dyn Fn(&FileHandle, usize) -> (u64, Option<Vec<u8>>) + 'a;

/// Decode a buffer of fanotify records.
///
/// The records are variable-length and only 4-byte aligned, while the metadata
/// struct needs 8: every struct is copied out with read_unaligned rather than
/// referenced in place (a misaligned reference is undefined behaviour), and
/// every offset is checked against its record's end before it is read. This
/// runs as root; a malformed record is skipped, never read past. `resolve`
/// turns a directory file handle (header copy, offset of the header in `buf`)
/// into an inode and optional path.
fn parse_events(buf: &[u8], resolve: &Resolve<'_>) -> Vec<Event> {
    const META: usize = std::mem::size_of::<EventMetadata>();
    const HDR: usize = std::mem::size_of::<InfoHeader>();
    let total = buf.len();
    let mut out = Vec::new();
    let mut off = 0usize;

    while off + META <= total {
        let meta: EventMetadata =
            unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const EventMetadata) };
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
                name: Vec::new(),
            });
            off = end;
            continue;
        }

        // Info records follow the fixed metadata header.
        let mut ioff = off + (meta.metadata_len as usize).max(META);
        while ioff + HDR <= end {
            let hdr: InfoHeader =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().add(ioff) as *const InfoHeader) };
            let hlen = hdr.len as usize;
            if hlen < HDR || ioff + hlen > end {
                break;
            }
            if hdr.info_type == FAN_EVENT_INFO_TYPE_DFID_NAME {
                if let Some(ev) = decode_dfid_name(buf, meta.mask, ioff, ioff + hlen, resolve) {
                    out.push(ev);
                }
            }
            ioff += hlen;
        }
        off = end;
    }
    out
}

/// Decode one DFID_NAME info record occupying buf[start..end]: header (4),
/// fsid (8), struct file_handle (8 + handle_bytes), then a NUL-terminated name.
/// Returns None for anything that does not fit.
fn decode_dfid_name(
    buf: &[u8],
    mask: u64,
    start: usize,
    end: usize,
    resolve: &Resolve<'_>,
) -> Option<Event> {
    let fh_off = start + 4 + 8;
    if fh_off + 8 > end {
        return None;
    }
    let fh: FileHandle =
        unsafe { std::ptr::read_unaligned(buf.as_ptr().add(fh_off) as *const FileHandle) };
    let handle_bytes = fh.handle_bytes as usize;
    let name_off = fh_off + 8 + handle_bytes;
    if handle_bytes > MAX_HANDLE_SZ || name_off >= end {
        return None;
    }
    let nul = buf[name_off..end].iter().position(|&b| b == 0)?;
    let name = buf[name_off..name_off + nul].to_vec();
    if name.is_empty() || name == b"." {
        return None;
    }
    let (ino, path) = resolve(&fh, fh_off);
    Some(Event {
        mask,
        parent_ino: ino,
        parent_path: path,
        name,
    })
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
            libc::close(self.mount_fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One fanotify event carrying a DFID_NAME record, laid out as the kernel
    /// writes it (info records padded to 4 bytes).
    fn record(mask: u64, handle_type: i32, payload: &[u8], name: &[u8]) -> Vec<u8> {
        let mut info = vec![FAN_EVENT_INFO_TYPE_DFID_NAME, 0, 0, 0];
        info.extend_from_slice(&[0u8; 8]); // fsid
        info.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        info.extend_from_slice(&handle_type.to_ne_bytes());
        info.extend_from_slice(payload);
        info.extend_from_slice(name);
        info.push(0);
        while !info.len().is_multiple_of(4) {
            info.push(0);
        }
        let ilen = info.len() as u16;
        info[2..4].copy_from_slice(&ilen.to_ne_bytes());
        let mut ev = Vec::new();
        ev.extend_from_slice(&((24 + info.len()) as u32).to_ne_bytes()); // event_len
        ev.extend_from_slice(&[3, 0]); // vers, reserved
        ev.extend_from_slice(&24u16.to_ne_bytes()); // metadata_len
        ev.extend_from_slice(&mask.to_ne_bytes());
        ev.extend_from_slice(&(-1i32).to_ne_bytes()); // fd
        ev.extend_from_slice(&42i32.to_ne_bytes()); // pid
        ev.extend_from_slice(&info);
        ev
    }

    fn by_type(fh: &FileHandle, _off: usize) -> (u64, Option<Vec<u8>>) {
        (fh.handle_type as u64, None)
    }

    #[test]
    fn mount_points_come_from_mountinfo() {
        let info = b"25 30 0:23 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n\
             31 25 0:41 / /mnt/disk\\040two rw shared:9 - btrfs /dev/sdb1 rw\n\
             32 25 0:42 / /mnt/disk rw shared:9 - btrfs /dev/sdc1 rw\n";
        let m = |p: &str| longest_mount(info, p.as_bytes()).map(|v| String::from_utf8(v).unwrap());
        assert_eq!(m("/mnt/disk/sub/a").as_deref(), Some("/mnt/disk"));
        assert_eq!(m("/mnt/disk two/x").as_deref(), Some("/mnt/disk two"));
        assert_eq!(m("/mnt/diskother").as_deref(), Some("/"));
        assert_eq!(m("/home/alice").as_deref(), Some("/"));
        assert_eq!(longest_mount(b"", b"/home"), None);
        assert_eq!(unescape_mount(b"/a\\040b"), b"/a b".to_vec());
        assert_eq!(unescape_mount(b"/plain\\09"), b"/plain\\09".to_vec());
    }

    #[test]
    fn decodes_a_create_event() {
        let buf = record(
            FAN_CREATE | FAN_ONDIR,
            1,
            &[7, 0, 0, 0, 9, 9, 9, 9],
            b"photos",
        );
        let evs = parse_events(&buf, &by_type);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].name, b"photos".to_vec());
        assert!(evs[0].is_dir() && evs[0].is_create());
        assert_eq!(evs[0].parent_ino, 1, "resolver saw the handle type");
    }

    #[test]
    fn records_at_unaligned_offsets() {
        // 24 + 4 + 8 + 8 + 8 + len("abcd") + NUL = 57, padded to 60: 4- but
        // not 8-aligned, so the second record's u64 mask sits misaligned
        let mut buf = record(FAN_CREATE, 1, &[1; 8], b"abcd");
        assert_eq!(buf.len() % 8, 4);
        buf.extend(record(FAN_DELETE, 1, &[2; 8], b"second"));
        let evs = parse_events(&buf, &by_type);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[1].name, b"second".to_vec());
        assert!(evs[1].is_delete());
    }

    #[test]
    fn truncated_buffers_yield_only_whole_events() {
        let mut buf = record(FAN_CREATE, 1, &[1; 8], b"first");
        let whole = buf.len();
        buf.extend(record(FAN_CREATE, 1, &[1; 8], b"cut-off"));
        for cut in 0..buf.len() {
            let evs = parse_events(&buf[..cut], &by_type);
            assert_eq!(
                evs.len(),
                if cut >= whole { 1 } else { 0 },
                "cut at {}",
                cut
            );
        }
    }

    #[test]
    fn malformed_records_are_skipped() {
        // handle larger than the kernel allows
        let big = record(FAN_CREATE, 1, &[0; 200], b"x");
        assert!(parse_events(&big, &by_type).is_empty());
        // name with no terminator inside the record
        let mut noterm = record(FAN_CREATE, 1, &[0; 8], b"name");
        let n = noterm.len();
        for b in &mut noterm[n - 4..] {
            *b = b'z';
        }
        assert!(parse_events(&noterm, &by_type).is_empty());
        // info header with length 0 must not loop forever
        let mut zero = record(FAN_CREATE, 1, &[0; 8], b"name");
        zero[24 + 2] = 0;
        zero[24 + 3] = 0;
        assert!(parse_events(&zero, &by_type).is_empty());
        // event_len smaller than the metadata
        let mut short = record(FAN_CREATE, 1, &[0; 8], b"name");
        short[0..4].copy_from_slice(&8u32.to_ne_bytes());
        assert!(parse_events(&short, &by_type).is_empty());
    }

    #[test]
    fn queue_overflow_is_reported() {
        let mut ev = Vec::new();
        ev.extend_from_slice(&24u32.to_ne_bytes());
        ev.extend_from_slice(&[3, 0]);
        ev.extend_from_slice(&24u16.to_ne_bytes());
        ev.extend_from_slice(&FAN_Q_OVERFLOW.to_ne_bytes());
        ev.extend_from_slice(&(-1i32).to_ne_bytes());
        ev.extend_from_slice(&0i32.to_ne_bytes());
        let evs = parse_events(&ev, &by_type);
        assert_eq!(evs.len(), 1);
        assert!(evs[0].mask & FAN_Q_OVERFLOW != 0);
    }

    #[test]
    fn never_panics_on_garbage() {
        // xorshift: deterministic, no dependency
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..5_000 {
            let len = (next() % 600) as usize;
            let buf: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let _ = parse_events(&buf, &by_type);
        }
        // and mutations of a valid record, which get past the first checks
        let base = record(FAN_CREATE, 1, &[1; 8], b"valid-name");
        for _ in 0..20_000 {
            let mut m = base.clone();
            for _ in 0..(1 + next() % 4) {
                let i = (next() as usize) % m.len();
                m[i] = next() as u8;
            }
            let _ = parse_events(&m, &by_type);
        }
    }
}
