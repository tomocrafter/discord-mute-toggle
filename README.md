# discord-mute-toggle

Toggle your Discord microphone mute with **Right Alt**, even on Wayland.

Discord's global keybinds rely on X11 key grabs, so they stop working once you move to a Wayland session (GNOME, KDE, ...). This project brings the mute toggle back with a small resident Rust app that talks to the Discord desktop client over its local RPC socket. It works whether Discord runs natively on Wayland or under XWayland.

Only the microphone is toggled (self mute). Deafen is left alone.

## How it works

```
 /dev/input/event*                               $XDG_RUNTIME_DIR/discord-ipc-0
        │  (read-only)                                         ▲
        ▼                                                      │ SET_VOICE_SETTINGS {mute}
┌───────────────────┐  /run/discord-mute-toggle.sock  ┌────────┴────────────┐
│ keyd (system)     │ ──── 1 byte per Right Alt ────▶ │ discord-mute-toggle │
│ DynamicUser+input │        (owner: you, 0600)       │ (user service)      │
└───────────────────┘                                 └─────────────────────┘
```

Reading keys on Wayland without the compositor's help means reading `/dev/input` directly. The usual advice is `usermod -aG input $USER`, but that lets every process you run read every key you type. This project splits the work instead:

- **keyd** is a tiny system service that runs as a throwaway `DynamicUser` with only the `input` group. It opens the keyboards read-only, has no network and no capabilities, and runs under a strict systemd sandbox. The only thing it ever sends out is one byte each time Right Alt is pressed; no other key information leaves the process.
- systemd creates the notification socket `/run/discord-mute-toggle.sock`, owned by your user with mode `0600`, so no other user can connect to it.
- **discord-mute-toggle** runs as your user. It never touches `/dev/input`. It receives the notification, reads the current state with `GET_VOICE_SETTINGS`, and flips it with `SET_VOICE_SETTINGS`.

Keyboards are rescanned every few seconds, so hot-plugged keyboards work without a restart. Every keyboard that has a Right Alt key is watched. If Discord restarts, the app reconnects on the next key press. The OAuth token is refreshed automatically.

## Requirements

- Linux with systemd
- A Rust toolchain (`cargo`) to build
- The Discord desktop client, logged in
- A Discord application of your own (free, see below). Discord only allows the RPC voice scopes for the application's owner and its testers, so everyone has to register their own.

## Setup

### 1. Create a Discord application

1. Open the [Discord Developer Portal](https://discord.com/developers/applications) and click **New Application**. Any name works (for example `Mute Toggle`); it is shown in the authorization prompt.
2. Open **OAuth2** in the sidebar.
3. Copy the **Client ID**.
4. Click **Reset Secret** and copy the **Client Secret**. It is only shown once.
5. Under **Redirects**, click **Add Redirect**, enter `http://localhost`, and click **Save Changes**. Nothing is ever sent to this URL, but Discord requires at least one redirect for the authorization to succeed.

You don't need to add a bot, choose scopes in the URL generator, or invite the app anywhere.

### 2. Build and install the user service

```sh
git clone https://github.com/tomocrafter/discord-mute-toggle.git
cd discord-mute-toggle
./install-user.sh
```

The first run builds the project and creates `~/.config/discord-mute-toggle/config.json` (mode `0600`) from `config.example.json`, then stops. Fill in your Client ID and Client Secret:

```json
{
  "client_id": "YOUR_CLIENT_ID",
  "client_secret": "YOUR_CLIENT_SECRET",
  "redirect_uri": "http://localhost"
}
```

Then run it again to install and start the user service:

```sh
./install-user.sh
```

Discord shows an authorization prompt the first time. Click **Authorize**. The token is saved to `~/.config/discord-mute-toggle/token.json` (mode `0600`), and you won't be asked again.

### 3. Install keyd (needs root)

```sh
sudo ./install-keyd.sh
```

This installs `/usr/local/libexec/discord-mute-toggle-keyd` and the `discord-mute-toggle-keyd.socket` / `.service` system units, and enables the socket for the user who ran `sudo`.

### 4. Try it

Join a voice channel and press Right Alt. Check the logs if nothing happens:

```sh
journalctl --user -u discord-mute-toggle -f   # Discord side
journalctl -u discord-mute-toggle-keyd -f     # keyboard side
```

A healthy start looks like this:

```
Discord に接続しました
keyd に接続しました
```

If you still have a Right Alt keybind configured in Discord itself, remove it. Otherwise both will fire and cancel each other out once Discord's own keybinds start working again.

## Updating

```sh
git pull
./install-user.sh
sudo ./install-keyd.sh
```

## Uninstalling

```sh
systemctl --user disable --now discord-mute-toggle.service
rm ~/.local/bin/discord-mute-toggle ~/.config/systemd/user/discord-mute-toggle.service
rm -r ~/.config/discord-mute-toggle

sudo systemctl disable --now discord-mute-toggle-keyd.socket discord-mute-toggle-keyd.service
sudo rm /usr/local/libexec/discord-mute-toggle-keyd \
        /etc/systemd/system/discord-mute-toggle-keyd.socket \
        /etc/systemd/system/discord-mute-toggle-keyd.service
sudo systemctl daemon-reload
```

You can also delete the application in the Developer Portal, and revoke it under **User Settings → Authorized Apps** in Discord.

## Troubleshooting

- **`/run/discord-mute-toggle.sock に接続できない`**: keyd is not installed or its socket is not running. Run `sudo ./install-keyd.sh`, or check `systemctl status discord-mute-toggle-keyd.socket`.
- **`Discord の IPC ソケットが見つからない`**: Discord is not running. The app connects on the next key press after Discord starts.
- **Authorization fails with an `invalid_grant` or redirect error**: make sure `http://localhost` is saved under Redirects and that `redirect_uri` in `config.json` matches it exactly.
- **Authorization keeps being requested**: delete `~/.config/discord-mute-toggle/token.json` and restart the service to authorize again.
- **A keyboard is not picked up**: `journalctl -u discord-mute-toggle-keyd` lists every device being watched. Only devices that report both Right Alt and `A` keys count as keyboards.

## License

[MIT](LICENSE)
