//! 権限分離した入力ヘルパー。input グループ権限で evdev を読み、
//! Right Alt の押下だけを 1 バイトずつソケットの接続先に流す。
//! 他のキーの情報は一切外に出さない。
//!
//! 待ち受けソケットは systemd のソケットアクティベーション (fd 3) で受け取る。
//! ソケットの所有者・パーミッションは .socket ユニット側で絞る。
//!
//! 常駐するので、アイドル時は一切起きないようにしている:
//! - EVIOCSMASK でカーネル側で Right Alt 以外のイベントを捨てる
//!   (マウス移動や他のキー入力ではスレッドが起こされない)
//! - デバイスの抜き差しはポーリングせず inotify で /dev/input を見る

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::{env, thread};

use evdev::{Device, EventType, KeyCode};

const TRIGGER_KEY: KeyCode = KeyCode::KEY_RIGHTALT;
const SD_LISTEN_FDS_START: i32 = 3;
const INPUT_DIR: &str = "/dev/input";

type Opened = Arc<Mutex<HashSet<PathBuf>>>;

fn is_keyboard(dev: &Device) -> bool {
    dev.supported_keys()
        .is_some_and(|k| k.contains(TRIGGER_KEY) && k.contains(KeyCode::KEY_A))
}

/// `struct input_mask` (linux/input.h)
#[repr(C)]
struct InputMask {
    ty: u32,
    codes_size: u32,
    codes_ptr: u64,
}

/// EVIOCSMASK = _IOW('E', 0x93, struct input_mask)
const EVIOCSMASK: libc::c_ulong =
    (1 << 30) | ((size_of::<InputMask>() as libc::c_ulong) << 16) | (0x45 << 8) | 0x93;
const EV_CNT: u32 = 0x20;
const KEY_CNT: usize = 0x300;

/// このクライアント (fd) に届くイベントを Right Alt の EV_KEY だけに絞る。
/// 中身が空になった SYN_REPORT もカーネルが捨てるので、他の入力では起こされない。
fn restrict_to_trigger(dev: &Device) -> io::Result<()> {
    let mut key_bits = [0u8; KEY_CNT / 8];
    let code = TRIGGER_KEY.code() as usize;
    key_bits[code / 8] |= 1 << (code % 8);
    let none = [0u8; KEY_CNT / 8];

    // EV_SYN (0) はマスクできない。マスク未設定の型は全部通ってしまうので、全型に設定する
    for ty in 1..EV_CNT {
        let is_key = ty == EventType::KEY.0 as u32;
        let codes = if is_key { &key_bits } else { &none };
        let mask = InputMask {
            ty,
            codes_size: codes.len() as u32,
            codes_ptr: codes.as_ptr() as u64,
        };
        let ret = unsafe { libc::ioctl(dev.as_raw_fd(), EVIOCSMASK, &mask) };
        // マスクを持たない型は EINVAL が返るので無視する。EV_KEY に失敗したら諦める
        if ret < 0 && is_key {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn watch_device(path: PathBuf, mut dev: Device, tx: Sender<()>, opened: Opened) {
    let name = dev.name().unwrap_or("?").to_owned();
    eprintln!("監視開始: {} ({name})", path.display());
    'outer: while let Ok(events) = dev.fetch_events() {
        for ev in events {
            // value: 1 = 押下, 0 = 離す, 2 = リピート
            if ev.event_type() == EventType::KEY
                && ev.code() == TRIGGER_KEY.code()
                && ev.value() == 1
                && tx.send(()).is_err()
            {
                break 'outer;
            }
        }
    }
    eprintln!("監視終了: {} ({name})", path.display());
    opened.lock().unwrap().remove(&path);
}

fn try_open(path: &Path, tx: &Sender<()>, opened: &Opened) {
    if opened.lock().unwrap().contains(path) {
        return;
    }
    let Ok(dev) = Device::open(path) else {
        return; // udev がまだ権限を付けていない、など。IN_ATTRIB で再試行される
    };
    if !is_keyboard(&dev) {
        return;
    }
    if let Err(e) = restrict_to_trigger(&dev) {
        eprintln!("{}: EVIOCSMASK 失敗: {e}", path.display());
        return;
    }
    opened.lock().unwrap().insert(path.to_owned());
    let (path, tx, opened) = (path.to_owned(), tx.clone(), opened.clone());
    thread::spawn(move || watch_device(path, dev, tx, opened));
}

fn is_event_node(name: &OsStr) -> bool {
    name.as_bytes().starts_with(b"event")
}

/// 起動時に一度だけ全デバイスを見て、以降は inotify で抜き差しに追従する。
fn spawn_device_watcher(tx: Sender<()>) -> io::Result<()> {
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let inotify = unsafe { OwnedFd::from_raw_fd(fd) };
    let dir = std::ffi::CString::new(INPUT_DIR).unwrap();
    // デバイスノードは root:root で作られ、直後に udev が input グループに変える (IN_ATTRIB)
    let ret =
        unsafe { libc::inotify_add_watch(fd, dir.as_ptr(), libc::IN_CREATE | libc::IN_ATTRIB) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    let opened = Opened::default();
    // watch を張ってから走査するので、その間に挿されたデバイスも取りこぼさない
    for entry in std::fs::read_dir(INPUT_DIR)?.flatten() {
        if is_event_node(&entry.file_name()) {
            try_open(&entry.path(), &tx, &opened);
        }
    }

    thread::spawn(move || {
        // inotify_event は 4 バイト境界に揃っている必要がある
        let mut buf = [0u32; 1024];
        loop {
            let n = unsafe {
                libc::read(
                    inotify.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    size_of_val(&buf),
                )
            };
            if n <= 0 {
                eprintln!("inotify の読み取りに失敗: {}", io::Error::last_os_error());
                return;
            }
            let bytes =
                unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n as usize) };
            let mut off = 0;
            while off < bytes.len() {
                let ev = unsafe { &*bytes.as_ptr().add(off).cast::<libc::inotify_event>() };
                let name_start = off + size_of::<libc::inotify_event>();
                off = name_start + ev.len as usize;
                let name = bytes[name_start..off]
                    .split(|&b| b == 0)
                    .next()
                    .unwrap_or_default();
                let name = OsStr::from_bytes(name);
                if is_event_node(name) {
                    try_open(&Path::new(INPUT_DIR).join(name), &tx, &opened);
                }
            }
        }
    });
    Ok(())
}

fn main() {
    if env::var("LISTEN_FDS").as_deref() != Ok("1") {
        eprintln!("systemd のソケットアクティベーション経由で起動してください");
        std::process::exit(1);
    }
    let listener = unsafe { UnixListener::from_raw_fd(SD_LISTEN_FDS_START) };

    let clients: Arc<Mutex<Vec<UnixStream>>> = Arc::default();
    {
        let clients = clients.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                // 読まないクライアントに詰まらされないよう、書けなければ切り捨てる
                if stream.set_nonblocking(true).is_ok() {
                    clients.lock().unwrap().push(stream);
                }
            }
        });
    }

    let (tx, rx) = mpsc::channel();
    if let Err(e) = spawn_device_watcher(tx) {
        eprintln!("{INPUT_DIR} を監視できない: {e}");
        std::process::exit(1);
    }
    for () in rx {
        // 書けなくなった (切断済みの) クライアントは捨てる
        clients
            .lock()
            .unwrap()
            .retain_mut(|c| c.write_all(b"\x01").is_ok());
    }
}
