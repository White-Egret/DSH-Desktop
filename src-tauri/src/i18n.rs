//! 轻量双语支持（zh / en）。
//!
//! 设计：所有「用户可见」的 Rust 侧文案（启动器日志、状态消息、错误提示、
//! 托盘菜单、引导向导输出）都收敛为模板 key，模板里用 {0} {1} … 占位。
//! 语言在 Desktop 启动时由 lib.rs 从配置读入（i18n::set_lang），
//! 保存设置时也会即时切换（托盘菜单文字同步刷新）。
//! 注意：DSH 自身 stdout/stderr 属于第三方程序的输出，本模块不翻译。

use std::sync::atomic::{AtomicBool, Ordering};

static IS_EN: AtomicBool = AtomicBool::new(false);

pub fn set_lang(lang: &str) {
    IS_EN.store(lang.trim().eq_ignore_ascii_case("en"), Ordering::SeqCst);
}

pub fn is_en() -> bool {
    IS_EN.load(Ordering::SeqCst)
}

/// 取模板（含 {0}/{1}… 占位符）。所有调用点 key 已核对存在；
/// 未知 key 返回静态标记串，便于日志里直接暴露拼写问题。
pub fn t(key: &str) -> &'static str {
    let en = is_en();
    match key {
        // ---------- 启动 / 状态 ----------
        "err_status_locked" => if en { "Cannot start: current DSH status is \"{0}\"" } else { "当前 DSH 状态为「{0}」，无法重复启动" },
        "log_autostart_wait" => if en { "[launcher] Autostart triggered: waiting {0} s first to avoid the boot-time IO spike..." } else { "[launcher] 检测到开机自启触发，先等待 {0} 秒错开系统冷启动高峰..." },
        "status_autostart_delay" => if en { "Autostart: DSH will start in {0} s (avoids the boot spike; click Stop to cancel)" } else { "开机自启：等待 {0} 秒后启动 DSH（错开系统冷启动高峰，可点「停止」取消）" },
        "log_autostart_cancelled" => if en { "[launcher] Autostart delay cancelled." } else { "[launcher] 开机自启延迟被取消。" },
        "log_autostart_resume" => if en { "[launcher] Delay finished, launching DSH." } else { "[launcher] 延迟结束，开始启动 DSH。" },
        "err_node_missing" => if en { "Node.js (node.exe) not found.\nDSH requires Node.js: install the LTS from https://nodejs.org, or check the paths in Settings, then retry." } else { "未找到 Node.js（node.exe）。\nDSH 依赖 Node.js 运行：请先安装 Node.js LTS （https://nodejs.org），或打开「设置」确认路径后重试。" },
        "err_dsh_missing" => if en { "DSH not found: {0}.\nInstall it globally first: npm install -g {1}\nor pick the dsh path in Settings." } else { "未找到 DSH：{0}。\n请先全局安装：npm install -g {1}\n或在「设置」中手动选择 dsh 路径。" },
        "err_dsh_missing_auto" => if en { "DSH not found (auto-detection failed).\nInstall it globally first: npm install -g {0}\nor pick the dsh path in Settings." } else { "未找到 DSH（自动检测失败）。\n请先全局安装：npm install -g {0}\n或在「设置」中手动选择 dsh 路径。" },
        "log_npm_missing_hint" => if en { "[launcher] Note: npm.cmd not found; \"Update DSH / check latest version\" is unavailable (launching still works). Configure it in Settings." } else { "[launcher] 提示：未找到 npm.cmd，「更新 DSH / 查询最新版本」不可用，但不影响启动。可在「设置」中配置。" },
        "err_cwd_invalid" => if en { "Invalid configured path: the process working directory does not exist ({0}, derived from home dir {1}). Check the DSH home dir in Settings." } else { "配置路径无效：启动进程工作目录不存在（{0}，由家目录 {1} 推导）。请在「设置」中检查 DSH 家目录。" },
        "err_port_busy" => if en { "Port {0} is occupied by another process (possibly an already-running DSH, or another program). This app will not force-kill unknown processes." } else { "端口 {0} 已被其他进程占用（可能是已在运行的 DSH，也可能是其他程序）。本程序不会强制结束未知进程。" },
        "err_spawn_fail" => if en { "Failed to start DSH: {0} (path: {1}). Check that the path is valid and executable." } else { "启动 DSH 失败: {0}（路径: {1}）。请检查路径是否有效、程序是否有执行权限。" },
        "log_start_cmd" => if en { "[launcher] Command: \"{0}\" web --port {1} --no-open{2} (cwd: {3}, DSH_HOME: {4}, PID: {5}; DSH log file: {6})" } else { "[launcher] 启动命令: \"{0}\" web --port {1} --no-open{2}（进程工作目录: {3}，DSH_HOME: {4}，PID: {5}；DSH 输出日志: {6}）" },
        "log_wait_ready" => if en { "[launcher] Waiting for DSH to become ready (default http://127.0.0.1:{0}{1})..." } else { "[launcher] 正在等待 DSH 就绪（默认 http://127.0.0.1:{0}{1}）..." },
        "wait_suffix_timeout" => if en { ", timeout {0} s" } else { "，超时 {0} 秒" },
        "wait_suffix_infinite" => if en { ", no timeout (waits while the DSH process is alive)" } else { "，无超时限制（DSH 进程存活期间持续等待）" },
        "log_detected_url" => if en { "[launcher] Detected DSH's actual listen address: {0} (the page will be loaded from it)" } else { "[launcher] 检测到 DSH 实际监听地址: {0}（将以该地址为准加载页面）" },
        "log_exit_before_ready" => if en { "[launcher] The DSH process exited before the service was ready (exit code {0}) — start failed! Open the log to see DSH's raw output." } else { "[launcher] DSH 进程在服务就绪前已退出（退出码 {0}），启动失败！请打开日志查看 DSH 的原始输出。" },
        "err_exit_before_ready" => if en { "DSH failed to start: the process exited before the port was ready (exit code {0})" } else { "DSH 启动失败：进程在端口就绪前退出（退出码 {0}）" },
        "log_proc_check_fail" => if en { "[launcher] Failed to check the DSH process state: {0}" } else { "[launcher] 检查 DSH 进程状态失败: {0}" },
        "log_ready_embed" => if en { "[launcher] DSH is ready (waited {0} s), embedding page {1}..." } else { "[launcher] DSH 服务已就绪（共等待 {0} 秒），正在内嵌页面 {1} …" },
        "log_timeout_stop" => if en { "[launcher] Timed out waiting for DSH ({0} s); stopping the process tree. If DSH cold start really needs longer, raise the timeout in Settings or set it to 0 (wait forever)." } else { "[launcher] 等待 DSH 就绪超时（{0} 秒），正在停止进程树。若 DSH 冷启动确实很慢，可在设置中调大超时或设为 0（一直等待）。" },
        "err_timeout" => if en { "Start timeout: DSH was not ready within {0} s; the process has been stopped. Raise the timeout in Settings, or set 0 to wait forever." } else { "启动超时：DSH 在 {0} 秒内未就绪，进程已停止。可在设置中调大超时时间，或设为 0 表示一直等待。" },
        "log_stopping_tree" => if en { "[launcher] Terminating the DSH process tree: taskkill /PID {0} /T /F ..." } else { "[launcher] 正在结束 DSH 进程树: taskkill /PID {0} /T /F ..." },
        "log_taskkill_out" => if en { "[launcher] taskkill: {0}" } else { "[launcher] taskkill: {0}" },
        "log_taskkill_fail" => if en { "[launcher] taskkill failed: {0} (the Job Object will clean up when this app exits)" } else { "[launcher] taskkill 执行失败: {0}（Job Object 会在程序退出时兜底清理）" },
        "log_stopped" => if en { "[launcher] DSH stopped." } else { "[launcher] DSH 已停止。" },
        "log_connect_existing" => if en { "[launcher] Connected to the existing service on port {0}. Note: this service was not started by this app; quitting the app will not stop it." } else { "[launcher] 连接到端口 {0} 上的现有服务。注意：该服务不是本程序启动的，关闭本程序不会停止它。" },
        "err_connect_no_listener" => if en { "No service is listening on port {0}; cannot connect." } else { "端口 {0} 当前没有服务在监听，无法连接。" },
        "log_disconnected_external" => if en { "[launcher] Disconnected from the external service (it was not started by this app and keeps running)." } else { "[launcher] 已断开与外部服务的连接（该服务不是本程序启动的，仍在运行）。" },
        "err_restart_not_running" => if en { "DSH is not running; cannot restart" } else { "DSH 当前未在运行，无法重启" },

        // ---------- 内嵌页面 / 刷新 ----------
        "log_no_main_window" => if en { "[launcher] Main window not found; cannot embed the DSH page." } else { "[launcher] 找不到主窗口，无法内嵌 DSH 页面。" },
        "log_embed_fail" => if en { "[launcher] Failed to embed the DSH page: {0}" } else { "[launcher] 内嵌 DSH 页面失败: {0}" },
        "log_invalid_url" => if en { "[launcher] Invalid load URL: {0}" } else { "[launcher] 非法的加载地址: {0}" },
        "err_not_running_refresh" => if en { "DSH service is not running." } else { "DSH 服务未在运行。" },
        "log_refreshed_page" => if en { "[launcher] Refreshed the DSH page (service not restarted)." } else { "[launcher] 已刷新 DSH 页面（DSH 服务未重启）。" },
        "log_reopened_page" => if en { "[launcher] Re-opened the DSH page (service not restarted)." } else { "[launcher] 已重新打开 DSH 页面（DSH 服务未重启）。" },

        // ---------- 配置 / 通用命令错误 ----------
        "err_cfg_dir" => if en { "Cannot create config dir {0}: {1}" } else { "无法创建配置目录 {0}: {1}" },
        "err_cfg_write" => if en { "Cannot write config file {0}: {1}" } else { "无法写入配置文件 {0}: {1}" },
        "err_port_invalid" => if en { "Invalid port: must be a number from 1 to 65535" } else { "端口无效：必须是 1 到 65535 之间的数字" },
        "err_home_empty" => if en { "The DSH home directory must not be empty" } else { "DSH 家目录不能为空" },
        "err_paths_empty" => if en { "Paths must not be empty (click \"Auto-detect\" to fill them)" } else { "路径不能为空（可点击「自动检测」填写）" },
        "log_config_saved" => if en { "[launcher] Configuration saved to {0} (port / args take effect on the next DSH start)" } else { "[launcher] 配置已保存到 {0}（端口 / 参数将在下次启动 DSH 时生效）" },
        "log_locale_synced" => if en { "[launcher] DSH UI language synced to settings.yaml (preference={0}); DSH's own UI updates after DSH restarts." } else { "[launcher] 已同步 DSH 界面语言（settings.yaml: preference={0}），DSH 界面将在其下次启动/重启后变化。" },
        "err_locale_sync_fail" => if en { "[launcher] Warning: failed to write the locale into DSH settings.yaml: {0}" } else { "[launcher] 警告：写入 DSH settings.yaml 语言设置失败：{0}" },
        "log_lang_changed" => if en { "[launcher] Interface language switched to {0}. Tray/menu texts update immediately; a restart is never required." } else { "[launcher] 界面语言已切换为「{0}」。托盘菜单等文字已同步更新；无需重启即可生效（重启后同样生效）。" },
        "err_cmd_timeout" => if en { "Command timed out after {0} s" } else { "命令执行超时（{0} 秒）" },
        "err_cmd_spawn" => if en { "Cannot start {0}: {1}" } else { "无法启动 {0}: {1}" },
        "err_cmd_wait" => if en { "Failed to wait for the command to exit: {0}" } else { "等待命令退出失败: {0}" },
        "err_logdir_create" => if en { "Cannot create the log directory {0}: {1}" } else { "无法创建日志目录 {0}: {1}" },
        "err_logdir_open" => if en { "Cannot open the log directory: {0}" } else { "无法打开日志目录: {0}" },
        "err_bad_url" => if en { "Invalid URL: {0}" } else { "非法链接: {0}" },
        "err_browser_open" => if en { "Cannot open the browser: {0}" } else { "无法打开浏览器: {0}" },
        // 配置项里的 cmd 元字符（见 process.rs 的 command_for / validate_arg_token）
        "err_cfg_arg_danger" => if en { "Unsafe {0} value \"{1}\": the character \"{2}\" is not allowed. This field is passed to the npm / DSH command line, so shell metacharacters (& | < > ^ % ! and quotes) are rejected; ordinary flags such as --host=127.0.0.1 or --no-open are unaffected." } else { "{0} 的值「{1}」不安全：含有不允许的字符「{2}」。该字段会作为参数交给 npm / DSH 的命令行，因此拒绝 shell 元字符（& | < > ^ % ! 及引号）；普通参数如 --host=127.0.0.1、--no-open 不受影响。" },
        "err_cfg_prog_danger" => if en { "Unsafe program path \"{0}\": the character \"{1}\" is not allowed (cmd.exe could expand it or break the quoting). Please pick this file again in Settings." } else { "程序路径「{0}」不安全：含有不允许的字符「{1}」（可能被 cmd.exe 展开或破坏引号配对）。请在「设置」中重新选择该程序。" },
        // ---------- 配置路径策略（MEDIUM-3，见 config.rs 的 validate_home_dir / validate_program_file） ----------
        "err_path_empty" => if en { "{0}: the path is empty. Fill it in or click \"Auto-detect\" in Settings." } else { "{0}：路径为空。请在「设置」中填写，或点击「自动检测」。" },
        "err_path_relative" => if en { "{0}: must be an absolute path (e.g. C:\\Users\\you\\.dsh); relative paths are rejected." } else { "{0}：必须是绝对路径（例如 C:\\Users\\你\\.dsh），不接受相对路径。" },
        "err_path_unc" => if en { "{0}: network share paths (\\\\server\\share) are rejected — writing there leaks credentials via NTLM, and running a program from there hands the binary to whatever answers that share." } else { "{0}：不接受网络共享路径（\\\\服务器\\共享）。往共享写会触发 NTLM 认证而外泄凭据，从共享执行程序等于把可执行文件交给对端服务器。" },
        "err_path_traversal" => if en { "{0}: the path contains \"..\" parent-directory segments and was rejected. Please pick the folder directly." } else { "{0}：路径中包含「..」上级跳转，已拒绝。请直接选择目标文件夹。" },
        "err_path_root" => if en { "{0}: cannot be a drive root, the user profile folder itself, or one of its parents — otherwise the DSH process working directory would land somewhere that can see every user's files." } else { "{0}：不能是驱动器根目录、用户目录本身或其上层目录——否则 DSH 进程的工作目录会落在能看见所有用户文件的位置。" },
        "err_path_system" => if en { "{0}: cannot be inside a system or program directory (Windows / Program Files / ProgramData)." } else { "{0}：不能位于系统或程序目录内（Windows / Program Files / ProgramData）。" },
        "err_path_temp" => if en { "{0}: this program lives in a temp directory, which is rejected (trivial to hijack or pre-plant). Please install Node.js / DSH to a permanent location." } else { "{0}：该程序位于临时目录，已拒绝（临时目录极易被劫持或抢先植入）。请把 Node.js / DSH 安装到固定位置。" },
        "err_prog_ext" => if en { "{0}: only .exe / .cmd / .bat can be launched (a .ps1 or extension-less file would go through unpredictable file associations). Re-select the program in Settings." } else { "{0}：只允许启动 .exe / .cmd / .bat 文件（.ps1 或无扩展名会走不可预测的文件关联）。请在「设置」中重新选择。" },
        "err_prog_missing" => if en { "{0}: no such executable file. Click \"Auto-detect\" in Settings or pick the path again." } else { "{0}：找不到该可执行文件。请在「设置」中点「自动检测」或重新选择路径。" },

        // ---------- 版本 / 包检测 ----------
        "err_ver_flags" => if en { "Cannot read the local DSH version (--version / -v / -V all failed)" } else { "无法获取本地 DSH 版本（--version / -v / -V 均不可用）" },
        "err_ver_no_dsh" => if en { "dsh.cmd not found ({0}); cannot read the local version" } else { "找不到 dsh.cmd（{0}），无法获取本地版本" },
        "err_ver_view_empty" => if en { "npm view {0} version returned empty" } else { "npm view {0} version 返回为空" },
        "err_view_fail" => if en { "npm view failed: {0}" } else { "npm view 失败: {0}" },
        "err_no_npm_view" => if en { "npm.cmd not found ({0}); cannot query the latest version" } else { "找不到 npm.cmd（{0}），无法查询最新版本" },
        "err_no_npm_detect" => if en { "Cannot start npm: {0}. Set the npm path in Settings first." } else { "无法启动 npm: {0}，请先在设置中配置 npm 路径" },
        "log_pkg_none" => if en { "No dsh-related package found among the global npm packages.\n\nFull output:\n{0}" } else { "未在 npm 全局包中发现 dsh 相关包。\n\n完整输出:\n{0}" },
        "log_pkg_found" => if en { "Detected global packages:\n{0}\n\nFull output:\n{1}" } else { "检测到全局包:\n{0}\n\n完整输出:\n{1}" },
        "err_pkg_list_fail" => if en { "npm list failed:\n{0}" } else { "npm list 执行失败:\n{0}" },

        // ---------- 更新 DSH ----------
        "err_update_busy" => if en { "An update task is already running; please wait" } else { "已有更新任务正在进行，请等待完成" },
        "err_no_npm_update" => if en { "Cannot start npm: {0}. Fix the npm program path in Settings." } else { "找不到 npm: {0}，请在「设置」中修改 npm 程序路径" },
        "log_update_cmd" => if en { "[update] Running: \"{0}\" {1} (process cwd: {2} — unrelated to DSH workspaces, PID: {3})" } else { "[update] 执行: \"{0}\" {1}（进程工作目录: {2}，PID: {3}；DSH 工作区与此目录无关）" },
        "err_npm_spawn" => if en { "npm failed to start: {0}" } else { "npm 启动失败: {0}" },
        "err_npm_wait" => if en { "Failed to wait for npm to exit: {0}" } else { "等待 npm 退出失败: {0}" },
        "pick_filter_exec" => if en { "Command scripts / executables" } else { "命令脚本 / 可执行文件" },
        "log_update_ok" => if en { "[update] npm update succeeded (exit code 0)." } else { "[update] npm 更新成功（退出码 0）。" },
        "msg_update_success" => if en { "Update succeeded" } else { "更新成功" },
        "log_update_fail_code" => if en { "[update] npm update failed (exit code {0}); DSH will not be restarted. Check the log." } else { "[update] npm 更新失败（退出码 {0}），不会自动启动 DSH。请查看日志。" },
        "msg_update_fail_code" => if en { "Update failed (exit code {0}); see the log" } else { "更新失败（退出码 {0}），详见日志" },
        // 更新进行中的实时进度（页面进度区每秒刷新一次）
        "update_npm_progress" => if en { "Downloading and installing the new DSH via npm… {0} package file(s) fetched ({1} s elapsed)" } else { "正在通过 npm 下载并安装新版 DSH… 已获取 {0} 个包文件（用时 {1} 秒）" },

        // ---------- 开机自启 ----------
        "log_autostart_on" => if en { "[launcher] Start-with-Windows enabled." } else { "[launcher] 已开启开机自动启动。" },
        "log_autostart_off" => if en { "[launcher] Start-with-Windows disabled." } else { "[launcher] 已关闭开机自动启动。" },
        "err_autostart_enable" => if en { "Failed to enable autostart: {0}" } else { "启用开机自启失败：{0}" },
        "err_autostart_disable" => if en { "Failed to disable autostart: {0}" } else { "取消开机自启失败：{0}" },
        "log_tray_autostart_fail" => if en { "[launcher] Failed to toggle autostart: {0}" } else { "[launcher] 切换开机自启失败：{0}" },
        "log_tray_autostart_state" => if en { "[launcher] Autostart {0}." } else { "[launcher] 开机自启已{0}。" },
        "word_on" => if en { "enabled" } else { "开启" },
        "word_off" => if en { "disabled" } else { "关闭" },

        // ---------- 托盘菜单 ----------
        "tray_show" => if en { "Show Main Window" } else { "显示主窗口" },
        "tray_autostart" => if en { "Start with Windows" } else { "开机自动启动" },
        "tray_quit" => if en { "Exit" } else { "退出" },

        // ---------- 首次运行引导：Node.js ----------
        "err_setup_busy" => if en { "A guided setup task is already running" } else { "已有引导安装任务正在进行" },
        // ---------- 引导安装的下载完整性校验（SHA-256） ----------
        "setup_tempdir_fail" => if en { "Cannot create the private temp directory for the download ({0}); installation aborted." } else { "无法为下载创建私有临时目录（{0}），已中止安装。" },
        "setup_verify_bad_version" => if en { "Refusing to verify/install: the resolved Node.js version \"{0}\" is not a plain numeric version string." } else { "拒绝校验/安装：解析出的 Node.js 版本号「{0}」不是合法的纯数字版本串。" },
        "setup_verify_dl_fail" => if en { "Cannot download the official SHASUMS256.txt checksum list ({0}). Refusing to install an unverified package — check your network and retry, or install manually from {1}." } else { "无法下载官方校验清单 SHASUMS256.txt（{0}）。为避免安装未经验证的文件，已中止——请检查网络后重试，或到 {1} 手动下载安装。" },
        "setup_verify_no_entry" => if en { "The official SHASUMS256.txt has no entry for {0}; cannot verify the download, so installation aborted. Retry later or install manually from {1}." } else { "官方校验清单中没有 {0} 这一条目，无法验证下载文件，已中止安装。可稍后重试，或到 {1} 手动下载安装。" },
        "setup_verify_bad_digest" => if en { "The official digest entry for {0} is malformed (expected 64 hex characters); installation aborted for safety. If this persists, download manually from {1}." } else { "官方清单中 {0} 的校验值格式异常（应为 64 位十六进制字符），出于安全已中止安装。若持续如此，请从 {1} 手动下载。" },
        "setup_dl_too_large" => if en { "The downloaded file is implausibly large ({0} bytes, limit {1}); installation aborted." } else { "下载文件体积异常偏大（{0} 字节，上限 {1}），已中止安装。" },
        "setup_hash_mismatch" => if en { "SHA-256 mismatch for {0}! Official list: {1} Downloaded: {2} The file was deleted and installation aborted — the download may have been tampered with (proxy/network interference). You can retry, or install manually from https://nodejs.org." } else { "{0} 的 SHA-256 与官方清单不一致！官方记录：{1} 实际下载：{2} 已删除该文件并中止安装——下载可能被篡改（代理/网络劫持）。可稍后重试，或到 https://nodejs.org 手动安装。" },
        "setup_hash_ok" => if en { "v{0} installer verified (SHA-256 {1})" } else { "v{0} 安装包校验通过（SHA-256 {1}）" },
        "setup_dl_start" => if en { "Downloading the official Node.js v{0} LTS installer: {1}" } else { "开始下载官方 Node.js v{0} LTS 安装包：{1}" },
        "setup_lts_resolved" => if en { "[launcher] Resolved the newest LTS on the v{0} line: v{1}" } else { "[launcher] 已解析 v{0} 线当前最新 LTS：v{1}" },
        "setup_lts_fallback" => if en { "[launcher] Cannot resolve the newest LTS ({0}); using the pinned v{1} instead." } else { "[launcher] 未能解析最新 LTS（{0}），改用固定版本 v{1}。" },
        "setup_node_no_msi_entry" => if en { "no -x64.msi entry found in the official SHASUMS256.txt" } else { "官方 SHASUMS256.txt 中未找到 -x64.msi 条目" },
        "setup_path_refreshed" => if en { "[launcher] Node.js has updated the system PATH; this app refreshed its own PATH ({0} new dir(s)) so npm installs can find node." } else { "[launcher] Node.js 已写入系统 PATH，本进程 PATH 同步刷新（新增 {0} 个目录），后续 npm 安装可以找到 node。" },
        "setup_dl_progress" => if en { "Downloading the Node.js installer… {0}% ({1} / {2} MB)" } else { "正在下载 Node.js 安装包… {0}%（{1} / {2} MB）" },
        "setup_dl_progress_unknown" => if en { "Downloading the Node.js installer… {0} MB so far" } else { "正在下载 Node.js 安装包… 已下载 {0} MB" },
        "setup_npm_progress" => if en { "Downloading and installing DSH via npm… {0} package file(s) fetched ({1} s)" } else { "正在通过 npm 下载并安装 DSH… 已获取 {0} 个包文件（用时 {1} 秒）" },
        "err_lang_persist_fail" => if en { "[launcher] Failed to persist the language choice: {0}" } else { "[launcher] 语言选择保存失败: {0}" },
        "setup_curl_fallback" => if en { "curl download unavailable ({0}); falling back to PowerShell..." } else { "curl 下载不可用（{0}），改用 PowerShell…" },
        "setup_no_dl_tool" => if en { "No usable download tool found" } else { "未找到可用的下载工具" },
        "setup_no_curl" => if en { "curl.exe not found" } else { "未找到 curl.exe" },
        "setup_curl_fail" => if en { "curl failed: {0}" } else { "curl 执行失败: {0}" },
        "setup_net_denied" => if en { "Cannot reach the network or access was denied" } else { "无法连接网络或访问被拒绝" },
        "setup_ps_fail" => if en { "PowerShell download failed" } else { "PowerShell 下载失败" },
        "setup_ps_dl_fail" => if en { "{0}: {1}. Check your network and retry, or download Node.js manually from {2}." } else { "{0}：{1}。请检查网络连接后重试，或到 {2} 手动下载安装 Node.js。" },
        "setup_dl_timeout" => if en { "Download timed out or failed ({0}). Check your network, or download Node.js manually from {1}." } else { "下载超时或失败（{0}）。请检查网络连接，或到 {1} 手动下载安装 Node.js。" },
        "setup_dl_incomplete" => if en { "Download failed: the file is incomplete ({0} bytes). Check your network and retry, or download manually from {1}." } else { "下载失败：文件不完整（{0} 字节）。请检查网络后重试，或到 {1} 手动下载安装。" },
        "setup_dl_missing" => if en { "Download failed: the file was not saved to {0}. Check your network connection and retry, or download manually from {1}." } else { "下载失败：文件未能保存到 {0}。请检查网络连接后重试，或到 {1} 手动下载安装。" },
        "setup_install_launch" => if en { "Download finished. Launching the official installer — please confirm the UAC prompt and wait until it completes..." } else { "下载完成，正在启动官方安装程序。请在弹出的安装窗口 / UAC 提示中确认并等待完成……" },
        "setup_msi_launch_fail" => if en { "Cannot launch the installer (msiexec): {0}. Install Node.js manually from {1}, then click \"Re-check\"." } else { "无法启动安装程序（msiexec）: {0}。可到 {1} 手动下载安装 Node.js 后点击「重新检测」。" },
        "setup_install_timeout" => if en { "Timed out waiting for the installer (30 minutes); giving up. Install manually from https://nodejs.org, then click \"Re-check\" here." } else { "等待安装程序完成超时（30 分钟），已中止等待。可到 https://nodejs.org 手动安装，然后回到本窗口点击「重新检测」。" },
        "setup_wait_exit_fail" => if en { "Failed while waiting for the installer to exit: {0}" } else { "等待安装程序退出失败: {0}" },
        "setup_verifying" => if en { "The installer has exited; re-detecting Node.js..." } else { "安装程序已退出，正在重新检测 Node.js…" },
        "setup_node_detected" => if en { "Detected Node.js: {0} {1}" } else { "检测到 Node.js：{0} {1}" },
        "setup_node_not_detected" => if en { "The installer exited (code {0}) but node.exe was not detected. Right after installing, PATH may only refresh after a restart — click \"Re-check\" or install manually from {1}." } else { "安装程序已退出（退出码 {0}），但未检测到 node.exe。刚装完可能需要重新打开程序使 PATH 生效；可点击「重新检测」，或到 {1} 手动安装。" },
        "setup_node_cancelled" => if en { "Installation was cancelled (exit code 1602). Try once more, or install manually from https://nodejs.org." } else { "安装被取消（退出码 1602）。可再次尝试一键安装，或到 https://nodejs.org 手动安装。" },
        "setup_node_fail_code" => if en { "Installation failed (msiexec exit code {0}). Common causes: insufficient permissions (UAC declined), low disk space. Retry, or download manually from https://nodejs.org." } else { "安装失败（msiexec 退出码 {0}）。常见原因：权限不足（UAC 被拒绝）、磁盘空间不足。可重试或到 https://nodejs.org 手动下载安装。" },
        "setup_word_ok" => if en { "succeeded" } else { "成功" },
        "setup_word_fail" => if en { "failed" } else { "失败" },
        "setup_node_result_line" => if en { "[setup] Guided Node.js installation {0}: {1}" } else { "[setup] Node.js 引导安装{0}：{1}" },

        // ---------- 首次运行引导：DSH ----------
        "err_task_busy" => if en { "An install task is already running" } else { "已有安装任务正在进行" },
        "setup_npm_missing" => if en { "npm.cmd not found ({0}). Install Node.js (which includes npm) first, or set the npm path in Settings." } else { "未找到 npm.cmd（{0}）。请先安装 Node.js（含 npm），或在「设置」中配置 npm 路径。" },
        "setup_dsh_executing" => if en { "[setup] Running: \"{0}\" install -g {1}" } else { "[setup] 正在执行: \"{0}\" install -g {1}" },
        "setup_npm_spawn_fail" => if en { "npm failed to start: {0} (wrong path or missing permission?)" } else { "npm 启动失败: {0}（路径错误或权限不足？）" },
        "setup_npm_timeout" => if en { "npm install timed out (15 minutes) and was aborted. Check your network and retry, or run the install command manually." } else { "npm 安装超时（15 分钟），已中止。请检查网络后重试，或手动执行安装命令。" },
        "setup_npm_wait_fail" => if en { "Failed while waiting for npm to exit: {0}" } else { "等待 npm 退出失败: {0}" },
        "setup_dsh_success_msg" => if en { "DSH installed globally: {0}" } else { "DSH 全局安装成功：{0}" },
        "setup_dsh_ok_log" => if en { "[setup] DSH global install succeeded." } else { "[setup] DSH 全局安装成功。" },
        "setup_dsh_notfound" => if en { "npm reported success but dsh.cmd was not found. Click \"Re-check\", or restart this app." } else { "npm 报告成功但未找到 dsh.cmd。可点击「重新检测」，或重启程序后再试。" },
        "setup_dsh_fail_code" => if en { "npm install -g {0} failed (exit code {1}). Common causes: no network, npm registry unreachable, insufficient permission for the global dir. See the log, or copy the command and run it yourself." } else { "npm install -g {0} 失败（退出码 {1}）。常见原因：无网络、npm 源不可达、全局目录权限不足。详见日志，也可复制命令手动执行。" },
        "log_setup_done" => if en { "[launcher] First-run setup finished; configuration saved." } else { "[launcher] 初始化引导已完成，配置已写入。" },

        // 未知 key：返回静态标记（正常路径不会命中；出现即说明 key 拼写有遗漏）
        _ => "[i18n-key-missing]",
    }
}

/// 渲染模板：按位置替换 {0} {1} ...（参数可为任意 Display 类型）
pub fn fmt(key: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut s = t(key).to_string();
    for (i, a) in args.iter().enumerate() {
        s = s.replace(&format!("{{{}}}", i), &a.to_string());
    }
    s
}
