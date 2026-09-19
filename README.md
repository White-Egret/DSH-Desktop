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
- **Safe Mode**: start DSH from a separate, pristine home (`%USERPROFILE%\.dsh-safe`) on port 3081 to inspect and repair the daily environment; borrows only the `.credentials.yaml` file (its content never leaves the Rust side), optionally archives the previous safe home instead of deleting it ("reset baseline on entry", off by default), and verifies the repair after switching back — see [Safe Mode](#safe-mode)
- Version check against both npm dist-tags (`npm view <pkg> dist-tags`); the toolbar flags "update available" when your install trails the newest channel
- Close-to-tray or quit-on-close behavior (configurable); tray menu with Show Main Window / Start with Windows / Exit; tray restore does show + unminimize + set_focus
- Start with Windows (official autostart plugin, HKCU registry only, no admin rights); autostart runs silently in tray and delays DSH launch by 12 s to avoid the boot-time IO spike
- First-run setup wizard: detects Node.js/npm/DSH and can guide installation (official nodejs.org LTS installer download or `npm install -g @deepseek-ai/dsh`), with a **choosable Node.js / DSH install location** (e.g. onto another drive) — fully skippable
- **Node.js minimum-version guard (22.19.0)**: detects an installed-but-too-old Node.js, warns with the exact versions, offers a one-click upgrade to the official LTS (same verified installer flow), keeps a "download it myself" link and a "keep this version and continue" escape hatch, and refuses to start DSH with a plain-language reason instead of letting it die on an opaque error — see [Node.js version check](#nodejs-version-check)
- **Bilingual UI (Chinese / English)**: choose a language in Preferences; the whole launcher (toolbar, status, dialogs, logs, tray menu) switches, and DSH's own web UI follows via its `settings.yaml`
- **Light / Dark / Follow-system appearance**: pick it in Preferences; the launcher (toolbar, dialogs, wizard, native title bar) and the embedded DSH page switch together through `ui-theme.preference` in DSH's `settings.yaml` — open DSH pages follow **live, no DSH restart needed**
- **Window layout memory**: the main window's size, position and maximized state are remembered and restored on the next launch (official `tauri-plugin-window-state`); the plugin validates coordinates against the monitors that are actually attached, so a window can never come back off-screen after a monitor is unplugged — see [Window layout](#window-layout)
- **Toolbar display mode (pinned / auto-hide)**: keep the toolbar always visible, or let it tuck away above the window and slide down when the mouse reaches the top 8px strip; leaving the toolbar hides it again half a second later (in auto mode the content area fills the whole window and the toolbar floats above it). `Ctrl+Shift+H` toggles it. **Safe Mode always forces pinned** — see [Toolbar display mode](#toolbar-display-mode)
- Single-instance lock: launching a second copy just focuses the existing window

## Requirements

| Item | Requirement |
|---|---|
| OS | Windows 10 or 11, x64 |
| WebView2 Runtime | Usually preinstalled with Edge on Windows 10/11; installers bootstrap it if missing |
| Node.js | **22.19.0 or newer** (DSH's runtime floor). **Not bundled.** |
| DSH | Installed globally via npm. **Not bundled.** |

## Prerequisites

You need both of these before DSH Desktop can start a service:

1. **Node.js** (with npm) — install the official LTS from <https://nodejs.org/en/download>. **22.19.0 or newer is required** (see [Node.js version check](#nodejs-version-check)).
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

### Choosing where Node.js gets installed

The "Node.js is missing" step has an **install location** field, **pre-filled with this machine's official default** (`%ProgramFiles%\nodejs`, normally `C:\Program Files\nodejs` — and it follows `Program Files` onto another drive if Windows was installed there, because the backend derives it from the environment rather than hard-coding a string). To install elsewhere, edit it to e.g. `D:\nodejs`, or use Browse; clearing it falls back to the official default. The path is handed to the official MSI as `INSTALLDIR=<path>` — the MSI's own directory page is bound to that public property (read straight out of this machine's cached MSI: `WIXUI_INSTALLDIR = INSTALLDIR`), but the app runs it with `/passive`, which never draws that page, so this is the only place the question can be asked.

Worth knowing:

- **What you typed is never overwritten.** The field starts at the default path, but once you edit it (or pick a folder with Browse), a later "Re-check" will not put the default back.

- **It is still a per-machine install.** The official MSI is `ALLUSERS=1`, so installing onto `D:` still goes through UAC — moving the folder does not change the permission story.
- **First-time installs only.** The "Node.js too old → upgrade" path deliberately does **not** offer a directory change: switching `INSTALLDIR` during an upgrade of the same ProductCode leaves the old directory and the old PATH entry behind. To relocate an existing Node.js, uninstall it first and install again.
- **The result is verified.** MSI silently ignores properties it does not recognise (and still exits 0), so after installing the app confirms that `node.exe` really is in the directory you asked for; if it is not, it reports that honestly instead of showing a success message.
- **The path is validated** — absolute, no `..`, drive must exist, no `< > " | ? *`, not inside `Windows` / `ProgramData`, not the `Program Files` root itself, 200-character limit. Anything else is refused up front, **before** the 30 MB download starts.

### Choosing where DSH gets installed

The "DSH is missing" step has the same kind of **install location** field, **pre-filled with npm's global directory** (the app asks `npm config get prefix` once and only falls back to `%APPDATA%\npm` if that fails; normally `C:\Users\<you>\AppData\Roaming\npm` — i.e. where DSH already lives on this machine). To install onto another drive, edit it to e.g. `D:\dsh`, or use Browse; clearing it falls back to npm's own default.

The value is handed to npm as `--prefix <dir>`. Note this is **not** a per-machine install like Node.js: it needs no administrator rights (the folder just has to be writable by you). The trade-off is that npm's global directory is shared by *all* global packages — later `npm install -g <something-else>` in a terminal still goes to npm's default directory; the two coexist and do not interfere.

Worth knowing:

- **The folder is added to your user PATH.** After a successful install the app appends it to `HKCU\Environment`'s `Path` (kept as `REG_EXPAND_SZ`, system PATH untouched, no admin rights) so `dsh` also works in a terminal and the app keeps detecting it. When the chosen folder *is* npm's default one, nothing is touched at all. If the write fails (rare — e.g. the registry is locked down by policy) that is reported honestly rather than glossed over; the app itself starts DSH by its full path and is unaffected.
- **The result is verified.** npm's exit code 0 only means npm *thinks* it succeeded, so after installing the app confirms `dsh.cmd` really is in the directory you asked for; if it is not, it reports the error together with the location it actually detected.
- **Updates go back to the same place.** "Update DSH" no longer always targets the default directory: it derives the directory from where `dsh.cmd` actually lives and passes that as `--prefix` — otherwise a second copy of DSH would be left behind and "which one is detected" would become a matter of luck.
- **What you typed is never overwritten.** Same rule as the Node.js field: once you edit it (or pick a folder with Browse), "Re-check" will not put the default back.
- **The path is validated** — absolute, no `..`, drive must exist, no `< > " | ? *`, not inside `Windows` / `ProgramData`, not the `Program Files` root itself, 200-character limit — and it must not contain `%` or `!`, because the value travels through `cmd.exe` to `npm.cmd`, which expands `%VAR%` even inside double quotes and treats `!` specially once delayed expansion is on. Those cannot be escaped reliably, so they are refused.
- **The folder is created for you.** npm fails a non-existent `--prefix` outright (`ENOENT … lstat`) — unlike the MSI it will not create the prefix itself — so the app creates the directory first and reports a clear error if it cannot.

> **Installer integrity**: before `msiexec` is ever invoked, the downloaded installer is matched against the SHA-256 digest listed in Node.js's official `SHASUMS256.txt` for that exact version, and it lands in a one-shot randomly-named private temp directory that is deleted afterwards (the old predictable `%TEMP%\node-vX.Y.Z-x64.msi` path could be pre-created as a symlink or swapped by another process). If the manifest can't be fetched, has no entry for the file, or the digest differs, the install aborts — there is deliberately **no "install anyway" fallback**; use the manual download link instead.
>
> **Where that check's trust actually sits** (security review M-3). The manifest and the installer are fetched over the same channel, so the integrity check reduces to the transport. Two things are therefore pinned: downloads are restricted to `https://nodejs.org/dist/`, and `curl` is invoked with `--proto '=https' --proto-redir '=https'` so an `https → http` downgrade on a redirect is refused (curl allows that by default — a proxy that rewrites the redirect could otherwise pull the manifest *and* the rest of the transfer in the clear, and the digests would still agree). Beyond that the trust root is TLS plus **this machine's certificate store**: Windows' bundled `curl.exe` speaks Schannel, so it will honour any root certificate an enterprise policy or a piece of local malware has installed, and in such an environment an attacker can swap manifest and MSI together. That is an environmental limit the launcher cannot close by itself — the real fix is shipping a pinned expected hash with the app, which is not there today.
>
> The PowerShell fallback (used when `curl.exe` is missing) passes its script with `-EncodedCommand` as a Base64(UTF-16LE) payload, so no URL, version or path text is ever interpreted as PowerShell syntax. The one-shot temp directory name is drawn from the OS CSPRNG (`BCryptGenRandom`).

## Node.js version check

DSH's runtime floor is **Node.js 22.19.0**: `node:sqlite` (used by the SQLite session store) lost its experimental flag at 22.13, native TypeScript type-stripping became the default at 22.18, and one DSH dependency (`@earendil-works/pi-ai`) declares `engines.node >=22.19.0`. Because the published `@deepseek-ai/dsh` package does **not** declare `engines`, npm will happily install it on an older Node — the failure only shows up at runtime (on Node 21.7.3, for example, DSH does not start at all).

The launcher therefore checks the detected `node --version` against that floor and turns the cryptic failure into a decision:

| Where | What happens |
|---|---|
| First-run wizard | The Node.js row shows **⚠ version too old** with `21.7.3 → v22.19.0+`, plus a warning panel offering **Download & install the latest LTS** (the same SHA-256-verified official MSI flow as the missing-Node step), **Download it myself from nodejs.org**, **Re-check**, and **Keep this version and continue**. |
| Pressing Start | The launch is refused with an explicit reason ("detected 21.7.3, DSH requires v22.19.0 or newer") instead of letting DSH exit with an opaque error. The message says how to fix it. |
| Preferences | A **Node.js version** row shows the state (`✔ OK` / `✘ too old` / `? cannot tell`), the **Download & install official LTS** button runs the guided install any time (not only on first run), and the **Keep this version and start anyway** checkbox records your decision. |

- **"Keep this version and continue"** is written to `node_min_ack` in `config.json` (one key, read-modify-write — it never rewrites your other settings) and disables the Start check. It records *the floor you accepted*: if a future version of this app raises the requirement, the check comes back rather than silently staying off. Unchecking the box in Preferences restores the check immediately.
- **The version string is never guessed.** A missing Node, an unreadable `node --version`, or a version that cannot be parsed is reported as `? cannot tell` — a warning, but never a blocked launch, because a version string we cannot read is not evidence that the runtime is broken.
- **Upgrading an existing Node.js is safe for DSH.** The official MSI installs over the current Node.js in the same directory (PATH unchanged), and DSH's global command lives in the user's npm prefix — so the guided upgrade does not require reinstalling DSH. If the version still looks stale right after the installer exits, restart the app (PATH may only refresh after a restart) and use **Re-check**.

## Installation

If you'd rather not build from source, you can download a ready-made **Windows 10/11 installer** (DSH-Desktop-windows-nsis.zip) directly from:

- <https://github.com/White-Egret/DSH-Desktop/releases>

or

- <https://tfevx3uq.qwenwork.host/DSH-Desktop-windows-nsis>

No local Rust toolchain needed — GitHub Actions builds the installers for you (see next section). Grab the artifacts of the latest successful build:

- `DSH-Desktop-windows-nsis` → contains `DSH Desktop_<version>_x64-setup.exe` — NSIS installer (recommended)
- `DSH-Desktop-windows-msi` → MSI installer
- `DSH-Desktop-windows-portable-exe` → standalone portable exe (same features except the NSIS first-run moment)

Install with the NSIS setup exe, or just run the portable exe. On first launch the environment check wizard appears once (it disappears permanently once `%APPDATA%\com.dsh.desktop\config.json` exists).

**Verify what you downloaded.** Every build publishes a `SHA256SUMS.txt` — attached to the release when the maintainer put it there — **and** records a signed build-provenance attestation against the artifacts of that run. Both come from the build itself, not from whoever hands you the file; that is the whole point: get the bytes from anywhere, check them against a source the file's host does not control. The same checksums are printed at the end of every build log (the "生成 SHA-256 校验清单" step), which is the quickest place to read them without downloading anything.

```powershell
# 1) Hash check: compare against the line for this file in SHA256SUMS.txt (case-insensitive)
Get-FileHash .\DSH-Desktop-windows-nsis.zip -Algorithm SHA256

# 2) Stronger: prove the binary really came out of this repo's build-windows.yml
#    (needs GitHub CLI; run it on the .exe inside the zip)
gh attestation verify ".\DSH Desktop_<version>_x64-setup.exe" --repo White-Egret/DSH-Desktop   # 去掉尖括号
```

Option 2 is a Sigstore-signed statement binding the file's hash to this repository, the workflow file and the exact commit that built it, so there is no string to trust in advance. It applies to the installer or portable `.exe` (`gh` verifies files, not archives — unzip first) and lives with the workflow run rather than on the release page.

Both checks above work on files from the mirror as well, and that is exactly what makes that mirror safe to keep using: it stores byte-identical copies of the same build output. `gh` looks the attestation up by the file's digest, so it does not care where you got the file from — and asking the mirror for *its own* checksum would prove nothing, since a swapped file and a swapped checksum travel together.

> These artifacts are **not code-signed**, so Windows SmartScreen warns about an unknown publisher on first run (and some managed corporate machines may refuse unsigned executables outright). Why this is not simply "buy a certificate": code signing proves *who* published the file, which a hash cannot; EV certificates lost their instant SmartScreen bypass in 2024, so every option — including paid ones — starts with zero reputation and earns it release by release; and the cheap managed service (Azure Artifact Signing, ~$9.99/month) is not open to individual developers outside the USA/Canada. The free route for a project like this one is [SignPath Foundation](https://signpath.io) (code signing for qualifying open-source projects), which is under evaluation. Until then, the hash and the attestation above are what let you confirm the file is the one this repo built.

**Where it installs.** The NSIS installer defaults to **`C:\Users\<you>\DSH Desktop`** — a per-user install (no admin rights) — and the directory page lets you pick any other folder before installing. Upgrades go to the existing location: the installer reads the previous path from the registry and keeps it, so an updated copy does not appear under the new default. The MSI package is a separate, per-machine installer and still defaults to `C:\Program Files\DSH Desktop`.

> That default comes from a vendored copy of Tauri's NSIS template (`src-tauri/nsis/installer.nsi`, wired up as `bundle.windows.nsis.template`), because Tauri has no config option for the default install directory ([tauri-apps/tauri#11015](https://github.com/tauri-apps/tauri/issues/11015)) and NSIS fixes it in `.onInit`, before any installer hook could override it. The file is **generated, not hand-edited**: `node scripts/gen-nsis-template.mjs` fetches the template matching the **pinned** `@tauri-apps/cli` version from `package-lock.json` and applies that single change. Rerun it after bumping the CLI so the template cannot silently drift from the bundler.

## Build from source

Requirements: Node.js 18+ (or 20 LTS), Rust stable (MSVC toolchain), Visual Studio Build Tools, WebView2.

```bash
npm ci          # installs @tauri-apps/cli
npm run tauri build
```

Artifacts land in `src-tauri/target/release/bundle/nsis/`, `.../msi/`, and the raw exe in `src-tauri/target/release/`.

> The NSIS installer's default install directory lives in `src-tauri/nsis/installer.nsi` (see [Installation](#installation)). If you bump `@tauri-apps/cli`, regenerate it with `node scripts/gen-nsis-template.mjs` and commit the result.

## GitHub Actions build

`.github/workflows/build-windows.yml` builds automatically on push to `main`/`master`, on tags `v*`, on PRs, and via manual dispatch. It produces the artifacts listed under [Installation](#installation). First build takes roughly 8–15 minutes (Rust compile); later builds are faster thanks to caching.

> If Actions didn't trigger: check *Settings → Actions → General → Actions permissions* and make sure `.github/workflows/build-windows.yml` is committed.

## Usage

1. Install/start **DSH Desktop**. Default window is 1376×774.
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
| npm cache location | empty (npm config untouched) | written into npm's own `~/.npmrc` (the `cache` line), so terminal npm follows it too — see *npm cache location* |
| dsh path | empty → auto-detected | `dsh.cmd` / `dsh.exe` / `dsh.bat`; used to launch DSH |
| DSH home dir | `%USERPROFILE%\.dsh` | passed to DSH as `DSH_HOME`; process cwd is its parent; not your workspace |
| Port | `3080` | must be 1–65535; validated on save; takes effect on next DSH start |
| Ready timeout | `300` seconds | cold start can take minutes; **0 = wait forever** (as long as the process lives) |
| When clicking X | hide to tray | or "quit program" (stops the DSH process started by this session) |
| Extra args | empty | appended after `dsh web --port N --no-open`; plain flags only (see below) |
| Package name | `@deepseek-ai/dsh` | used for the `dist-tags` version query **and** to build the update command |
| Start with Windows | off | immediate effect, `HKCU\...\Run`, also toggleable from the tray menu |
| Interface language | `zh` (中文) | `zh` / `en`; switches the whole launcher and syncs DSH's `settings.yaml` — see [Language](#language) |
| Appearance | `system` (follow system) | `light` / `dark` / `system`; switches the launcher and syncs DSH's `ui-theme.preference` — see [Appearance](#appearance) |

> There is deliberately **no "update args" setting any more**: the update command is always `npm install -g <package name>@<channel>` (plus a `--prefix` derived from where `dsh.cmd` actually lives, when DSH is installed outside npm's default directory — see *Choosing where DSH gets installed*), and the channel (`latest` / `next`) is chosen in the **⤓ 更新 DSH** dialog itself — see [Update DSH](#update-dsh). Extra npm knobs (registry, proxy) belong in an `.npmrc` next to the DSH home dir.

### npm cache location

On Windows npm keeps its cache in **`%LOCALAPPDATA%\npm-cache`** (`C:\Users\<you>\AppData\Local\npm-cache`; `~/.npm` on Linux/macOS). That is a *different* place from the global directory (`%APPDATA%\npm`) that the DSH install-location option moves, so the cache gets its own field here: fill it in to move the cache to another drive, clear it to move back. Next to the field the app shows two things — **the value in npm's config** and **the value actually in effect** (they differ when an environment variable or a project-level `.npmrc` overrides it, and that case is flagged with ⚠).

This one setting is different in kind from the rest: **it edits npm's own file.**

- **It writes the `cache` line in `~/.npmrc`** (the path comes from `npm config get userconfig`, not a guess), so terminal npm uses the same location. On top of that, every npm the app runs itself (install/update DSH, package list, version query) passes `--cache <dir>` explicitly, so the command in the log behaves identically when copy-pasted.
- **Only that one line is touched; everything else is preserved verbatim** — comments, `registry=`, `_authToken=`, … (minimal line edit plus an atomic replace, the same rule the app follows for DSH's `settings.yaml`). It deliberately does **not** call `npm config set`: npm's ini writer was measured to drop comments from the file.
- **Empty = delete the line**, falling back to npm's default location (rather than pinning the default explicitly).
- **Value rules**: absolute, no `..`, drive must exist, no `< > " | ? *`, not inside `Windows` / `ProgramData`, no `%` or `!` (the value travels through `cmd.exe`), **no `#` or `;`** (npm's ini parser treats them as comment starts — measured: `cache=D:\a#b` reads back as `D:\a`, i.e. silently a different folder), and a **100-character limit**: npm writes content-addressed paths 158 characters deep below the cache root (measured), and beyond the classic 260-character limit Explorer/PowerShell can no longer delete that tree — npm itself still works, but "cannot be cleaned up" is the worse outcome.
- **The app does not create the folder** — npm does that itself (measured: pointing at a non-existent directory still installs fine).

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

The launcher document itself is served under a strict CSP (`script-src 'self'`, no `unsafe-inline`/`unsafe-eval`, `object-src 'none'`, `base-uri 'none'`, `form-action 'none'`, `frame-src 'none'` so no remote content can ever be pulled into the privileged document) and with the asset protocol disabled. All untrusted strings — DSH output, error text, detected paths — are rendered with `textContent`, never as HTML. Tauri additionally injects nonces/hashes for its own bundled assets at compile time, so `script-src 'self'` keeps working without weakening.

- **`freezePrototype` is `false`, deliberately — not an oversight.** Upstream's `freezePrototype` hardening injects a script that freezes `Object.prototype` into **every** webview, including the embedded DSH page. DSH's frontend then breaks: its Monaco theme service assigns to an inherited property in strict mode (`target.constructor = …`) and throws `TypeError: Cannot assign to read only property 'constructor'`, which takes down the conversation render slot — the reply flashes and disappears, while the same page in an ordinary browser is fine. The hardening exists to stop prototype-pollution attacks against the injected IPC bridge, but the `dsh` webview has no capability at all and this document is a fully local static page that never turns untrusted text into HTML — so freezing buys nothing here and costs a broken DSH page. It is therefore off app-wide. Do not turn it back on; if a future Tauri release offers a per-webview exemption, re-evaluate it for the `dsh` webview only.

> A desktop shell's CSP and prototype freezing do **not** protect DSH's own web application from itself. Anything that renders untrusted model output is an injection target in its own origin, so the equivalent defenses — escaping every interpolated value, never building code out of data, keeping session tokens out of reach — have to live in the DSH web app itself.

> Tauri delivers this policy by injecting a `<meta http-equiv="Content-Security-Policy">` tag into the built HTML (`tauri_utils::html::create_csp_meta_tag`), **not** an HTTP header. Per the CSP spec, `frame-ancestors`, `sandbox` and `report-uri` are ignored in `<meta>`, so they are deliberately left out here — every directive configured above is one that actually takes effect. If a header-delivered policy is ever needed, `app.security.headers` is the mechanism.

### Session token at rest

DSH's `next` channel prints its listen address as `http://127.0.0.1:<port>/?token=<base64url>`. That token can be exchanged for a 30-day signed cookie — it *is* your DSH session — so the launcher treats it as a credential rather than a URL detail.

It is still remembered, because **Connect to existing service** and **re-open page** read no process output and would otherwise make you authenticate again. What changed in 1.2.6 (security review M-2) is that the remembered address is not readable on disk any more:

- **`config.json` keeps it encrypted.** The address is sealed with Windows DPAPI (`CryptProtectData`, **CurrentUser** scope, plus an app-specific entropy string) and stored hex-encoded as `last_url_enc`. A plaintext `last_url` written by ≤ 1.2.5 is migrated to the encrypted key on the next start and the plaintext key is deleted. A hand-edited or undecryptable blob is simply ignored (the page then loads the bare `http://127.0.0.1:<port>`), and **no failure path ever falls back to writing plaintext** — if sealing fails, the address just is not remembered this time. DPAPI is a Windows facility, and this launcher only ships for Windows.
- **Logs are masked.** DSH prints that address to its own stdout, so the raw text used to land in `desktop.log`, in `<home>\logs\dsh.log` and in the log panel (where *复制日志 / 复制错误信息* can put it on the clipboard). Every line now goes through a redactor that replaces the value of any `token=` parameter with `***` — you will see `?token=***` — before it reaches either file, the panel or the clipboard. Nothing else in the output is altered.
- **What this does and does not cover.** DPAPI binds the ciphertext to your Windows user account: another user on the machine, a copy of the file taken off the disk (backup, sync folder, support bundle, screenshot) or a stolen disk image cannot read it. It does **not** stop another process running as *you* — the entropy is compiled into the binary rather than kept secret, and DPAPI will decrypt for any code in your session. "Malware already running as this user" is out of scope for this layer; DSH itself also still holds the token in its process memory and the resulting session cookie in the WebView2 profile directory.

### Config writes are atomic

`config.json`, the `ui-language` sidecar and DSH's `settings.yaml` are written **atomically** (security review L-6): the new content goes to a temp file in the *same directory*, is flushed with `fsync`, and is then moved over the target with `rename`. A crash, power loss or kill in the middle therefore leaves the *previous* file intact instead of a half-written one. That matters because the old code used a plain overwrite — it is "open + truncate + write" — and a truncated `config.json` is silently swallowed by `unwrap_or_default()` on load, i.e. one badly-timed crash away from "all my settings are gone".

- The temp file is created with `create_new`, so a name that already exists (possibly a symlink someone pre-placed) makes the write **fail** rather than being followed. It also lives next to the target on purpose: `rename` only replaces atomically within one volume, so a temp file in `%TEMP%` would defeat the whole exercise.
- If the final `rename` is refused because a scanner, backup or sync tool is holding the target open, it is retried a few times and then reported as an ordinary write failure. It **never** degrades to a non-atomic direct overwrite. Note this is a robustness fix, not a privilege boundary: the failure mode it removes is lost settings, not escalated access.
- On a future Unix port the callers write nothing sensitive, so the default mode is fine; a caller that needs `0600` must `set_permissions` the temp file *before* the rename, since replacing the file drops the old inode's mode.

### Opening external links

`open_in_browser` (first-run wizard, About/help links) hands the URL to `rundll32 url.dll,FileProtocolHandler` instead of `explorer <url>` (security review L-5). Both pass the URL as a real argv element — there is no shell involved either way — but `explorer` is a file manager first and a protocol launcher second, and its parsing of URLs containing unusual characters is less predictable, whereas `rundll32 url.dll,FileProtocolHandler` goes straight to `ShellExecute`'s URL handling.

`rundll32.exe` is also invoked by its **absolute** `%SystemRoot%\System32` path. `CreateProcess` searches the current directory before the system directory, so a bare name would let an executable dropped in the launcher's own working directory win — a hijack path with no reason to exist. If `%SystemRoot%` cannot be read the code falls back to the bare name rather than failing to open the link at all; only `http://` and `https://` URLs are accepted either way.

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

## Appearance

Same three-way semantics as DSH's own "General settings → Appearance", with the launcher and the DSH page kept in sync:

- Pick **⚙ Preferences → Appearance** and save. The toolbar, status area, dialogs, first-run wizard and the native title bar re-skin immediately; **Follow system** subscribes to the OS light/dark flip and reacts in real time. No Desktop restart required (and after restarting, the choice is read from `config.json`).
- **The DSH page follows live**: on save, the app writes the matching value into `<DSH home dir>\settings.yaml`:

  ```yaml
  ui-theme:
    preference: dark   # or: light / system (sibling keys like fontSize are preserved)
  ```

  DSH's settings-file provider watches this file and pushes the change to already-open pages, so **no DSH restart is needed** — the page re-skins instantly (unlike the interface language, which needs a restart).
- Like the language sync, this is a minimal, targeted line edit: only `ui-theme.preference` is added/changed; everything else in `settings.yaml` is preserved.
- Upgrade compatibility: an old `config.json` without the appearance field inherits DSH's current theme on first load (reading `settings.yaml`), so upgrading the launcher never re-skins your DSH page by itself. From then on, saving in Preferences takes over the value.
- Changing the appearance inside DSH's own settings affects the DSH page only; the launcher keeps whatever was last saved in its Preferences.

## Toolbar display mode (pinned / auto-hide)

The toolbar across the top has two display modes, switched in **⚙ Preferences → Auto-hide toolbar**. The choice is stored as `toolbar_mode` in `config.json` (`pinned` / `auto`) and applied on the next launch:

- **Pinned (`pinned`, default)**: the toolbar is always visible and the embedded DSH page sits below it. This is the historical behaviour and the default after upgrading, so an upgrade alone never changes how the app feels.
- **Auto-hide (`auto`)**: the toolbar starts parked above the window and the embedded page fills the **entire** window height; moving the mouse into the 8px strip at the top of the window slides the toolbar down, and moving off the toolbar hides it again after 500ms.

**Shortcut**: `Ctrl+Shift+H` toggles between the two modes. Note it only works while this launcher page holds keyboard focus — when focus is inside the embedded DSH page the keystroke belongs to DSH; in that case just move the mouse to the very top edge of the window.

**Animation**: purely `transform: translateY(-100%)` ↔ `translateY(0)` with `transition: transform 0.25s ease-in-out`. **Nothing animates `top` / `height` / `margin`**, so this runs on the compositor thread and never triggers per-frame reflow. In auto-hide mode the toolbar carries a subtle drop shadow so it reads as a layer above the content once it has slid out.

**Why not just use CSS stacking**: the embedded DSH page is not an `<iframe>` — it is a Tauri **native child webview**. Its z-order is always above the launcher page, and its position and size can only be set from the Rust side, so no CSS trick can make it yield space to a floating toolbar. The approach here is therefore to let the content area genuinely fill the whole window and have the shell draw the toolbar on top of it:

1. The frontend tells Rust whether the toolbar is hidden, via the `set_toolbar_hidden` command;
2. Rust computes the child webview's Y offset with the pure function `content_offset_for(mode, is_safe_mode, hidden)` (toolbar height, 43.2px, for pinned / Safe Mode; `0` for auto-hide while collapsed) and syncs its position **and height**. **The height is the client height minus that offset**, so `offset + height == client height` holds by construction. The embedded webview is a native child window: giving it the full client height would push its bottom edge 43.2px past the parent's client area, and the strip Windows clips is exactly the **visible bottom of the page** — while WebView2 still lays out against the height it was handed, so the bottom can never be reached. The invariant is pinned by the pure function `content_height(client_h, top)` and its Rust unit tests;
3. Once collapsed, the top 8px of the page is covered by the child webview, so mouse events never reach the launcher page. That case is covered by the **shell reading the system cursor position**: the frontend calls `probe_toolbar_hotzone` every 120ms, and Rust takes the screen coordinates from `GetCursorPos`, subtracts the client-area origin, divides by the scale factor, and checks whether the cursor is back inside the top strip. This is only a fallback poll while collapsed — with the toolbar showing, everything goes through ordinary DOM events (`mouseenter` / `mouseleave`).

Two more details: auto-hide is suspended while a toolbar dropdown or dialog is open, otherwise the menu would slide away with the toolbar; and collapsing happens in **two stages** — the toolbar slides up first, and only after the 260ms animation finishes does the content area expand to the full window. Moving the mouse back to the top during that window cancels the collapse.

**Safe Mode always forces pinned (hard constraint)**: the "Exit Safe Mode" button and the amber badge live on the toolbar, so hiding it would trap the user inside Safe Mode. Four safeguards enforce this:

- On the Rust side, `content_offset_for` puts `safe_mode` in the **first, short-circuiting branch**;
- The `set_toolbar_hidden` command computes `hidden && !is_active` and returns the **value that actually took effect** so the frontend can correct itself;
- Entering Safe Mode calls `force_toolbar_shown` to clear any leftover collapsed state;
- The preference toggle is disabled in Safe Mode, with a "Safe Mode forces the toolbar to stay pinned" note.

**One more caveat**: auto-hide only takes effect once the DSH page is ready. Before DSH has started (or while it is still coming up) the status area *is* the whole content, so the toolbar has to stay — pinned behaviour is used in that case. If the toggle looks like it is doing nothing, check whether DSH is actually running.

## Window layout

The main window's **size, position and maximized state** are remembered and restored on the next launch. This uses Tauri's official `tauri-plugin-window-state`; there is deliberately **no hand-rolled debounce + JSON persistence**.

- **Why the official plugin matters**: the failure mode of a home-grown implementation is not saving, it is **restoring** — after a monitor is unplugged, the coordinates on disk may point at a screen that no longer exists, so the window reopens outside the visible area and the only way out is deleting the state file. The plugin walks the *currently attached* monitors and only applies the saved coordinates when some monitor **intersects** the saved position + size. That is the official fix for exactly that bug. This app's own code only decides *when not to remember*.
- **What is tracked**: `POSITION | SIZE | MAXIMIZED` — deliberately **not `VISIBLE`**. Closing to tray (the default) leaves the window hidden at exit, so restoring visibility would come back "hidden" and look like "clicking the tray icon does nothing" (the very failure `show_main_window`'s repaint fallback exists to fix). Decorations and fullscreen are likewise left to `tauri.conf.json` instead of being rewritten by history.
- **State file**: `%APPDATA%\com.dsh.desktop\.window-state.json` (the plugin's default location). To *reset* the layout, **close the app first, then delete that file**; the next launch is back to the `tauri.conf.json` default of 1376×774, centered. A corrupt file (hand-edited, truncated) is treated as "nothing recorded" rather than a startup failure — which is also why legacy files need no cleanup: an unrecognized format simply means no state.
- **Safe Mode is completely unaffected**: entering Safe Mode **tears down the entire daily DSH instance** and starts a brand-new safe instance from a separate home (`.dsh-safe`, port 3081) — at the *runtime* layer that is a clean restart. The **desktop shell and its window do not restart, though** (it is still the same window), so the two paths are isolated separately: launched through a safe-mode entry point (`--safe` / `--safe-mode` / `DSH_SAFE_MODE=1`) the plugin neither restores nor saves anything; pressing the button in-app first freezes the daily layout (snapshot + flush to disk) and then **resets the window to the `tauri.conf.json` default geometry**, so nothing you drag or resize while Safe Mode is active is ever written to disk, and the daily layout comes back on exit. See [Safe Mode](#safe-mode).
- Sizes are stored in **physical pixels** and positions are validated against the current monitors, so the geometry does not drift as you change DPI or move between displays.

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

## Safe Mode

Like an operating system's safe mode: DSH is started from a **separate, pristine home directory** so you can inspect and repair problems in the daily environment. The entry point is the **🛡 Safe Mode** toolbar button (between "Update DSH" and "Log").

**Mechanism**: equivalent to `DSH_HOME="%USERPROFILE%\.dsh-safe" dsh web --port 3081 --no-open`. `.dsh-safe` sits next to the daily `.dsh` (never inside app_data_dir); when DSH finds a home that is empty apart from the credential file, it rebuilds all factory defaults itself.

**Entry flow** (one click, every step visible in the log):

1. **Pre-check**: the daily instance is fully stopped first (`taskkill /T` takes its node children with it) and both the daily port and 3081 must be free; any failure **prompts and refuses entry — never forced** (an in-progress update also refuses).
2. **Baseline reset** (a Preferences switch, **off by default**): with it off, an existing `.dsh-safe` is **reused**, so the previous safe session's configuration and logs survive; with it on, an existing `.dsh-safe` is renamed to `.dsh-safe-archive-<YYYYMMDD-HHMMSS>` — **archived, never deleted** — and rebuilt empty, so every entry is the same factory baseline. On the very first entry (no directory yet) both paths are identical: an empty home holding only the borrowed credentials.
3. **Credential borrowing**: the daily home's `.credentials.yaml` is **overwritten-copied** into `.dsh-safe` on every entry (always the currently valid key). It is the only file copied; a missing/empty source does not block entry (the banner tells you and DSH runs its first-run flow). **The content is never logged, never sent to the frontend over IPC, and never placed in environment variables**; permissions are tightened to 0600 on Unix.
4. **Launch**: the child process also gets `DSH_DAILY_HOME=<daily home>` so a repair agent naturally knows the repair target (path only, never secrets), and the page is loaded through the exact same output-parsing / readiness-wait / embed logic as the daily mode, using the auth URL on 3081.

**Visual distinction**: the window title is prefixed with "[Safe Mode]" (localized), the toolbar switches to an amber accent with a 🛡 badge, and an onboarding banner states the three key facts (you are in safe mode / the daily home path / the credential-borrowing result). Daily control buttons are disabled while safe mode is active, and the toolbar is **forced to stay pinned** — the "Auto-hide toolbar" preference has no effect here, because the exit button and the badge live on the toolbar and hiding it would trap you in Safe Mode (see [Toolbar display mode](#toolbar-display-mode)).

> The amber marking lands **immediately** — there is no intermediate state where the toolbar is still grey until you click it. There used to be a race here: when the safe instance became ready, the shell broadcast "running" before mounting the embedded page, so the frontend recomputed the toolbar once under daily-mode rules *before* it learned Safe Mode had been entered. The toolbar slid away under auto-hide rules and exposed the daily-mode grey backing strip. The marking is now applied synchronously the moment the status arrives (ahead of every toolbar/button decision), plus a purely declarative CSS backstop (`body.safe-mode.tb-auto` forces the toolbar to stay in place and hides the backing strip outright).

**Window layout**: window memory is strictly isolated from Safe Mode. Safe Mode *is* a restart into a separate environment — the daily DSH instance is torn down entirely and the safe instance starts fresh from its own home on 3081; but the desktop shell and the window itself do not restart, so the isolation is taken over explicitly by the window-memory side: on entry the daily layout is frozen (snapshot + flushed to disk) and the window is reset to the `tauri.conf.json` default size and position; anything you drag or resize while Safe Mode is active is **never** written to disk; on exit (including quitting the app straight from Safe Mode) the daily layout is restored verbatim. See [Window layout](#window-layout).

**Exit & repair-verification loop**: "Exit Safe Mode" kills the safe instance (the Child handle lives in Tauri State; app quit and window destruction clean it up too, and a hard-kill of the desktop app is covered by the Windows Job Object at kernel level — no orphan process keeps 3081), then restarts the daily instance through the normal path. If the daily instance is not ready within the verification window (default 80 s, configurable in Preferences, 0 = off), the app prompts "the repair may not have succeeded" and offers a one-click **return to Safe Mode**.

**Logs**: the safe instance's output streams into the Log panel and `desktop.log`, but **no** mirror file is written into `.dsh-safe` (the "empty apart from credentials" factory baseline stays intact).

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

DSH stdout/stderr are never hidden: they stream live to the log dialog and to both files — with one exception: the session token inside DSH's own authentication URL is masked to `?token=***` on its way to the panel, the clipboard and both files (see [Session token at rest](#session-token-at-rest)).

## Troubleshooting

- **未找到 Node.js (Node.js not found)** — install Node.js LTS from <https://nodejs.org>, reopen Preferences → 自动检测, or browse to `node.exe`'s directory manually.
- **未找到 npm** — usually fixed by installing Node.js; npm.cmd sits in the same directory as node.exe (e.g. `C:\Program Files\nodejs\npm.cmd`).
- **未找到 DSH** — run `npm install -g @deepseek-ai/dsh` (see wizard), or point Preferences to the existing `dsh.cmd` (typically `%APPDATA%\npm\dsh.cmd` — or the folder you chose in the wizard if you moved it).
- **端口被占用 (Port busy)** — choose Connect to existing service (if it's another DSH instance), change the port in Preferences, or handle the occupying process yourself in Task Manager. This app never kills unknown processes.
- **DSH 启动超时 (Start timeout)** — cold starts can be slow; raise the timeout in Preferences or set it to `0` (wait indefinitely while the process is alive).
- **DSH 启动后立即退出 (Exits immediately)** — see the red error line (last stderr) and full log output; common causes: wrong home dir, broken global npm install, port conflicts inside DSH config.
- **配置路径无效 (Invalid path)** — the error names the exact path; fix it in Preferences (auto-detect usually repairs it).
- **Closed the window but it's still running** — X hides to tray by default; use tray → Exit to quit. Change this in Preferences ("点击窗口 X 时").
- **Tray icon doesn't reopen the window** — fixed pattern already implemented (show/unminimize/set-focus on main thread + WebView repaint nudge); if you still hit it, report with the desktop.log attached.
- **Reset the window size/position (or the window ended up off-screen)** — close the app, then delete `%APPDATA%\com.dsh.desktop\.window-state.json`; the next launch is back to 1376×774, centered. Switching monitors normally needs none of this: on restore the plugin only applies coordinates that **intersect an attached monitor**. See [Window layout](#window-layout).
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

**DSH worked before, then stopped starting — could Node.js be the cause?**
Yes, if your Node.js is older than 22.19.0 (for example 21.x). The launcher now refuses such a launch and tells you the detected version and the required one instead of showing DSH's opaque exit; upgrade the Node.js runtime with the wizard / Preferences button, or tick "Keep this version and start anyway" if you want to try regardless. See [Node.js version check](#nodejs-version-check).

**The toolbar disappeared — it only shows up when I move the mouse to the very top edge?**
That is the "auto-hide toolbar" mode (turn it off in ⚙ Preferences). The toolbar starts parked above the window, slides down when the mouse enters the 8px strip at the top, and hides again half a second after the mouse leaves it; `Ctrl+Shift+H` switches back to pinned. Two things to note: auto-hide only kicks in once the DSH page is **ready** (while DSH is not running the toolbar stays visible), and the shortcut only works while this launcher page holds focus — if focus is inside the embedded DSH page, move the mouse to the very top edge instead. See [Toolbar display mode](#toolbar-display-mode).

**Why can't the toolbar auto-hide in Safe Mode?**
The "Exit Safe Mode" button and the amber badge live on the toolbar, so hiding it would trap you in Safe Mode. Safe Mode therefore **forces pinned**, and the preference toggle is disabled while it is active. See [Toolbar display mode](#toolbar-display-mode).

## License

Released under the [MIT License](LICENSE).
