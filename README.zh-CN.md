# DSH Desktop

Windows 10/11 轻量桌面端（Tauri v2 + Rust + 原生 HTML/CSS/JS），用于启动、配置、运行、更新本机 npm 全局安装的 DSH（DeepSeek Harness）Web 服务。英文主文档见 [README.md](README.md)。

> **重要 —— 本程序不内置任何运行时**
>
> - 本程序**不内置 Node.js**。
> - 本程序**不内置 DSH**。
> - 也不下载/部署任何便携版 Node/DSH 运行环境。
> - 用户需主动安装 Node.js 与 DSH（首次运行向导可引导安装，见「环境要求」）。

## 概览

- **单窗口设计**：顶部 43.2px 工具栏（操作按钮 + 一行状态：状态点 · 端口 · 版本），DSH 就绪后其 Web 界面**直接内嵌在工具栏下方**，绝不打开默认浏览器（启动命令固定带 `--no-open`）。
- 程序按你配置的路径与端口启动 DSH：先 TCP + HTTP 轮询确认服务真正就绪，再加载页面；若 DSH 在就绪前闪退，立即显示最后一条 stderr 报错，绝不无限等待。
- **只管理自己启动的进程**：退出时用 `taskkill /PID <pid> /T /F` + Windows Job Object 结束本次启动的 DSH 进程树，绝不使用 `taskkill /IM node.exe /F` 这类误杀式命令。

## 功能特性

- 端口可配置（默认 **3080**），路径自动检测 + 设置页手动选择
- 端口占用保护：提示而非强杀——可连接现有服务、修改端口、重新检测
- 实际地址识别：DSH 输出中打印了真实监听地址时（如 `dsh web: http://127.0.0.1:3080`），优先按该地址加载
- 文件日志：Desktop 日志 `%APPDATA%\com.dsh.desktop\desktop.log`；DSH 输出日志 `%USERPROFILE%\.dsh\logs\dsh.log`；UI 提供「打开日志目录」「复制错误信息」「复制日志」
- 刷新页面：只重载内嵌网页，不重启后台服务（`F5` / `Ctrl+R`）
- 一键更新 DSH（npm 全局更新，输出实时可见）
- 关闭行为可配：点 X 隐藏到托盘（默认）或退出程序；托盘恢复执行 show + unminimize + set_focus
- 开机自启（官方插件，仅写 HKCU 注册表，无需管理员）；自启时静默托盘化并延迟 12 秒错开冷启动高峰
- 首次运行向导：检测 Node/npm/DSH 并引导安装（nodejs.org 官方 LTS 安装包在线下载，或代为执行 `npm install -g @deepseek-ai/dsh`），全程可跳过
- **安装包完整性校验**：调用 `msiexec` 之前，先把下载的 MSI 与该版本官方 `SHASUMS256.txt` 中记录的 SHA-256 逐字节比对；下载落在一次性随机命名的私有临时目录中，用完即删（原先可预测的 `%TEMP%\node-vX.Y.Z-x64.msi` 路径可能被抢先做成符号链接或被替换）。清单取不到、没有该文件条目、或哈希不一致时**直接中止安装，不提供「仍然安装」选项**，请改用手动下载链接
- **中英双语界面**：设置页可切换「语言 / Language」；工具栏、状态区、弹窗、托盘菜单与启动器日志全部跟随切换，并通过 DSH 的 `settings.yaml` 联动 DSH 自身界面语言
- 单实例锁：重复双击只聚焦已有窗口

## 环境要求（Requirements）

| 项目 | 要求 |
|---|---|
| 操作系统 | Windows 10 或 11，x64 |
| WebView2 | Win10/11 一般已随 Edge 预装；安装包缺失时可引导安装 |
| Node.js | DSH 的运行依赖（建议 LTS）。**不随本程序分发** |
| DSH | 通过 npm 全局安装。**不随本程序分发** |

## 环境准备（Prerequisites）

启动服务前必须具备：

1. **Node.js（含 npm）**——从官网 <https://nodejs.org/en/download> 安装 LTS 版；
2. **DSH**——全局安装：

   ```bash
   npm install -g @deepseek-ai/dsh
   ```

可用以下命令自检：

```bash
node --version
npm --version
dsh --version
```

缺什么时程序会给出明确错误（哪个组件没找到、找过哪些位置、怎么修），不会无限等待。首次运行向导也会自动检测：缺 Node 时提供一键下载运行 nodejs.org 官方 LTS 安装包（运行时在线下载，绝不内置），缺 DSH 时可代为执行 `npm install -g @deepseek-ai/dsh`。每一步都可以跳过（稍后手动安装），失败原因（无网络/下载失败/权限不足/用户取消）都会明确提示。

## 安装（Installation）

如果不想从源码安装，可以直接下载现成的 **Windows 10/11 安装包**：

- <https://tfevx3uq.qwenwork.host/DSH-Desktop-windows-nsis>

无需本地 Rust 环境——GitHub Actions 云端构建（见下一节）。取最新成功构建的产物：

- `DSH-Desktop-windows-nsis` → 内含 `DSH Desktop_<版本>_x64-setup.exe`（NSIS 安装包，推荐）
- `DSH-Desktop-windows-msi` → MSI 安装包
- `DSH-Desktop-windows-portable-exe` → 免安装独立 exe

用 NSIS 安装包安装或直接运行便携 exe。首次启动会出现一次环境检查向导；一旦 `%APPDATA%\com.dsh.desktop\config.json` 存在即不再出现。

## 从源码构建（Build from source）

需要：Node.js 18+/20 LTS、Rust stable（MSVC）、Visual Studio Build Tools、WebView2。

```bash
npm ci
npm run tauri build
```

产物位于 `src-tauri/target/release/bundle/nsis/`、`.../msi/`，独立 exe 在 `src-tauri/target/release/`。

## GitHub Actions 构建

`.github/workflows/build-windows.yml` 在 push 到 `main`/`master`、打 `v*` 标签、PR 及手动触发时自动构建，产物见「安装」一节。首次构建约 8~15 分钟（Rust 编译），之后有缓存会快很多。

## 使用（Usage）

1. 安装/运行 **DSH Desktop**，窗口默认 1200×760。
2. 首次启动出现环境检查向导：
   - 已装齐 → 点「完成，进入主界面」；
   - 有缺失 → 用引导按钮安装，或直接跳过进入主界面（之后启动会给出明确错误）。
3. 点 **▶ 启动**（或让程序自动拉起）：状态区显示等待秒数，就绪后无缝切换为内嵌的 DSH 页面。
4. 工具栏右侧始终显示：状态点 · 当前端口 · 版本。版本在每次程序启动 / DSH 服务启动或重启时自动检测（本地 `dsh --version` + 远端 `npm view`），无需手动点击。
5. 默认点 X 是隐藏到托盘（可在设置改为退出）；从托盘图标/菜单唤回窗口；托盘菜单「退出」才真正结束本程序及其启动的 DSH 进程树。

程序实际执行的命令示例（以你的配置为准）：

```text
默认访问地址:        http://127.0.0.1:3080
DSH 启动命令示例:     dsh web --port 3080
全局安装命令示例:     npm install -g @deepseek-ai/dsh
```

## 配置（Configuration）

配置文件：`%APPDATA%\com.dsh.desktop\config.json`（当前用户目录，绝不写入 Program Files 或程序安装目录）。

工具栏打开 **⚙ 设置**。所有路径支持自动检测：留空或失效时，程序通过 `where` 查找与常见目录扫描（如 `%ProgramFiles%\nodejs`、`%APPDATA%\npm`）自动补全，也可手动浏览选择。

| 配置项 | 默认值 | 说明 |
|---|---|---|
| npm 程序路径 | 空 → 自动检测 | 支持 `npm.cmd` / `npm.exe`；用于更新 / 查询最新版本 |
| dsh 路径 | 空 → 自动检测 | 支持 `dsh.cmd` / `dsh.exe` / `dsh.bat` |
| DSH 家目录 | `%USERPROFILE%\.dsh` | 经 `DSH_HOME` 传给 DSH；进程工作目录取其上一级；不是工作区 |
| 端口 | `3080` | 必须为 1~65535，保存时校验，下次启动生效 |
| 就绪超时 | `300` 秒 | 冷启动可能较慢；**0 = 一直等待** |
| 点击窗口 X 时 | 隐藏到托盘 | 可改「退出程序」（结束本次启动的 DSH 进程） |
| 附加参数 | 空 | 追加在 `dsh web --port N --no-open` 之后；只接受普通参数（见下） |
| 包名 | `@deepseek-ai/dsh` | 版本检测 / 更新使用 |
| 更新参数 | `install -g @deepseek-ai/dsh@latest` | 作为独立参数传给 npm；只接受普通参数（见下） |
| 开机自动启动 | 关 | 即时生效，写 HKCU\...\Run，无需管理员 |
| 界面语言 | `zh`（中文） | `zh` / `en` 可选；见下方「界面语言」 |

### 参数与路径策略（保存时校验，每次使用时再校验一次）

这些配置项不是「一段文字」，而是能力：`dsh_path` / `npm_path` 会被**执行**；`extra_args` / `update_args` / `package_name` 会成为命令行参数；而 **DSH 家目录的上一级会被当作 DSH 进程的工作目录**（同时也是 `npm` 读取 `./.npmrc` 的位置）。由于 `config.json` 是明文的当前用户文件、且开机自启会静默按它执行，因此校验发生在**每一次使用点**，不只是点「保存」时。

- **参数**：拒绝 shell 元字符（`& | < > ^ % !` 与引号）。因为 `cmd.exe` 会对整条命令行二次解析，`a&calc.exe` 会变成两条命令。普通参数（`--host=127.0.0.1`、`--port`、`--no-open`、路径等）不受影响。
- **所有路径**：必须是带驱动器的绝对路径；拒绝 `\\服务器\共享`（UNC）—— 往共享写会触发 NTLM 认证而外泄凭据，从共享执行程序等于把可执行文件交给对端；拒绝含 `..` 的路径（直接拒绝而不是折叠规范化，否则 `C:\Windows\..\x` 会被洗白成合法路径）。
- **程序路径**：扩展名必须是 `.exe` / `.cmd` / `.bat`（`.ps1` 或无扩展名会走不可预测的文件关联），启动时文件必须真实存在，且不能位于 `%TEMP%` 内。
- **执行方式**：`.exe` 直接走 `CreateProcess`，链路上根本没有 `cmd.exe`；`.cmd` / `.bat` 这类 shim 只能经 `cmd.exe`，所以命令行由本程序自己拼装并逐令牌加引号，不依赖默认的字符串转义。
- **家目录**：不能是驱动器根目录、不能是用户目录本身或它的任一层父目录、不能在 `Windows` / `Program Files` / `ProgramData` 内。（Node.js 本身仍可以正常装在 `Program Files` —— 该限制只针对家目录。）
- 路径会被规范化后写回，磁盘上保留的就是通过校验的形态。

任一项不通过时，程序会拒绝启动/更新并在状态区显示原因 —— 被手改或被植入的 `config.json` 无法把「点一下启动」变成执行另一个程序。

### Webview 隔离与 CSP

DSH 网页界面属于不可信内容（它渲染模型输出），却以第二个原生 webview（label=`dsh`）的形式内嵌在主窗口的工具栏下方。有两条彼此独立的机制把它挡在特权面之外：

- **capability 用 `webviews` 绑定，而不是 `windows`。** Tauri v2 中，`windows` 一旦命中窗口标签，就会对该窗口内的**每一个 webview** 生效 —— 所以写 `windows: ["main"]` 等于把 `core:*` 同时发给内嵌的 DSH 页面。因此 `capabilities/default.json` 只列 `"webviews": ["main"]`（启动器自身的 webview），并刻意不写 `windows`，这也是官方对多 webview 窗口的建议。
- **它的 origin 属于 remote。** 内嵌页面加载的是 `http://127.0.0.1:<port>`，Tauri 将其判定为远程来源，而远程来源默认无法触达 `invoke_handler` 里的自定义命令，除非某个 capability 在 `remote.urls` 中显式放行。**永远不要给 `dsh` 这个 webview 加这种放行** —— 隔离成立靠的就是这一条加上面的作用域绑定。

启动器自身页面运行在严格 CSP 下（`script-src 'self'`，不允许 `unsafe-inline`/`unsafe-eval`，`object-src 'none'`、`base-uri 'none'`、`form-action 'none'`、`frame-src 'none'`（不可信内容无法被拉进特权文档）），资源协议（asset protocol）关闭，并开启 `freezePrototype`（阻止通过原型链污染攻击被注入的 IPC 桥）。所有不可信字符串 —— DSH 输出、错误信息、检测到的路径 —— 一律用 `textContent` 渲染，绝不拼成 HTML。此外 Tauri 会在编译期为自身资源注入 nonce/hash，所以 `script-src 'self'` 无需放宽即可正常工作。

> Tauri 是以「往构建后的 HTML 里注入 `<meta http-equiv="Content-Security-Policy">` 标签」的方式下发这份策略的（见 `tauri-utils/src/html.rs` 的 `create_csp_meta_tag`），而不是 HTTP 响应头。按 CSP 规范，`<meta>` 里的 `frame-ancestors`、`sandbox`、`report-uri` 会被浏览器忽略，所以上面刻意没有列它们 —— 列出的每一条都是真正生效的。若将来确实需要防内嵌，得改用 `app.security.headers` 以 HTTP 头下发。

## 界面语言（Language）

启动器界面支持简体中文 / English 双语：

- 在 **⚙ 设置 → 语言 / Language** 选择后保存：工具栏、状态区、弹窗、托盘菜单与所有启动器日志即时切换（无需重启 Desktop；重启后同样按配置生效）。选回「中文」即全部恢复中文。
- **DSH 自身界面**：保存时程序会把对应值写入 `<DSH 家目录>\settings.yaml`：

  ```yaml
  locale:
    preference: zh   # 或 en
  ```

  写入采用最小改动方式，只增改 `locale.preference`，不会破坏该文件中的其他配置。DSH 启动时读取此文件，因此**需重启 DSH**（工具栏「⟳ 重启」）其界面语言才会变化；DSH 正在运行时切换语言，日志中会给出提示。
- DSH 的原始 stdout/stderr 与 npm 输出属于第三方程序内容，日志中原样呈现、不做翻译。

## 默认端口（Default port）

- 默认端口为 **3080**。
- 可在设置页修改；启动命令、健康检查轮询、内嵌 WebView 加载地址全部跟随配置端口。
- 示例：`http://127.0.0.1:3080`、`dsh web --port 3080`。
- 每次启动前检测端口占用；被占用时**绝不强杀未知进程**，面板提供「连接现有服务」「修改端口」「重新检测」。若 DSH 输出中给出不同实际地址（如 `dsh web: http://127.0.0.1:3080`），以实际地址优先加载。

## 更新 DSH（Update DSH)

工具栏点「⤓ 更新 DSH」→ 确认 → 自动停止服务 → 执行 `"<npm>" install -g @deepseek-ai/dsh@latest`（每个参数独立传递，见上方「参数与路径策略」）→ 更新期间**页面中央实时显示进度**（「已获取 N 个包文件 / 用时」+ npm 输出滚动，同时写入日志，来源标记 update）→ 成功后自动重启 DSH。「检测全局包名」（`npm list -g --depth=0`）可确认包名。版本栏显示本地 `--version` 与远端 `npm view` 结果对比。

## 日志（Logs）

两份日志都在用户目录下（无需管理员权限），超过 5MB 自动滚动为 `*.old`：

| 日志 | 路径 | 内容 |
|---|---|---|
| Desktop 日志 | `%APPDATA%\com.dsh.desktop\desktop.log` | launcher/update/setup 事件 + DSH stdout/stderr 镜像 |
| DSH 输出日志 | `%USERPROFILE%\.dsh\logs\dsh.log` | DSH 原始输出（跟随家目录设置：`<家目录>\logs\dsh.log`） |

UI 支持：「日志」实时面板（自动滚动/清空）、**打开日志目录**、**复制错误信息**（错误/端口占用状态下出现）、**复制日志**。DSH 的 stdout/stderr 绝不隐藏：实时进日志面板并同时写入上述两个文件。

## 故障排查（Troubleshooting)

- **未找到 Node.js** — 到 <https://nodejs.org> 安装 LTS；或在设置里点「自动检测」/手动浏览选择。
- **未找到 npm** — 通常装好 Node.js 即有；npm.cmd 与 node.exe 同目录（如 `C:\Program Files\nodejs\npm.cmd`）。
- **未找到 DSH** — 执行 `npm install -g @deepseek-ai/dsh`（或用向导），或在设置中指向已有 `dsh.cmd`（通常在 `%APPDATA%\npm\`）。
- **端口被占用** — 选「连接现有服务」（若是另一个 DSH 实例）、在设置中修改端口、或在任务管理器自行处理占用进程；本程序绝不强杀。
- **DSH 启动超时** — 冷启动慢属正常；调大超时或设为 `0`（进程活着就一直等）。
- **DSH 启动后立即退出** — 看红色错误行（最后一条 stderr）与完整日志；常见原因：家目录配置错误、全局安装损坏、DSH 自身配置冲突。
- **配置路径无效** — 错误会给出具体路径；在设置中修正即可（自动检测通常能修复）。
- **关了窗口还在运行** — 默认点 X 是隐藏到托盘；请用托盘菜单「退出」。可在设置「点击窗口 X 时」修改行为。
- **托盘点不开窗口** — 现已实现 show/unminimize/set_focus + 重绘兜底；如仍遇到，请附 desktop.log 反馈。
- **更新失败** — 查看日志里 npm 的完整输出；多为网络问题或全局目录权限不足（本程序从不请求管理员权限）。
- **WebView2 缺失** — NSIS/MSI 安装包会引导安装 WebView2 运行时。

## FAQ

**内置 Node.js 或 DSH 吗？**
否。既不内置也不嵌入，更不部署便携运行时；程序只使用你自己安装的官方版本（必要时引导你在线安装官方包）。

**配置存在哪里？**
`%APPDATA%\com.dsh.desktop\config.json` —— 当前用户目录，无需管理员权限，绝不在 Program Files。

**用什么端口？**
默认 3080，可在设置页修改（1~65535，带校验）。见「默认端口」。

**会管理不是它启动的 DSH 进程吗？**
不会。只按 PID + Job Object 结束自己启动的进程树；外部服务只能「连接查看」，退出时不受影响。

**关闭窗口等于退出吗？**
默认不是——隐藏到托盘。可在设置把关闭按钮改为「退出程序」。

**为什么开机自启要等 12 秒？**
刚登录时磁盘 IO 高峰、Node/网络未必就绪，延迟可避免大多数超时失败；等待期间点「停止」可取消。

## 许可证（License）

以 [MIT 许可证](LICENSE) 发布。
