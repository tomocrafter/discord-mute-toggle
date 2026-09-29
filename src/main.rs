//! Wayland だと Discord のグローバルキーバインドが効かないので、
//! keyd (権限分離した evdev ヘルパー) から Right Alt の押下を受け取り、
//! Discord RPC (IPC) 経由でマイクミュートをトグルする常駐アプリ。

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const SCOPES: [&str; 3] = ["rpc", "rpc.voice.read", "rpc.voice.write"];
const TOKEN_URL: &str = "https://discord.com/api/oauth2/token";

#[derive(Deserialize)]
struct Config {
    client_id: String,
    client_secret: String,
    #[serde(default)]
    redirect_uri: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Token {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
}

fn config_dir() -> PathBuf {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap()).join(".config"));
    base.join("discord-mute-toggle")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

// ---------- Discord IPC ----------

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;

struct Rpc {
    sock: UnixStream,
    nonce: u64,
}

impl Rpc {
    fn open(client_id: &str) -> Result<Self> {
        let runtime = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        let sock = (0..10)
            .find_map(|i| UnixStream::connect(format!("{runtime}/discord-ipc-{i}")).ok())
            .ok_or_else(|| anyhow!("Discord の IPC ソケットが見つからない (Discord 未起動?)"))?;
        let mut rpc = Rpc { sock, nonce: 0 };
        rpc.write(OP_HANDSHAKE, &json!({ "v": 1, "client_id": client_id }))?;
        let ready = rpc.read_frame()?;
        if ready["evt"] != "READY" {
            bail!("handshake 失敗: {ready}");
        }
        Ok(rpc)
    }

    fn write(&mut self, op: u32, payload: &Value) -> Result<()> {
        let body = serde_json::to_vec(payload)?;
        let mut buf = Vec::with_capacity(8 + body.len());
        buf.extend_from_slice(&op.to_le_bytes());
        buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        buf.extend_from_slice(&body);
        self.sock.write_all(&buf)?;
        Ok(())
    }

    /// FRAME を 1 つ読む。PING には応答し、CLOSE はエラーにする。
    fn read_frame(&mut self) -> Result<Value> {
        loop {
            let mut header = [0u8; 8];
            self.sock.read_exact(&mut header)?;
            let op = u32::from_le_bytes(header[..4].try_into().unwrap());
            let len = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
            let mut body = vec![0u8; len];
            self.sock.read_exact(&mut body)?;
            let value: Value = serde_json::from_slice(&body)?;
            match op {
                OP_FRAME | OP_HANDSHAKE => return Ok(value),
                OP_PING => self.write(OP_PONG, &value)?,
                OP_CLOSE => bail!("Discord が接続を閉じた: {value}"),
                _ => {}
            }
        }
    }

    fn command(&mut self, cmd: &str, args: Value) -> Result<Value> {
        self.nonce += 1;
        let nonce = self.nonce.to_string();
        self.write(
            OP_FRAME,
            &json!({ "cmd": cmd, "args": args, "nonce": nonce }),
        )?;
        loop {
            let res = self.read_frame()?;
            if res["nonce"] != nonce.as_str() {
                continue; // DISPATCH イベントなど
            }
            if res["evt"] == "ERROR" {
                bail!("{cmd} 失敗: {}", res["data"]);
            }
            return Ok(res["data"].clone());
        }
    }
}

// ---------- OAuth ----------

fn token_request(form: &[(&str, &str)]) -> Result<Token> {
    #[derive(Deserialize)]
    struct Resp {
        access_token: String,
        refresh_token: String,
        expires_in: u64,
    }
    let resp: Resp = ureq::post(TOKEN_URL)
        .send_form(form.iter().copied())?
        .body_mut()
        .read_json()?;
    let token = Token {
        access_token: resp.access_token,
        refresh_token: resp.refresh_token,
        expires_at: now() + resp.expires_in,
    };
    let path = config_dir().join("token.json");
    fs::write(&path, serde_json::to_vec_pretty(&token)?)?;
    fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(token)
}

fn authorize(rpc: &mut Rpc, cfg: &Config) -> Result<Token> {
    eprintln!("Discord に認可ダイアログを出しています。Discord 側で「認証」を押してください");
    let data = rpc.command(
        "AUTHORIZE",
        json!({ "client_id": cfg.client_id, "scopes": SCOPES }),
    )?;
    let code = data["code"]
        .as_str()
        .context("AUTHORIZE の応答に code がない")?;
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", &cfg.client_id),
        ("client_secret", &cfg.client_secret),
    ];
    if let Some(uri) = &cfg.redirect_uri {
        form.push(("redirect_uri", uri));
    }
    token_request(&form)
}

fn refresh(cfg: &Config, token: &Token) -> Result<Token> {
    token_request(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &token.refresh_token),
        ("client_id", &cfg.client_id),
        ("client_secret", &cfg.client_secret),
    ])
}

fn load_token() -> Option<Token> {
    let data = fs::read(config_dir().join("token.json")).ok()?;
    serde_json::from_slice(&data).ok()
}

/// 接続して AUTHENTICATE まで済ませる。
/// 保存済みトークン → リフレッシュ → 新規認可 の順に試す。
fn connect(cfg: &Config) -> Result<Rpc> {
    let mut rpc = Rpc::open(&cfg.client_id)?;
    let mut token = load_token();

    if let Some(t) = &token
        && t.expires_at <= now() + 60 {
            token = refresh(cfg, t)
                .inspect_err(|e| eprintln!("トークン更新失敗: {e:#}"))
                .ok();
        }
    if let Some(t) = &token {
        match rpc.command("AUTHENTICATE", json!({ "access_token": t.access_token })) {
            Ok(_) => return Ok(rpc),
            Err(e) => eprintln!("保存済みトークンでの認証失敗: {e:#}"),
        }
    }
    let t = authorize(&mut rpc, cfg)?;
    rpc.command("AUTHENTICATE", json!({ "access_token": t.access_token }))?;
    Ok(rpc)
}

fn toggle_mute(rpc: &mut Rpc) -> Result<bool> {
    let current = rpc.command("GET_VOICE_SETTINGS", json!({}))?;
    let mute = !current["mute"].as_bool().unwrap_or(false);
    rpc.command("SET_VOICE_SETTINGS", json!({ "mute": mute }))?;
    Ok(mute)
}

// ---------- keyd ----------

const KEYD_SOCKET: &str = "/run/discord-mute-toggle.sock";

/// root 側の keyd から Right Alt 押下通知 (1 バイト) を受け取る。切れたら繋ぎ直す。
fn spawn_keyd_reader(tx: Sender<()>) {
    thread::spawn(move || {
        let mut warned = false;
        loop {
            match UnixStream::connect(KEYD_SOCKET) {
                Ok(mut s) => {
                    eprintln!("keyd に接続しました");
                    warned = false;
                    let mut buf = [0u8; 64];
                    while let Ok(n @ 1..) = s.read(&mut buf) {
                        for _ in 0..n {
                            if tx.send(()).is_err() {
                                return;
                            }
                        }
                    }
                    eprintln!("keyd との接続が切れた");
                }
                Err(e) if !warned => {
                    eprintln!("{KEYD_SOCKET} に接続できない: {e}");
                    warned = true;
                }
                Err(_) => {}
            }
            thread::sleep(Duration::from_secs(3));
        }
    });
}

// ---------- main ----------

fn load_config(path: &Path) -> Result<Config> {
    let data = fs::read(path).with_context(|| format!("{} を読めない", path.display()))?;
    Ok(serde_json::from_slice(&data)?)
}

fn main() -> Result<()> {
    let cfg = load_config(&config_dir().join("config.json"))?;

    let (tx, rx) = mpsc::channel();
    spawn_keyd_reader(tx);

    let mut rpc = connect(&cfg)
        .inspect_err(|e| eprintln!("Discord に接続できない: {e:#}"))
        .ok();
    if rpc.is_some() {
        eprintln!("Discord に接続しました");
    }

    for () in rx {
        // 接続が切れていたら (Discord 再起動など) 1 回だけ繋ぎ直して再試行する
        for attempt in 0..2 {
            if rpc.is_none() {
                match connect(&cfg) {
                    Ok(r) => rpc = Some(r),
                    Err(e) => {
                        eprintln!("Discord に接続できない: {e:#}");
                        break;
                    }
                }
            }
            match toggle_mute(rpc.as_mut().unwrap()) {
                Ok(mute) => {
                    eprintln!(
                        "{}",
                        if mute {
                            "ミュート"
                        } else {
                            "ミュート解除"
                        }
                    );
                    break;
                }
                Err(e) => {
                    eprintln!("トグル失敗 (試行 {}): {e:#}", attempt + 1);
                    rpc = None;
                }
            }
        }
    }
    Ok(())
}
