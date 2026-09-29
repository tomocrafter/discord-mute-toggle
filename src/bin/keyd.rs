//! 権限分離した入力ヘルパー。input グループ権限で evdev を読み、
//! Right Alt の押下だけを 1 バイトずつソケットの接続先に流す。
//! 他のキーの情報は一切外に出さない。
//!
//! 待ち受けソケットは systemd のソケットアクティベーション (fd 3) で受け取る。
//! ソケットの所有者・パーミッションは .socket ユニット側で絞る。

use std::collections::HashSet;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, thread};

use evdev::{Device, EventType, KeyCode};

const TRIGGER_KEY: KeyCode = KeyCode::KEY_RIGHTALT;
const SD_LISTEN_FDS_START: i32 = 3;

fn is_keyboard(dev: &Device) -> bool {
    dev.supported_keys()
        .is_some_and(|k| k.contains(TRIGGER_KEY) && k.contains(KeyCode::KEY_A))
}

fn watch_device(
    path: PathBuf,
    mut dev: Device,
    tx: Sender<()>,
    opened: Arc<Mutex<HashSet<PathBuf>>>,
) {
    let name = dev.name().unwrap_or("?").to_owned();
    eprintln!("監視開始: {} ({name})", path.display());
    'outer: while let Ok(events) = dev.fetch_events() {
        for ev in events {
            // value: 1 = 押下, 0 = 離す, 2 = リピート
            if ev.event_type() == EventType::KEY
                && ev.code() == TRIGGER_KEY.code()
                && ev.value() == 1
                && tx.send(()).is_err() {
                    break 'outer;
                }
        }
    }
    eprintln!("監視終了: {} ({name})", path.display());
    opened.lock().unwrap().remove(&path);
}

/// キーボードの抜き差しに追従するため、定期的に /dev/input を走査する。
fn spawn_device_scanner(tx: Sender<()>) {
    let opened = Arc::new(Mutex::new(HashSet::new()));
    thread::spawn(move || {
        loop {
            for (path, dev) in evdev::enumerate() {
                if !is_keyboard(&dev) || !opened.lock().unwrap().insert(path.clone()) {
                    continue;
                }
                let (tx, opened) = (tx.clone(), opened.clone());
                thread::spawn(move || watch_device(path, dev, tx, opened));
            }
            thread::sleep(Duration::from_secs(3));
        }
    });
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
    spawn_device_scanner(tx);
    for () in rx {
        // 書けなくなった (切断済みの) クライアントは捨てる
        clients
            .lock()
            .unwrap()
            .retain_mut(|c| c.write_all(b"\x01").is_ok());
    }
}
