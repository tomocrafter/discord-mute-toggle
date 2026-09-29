//! 依存を増やさないための最小限の inotify ラッパー。

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub use libc::{IN_ATTRIB, IN_CREATE};

pub struct Inotify(OwnedFd);

impl Inotify {
    /// `dir` の直下で `mask` に当たる変化を監視する。
    pub fn watch(dir: &Path, mask: u32) -> io::Result<Self> {
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let this = Inotify(unsafe { OwnedFd::from_raw_fd(fd) });
        let dir = CString::new(dir.as_os_str().as_bytes())?;
        if unsafe { libc::inotify_add_watch(fd, dir.as_ptr(), mask) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(this)
    }

    /// 次の変化まで待ち、変化したエントリの名前を返す。
    pub fn read_names(&self) -> io::Result<Vec<OsString>> {
        // inotify_event は 4 バイト境界に揃っている必要がある
        let mut buf = [0u32; 1024];
        let n = unsafe {
            libc::read(
                self.0.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                size_of_val(&buf),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n as usize) };
        let mut names = Vec::new();
        let mut off = 0;
        while off < bytes.len() {
            let ev = unsafe { &*bytes.as_ptr().add(off).cast::<libc::inotify_event>() };
            let name_start = off + size_of::<libc::inotify_event>();
            off = name_start + ev.len as usize;
            let name = bytes[name_start..off]
                .split(|&b| b == 0)
                .next()
                .unwrap_or_default();
            if !name.is_empty() {
                names.push(OsStr::from_bytes(name).to_owned());
            }
        }
        Ok(names)
    }
}
