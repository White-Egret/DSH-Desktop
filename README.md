# DSH Desktop

A lightweight Windows 10/11 desktop app (Tauri v2 + Rust + vanilla HTML/CSS/JS) for launching, managing, and updating a locally npm-installed DSH (DeepSeek Harness) Web service.

> **Important — what this app does NOT bundle**
>
> - This desktop does **not** bundle Node.js.
> - This desktop does **not** bundle DSH.
> - It does not ship or deploy any portable Node/DSH runtime either.
> - Users must install Node.js and DSH from choice (the first-run wizard can guide you through this; see [Prerequisites](#prerequisites)).

## Overview

DSH Desktop wraps the locally installed `dsh` CLI into a native window:

- One single window: a thin toolbar on top, and the DSH web UI embedded below it once the service is ready. The default browser is never opened (the launch command always includes `--no-open`).
- The app starts DSH with **your** configured paths and port, waits until the HTTP service is actually ready (TCP + HTTP polling), then embeds the page. If DSH exits before becoming ready, you get the last stderr line immediately instead of an infinite wait.
- Process ownership is strict: the app only manages the DSH process tree it started itself (`taskkill /PID <pid> /T /F` plus a Windows Job Object). It never uses image-name kills like `taskkill /IM node.exe /F`, so other Node programs on your machine are safe.

## Features

- Single-window design with embedded DSH web UI (multi-webview), auto-resizing with the window
- Configurable port (default **3080**), paths auto-detected with manual override in Preferences
- Port-in-use protection: prompts instead of killing unknown processes — connect to the existing service, change port, or re-check
- Actual-address detection: if DSH prints its real listen URL (e.g. `dsh web: http://127.0.0.1:3080`), the app loads that address preferentially
- **next-channel browser-session token**: DSH `next` (0.1.2+) protects the Web page with a per-process launch token — it prints e.g. `dsh web: http://127.0.0.1:3080?token=...`, and a bare URL gets `401 authentication required`. The launcher keeps the full printed address (including `?token=...`) when embedding the page; the first authenticated load makes DSH mint a signed cookie (HttpOnly, 30 days by default) that keeps refreshes and subsequent launches working. The last loaded page address is remembered too (`last_url` in config.json, shape/port-validated before every use), so "connect to existing service" and page re-open also carry the token
- File logging: Desktop log at `%APPDATA%\com.dsh.desktop\desktop.log`, DSH output log at `%USERPROFILE%\.dsh\logs\dsh.log`; UI buttons to open the log folder and copy errors/log text
- Refresh Page: reloads only the embedded DSH page without restarting the backend service (`F5` / `Ctrl+R`)
- One-click update of DSH via npm, live output streaming; choose the `latest` **or** `next` channel in the confirmation dialog (upgrade *and* downgrade are offered regardless of what is installed), with a backup reminder for the DSH home dir
- Version check against both npm dist-tags (`npm view <pkg> dist-tags`); the toolbar flags "update available" when your install trails the newest channel
- Close-to-tray or quit-on-close behavior (configurable); tray menu with Show Main Window / Start with Windows / Exit; tray restore does show + unminimize + set_focus
- Start with Windows (official autostart plugin, HKCU registry only, no admin rights); autostart runs silently in tray and delays DSH launch by 12 s to avoid the boot-time IO spike
- First-run setup wizard: detects Node.js/npm/DSH and can guide installation (official nodejs.org LTS installer download or `npm install -g @deepseek-ai/dsh`) — fully skippable
- **Bilingual UI (Chinese / English)**: choose a language in Preferences; the whole launcher (toolbar, status, dialogs, logs, tray menu) switches, and DSH's own web UI follows via its `settings.yaml`
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

> **Installer integrity**: before `msiexec` is ever invoked, the downloaded installer is matched against the SHA-256 digest listed in Node.js's official `SHASUMS256.txt` for that exact version, and it lands in a one-shot randomly-named private temp directory that is deleted afterwards (the old predictable `%TEMP%\node-vX.Y.Z-x64.msi` path could be pre-created as a symlink or swapped by another process). If the manifest can't be fetched, has no entry for the file, or the digest differs, the install aborts — there is deliberately **no "install anyway" fallback**; use the manual download link instead.

## Installation

If you'd rather not build from source, you can download a ready-made **Windows 10/11 installer** directly from:

- <https://github.com/White-Egret/DSH-Desktop/releases>

or

- <https://tfevx3uq.qwenwork.host/DSH-Desktop-windows-nsis>

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
4. Toolbar right side always shows: status dot · current port · version info (with an "update available" badge when a newer `latest` / `next` exists — see [Version check](#version-check-latest-vs-next)).
5. Closing the window hides to tray by default (configurable); use the tray icon or menu to bring it back; tray menu **退出 (Exit)** truly quits and stops the DSH process tree this app started.

Example commands the app effectively runs (using your configured values):

```text
Default DSH web URL:      http://127.0.0.1:3080
Example DSH start command: dsh web --port 3080
Example global install:    npm install -g @deepseek-ai/dsh
```

## Configuration

Config file: `%APPDATA%\com.dsh.desktop\config.json` (per-user; never written to Program Files or the install directory).

Open **⚙ 首选项 (Preferences)** from the toolbar. All fields support auto-detection: leave them empty/broken and the app finds Node.js, npm, and dsh automatically (`where` lookup + common install directories such as `%ProgramFiles%\nodejs` and `%APPDATA%\npm`). Detected results fill the form automatically.

| Setting | Default | Notes |
|---|---|---|
| npm program path | empty → auto-detected | `npm.cmd` / `npm.exe`; used for update / version queries |
| dsh path | empty → auto-detected | `dsh.cmd` / `dsh.exe` / `dsh.bat`; used to launch DSH |
| DSH home dir | `%USERPROFILE%\.dsh` | passed to DSH as `DSH_HOME`; process cwd is its parent; not your workspace |
| Port | `3080` | must be 1–65535; validated on save; takes effect on next DSH start |
| Ready timeout | `300` seconds | cold start can take minutes; **0 = wait forever** (as long as the process lives) |
| When clicking X | hide to tray | or "quit program" (stops the DSH process started by this session) |
| Extra args | empty | appended after `dsh web --port N --no-open`; plain flags only (see below) |
| Package name | `@deepseek-ai/dsh` | used for the `dist-tags` version query **and** to build the update command |
| Start with Windows | off | immediate effect, `HKCU\...\Run`, also toggleable from the tray menu |
| Interface language | `zh` (中文) | `zh` / `en`; switches the whole launcher and syncs DSH's `settings.yaml` — see [Language](#language) |

> There is deliberately **no "update args" setting any more**: the update command is always `npm install -g <package name>@<channel>`, and the channel (`latest` / `next`) is chosen in the **⤓ 更新 DSH** dialog itself — see [Update DSH](#update-dsh). Extra npm knobs (registry, proxy) belong in an `.npmrc` next to the DSH home dir.

### Argument & path policy (enforced on save *and* at every use)

These fields are capabilities, not text: `dsh_path` / `npm_path` are **executed**; `extra_args` / `package_name` become command-line arguments; and the **parent of the DSH home dir is the DSH process's working directory** (also where `npm` reads a `./.npmrc`). Because `config.json` is plain per-user JSON that autostart executes silently, validation runs on every use — not only when you press Save.

- **Arguments**: shell metacharacters (`& | < > ^ % !` and quotes) are rejected, because `cmd.exe` re-parses the whole command line and would treat `a&calc.exe` as two commands. Ordinary flags (`--host=127.0.0.1`, `--port`, `--no-open`, paths) are unaffected.
- **All paths**: absolute and drive-qualified only; `\\server\share` (UNC) rejected — writing there leaks credentials via NTLM auth, and executing from there trusts whatever answers the share; `..` segments rejected outright rather than folded away, so `C:\Windows\..\x` cannot be laundered into a valid path.
- **Program paths**: extension must be `.exe` / `.cmd` / `.bat` (a `.ps1` or extension-less name would resolve through unpredictable file associations), must exist at launch time, and must not live under `%TEMP%`.
- **Execution**: `.exe` is launched through `CreateProcess` directly — `cmd.exe` is nowhere on that path. `.cmd` / `.bat` shims *must* go through `cmd.exe`, so the launcher composes and quotes that command line itself (one pair of quotes per token) rather than relying on the standard library's automatic argument escaping, which `cmd.exe` then re-parses.
- **Home dir**: not a drive root, not the user profile folder itself or any parent of it, and not inside `Windows` / `Program Files` / `ProgramData`. (Node.js itself may still legitimately live in `Program Files` — that restriction applies to the home dir only.)
- Paths are normalized and written back, so the file on disk keeps the vetted form.

If a check fails the app refuses to start or update and shows the reason in the status area; a hand-edited or tampered `config.json` cannot turn "press Start" into running some other binary.

### Webview isolation & CSP

The DSH web UI is untrusted content (it renders model output), yet it is shown *inside* the main window as a second native webview labelled `dsh` — directly under the toolbar. Two independent mechanisms keep it away from the privileged surface:

- **The capability is scoped by `webviews`, not `windows`.** In Tauri v2 a matching `windows` pattern enables a capability on **every webview inside that window** — so `windows: ["main"]` would silently grant `core:*` to the embedded DSH page too. `capabilities/default.json` therefore lists `"webviews": ["main"]` (the launcher's own webview) and omits `windows`, per upstream's guidance for multiwebview windows.
- **Its origin is remote.** The embedded page loads from `http://127.0.0.1:<port>`, which Tauri classifies as a remote origin, and remote origins cannot reach `invoke_handler` commands unless a capability explicitly declares them under `remote.urls`. **Never add such a grant for the `dsh` webview** — that line, plus the scoping above, is what the isolation rests on.

The launcher document itself is served under a strict CSP (`script-src 'self'`, no `unsafe-inline`/`unsafe-eval`, `object-src 'none'`, `base-uri 'none'`, `form-action 'none'`, `frame-src 'none'` so no remote content can ever be pulled into the privileged document), with the asset protocol disabled and `freezePrototype` on (blocking prototype-pollution attacks against the injected IPC bridge). All untrusted strings — DSH output, error text, detected paths — are rendered with `textContent`, never as HTML. Tauri additionally injects nonces/hashes for its own bundled assets at compile time, so `script-src 'self'` keeps working without weakening.

> Tauri delivers this policy by injecting a `<meta http-equiv="Content-Security-Policy">` tag into the built HTML (`tauri_utils::html::create_csp_meta_tag`), **not** an HTTP header. Per the CSP spec, `frame-ancestors`, `sandbox` and `report-uri` are ignored in `<meta>`, so they are deliberately left out here — every directive configured above is one that actually takes effect. If a header-delivered policy is ever needed, `app.security.headers` is the mechanism.

## Language

The launcher UI is bilingual (Simplified Chinese / English).

- Pick **语言 / Language → English** in **⚙ Preferences** and save. The toolbar, status area, dialogs, tray menu and every launcher-generated log line switch to English; choosing **中文** switches everything back. A restart is not required (and after restarting Desktop everything stays in your chosen language, read from `config.json`).
- **DSH's own web interface**: on save, the app also writes the matching value into `<DSH home dir>\settings.yaml`:

  ```yaml
  locale:
    preference: en   # or: zh
  ```

  This is done as a minimal, targeted line edit — any other keys you keep in `settings.yaml` are preserved. DSH reads this file when it starts, so **restart DSH** (toolbar ⟳ Restart, or Stop + Start) for its interface language to change. If DSH is running when you change the language, the app logs a hint telling you a DSH restart is needed.
- The first-run setup wizard follows the same rule: it renders in Chinese by default; switch to English any time in Preferences.
- DSH's raw `stdout`/`stderr` and npm's output are third-party program output — they appear verbatim in the log (never rewritten).

## Default port

- The default port is **3080**.
- You can change it in Preferences; startup command, health polling, and the embedded WebView URL all follow the configured value.
- Example URLs/commands: `http://127.0.0.1:3080`, `dsh web --port 3080`.
- Before each launch the app checks whether the configured port is free. If it is occupied (possibly by an already-running DSH, possibly by another program) the app **never force-kills** it; it shows a panel offering *Connect to existing service*, *Change port*, and *Re-check*. If DSH's own output announces a different actual address (e.g. `dsh web: http://127.0.0.1:3080`), that real address wins for embedding.

## Version check: `latest` vs `next`

The app queries **both** npm dist-tags in one call:

```text
npm view @deepseek-ai/dsh dist-tags
→ { next: '0.2.0-rc.1', latest: '0.1.0', … }
```

`latest` is the **stable** release currently published for everyone; `next` is the **beta of the next version** — a *newer* number than `latest`, shipped for early testing. The dialog itself spells this out above the two options, so the meaning of the channels is never left implicit. The launcher compares your installed version against the **newest** of the two (i.e. `next` when `next` differs from `latest`, otherwise `latest`):

| Toolbar shows | Meaning |
|---|---|
| `0.2.0-rc.1 (up to date)` | you already have the newest version of either channel |
| `0.1.0 → 0.2.0-rc.1` + **next update** badge | something newer than your install exists (on the `next` channel) |
| `0.1.0 → 0.1.5` + **update available** badge | newer on `latest`; no separate `next` channel |
| `0.1.0 (update status unknown)` | local version read, registry query failed (reason in the log) |
| `unknown` | local version unreadable — no comparison possible |

Checking happens automatically on program start and whenever the DSH service (re)starts, with a 60 s cooldown so it can't stampede the registry — there is no manual "Check Version" button. The raw values for both channels are always written to the log.

## Update DSH

Click **⤓ 更新 DSH (Update)** → the dialog lists **both channels with their current version numbers** → pick one → confirm → the service stops → the install runs → success restarts DSH automatically.

**You may choose either channel regardless of what is installed right now.** Someone who wants to try the newer build picks `next`; someone who wants to fall back to the older stable release picks `latest`. The dialog never disables an option because of your current version — the only thing that decides the command is your selection:

| Channel | Command that runs |
|---|---|
| `latest` (stable release) | `"<npm>" install -g @deepseek-ai/dsh@latest` |
| `next` (beta of the next version, newer) | `"<npm>" install -g @deepseek-ai/dsh@next` |

- The dialog pre-selects the **newest** channel (the one the status bar pointed at), because that's the usual intent of an "Update" click. It deliberately does *not* pre-select the other one when you already have the newest: pre-choosing a downgrade you didn't ask for is worse than a harmless re-install. The other channel is one click away and labelled with its own version number and an "installed / newest / older" tag.
- Each argument is passed to npm as a separate token (see [Argument & path policy](#argument--path-policy-enforced-on-save-and-at-every-use)). The channel itself is **not** a configurable field: the backend accepts only the literal strings `latest` and `next` and rejects anything else, so the picker cannot be turned into an argument-injection vector. The concrete version is resolved by npm at install time, which is why you can still pick a channel while its version number is displayed as "unknown" (registry unreachable).
- **⚠ Back up the DSH home dir (`%USERPROFILE%\.dsh` by default) before either direction.** The dialog shows your *actual configured* path, and the reminder is repeated in the log when the run starts: a newer version may rewrite the config/session format, and rolling back to an older one can just as well fail to read what the newer one wrote.
- While installing, the **page shows live progress** — a "package files fetched / elapsed" counter plus scrolling npm output, also mirrored into the log (source tag `update`). Buttons are disabled during the update.
- Use **检测全局包名** (`npm list -g --depth=0`) to confirm the package name.

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

- **未找到 Node.js (Node.js not found)** — install Node.js LTS from <https://nodejs.org>, reopen Preferences → 自动检测, or browse to `node.exe`'s directory manually.
- **未找到 npm** — usually fixed by installing Node.js; npm.cmd sits in the same directory as node.exe (e.g. `C:\Program Files\nodejs\npm.cmd`).
- **未找到 DSH** — run `npm install -g @deepseek-ai/dsh` (see wizard), or point Preferences to the existing `dsh.cmd` (typically `%APPDATA%\npm\dsh.cmd`).
- **端口被占用 (Port busy)** — choose Connect to existing service (if it's another DSH instance), change the port in Preferences, or handle the occupying process yourself in Task Manager. This app never kills unknown processes.
- **DSH 启动超时 (Start timeout)** — cold starts can be slow; raise the timeout in Preferences or set it to `0` (wait indefinitely while the process is alive).
- **DSH 启动后立即退出 (Exits immediately)** — see the red error line (last stderr) and full log output; common causes: wrong home dir, broken global npm install, port conflicts inside DSH config.
- **配置路径无效 (Invalid path)** — the error names the exact path; fix it in Preferences (auto-detect usually repairs it).
- **Closed the window but it's still running** — X hides to tray by default; use tray → Exit to quit. Change this in Preferences ("点击窗口 X 时").
- **Tray icon doesn't reopen the window** — fixed pattern already implemented (show/unminimize/set-focus on main thread + WebView repaint nudge); if you still hit it, report with the desktop.log attached.
- **Update failed** — check the npm output in the log; typically network issues or global-directory permissions (this app never requests admin). If the install succeeded but DSH misbehaves, that usually means config written by a *different* version — restore the DSH home dir backup you took before switching channels, then retry.
- **WebView2 missing** — the NSIS/MSI installers guide you through installing the WebView2 runtime (usually preinstalled with Edge).

## FAQ

**Does this bundle Node.js or DSH?**
No. Nothing is bundled or embedded, and no portable runtime is deployed. The app only uses whatever Node/npm/DSH you have installed (or helps you install official builds at runtime).

**Where is my configuration stored?**
`%APPDATA%\com.dsh.desktop\config.json` — per-user, no admin rights required, never inside Program Files.

**What is the difference between `latest` and `next`, and which should I install?**
They are npm dist-tags, not versions. `latest` is the **stable** release — what a bare `npm install -g @deepseek-ai/dsh` targets. `next` is the **beta of the next version**: a newer number than `latest`, published for early testing, and it may contain breaking changes. The launcher reads both in one `npm view <pkg> dist-tags` call and, when the two differ, treats `next` as the newest version — so the toolbar reports an update unless what you have installed already *is* `next`. The dialog lets you install either channel no matter what you currently run, because trying the newer build and rolling back to the older stable release are both legitimate moves. Back up `%USERPROFILE%\.dsh` first either way. See [Update DSH](#update-dsh).

**Which port does it use?**
3080 by default; changeable in Preferences (1–65535, validated). See [Default port](#default-port).

**Does it manage DSH processes it didn't start?**
No. It only stops the DSH tree it launched itself (by PID + Job Object). External services can be *connected to* read-only; quitting leaves them running.

**Does closing the window quit the app?**
By default no — it hides to the tray. You can switch the close button to "quit program" in Preferences.

**Why does autostart wait 12 seconds?**
Right after login, disk IO spikes and Node/network may not be ready; the delay avoids most timeout failures. Cancel it anytime by clicking Stop during the wait.

## License

Released under the [MIT License](LICENSE).
