#!/bin/sh
# ユーザー側 (常駐アプリ) のインストール。sudo なしで実行する。
set -eu
cd "$(dirname "$0")"
cargo build --release
install -Dm755 target/release/discord-mute-toggle "$HOME/.local/bin/discord-mute-toggle"
install -Dm644 systemd/discord-mute-toggle.service "$HOME/.config/systemd/user/discord-mute-toggle.service"

config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/discord-mute-toggle"
mkdir -p "$config_dir"
if [ ! -f "$config_dir/config.json" ]; then
    install -m600 config.example.json "$config_dir/config.json"
    echo "$config_dir/config.json に client_id と client_secret を書いてから、もう一度実行してください"
    exit 0
fi

systemctl --user daemon-reload
systemctl --user enable discord-mute-toggle.service
systemctl --user restart discord-mute-toggle.service
echo "起動しました。初回は Discord に出る認可ダイアログで「認証」を押してください"
