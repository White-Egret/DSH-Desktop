# DSH Desktop

Windows 10 轻装桌面端（Tauri v2），用于启动、设置、运行、更新本机 npm 全局安装的 DSH（DeepSeek Harness）Web 服务。

- **单窗口设计**：整个使用过程只有一个窗口。顶部 43.2px 工具栏（左侧操作按钮、右侧一行状态：状态点 · 端口 · 版本），下方区域在启动等待/错误提示与 DSH 页面之间切换——DSH 就绪后其 Web 界面**直接内嵌在工具栏下方**，不再弹出独立窗口，也**绝不打开默认浏览器**（启动命令固定带 `--no-open`）
- 不弹终端、不下载运行时、不内置 Node/DSH、不读取也不修改你的 DSH 用户配置
- DSH 的 stdout / stderr 实时收集到「日志」弹窗（工具栏按钮打开），启动失败时错误原因直接显示为一行提示
- 等待就绪不着急：DSH 冷启动（尤其重启电脑后首次）可能需要一两分钟，默认耐心等待 **300 秒**，可设为 **0 = 一直等待**；等待期间显示已等待秒数
- **开机自动启动**（官方 autostart 插件）：设置里一个开关即可注册/取消；开机自启时**静默托盘化**——窗口不弹出，DSH 先延迟 12 秒（错开系统冷启动高峰）再后台拉起，点托盘图标随时恢复窗口
- **系统托盘常驻**：点窗口 X 不再直接退出，而是最小化到托盘；托盘右键菜单提供「显示主窗口 / 开机自动启动 / 退出」，真正退出时才结束 DSH 进程树（`taskkill /PID <pid> /T /F` + Job Object 双保险，程序崩溃也不残留进程）
- **刷新页面**（新增）：只重新加载内嵌的 DSH 网页，**不会停止或重启 DSH 后台服务**。
- 一键更新：`npm install -g @deepseek-ai/dsh@latest`，输出实时显示，成功后自动重启 DSH
- 单实例锁：重复双击只会聚焦已有窗口

## 一、获取程序（GitHub Actions 自动构建，无需本地安装 Rust）

你不需要在本机安装 Rust、Visual Studio Build Tools 或 Tauri CLI，全部在 GitHub 云端完成：

1. **创建仓库**：登录 GitHub → 右上角 `+` → *New repository* → 名称如 `dsh-launcher` → *Create repository*。
2. **上传代码**（三选一）：
   - 网页上传：仓库页 → *uploading an existing file* → 把本项目所有文件/文件夹拖进去（保持 `.github/workflows/build-windows.yml` 的相对路径不变）→ *Commit changes*。
   - 命令行：
     ```bash
     cd dsh-launcher
     git init
     git add .
     git commit -m "DSH Desktop"
     git branch -M main
     git remote add origin https://github.com/<你的用户名>/dsh-launcher.git
     git push -u origin main
     ```
   - GitHub Desktop：Add Existing Repository → Publish repository。
3. **触发构建**：push 到 `main` 会自动触发；也可到仓库 *Actions* 标签 → 选择 *Build Windows* → *Run workflow* 手动触发。
4. **下载产物**：*Actions* → 点进最新的成功运行记录 → 页面底部 *Artifacts* 区域下载：
   - `DSH-Desktop-windows-nsis` — NSIS 安装包（推荐，双击安装）
   - `DSH-Desktop-windows-msi` — MSI 安装包
   - `DSH-Desktop-windows-portable-exe` — 免安装独立 exe
5. 首次构建约 8~15 分钟（编译 Rust），之后有缓存会快很多。

> 如果 Actions 没有自动运行：检查仓库 *Settings → Actions → General → Actions permissions* 是否允许运行；确认 `.github/workflows/build-windows.yml` 已提交。

## 二、使用

1. 安装/运行 DSH Desktop（NSIS 安装包或独立 exe 均可），窗口默认 1200×760。
2. 首次启动会自动尝试启动 DSH。默认配置适配以下环境（不同则点「设置」改）：

| 配置项 | 默认值 | 说明 |
|---|---|---|
| npm.cmd 路径 | `D:\Programs\nodejs\npm.cmd` | 用于更新 DSH / 查询最新版本 |
| dsh.cmd 路径 | `C:\Users\admin\AppData\Roaming\npm\dsh.cmd` | 用于启动 DSH |
| DSH 家目录 | `%USERPROFILE%\.dsh`（如 `C:\Users\admin\.dsh`） | **DSH 摆放配置文件的地方**，通过 `DSH_HOME` 环境变量传给 DSH；不是工作目录 |
| 端口 | `3080` | |
| 就绪超时 | `300` 秒 | DSH 冷启动可能要一两分钟；**0 = 一直等待**（只要进程活着就一直等） |
| 附加参数 | 空 | 追加在 `dsh web --port N --no-open` 之后 |
| 更新命令 | `install -g @deepseek-ai/dsh@latest` | 拼在 `cmd /C npm.cmd` 之后 |
| 开机自动启动 | 关 | 设置里的开关**即时生效**（写入注册表 `HKCU\...\Run`，无需管理员权限），托盘菜单也可切换 |

3. 启动流程：工具栏下方显示一行「正在启动 DSH，已等待 X 秒…」→ 服务就绪后该区域无缝切换为**内嵌的 DSH 页面**。就绪前绝不加载网页，无需手动刷新。
4. 若 DSH 启动瞬间崩溃：提示立即变为红色错误行（含 DSH 退出前最后一条报错），完整输出见「日志」。
5. 若端口 3080 被占用：程序不会强杀未知进程，而是显示提示面板，可选择 **连接现有服务**（不接管该进程）或 **修改端口**。

### 界面说明

```
┌──────────────────────────────────────────────────────────────┐
│ ▶启动 ■停止 ⟳重启 ↻刷新页面 ⤓更新DSH 检查版本 日志 ⚙设置   ● 运行中 · 端口 3080 · 版本 1.0.1 │ ← 43.2px 浅灰工具栏
├──────────────────────────────────────────────────────────────┤
│                                                              │
│   等待中：一行提示 + 转圈（已等待 X 秒…）                        │
│   失败时：一行红色错误                                         │
│   就绪后：DSH 页面内嵌于此（铺满整个剩余区域）                    │
│                                                              │
└──────────────────────────────────────────────────────────────┘
```

- 窗口缩放时内嵌页面自动跟随调整大小。
- 打开「设置 / 日志 / 更新确认」弹窗时内嵌页面自动暂时隐藏，关闭弹窗后恢复。

### 刷新页面（不重启 DSH）

- **刷新页面** 按钮位于「重启」旁边，只重新加载内嵌的 DSH 网页——**不会停止或重启 DSH 服务**。
- 适用场景：更换壁纸后刷新界面、应用皮肤后刷新界面、前端显示没有及时更新、页面临时异常但不想重启整个 DSH。
- 刷新期间页面上会显示「正在刷新...」的提示，直到新页面出现。
- 保留当前访问地址（`http://127.0.0.1:3080` 或你配置的端口），不会重新走完整的 DSH 启动流程。
- 快捷键：`F5`、`Ctrl + R`（焦点在 Launcher 界面时生效；在 DSH 页面内部，WebView2 也原生支持 F5/Ctrl+R 刷新）。
- 如果 DSH 服务当前未运行，点击「刷新页面」会提示：`DSH service is not running.`
- **「重启 DSH」按钮仍然独立存在**，用于完整重启后台服务；两者不会混淆。

### 窗口与托盘行为

- **点主窗口的 X**：不再退出，而是**最小化到系统托盘**（后台继续运行，DSH 不受影响）。
- **托盘图标**（黑鲸鱼）：
  - 左键单击 / 双击 → 显示并聚焦主窗口；
  - 右键 → 菜单：「显示主窗口」「开机自动启动（勾选切换）」「退出」。
- **退出**（托盘菜单）= 停止 DSH 进程树并退出整个程序；程序崩溃时 Job Object 兜底清理，绝不残留进程。
- 若连接的是"外部进程"（不是本程序启动的），退出本程序不会动它，只会断开页面连接。

### 开机自动启动（官方 autostart 插件）

- 设置里的「开机自动启动」开关**即时生效**，也可从托盘右键菜单勾选切换，两处状态实时同步。
- 注册方式：Windows 注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，无需管理员权限。
- **开机自启的体验优化**：
  1. **静默托盘化**——开机登录后主窗口不弹出、不抢焦点，只在后台运行，需要时点托盘图标恢复窗口；
  2. **延迟 12 秒**——刚开机时系统 IO 高、Node 环境/网络未必就绪，立即启动 DSH 极易超时；程序检测到自启触发（命令行带 `--autostart`）后先等 12 秒再拉起 DSH，等待期间可随时点「停止」取消。

### 更新 DSH

工具栏点「更新 DSH」→ 确认（弹窗显示将执行的完整命令）→ 自动停止服务 → 执行 `cmd /C "D:\Programs\nodejs\npm.cmd" install -g @deepseek-ai/dsh@latest` → npm 输出实时写入日志（来源标记 `update`）→ 按退出码提示成功/失败 → 成功自动重启 DSH。更新过程按钮自动禁用，防止重复操作。包名可通过设置里的「检测全局包名」（`npm list -g --depth=0`）确认。

## 三、验收清单

| # | 项目 | 结果 |
|---|---|---|
| 1 | 双击无终端窗口 | release 版设置 `windows_subsystem = "windows"` + `CREATE_NO_WINDOW` |
| 2 | 全程一个窗口、不开浏览器 | DSH 页面内嵌主窗口（多 Webview），启动固定带 `--no-open` |
| 3 | 自动启动 | 双击即自动拉起 `cmd /C dsh.cmd web --port 3080 --no-open` |
| 4 | 就绪前等待提示，就绪后加载 | Rust 端 TCP+HTTP 轮询，就绪才创建内嵌 Webview；默认等 300 秒，可设 0 无限等 |
| 5 | 崩溃立报错 | 轮询同时 `try_wait` 监控进程，退出即停并显示最后一条报错 |
| 6/7 | 关窗无残留 | `taskkill /PID <pid> /T /F` + Job Object(KILL_ON_JOB_CLOSE) 双保险 |
| 8 | 重开正常 | 配置持久化于 `%APPDATA%\com.dsh.desktop\config.json` |
| 9/10 | 更新与重启 | 见"更新 DSH" |
| 11 | 不内置运行时 | 仅调用本机 npm/dsh |
| 12 | 免本地构建 | GitHub Actions windows-latest 全自动 |
| 13 | 开机自启 + 托盘 | 官方 autostart 插件；自启静默托盘化 + 延迟 12 秒；关窗=托盘，托盘菜单退出 |
| 14 | 刷新页面 | 只刷新内嵌 DSH 网页；后台 DSH 服务不重启；保留配置端口与当前地址 |

## 四、故障排查

- **提示找不到 dsh.cmd / npm.cmd**：设置 → 浏览选择正确路径（dsh.cmd 通常在 `%APPDATA%\npm\`，npm.cmd 在 Node.js 安装目录）。
- **启动后秒退**：看「日志」中 DSH 的原生报错；常见原因是 DSH 家目录配置错误导致 DSH 找不到配置。
- **等待超时被停止**：冷启动确实慢的话，设置里把超时调大或设为 `0`（一直等待）。
- **端口被占用**：确认是否已有 DSH 在跑（选择"连接现有服务"），或在任务管理器中自行处理后改端口/重启。
- **更新失败**：查看日志中 npm 的完整输出；多为网络问题或全局目录权限（本程序不请求管理员权限）。
- **WebView2 缺失**：NSIS/MSI 安装包会自动引导下载 WebView2 运行时（Win10 一般已随 Edge 安装）。
- **关了窗口以为退出了**：点 X 是最小化到托盘，程序和 DSH 都还在后台运行；要真正退出请用托盘右键菜单「退出」。
- **开机自启没生效**：确认设置开关已开启（或托盘菜单有勾选）；检查 `reg query "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v "DSH Desktop"`；注意免安装 portable exe 若被移动路径需重新开启一次开关。

## 五、本项目不做的事

- 不读取/修改你的 DSH 用户配置目录（`~/.dsh` 仅作为 `DSH_HOME` 传给 DSH 本身）
- 不打印或上传 API Key / credentials / 环境变量
- 不强杀未知端口占用进程，不使用 `taskkill /IM node.exe`
- 不内置 Node.js / DSH / 便携运行时
