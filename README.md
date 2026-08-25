# DSH Desktop

A lightweight Windows 10 desktop app (Tauri v2) for launching, managing, and updating the locally npm-installed DSH (DeepSeek Harness) Web service.

- **Single-window design**: only one window for the whole workflow. A 43.2px toolbar on top (action buttons on the left, one-line status on the right: status dot · port · version), and the area below alternates between startup/error messages and the DSH page — once DSH is ready, its Web UI is **embedded directly below the toolbar** (no separate window, no default browser; the launch command always includes `--no-open`).
- **Refresh Page** (new): reloads only the embedded DSH web page. It never stops or restarts the DSH backend service.
- No terminal windows, no downloaded runtime, no bundled Node/DSH; does not read or modify your DSH user configuration.
- DSH's stdout/stderr are streamed live into the "Log" dialog (opened from the toolbar). Startup failures show the reason as a single-line message.
- Patient readiness waiting: DSH cold start (especially right after a reboot) may take a minute or two. Default wait is **300 seconds**, configurable to **0 = wait forever**; the elapsed seconds are shown while waiting.
- **Start with Windows** (official autostart plugin): one switch in Settings; when autostarting, the app starts **silently in the tray** — no window, and DSH is launched after a 12-second delay (to avoid the cold-start IO/network spike). Click the tray icon any time to show the window.
- **Tray resident**: clicking the window's X no longer quits the app — it hides to the tray. The tray context menu offers "Show Main Window / Start with Windows / Exit"; only a real quit ends the DSH process tree (`taskkill /PID <pid> /T /F` + Job Object, so even a crash leaves no orphan processes).
- One-click update: `npm install -g @deepseek-ai/dsh@latest`, live output, DSH restarts automatically on success.
- Single-instance lock: a second launch just focuses the existing window.

## 1. Getting the app (built automatically by GitHub Actions – no local Rust needed)

You do not need Rust, Visual Studio Build Tools, or the Tauri CLI locally — everything runs on GitHub's cloud:

1. **Create a repository**: GitHub → top-right `+` → *New repository* → e.g. `dsh-desktop` → *Create repository*.
2. **Upload the code** (any one of these):
   - Web upload: repo page → *uploading an existing file* → drag all files/folders of this project in (keep the relative path of `.github/workflows/build-windows.yml` unchanged) → *Commit changes*.
   - Command line:
     ```bash
     cd dsh-desktop
     git init
     git add .
     git commit -m "DSH Desktop"
     git branch -M main
     git remote add origin https://github.com/<your-username>/dsh-desktop.git
     git push -u origin main
     ```
   - GitHub Desktop: Add Existing Repository → Publish repository.
3. **Trigger the build**: pushing to `main` triggers it automatically; you can also go to the repo *Actions* tab → select *Build Windows* → *Run workflow*.
4. **Download the artifacts**: *Actions* → latest successful run → *Artifacts* section at the bottom:
   - `DSH-Desktop-windows-nsis` — NSIS installer (recommended, double-click to install)
   - `DSH-Desktop-windows-msi` — MSI installer
   - `DSH-Desktop-windows-portable-exe` — standalone portable exe
5. First build takes about 8–15 minutes (Rust compilation); later builds are faster thanks to caching.

> If Actions did not run automatically: check *Settings → Actions → General → Actions permissions*, and make sure `.github/workflows/build-windows.yml` is committed.

## 2. Usage

1. Install/run **DSH Desktop** (NSIS installer or portable exe). Default window: 1200×760.
2. On first start it tries to launch DSH automatically. Defaults fit this environment (change them in Settings if yours differs):

| Setting | Default | Notes |
|---|---|---|
| npm.cmd path | `D:\Programs\nodejs\npm.cmd` | used to update DSH / query latest version |
| dsh.cmd path | `C:\Users\admin\AppData\Roaming\npm\dsh.cmd` | used to launch DSH |
| DSH home dir | `%USERPROFILE%\.dsh` (e.g. `C:\Users\admin\.dsh`) | where DSH stores its config; passed to DSH via `DSH_HOME`; not the workspace |
| Port | `3080` | |
| Ready timeout | `300` seconds | DSH cold start may take a minute or two; **0 = wait forever** (as long as the process is alive) |
| Extra args | empty | appended after `dsh web --port N --no-open` |
| Update command | `install -g @deepseek-ai/dsh@latest` | appended after `cmd /C npm.cmd` |
| Start with Windows | off | the Settings switch takes effect immediately (`HKCU\...\Run`, no admin rights); also toggleable from the tray menu |

3. Startup flow: the toolbar area shows "Starting DSH, waited X seconds…" → once ready, the area seamlessly switches to the **embedded DSH page**. The page is never loaded before the service is ready.
4. If DSH crashes at startup: the message turns into a red error line (with DSH's last stderr line); full output is in "Log".
5. If port 3080 is occupied: the app never force-kills unknown processes. It shows a panel with **Connect to existing service** (does not take over the process) or **Change port**.

### The UI

```
┌────────────────────────────────────────────────────────────────┐
│ ▶Start ■Stop ⟳Restart ↻Refresh ⤓Update DSH Check Version Log ⚙Settings  ● Running · Port 3080 · Version 1.0.1 │ ← 43.2px gray toolbar
├────────────────────────────────────────────────────────────────┤
│                                                                │
│   Waiting: one line + spinner ("waited X s…")                  │
│   Failure: one red error line                                  │
│   Ready:   DSH page embedded here (fills the whole area)        │
│                                                                │
└────────────────────────────────────────────────────────────────┘
```

- The embedded page resizes automatically with the window.
- Opening the Settings / Log / Update confirmation dialogs temporarily hides the embedded page; closing them restores it.

### Refresh Page

- **Refresh Page** sits next to **Restart** in the toolbar and reloads only the embedded DSH web page — it does **not** stop or restart the DSH service.
- Use cases: wallpaper changed, theme/skin applied, front-end not updating, page glitches when you don't want to restart the whole DSH.
- While refreshing, a "Refreshing..." overlay is shown until the new page appears.
- The current address is preserved (`http://127.0.0.1:3080`, or whatever port you configured) — the startup flow is never re-run.
- Shortcuts: `F5` and `Ctrl + R` (works when the launcher UI has focus; inside the DSH page, WebView2 also handles F5/Ctrl+R natively).
- If DSH is not running, clicking Refresh shows: `DSH service is not running.`
- **Restart DSH** remains an independent button that fully restarts the backend service.

### Window & tray behavior

- **Clicking the window's X**: does not quit — it **hides to the system tray** (the app and DSH keep running).
- **Tray icon** (black whale):
  - Left click / double-click → show and focus the main window;
  - Right click → menu: "Show Main Window", "Start with Windows (checkable)", "Exit".
- **Exit** (tray menu) = stop the DSH process tree and quit the whole app; the Job Object cleans up even on crash.
- If connected to an "external process" (not started by this app), quitting does not touch it — the page connection is simply dropped.

### Start with Windows (official autostart plugin)

- The Settings switch is immediately effective, and can also be toggled from the tray right-click menu; both stay in sync.
- Registration: Windows registry `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` — no admin rights.
- Autostart experience:
  1. **Silent tray mode** — after login the window does not pop up or steal focus; it runs in the background and you can open it from the tray icon;
  2. **12-second delay** — right after boot, IO is high and Node/network may not be ready; the app waits 12 seconds before launching DSH. You can click "Stop" to cancel during the wait.

### Updating DSH

Click "Update DSH" in the toolbar → confirm (the exact command is shown) → the service stops → `cmd /C "D:\Programs\nodejs\npm.cmd" install -g @deepseek-ai/dsh@latest` runs with output streamed to the log (source tag `update`) → success/failure shown by exit code → on success DSH restarts automatically. Buttons are disabled during the update. You can confirm the package name with "Detect global package" (`npm list -g --depth=0`).

## 3. Acceptance checklist

| # | Item | Result |
|---|---|---|
| 1 | Double-click opens no terminal | release uses `windows_subsystem = "windows"` + `CREATE_NO_WINDOW` |
| 2 | One window, no browser | DSH page embedded in the main window (multi-Webview), launch always includes `--no-open` |
| 3 | Auto start | double-click runs `cmd /C dsh.cmd web --port 3080 --no-open` |
| 4 | Wait before ready, load after ready | Rust TCP+HTTP polling; the embedded Webview is only created once ready; default 300 s, 0 = infinite |
| 5 | Crash reports immediately | polling also watches `try_wait`; exit stops and shows the last error line |
| 6/7 | No orphan processes on close | `taskkill /PID <pid> /T /F` + Job Object (KILL_ON_JOB_CLOSE) |
| 8 | Reopen works | config persisted at `%APPDATA%\com.dsh.launcher\config.json` |
| 9/10 | Update & restart | see "Updating DSH" |
| 11 | No bundled runtime | only uses local npm/dsh |
| 12 | No local build needed | GitHub Actions windows-latest fully automated |
| 13 | Autostart + tray | official autostart plugin; silent tray on autostart + 12 s delay; X = tray, tray menu = exit |
| 14 | Refresh Page | reloads the web UI only; DSH backend keeps running; keeps current port/URL |

## 4. Troubleshooting

- **Cannot find dsh.cmd / npm.cmd**: Settings → Browse to the correct paths (dsh.cmd is usually in `%APPDATA%\npm\`, npm.cmd in the Node.js install directory).
- **DSH exits right after start**: check DSH's native errors in "Log"; a common cause is a wrong DSH home dir.
- **Wait timeout stops it**: if cold start is really slow, raise the timeout in Settings or set it to `0` (wait forever).
- **Port occupied**: check whether a DSH is already running (choose "Connect to existing service"), or handle it in Task Manager and change the port/restart.
- **Update failed**: check the full npm output in the log; usually network issues or global directory permissions (this app does not ask for admin).
- **WebView2 missing**: the NSIS/MSI installers guide you to install the WebView2 runtime (usually already present with Edge on Win10).
- **You closed the window but the app is still running**: X only hides to the tray; use the tray menu "Exit" to truly quit.
- **Autostart not working**: check the Settings switch (or the tray menu checkmark); verify with `reg query "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v "DSH Desktop"`; note that the portable exe must be registered again if you move it.

## 5. What this app does not do

- Does not read/modify your DSH user config directory (`~/.dsh` is only passed to DSH as `DSH_HOME`)
- Does not print or upload API keys / credentials / environment variables
- Does not force-kill unknown processes occupying the port; never uses `taskkill /IM node.exe`
- Does not bundle Node.js / DSH / a portable runtime
