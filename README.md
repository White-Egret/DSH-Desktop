# DSH Desktop

A lightweight Windows 10/11 desktop app (Tauri v2 + Rust + vanilla HTML/CSS/JS) for launching, managing, and updating a locally npm-installed DSH (DeepSeek Harness) Web service.

> **Important — what this app does NOT bundle**
>
> - This desktop does **not** bundle Node.js.
> - This desktop does **not** bundle DSH.
> - It does not ship or deploy any portable Node/DSH runtime either.
> - Users must install Node.js and DSH themselves (the first-run wizard can guide you; see [Prerequisites](#prerequisites)).

## Overview

DSH Desktop wraps the locally installed `dsh` CLI into a native window:

- One single window: a thin toolbar on top, and the DSH web UI embedded below it once the service is ready. The default browser is never opened (the launch command always includes `--no-open`).
- The app starts DSH with **your** configured paths and port, waits until the HTTP service is actually ready (TCP + HTTP polling), then embeds the page. If DSH exits before becoming ready, you get the last stderr line immediately instead of an infinite wait.
- Process ownership is strict: the app only manages the DSH process tree it started itself (`taskkill /PID <pid> /T /F` plus a Windows Job Object). It never uses image-name kills like `taskkill /IM node.exe /F`, so other Node programs on your machine are safe.

## Features

- Single-window design with embedded DSH web UI (multi-webview), auto-resizing with the window
- Configurable port (default **3080**), paths auto-detected with manual override in Settings
- Port-in-use protection: prompts instead of killing unknown processes — connect to the existing service, change port, or re-check
- Actual-address detection: if DSH prints its real listen URL (e.g. `dsh web: http://127.0.0.1:3080`), the app loads that address preferentially
- File logging: Desktop log at `%APPDATA%\com.dsh.desktop\desktop.log`, DSH output log at `%USERPROFILE%\.dsh\logs\dsh.log`; UI buttons to open the log folder and copy errors/log text
- Refresh Page: reloads only the embedded DSH page without restarting the backend service (`F5` / `Ctrl+R`)
- One-click update of DSH via npm, live output streaming
- Close-to-tray or quit-on-close behavior (configurable); tray menu with Show Main Window / Start with Windows / Exit; tray restore does show + unminimize + set_focus
- Start with Windows (official autostart plugin, HKCU registry only, no admin rights); autostart runs silently in tray and delays DSH launch by 12 s to avoid the boot-time IO spike
- First-run setup wizard: detects Node.js/npm/DSH and can guide installation (official nodejs.org LTS installer download or `npm install -g @deepseek-ai/dsh`) — fully skippable
- **Bilingual UI (Chinese / English)**: choose a language in Settings; the whole launcher (toolbar, status, dialogs, logs, tray menu) switches, and DSH's own web UI follows via its `settings.yaml`
- Single-instance lock: launching a second copy just focuses the existing window

## Requirements

| Item | Requirement |
|---|---|
| OS | Windows 10 or 11, x64 |
| WebView2 Runtime | Usually preinstalled with Edge on Windows 10/11; installers bootstrap it if missing |
| Node.js | Required by DSH (LTS recommended). **Not bundled.** |
| DSH | Installed globally via npm. **Not bundled.** |

## Prerequisites

You need both of these before DSH Desktop can start a service:

1. **Node.js** (with npm) — install the official LTS from <https://nodejs.org/en/download>.
2. **DSH** — install globally:

   ```bash
   npm install -g @deepseek-ai/dsh
   ```

Verify manually if you like:

```bash
node --version
npm --version
dsh --version
```

If anything is missing when the app starts, it shows a clear error (which component was not found, where it looked, and how to fix it) instead of waiting forever. The first-run wizard can also do this for you: it detects the environment and offers to run the official Node.js LTS installer (downloaded from nodejs.org at runtime, never bundled) or to execute `npm install -g @deepseek-ai/dsh` for you. Every guided step is skippable ("稍后手动安装" / skip), and every failure mode (no network, download failed, permission denied, user cancelled) is reported explicitly.

## Installation

No local Rust toolchain needed — GitHub Actions builds the installers for you (see next section). Grab the artifacts of the latest successful build:

- `DSH-Desktop-windows-nsis` → contains `DSH Desktop_<version>_x64-setup.exe` — NSIS installer (recommended)
- `DSH-Desktop-windows-msi` → MSI installer
- `DSH-Desktop-windows-portable-exe` → standalone portable exe (same features except the NSIS first-run moment)

Install with the NSIS setup exe, or just run the portable exe. On first launch the environment check wizard appears once (it disappears permanently once `%APPDATA%\com.dsh.desktop\config.json` exists).

## Build from source

Requirements: Node.js 18+ (or 20 LTS), Rust stable (MSVC toolchain), Visual Studio Build Tools, WebView2.

```bash
npm ci          # installs @tauri-apps/cli
npm run tauri build
```

Artifacts land in `src-tauri/target/release/bundle/nsis/`, `.../msi/`, and the raw exe in `src-tauri/target/release/`.

## GitHub Actions build

`.github/workflows/build-windows.yml` builds automatically on push to `main`/`master`, on tags `v*`, on PRs, and via manual dispatch. It produces the artifacts listed under [Installation](#installation). First build takes roughly 8–15 minutes (Rust compile); later builds are faster thanks to caching.

> If Actions didn't trigger: check *Settings → Actions → General → Actions permissions* and make sure `.github/workflows/build-windows.yml` is committed.

## Usage

1. Install/start **DSH Desktop**. Default window is 1200×760.
2. On first run the setup wizard checks Node.js / npm / DSH:
   - Everything installed → click "完成，进入主界面" (done).
   - Something missing → use the guided buttons or skip and continue to the main UI anyway.
3. Click **▶ 启动 (Start)** (or let the app auto-start DSH): status shows "starting… waited X s", then the DSH page embeds seamlessly once ready.
4. Toolbar right side always shows: status dot · current port · version info.
5. Closing the window hides to tray by default (configurable); use the tray icon or menu to bring it back; tray menu **退出 (Exit)** truly quits and stops the DSH process tree this app started.

Example commands the app effectively runs (using your configured values):

```text
Default DSH web URL:      http://127.0.0.1:3080
Example DSH start command: dsh web --port 3080
Example global install:    npm install -g @deepseek-ai/dsh
```

## Configuration

Config file: `%APPDATA%\com.dsh.desktop\config.json` (per-user; never written to Program Files or the install directory).

Open **⚙ 设置 (Settings)** from the toolbar. All fields support auto-detection: leave them empty/broken and the app finds Node.js, npm, and dsh automatically (`where` lookup + common install directories such as `%ProgramFiles%\nodejs` and `%APPDATA%\npm`). Detected results fill the form automatically.

| Setting | Default | Notes |
|---|---|---|
| npm.cmd path | empty → auto-detected | used for update / version queries |
| dsh path | empty → auto-detected | `dsh.cmd` / `dsh.exe` / `dsh.bat`; used to launch DSH |
| DSH home dir | `%USERPROFILE%\.dsh` | passed to DSH as `DSH_HOME`; process cwd is its parent; not your workspace |
| Port | `3080` | must be 1–65535; validated on save; takes effect on next DSH start |
| Ready timeout | `300` seconds | cold start can take minutes; **0 = wait forever** (as long as the process lives) |
| When clicking X | hide to tray | or "quit program" (stops the DSH process started by this session) |
| Extra args | empty | appended after `dsh web --port N --no-open` |
| Package name | `@deepseek-ai/dsh` | used by version check / update |
| Update args | `install -g @deepseek-ai/dsh@latest` | appended after `cmd /C npm.cmd` |
| Start with Windows | off | immediate effect, `HKCU\...\Run`, also toggleable from the tray menu |
| Interface language | `zh` (中文) | `zh` / `en`; switches the whole launcher and syncs DSH's `settings.yaml` — see [Language](#language) |

## Language

The launcher UI is bilingual (Simplified Chinese / English).

- Pick **语言 / Language → English** in **⚙ Settings** and save. The toolbar, status area, dialogs, tray menu and every launcher-generated log line switch to English; choosing **中文** switches everything back. A restart is not required (and after restarting Desktop everything stays in your chosen language, read from `config.json`).
- **DSH's own web interface**: on save, the app also writes the matching value into `<DSH home dir>\settings.yaml`:

  ```yaml
  locale:
    preference: en   # or: zh
  ```

  This is done as a minimal, targeted line edit — any other keys you keep in `settings.yaml` are preserved. DSH reads this file when it starts, so **restart DSH** (toolbar ⟳ Restart, or Stop + Start) for its interface language to change. If DSH is running when you change the language, the app logs a hint telling you a DSH restart is needed.
- The first-run setup wizard follows the same rule: it renders in Chinese by default; switch to English any time in Settings.
- DSH's raw `stdout`/`stderr` and npm's output are third-party program output — they appear verbatim in the log (never rewritten).

## Default port

- The default port is **3080**.
- You can change it in Settings; startup command, health polling, and the embedded WebView URL all follow the configured value.
- Example URLs/commands: `http://127.0.0.1:3080`, `dsh web --port 3080`.
- Before each launch the app checks whether the configured port is free. If it is occupied (possibly by an already-running DSH, possibly by another program) the app **never force-kills** it; it shows a panel offering *Connect to existing service*, *Change port*, and *Re-check*. If DSH's own output announces a different actual address (e.g. `dsh web: http://127.0.0.1:3080`), that real address wins for embedding.

## Update DSH

Click **⤓ 更新 DSH (Update)** → confirm (exact command shown) → the service stops → `cmd /C "<npm>" install -g @deepseek-ai/dsh@latest` runs with output streamed to the log (source tag `update`) → success restarts DSH automatically. Buttons are disabled during the update. Use **检测全局包名** (`npm list -g --depth=0`) to confirm the package name. Version display: local `--version` vs latest `npm view`.

## Logs

Two log files, both under the user profile (no admin rights):

| Log | Path | Contents |
|---|---|---|
| Desktop log | `%APPDATA%\com.dsh.desktop\desktop.log` | launcher/update/setup events + mirror of DSH stdout/stderr |
| DSH output log | `%USERPROFILE%\.dsh\logs\dsh.log` | raw DSH stdout/stderr lines (follows the configured home dir: `<home>\logs\dsh.log`) |

Files rotate to `*.old` past 5 MB. In the UI:

- **日志 (Log)** button — live log dialog (autoscroll, clear).
- **打开日志目录** — opens the desktop-log folder in Explorer.
- **复制错误信息** — copies the current error line to the clipboard (shown on error/port-busy states).
- **复制日志** — copies the whole visible log text.

DSH stdout/stderr are never hidden: they stream live to the log dialog and to both files.

## Troubleshooting

- **未找到 Node.js (Node.js not found)** — install Node.js LTS from <https://nodejs.org>, reopen Settings → 自动检测, or browse to `node.exe`'s directory manually.
- **未找到 npm** — usually fixed by installing Node.js; npm.cmd sits in the same directory as node.exe (e.g. `C:\Program Files\nodejs\npm.cmd`).
- **未找到 DSH** — run `npm install -g @deepseek-ai/dsh` (see wizard), or point Settings to the existing `dsh.cmd` (typically `%APPDATA%\npm\dsh.cmd`).
- **端口被占用 (Port busy)** — choose Connect to existing service (if it's another DSH instance), change the port in Settings, or handle the occupying process yourself in Task Manager. This app never kills unknown processes.
- **DSH 启动超时 (Start timeout)** — cold starts can be slow; raise the timeout in Settings or set it to `0` (wait indefinitely while the process is alive).
- **DSH 启动后立即退出 (Exits immediately)** — see the red error line (last stderr) and full log output; common causes: wrong home dir, broken global npm install, port conflicts inside DSH config.
- **配置路径无效 (Invalid path)** — the error names the exact path; fix it in Settings (auto-detect usually repairs it).
- **Closed the window but it's still running** — X hides to tray by default; use tray → Exit to quit. Change this in Settings ("点击窗口 X 时").
- **Tray icon doesn't reopen the window** — fixed pattern already implemented (show/unminimize/set-focus on main thread + WebView repaint nudge); if you still hit it, report with the desktop.log attached.
- **Update failed** — check the npm output in the log; typically network issues or global-directory permissions (this app never requests admin).
- **WebView2 missing** — the NSIS/MSI installers guide you through installing the WebView2 runtime (usually preinstalled with Edge).

## FAQ

**Does this bundle Node.js or DSH?**
No. Nothing is bundled or embedded, and no portable runtime is deployed. The app only uses whatever Node/npm/DSH you have installed (or helps you install official builds at runtime).

**Where is my configuration stored?**
`%APPDATA%\com.dsh.desktop\config.json` — per-user, no admin rights required, never inside Program Files.

**Which port does it use?**
3080 by default; changeable in Settings (1–65535, validated). See [Default port](#default-port).

**Does it manage DSH processes it didn't start?**
No. It only stops the DSH tree it launched itself (by PID + Job Object). External services can be *connected to* read-only; quitting leaves them running.

**Does closing the window quit the app?**
By default no — it hides to the tray. You can switch the close button to "quit program" in Settings.

**Why does autostart wait 12 seconds?**
Right after login, disk IO spikes and Node/network may not be ready; the delay avoids most timeout failures. Cancel it anytime by clicking Stop during the wait.

## License

Released under the MIT License (choose otherwise per your repository needs — adding a `LICENSE` file overrides this note).
