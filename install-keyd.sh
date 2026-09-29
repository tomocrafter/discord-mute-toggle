#!/bin/sh
# root 側 (keyd) のインストール。sudo で実行する。
set -eu
cd "$(dirname "$0")"
user="${SUDO_USER:?sudo 経由で実行してください}"
if [ ! -x target/release/keyd ]; then
    echo "先に一般ユーザーで cargo build --release (または ./install-user.sh) を実行してください" >&2
    exit 1
fi
install -Dm755 -o root -g root target/release/keyd /usr/local/libexec/discord-mute-toggle-keyd
install -Dm644 systemd/discord-mute-toggle-keyd.service /etc/systemd/system/discord-mute-toggle-keyd.service
sed "s/@USER@/$user/" systemd/discord-mute-toggle-keyd.socket > /etc/systemd/system/discord-mute-toggle-keyd.socket
chmod 644 /etc/systemd/system/discord-mute-toggle-keyd.socket
systemctl daemon-reload
systemctl enable --now discord-mute-toggle-keyd.socket
systemctl restart discord-mute-toggle-keyd.service 2>/dev/null || true
echo "keyd をインストールしました"
