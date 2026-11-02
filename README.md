# niri-screenshot-daemon

A lightweight, event-driven screenshot watchdog that bridges [Niri](https://github.com/YaLTeR/niri) compositor screenshot events to [Satty](https://github.com/gabm/Satty) for instant annotation.

> **Note on repository naming:** This project replaces the legacy KDE Spectacle daemon workflow on Niri (hence the workspace directory name `niri-spectacle-daemon`) with a dedicated native Rust daemon (`niri-screenshot-daemon`) driving Satty.

---

## How It Works

1. **Event-driven IPC**: Connects directly to Niri's UNIX domain socket and reads the native `"EventStream"` JSON feed. When idle, the process remains suspended in a blocking socket read (0% CPU).
2. **File Flush Verification (Fast-Path & Fallback)**:
   - **4 ms initial check**: Niri writes screenshots asynchronously. On modern NVMe/SSD storage, the file is usually finalized within a couple of milliseconds. The daemon tests at the 4 ms mark to launch Satty with near-zero latency.
   - **1-second fallback retries**: If disk sync or compositor encoding takes longer, the daemon retries with 1-second delays (up to ~2 seconds total) before reporting an error.
3. **Multi-Session / Nested Compositor Support**: Only **one** daemon runs on the host compositor. When running nested Niri sessions (e.g. nested sessions, development sandboxes, containerized environments), the nested session calls `niri-screenshot-daemon --client`. The client transmits the nested socket path to the host daemon via a control socket and exits immediately.
4. **Unified Host UI**: When a screenshot is triggered inside *either* the host or any registered nested session, Satty is spawned strictly on the host `WAYLAND_DISPLAY`, keeping all annotation windows on the primary display.

---

## Requirements

- **Niri** (v0.1.8+ with native `ScreenshotCaptured` IPC stream)
- **Satty** (`satty` in `$PATH`)
- **wl-clipboard** (`wl-copy` in `$PATH` for `--copy-command wl-copy`)
- **Rust 1.85+** (Edition 2024)

---

## Building & Installation

A `Makefile` is provided for standard and multi-user setups.

```bash
# Build release binary (target/release/niri-screenshot-daemon)
make build

# Run unit tests and strict pedantic Clippy checks
make test

# Install to current user's ~/bin (creates ~/bin if missing, sets +x)
make install
```

`make install` automatically copies the binary to `~/bin/niri-screenshot-daemon` and also stages it for secondary local users if configured (e.g. nested session users).

---

## Setup & Usage

### 1. Host Niri Session

Add the daemon to your host compositor configuration (`~/.config/niri/config.kdl`):

```kdl
spawn-at-startup "niri-screenshot-daemon"
```

The daemon will:
- Listen on the active Niri socket (`$NIRI_SOCKET` or `/run/user/<UID>/niri-*.sock`).
- Bind its control socket at `/run/user/<UID>/niri-screenshot-daemon.sock`.
- Exit cleanly when the host Niri compositor session closes.

### 2. Nested Niri Session

In your nested Niri session's `config.kdl` or startup script:

```kdl
spawn-at-startup "niri-screenshot-daemon" "--client"
```

Or invoke manually inside the nested session:

```bash
# Auto-detects the nested $NIRI_SOCKET:
niri-screenshot-daemon --client

# Or specify a custom nested socket explicitly:
niri-screenshot-daemon --client --socket /tmp/nested-runtime/niri-0.sock
```

The client registers the nested socket with the running host daemon, receives confirmation, and terminates.

---

## Optional: systemd User Service

If you prefer managing user background services through systemd rather than `spawn-at-startup`, create `~/.config/systemd/user/niri-screenshot-daemon.service`:

```ini
[Unit]
Description=Niri Screenshot Watchdog & Satty Launcher
PartOf=graphical-session.target
After=graphical-session.target

[Service]
Type=simple
ExecStart=%h/bin/niri-screenshot-daemon
Restart=on-failure
RestartSec=2s

[Install]
WantedBy=graphical-session.target
```

Enable and start it with:

```bash
systemctl --user daemon-reload
systemctl --user enable --now niri-screenshot-daemon.service
```

---

## Configuration File (Optional)

Settings can be specified in `~/.config/niri-screenshot-daemon/config.toml` or passed via `--config <path>`:

```toml
# Extra socket paths or directories to monitor at startup
extra_sockets = [
    # "/run/user/1001",
    # "/tmp/nested-runtime/niri-0.sock",
]

# Control socket path (defaults to /run/user/<UID>/niri-screenshot-daemon.sock)
# control_socket = "/run/user/1000/niri-screenshot-daemon.sock"

# Host display overrides (defaults to $WAYLAND_DISPLAY and $XDG_RUNTIME_DIR)
# host_wayland = "wayland-0"
# host_runtime = "/run/user/1000"

# Optional background polling interval in seconds (default 0 = disabled, pure event-driven)
# poll_interval = 0
```

---

## Command-Line Options

| Flag | Description |
|---|---|
| `--client` | Runs in client mode: registers detected or provided sockets with the host daemon and exits. |
| `-s, --socket <PATH>` | Extra socket file or directory to monitor (can be specified multiple times). |
| `--control-socket <PATH>` | Custom path for the daemon control socket. |
| `-c, --config <FILE>` | Path to a custom TOML configuration file. |
| `--wayland-display <NAME>` | Target Wayland display for Satty (defaults to `$WAYLAND_DISPLAY` or `wayland-0`). |
| `--runtime-dir <PATH>` | Target runtime directory for Satty (defaults to `$XDG_RUNTIME_DIR` or `/run/user/<UID>`). |
| `--poll-interval <SECS>` | Optional directory polling interval (default: `0`, event-driven mode). |

---

## Satty Invocation Details

When a screenshot event is detected, Satty is invoked with:

```bash
satty \
  --filename "$PATH" \
  --output-filename "$PATH" \
  --copy-command "wl-copy" \
  --early-exit copy,save
```

This overwrites the screenshot in place if edited, copies the image to the Wayland clipboard upon hitting Copy, and exits immediately upon saving or copying.

---

## License

Dual-licensed under MIT or Apache-2.0.
