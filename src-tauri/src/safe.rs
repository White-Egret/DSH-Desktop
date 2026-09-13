//! 安全模式：用一个独立、纯净的家目录启动 DSH，用于检查和修复日常模式环境的问题
//! （类似操作系统的安全模式）。
//!
//! 核心机制（与产品需求一一对应）：
//! - 安全家目录固定为 `%USERPROFILE%\.dsh-safe`（与默认日常 `.dsh` 平级，绝不放
//!   进 Tauri 的 app_data_dir），以 `DSH_HOME=<safe_home> dsh web --port 3081
//!   --no-open` 启动；
//! - 凭据方案 = 只借用 `.credentials.yaml` 这一个文件（每次进入都覆盖拷贝，保证
//!   拿到当前有效密钥）；DSH 在「家目录里除该文件全空」时会自动重建全部原厂配置。
//!   **文件内容全程不离开 Rust 侧**：不写日志、不经 IPC 返回前端、不放进环境变量；
//! - 基线重置（可配置，**默认关闭**）：关闭时沿用已有 .dsh-safe（上一轮安全模式的
//!   配置与日志保留），开启时进入前把已存在的 .dsh-safe 整目录重命名为
//!   `.dsh-safe-archive-<YYYYMMDD-HHMMSS>` 归档（绝不删除），重建空目录后再写入
//!   借用的凭据 —— 每次进入都是恒定的原厂基线。首次进入两条路径结果相同
//!   （目录还不存在，都是只含凭据的空家目录）；
//! - 修复上下文：给安全实例额外注入 `DSH_DAILY_HOME=<日常家目录>`（只有路径，
//!   绝不含任何密钥），让修复 agent 天然知道修复目标；
//! - 生命周期：Child 句柄保存在 Tauri State（SafeState）中；应用退出 / 窗口销毁走
//!   process::cleanup_sync → cleanup_safe_sync，强杀场景由 Windows Job Object
//!   （KILL_ON_JOB_CLOSE）在内核层兜底，绝不留孤儿进程占用 3081。
//!
//! 复用原则（不另起炉灶）：spawn 包装（command_for / apply_no_window / PATH 组装）、
//! 输出读取与 URL 解析（spawn_log_reader + extract_local_url，含 ?token= 会话令牌）、
//! 就绪等待与页面内嵌（wait_ready_and_embed + open_dsh_webview）、进程树停止
//! （run_taskkill）、状态机与日志风格（set_status / emit_log / [safe] 前缀）全部
//! 来自 process.rs，本模块只负责安全模式特有的编排。

use crate::config;
use crate::process::{
    self, apply_no_window, command_for, current_status, destroy_dsh_webview, emit_log,
    port_in_use, resolve_dsh_prog, resolve_npm_for_path, run_taskkill, set_status,
    spawn_log_reader, AppState,
};
use crate::{detect, i18n, window_state};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// 安全模式固定端口（日常模式默认 3080，可配置；安全端口固定，避免与日常配置纠缠）
pub const SAFE_PORT: u16 = 3081;
/// 安全模式家目录名（位于 %USERPROFILE% 下，与默认日常 .dsh 平级）
pub const SAFE_DIR_NAME: &str = ".dsh-safe";
/// 日常家目录里唯一被借用的文件：DSH 的 API 密钥凭据
const CRED_FILE_NAME: &str = ".credentials.yaml";
/// 凭据文件体积上限（正常只有几百字节；异常大的文件拒绝整读进内存）
const CRED_MAX_BYTES: u64 = 10 * 1024 * 1024;

// ---------- 事件 payload ----------

/// 进入安全模式成功后随 safe-mode-change 事件发给前端的报告（前端引导横幅直接展示）。
/// 只含路径与状态文字，绝不含凭据内容。
#[derive(Clone, Serialize)]
pub struct SafeReport {
    /// 安全模式家目录（%USERPROFILE%\.dsh-safe）
    pub safe_home: String,
    /// 日常家目录（修复目标；已注入子进程的 DSH_DAILY_HOME）
    pub daily_home: String,
    /// 安全模式端口（固定 3081）
    pub port: u16,
    /// 本次进入时旧安全环境的归档目录（基线重置关闭或首次进入时为 None）
    pub archived_to: Option<String>,
    /// 凭据借用结果：borrowed | source-missing | source-empty | failed
    pub credential: String,
    /// 凭据借用结果的本地化说明（横幅原样显示）
    pub credential_message: String,
    /// 本次进入是否执行了基线重置（来自设置开关）
    pub reset_baseline: bool,
}

/// safe-mode-change 事件：进入成功 / 进入失败 / 安全实例闪退 / 退出。
/// phase: entered | failed | crashed | timeout | exited
#[derive(Clone, Serialize)]
pub struct SafeModeChange {
    pub active: bool,
    pub phase: String,
    pub report: Option<SafeReport>,
    /// 失败/闪退原因（已本地化），前端 toast 用
    pub message: Option<String>,
}

/// safe-verify 事件：退出安全模式后的修复验证结果
#[derive(Clone, Serialize)]
pub struct SafeVerifyEvent {
    pub success: bool,
    /// 失败时的本地化提示（「修复可能未成功」），前端弹窗展示并提供返回入口
    pub message: String,
}

/// get_safe_status 命令返回（前端初始化时恢复安全模式 UI）
#[derive(Serialize)]
pub struct SafeStatusReport {
    pub active: bool,
    pub busy: bool,
    pub report: Option<SafeReport>,
}

// ---------- 状态（Tauri manage 注入） ----------

/// 安全模式运行状态。与日常的 AppState 平行存在：同一时间两者最多只有一个
/// 持有活着的子进程（进入前预检强制日常实例完全退出，退出时先杀安全实例）。
pub struct SafeState {
    /// 安全实例是否存活（含 starting/running；闪退或退出后为 false）
    pub(crate) active: AtomicBool,
    /// 进入/退出流程互斥标志（防连点造成两条编排线程交叠）
    pub(crate) busy: AtomicBool,
    /// 安全模式子进程句柄（退出 / 应用关闭时据此 kill，避免孤儿进程占用 3081）
    pub(crate) child: Mutex<Option<Child>>,
    pub(crate) pid: Mutex<Option<u32>>,
    /// 最近一次进入的报告（前端页面重建后恢复横幅内容用）
    pub(crate) report: Mutex<Option<SafeReport>>,
    #[cfg(windows)]
    pub(crate) job: Mutex<Option<process::win::JobHandle>>,
}

impl SafeState {
    pub fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            child: Mutex::new(None),
            pid: Mutex::new(None),
            report: Mutex::new(None),
            #[cfg(windows)]
            job: Mutex::new(None),
        }
    }

    /// 关闭 Job Object 句柄（KILL_ON_JOB_CLOSE：最后一个句柄关闭时内核结束整个进程树）
    pub(crate) fn close_job(&self) {
        #[cfg(windows)]
        {
            if let Some(j) = self.job.lock().unwrap().take() {
                process::win::close_job(&j);
            }
        }
        #[cfg(not(windows))]
        {
            let _ = self;
        }
    }
}

impl Default for SafeState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------- 供 process.rs 调用的桥接函数 ----------

/// 安全模式是否激活（日常命令的门禁依据：start/stop/restart/connect/update 均拒绝）
pub fn is_active(app: &AppHandle) -> bool {
    app.try_state::<SafeState>()
        .map(|s| s.active.load(Ordering::SeqCst))
        .unwrap_or(false)
}

/// 状态事件覆盖：安全模式激活时，dsh-status 里的 pid/port 换指安全实例，
/// 并带上 safe_mode 标记（前端据此渲染琥珀色工具栏与徽标）。
/// set_status / get_status（process.rs）统一走这里，两种模式共用一个状态机字段。
pub fn overlay_status(
    app: &AppHandle,
    cfg_port: u16,
    daily_pid: Option<u32>,
) -> (bool, u16, Option<u32>) {
    let Some(s) = app.try_state::<SafeState>() else {
        return (false, cfg_port, daily_pid);
    };
    if s.active.load(Ordering::SeqCst) {
        let pid = *s.pid.lock().unwrap();
        (true, SAFE_PORT, pid)
    } else {
        (false, cfg_port, daily_pid)
    }
}

/// 发出 safe-mode-change 事件，并同步切换主窗口标题的「[安全模式]」前缀。
/// 所有激活状态变化的出口都收敛到这一个函数（进入成功 / 失败 / 闪退 / 超时 / 退出）。
pub fn emit_safe_change(
    app: &AppHandle,
    active: bool,
    phase: &str,
    report: Option<SafeReport>,
    message: Option<String>,
) {
    apply_safe_window_title(app, active);
    let _ = app.emit(
        "safe-mode-change",
        SafeModeChange {
            active,
            phase: phase.to_string(),
            report,
            message,
        },
    );
}

/// 安全实例在就绪前闪退时由 wait_ready_and_embed（process.rs）调用：
/// 解除激活标记并通知前端恢复正常 UI（错误本身由调用方按日常同款路径 set_status）。
pub(crate) fn on_safe_child_died(app: &AppHandle, message: Option<String>) {
    deactivate(app);
    emit_safe_change(app, false, "crashed", None, message);
}

/// 界面语言切换后刷新标题前缀（前缀文案是本地化的；process::save_config 调用）
pub fn refresh_title_if_active(app: &AppHandle) {
    if is_active(app) {
        apply_safe_window_title(app, true);
    }
}

/// 窗口标题的视觉区分：安全模式加「[安全模式] 」前缀（按界面语言）。
/// 窗口操作需主线程执行（与 lib.rs 的 show_main_window / apply_window_theme 同理）。
fn apply_safe_window_title(app: &AppHandle, active: bool) {
    let title = if active {
        format!("{}DSH Desktop", i18n::t("safe_title_prefix"))
    } else {
        "DSH Desktop".to_string()
    };
    let app2 = app.clone();
    let _ = app2.clone().run_on_main_thread(move || {
        if let Some(w) = app2.get_webview_window("main") {
            let _ = w.set_title(&title);
        }
    });
}

fn deactivate(app: &AppHandle) {
    if let Some(s) = app.try_state::<SafeState>() {
        s.active.store(false, Ordering::SeqCst);
    }
    // 同步清掉窗口布局记忆那边的「正在安全模式」标记（闪退 / 超时停止也走这里）；
    // 「本次会话进过安全模式」的标记不清，应用退出兜底还要用它。
    window_state::mark_safe_mode_inactive();
}

// ---------- 路径与时间戳 ----------

fn user_profile_base() -> Option<String> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 安全模式家目录：%USERPROFILE%\.dsh-safe（规范化 + 与日常家目录同一套路径策略校验）
pub fn safe_home_dir() -> Result<String, String> {
    let base = user_profile_base().ok_or_else(|| i18n::t("err_safe_home").to_string())?;
    let raw = PathBuf::from(base)
        .join(SAFE_DIR_NAME)
        .to_string_lossy()
        .to_string();
    config::validate_home_dir(&raw)
}

/// 归档目录名的时间戳（YYYYMMDD-HHMMSS）。Windows 用本地时间（对用户更直观），
/// 其他平台回退 UTC（复用 logger 的 civil 算法，不引入时间库依赖）。
#[cfg(windows)]
fn local_timestamp_compact() -> String {
    unsafe {
        let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut st);
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    }
}

#[cfg(not(windows))]
fn local_timestamp_compact() -> String {
    crate::logger::utc_compact_timestamp()
}

/// 轮询等待端口释放（taskkill /T /F 之后操作系统回收监听句柄需要一小会儿）
fn wait_port_free(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !port_in_use(port) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

// ---------- 凭据借用 ----------

/// 把日常家目录的 `.credentials.yaml` 整文件覆盖拷贝到安全家目录。
/// 每次进入都拷（保证拿到当前有效密钥）；源文件缺失 / 为空 / 读写失败都**不阻塞进入**，
/// 只返回状态供前端横幅提示（DSH 会走自己的首跑流程）。
///
/// 安全边界：文件内容只在 src → dest 之间按字节整体搬运，
/// 绝不写入任何日志、绝不通过 IPC 返回前端、绝不放进环境变量。
/// 返回 (状态码, 本地化说明)。
fn borrow_credentials(daily_home: &str, safe_home: &Path) -> (String, String) {
    let src = Path::new(daily_home).join(CRED_FILE_NAME);
    let dest = safe_home.join(CRED_FILE_NAME);
    let len = match std::fs::metadata(&src) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (
                "source-missing".to_string(),
                i18n::fmt("safe_cred_msg_missing", &[&src.display().to_string()]),
            );
        }
        Err(e) => {
            return (
                "failed".to_string(),
                i18n::fmt("safe_cred_msg_failed", &[&e.to_string()]),
            );
        }
    };
    if len > CRED_MAX_BYTES {
        let detail = i18n::fmt("safe_cred_too_large", &[&len]);
        return (
            "failed".to_string(),
            i18n::fmt("safe_cred_msg_failed", &[&detail]),
        );
    }
    let bytes = match std::fs::read(&src) {
        Ok(b) => b,
        Err(e) => {
            return (
                "failed".to_string(),
                i18n::fmt("safe_cred_msg_failed", &[&e.to_string()]),
            );
        }
    };
    // 全空白的文件视同「为空」：不拷贝，让 DSH 走首跑流程
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return (
            "source-empty".to_string(),
            i18n::fmt("safe_cred_msg_empty", &[&src.display().to_string()]),
        );
    }
    // 原子替换（安全审查 L-6 的同款问题）：半截写出的凭据文件会让安全模式下的 DSH
    // 直接认证失败，而这里返回的是「成功」—— 用户看到的是「凭据已就位」却用不了。
    if let Err(e) = crate::config::write_atomic(&dest, &bytes) {
        return (
            "failed".to_string(),
            i18n::fmt("safe_cred_msg_failed", &[&e.to_string()]),
        );
    }
    // Unix 下收紧为 0o600（Windows 继承用户目录 ACL，已有同等保护）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600));
    }
    (
        "borrowed".to_string(),
        i18n::t("safe_cred_msg_borrowed").to_string(),
    )
}

// ---------- 停止 / 清理 ----------

/// 停止安全模式子进程树（与 process::stop_internal 完全对称的实现：
/// taskkill /T /F → 回收句柄 → 关 Job Object → 销毁内嵌页面 → 状态回 idle）。
/// 幂等；仅在退出安全模式与安全实例超时停止时调用。
pub(crate) fn stop_safe_internal(app: &AppHandle) -> Result<(), String> {
    let sstate = app.state::<SafeState>();
    let pid = sstate.pid.lock().unwrap().take();
    let child = sstate.child.lock().unwrap().take();

    if pid.is_none() && child.is_none() {
        deactivate(app);
        set_status(app, "idle", None);
        return Ok(());
    }

    set_status(app, "stopping", None);
    if let Some(pid) = pid {
        emit_log(
            app,
            "launcher",
            i18n::fmt("log_stopping_tree", &[&pid]),
        );
        match run_taskkill(pid) {
            Ok(out) => {
                if !out.is_empty() {
                    emit_log(app, "launcher", i18n::fmt("log_taskkill_out", &[&out]));
                }
            }
            Err(e) => {
                emit_log(app, "launcher", i18n::fmt("log_taskkill_fail", &[&e]));
            }
        }
    }

    // 回收子进程句柄（最多等 3 秒；超时直接 kill 兜底，任何平台都不留残进程）
    if let Some(mut child) = child {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    sstate.close_job();
    deactivate(app);
    // 与日常模式共用同一个 label="dsh" 的内嵌 webview：停止后销毁，露出状态区
    destroy_dsh_webview(app);
    set_status(app, "idle", None);
    emit_log(app, "launcher", i18n::t("log_safe_stopped").to_string());
    Ok(())
}

/// 应用退出 / 窗口销毁时的兜底清理（process::cleanup_sync 调用；幂等）。
/// 不发事件、不动 UI —— 这条路径上窗口可能已经没了，只做进程清理。
pub fn cleanup_safe_sync(app: &AppHandle) {
    let Some(s) = app.try_state::<SafeState>() else {
        return;
    };
    let pid = s.pid.lock().unwrap().take();
    let child = s.child.lock().unwrap().take();
    if let Some(pid) = pid {
        let _ = run_taskkill(pid);
    }
    if let Some(mut child) = child {
        let _ = child.kill();
        let _ = child.wait();
    }
    s.close_job();
    s.active.store(false, Ordering::SeqCst);
}

// ---------- 进入安全模式 ----------

/// 进入安全模式的完整编排（在独立线程中执行；命令入口只做互斥检查）。
/// 顺序与产品需求一致：
///   A. 预检（不动日常实例）：更新中拒绝 / Node 与 DSH 可执行文件（与日常启动同一套
///      判定）/ 家目录路径策略 / 3081 空闲；
///   B. 预检（停日常）：日常实例完全退出（taskkill /T 带走 node 子进程）且日常端口
///      释放，失败则提示用户、绝不强行进入；
///   C. 基线重置（可配置，默认关闭）：开启时旧 .dsh-safe 归档为
///      .dsh-safe-archive-<时间戳> 并重建空目录，关闭时沿用已有目录；
///      凭据借用：只拷 .credentials.yaml（覆盖式）；
///   D. spawn `dsh web --port 3081 --no-open`（DSH_HOME / DSH_DAILY_HOME / 补全 PATH），
///      复用日常的日志读取与就绪等待，就绪后把 3081 页面内嵌进主窗口。
fn enter_safe_blocking(app: &AppHandle) -> Result<SafeReport, String> {
    let cfg = config::load(app);

    // ---- A. 预检：以下检查全部无状态副作用，失败时日常实例保持原样 ----
    if current_status(app) == "updating" {
        return Err(i18n::t("err_safe_updating").to_string());
    }
    // Node / DSH 可执行文件：与日常启动完全相同的判定与文案（helper 不 set_status，
    // 避免日常实例还在运行时把状态错标成 error）
    process::precheck_node(app, &cfg)?;
    let dsh_prog = resolve_dsh_prog(&cfg)?;
    let npm_for_path = resolve_npm_for_path(app, &cfg);
    // 日常家目录 = 修复目标（注入 DSH_DAILY_HOME）：先过路径策略，之后只用规范化值
    let daily_home = config::validate_home_dir(&cfg.dsh_home_dir)?;
    let safe_home = safe_home_dir()?;
    let safe_home_path = PathBuf::from(&safe_home);
    // 安全端口必须空闲：残留的安全实例或其他程序占用 3081 时拒绝进入（绝不强杀未知进程）。
    // 例外：用户把**日常端口**也配成了 3081 —— 那正是马上要停掉的实例，
    // 此时这次预检没有意义，交给下面的「日常端口已释放」检查兜底。
    if cfg.port != SAFE_PORT && port_in_use(SAFE_PORT) {
        return Err(i18n::fmt("err_safe_port_busy", &[&SAFE_PORT]));
    }

    // ---- B. 预检：日常实例必须完全退出、日常端口必须释放 ----
    let st = current_status(app);
    if matches!(st.as_str(), "running" | "starting" | "stopping") {
        emit_log(
            app,
            "launcher",
            i18n::t("log_safe_precheck_stop").to_string(),
        );
        process::stop_internal(app)?;
    } else if st == "running-external" {
        // 外部服务：本程序从不碰别人的进程，只解除连接；下面的端口检查会把它拦下
        destroy_dsh_webview(app);
        set_status(app, "idle", None);
    }
    if !wait_port_free(cfg.port, Duration::from_secs(10)) {
        let msg = i18n::fmt("err_safe_daily_busy", &[&cfg.port]);
        set_status(app, "error", Some(msg.clone()));
        return Err(msg);
    }
    emit_log(
        app,
        "launcher",
        i18n::fmt("log_safe_precheck_ok", &[&cfg.port, &SAFE_PORT]),
    );

    // ---- C. 基线重置（可配置，默认关闭：沿用已有安全环境）----
    let mut archived_to: Option<String> = None;
    if cfg.safe_reset_baseline {
        if safe_home_path.exists() {
            let ts = local_timestamp_compact();
            let parent = safe_home_path
                .parent()
                .map(|p| p.to_path_buf())
                .ok_or_else(|| i18n::t("err_safe_home").to_string())?;
            // 同一秒内重复进入也不会撞名（追加序号）；归档只重命名、绝不删除
            let mut archive = parent.join(format!("{}-archive-{}", SAFE_DIR_NAME, ts));
            let mut n = 1u32;
            while archive.exists() {
                archive = parent.join(format!("{}-archive-{}-{}", SAFE_DIR_NAME, ts, n));
                n += 1;
            }
            if let Err(e) = std::fs::rename(&safe_home_path, &archive) {
                let msg = i18n::fmt(
                    "err_safe_archive",
                    &[&safe_home, &archive.display().to_string(), &e.to_string()],
                );
                set_status(app, "error", Some(msg.clone()));
                return Err(msg);
            }
            archived_to = Some(archive.to_string_lossy().to_string());
            emit_log(
                app,
                "launcher",
                i18n::fmt("log_safe_archive", &[&archive.display().to_string()]),
            );
        }
    } else {
        emit_log(
            app,
            "launcher",
            i18n::fmt("log_safe_reset_off", &[&safe_home]),
        );
    }
    if let Err(e) = std::fs::create_dir_all(&safe_home_path) {
        let msg = i18n::fmt("err_safe_mkdir", &[&safe_home, &e.to_string()]);
        set_status(app, "error", Some(msg.clone()));
        return Err(msg);
    }

    // ---- C2. 凭据借用（只拷 .credentials.yaml；缺失/为空不阻塞，前端提示即可）----
    let (credential, credential_message) = borrow_credentials(&daily_home, &safe_home_path);
    if credential == "borrowed" {
        emit_log(
            app,
            "launcher",
            i18n::t("log_safe_cred_borrowed").to_string(),
        );
    } else {
        emit_log(
            app,
            "launcher",
            i18n::fmt("log_safe_cred_skip", &[&credential_message]),
        );
    }

    // ---- D. 启动安全实例（与日常启动同一套 spawn 包装）----
    let cwd = match config::cwd_of_home(&safe_home) {
        Ok(c) => c,
        Err(e) => {
            set_status(app, "error", Some(e.clone()));
            return Err(e);
        }
    };
    if !Path::new(&cwd).is_dir() {
        let msg = i18n::fmt("err_cwd_invalid", &[&cwd, &safe_home]);
        set_status(app, "error", Some(msg.clone()));
        return Err(msg);
    }

    // 清掉上一轮解析到的实际地址与 stderr 记忆（detected_url 为两种模式共享，
    // 与日常 start_internal 启动前的处理完全一致）
    let state = app.state::<AppState>();
    *state.detected_url.lock().unwrap() = None;
    let _ = state.take_last_stderr();

    // 命令固定为 dsh web --port 3081 --no-open：
    // 原厂基线，不带日常的 extra_args（那是日常环境的配置，可能正是被修复对象）
    let launch_args: Vec<String> = vec![
        "web".to_string(),
        "--port".to_string(),
        SAFE_PORT.to_string(),
        "--no-open".to_string(),
    ];
    let mut cmd = command_for(&dsh_prog, &launch_args).map_err(|e| {
        set_status(app, "error", Some(e.clone()));
        e
    })?;
    cmd.current_dir(&cwd);
    // DSH_HOME 指向纯净家目录；DSH_DAILY_HOME 只传路径（修复上下文），绝不含密钥
    cmd.env("DSH_HOME", &safe_home);
    cmd.env("DSH_DAILY_HOME", &daily_home);
    // dsh.cmd 是 npm 生成的 shim，回退分支同样依赖 PATH 里的 node（与日常启动同款）
    cmd.env(
        "PATH",
        detect::child_path_for(&[dsh_prog.as_str(), npm_for_path.as_str()]),
    );
    apply_no_window(&mut cmd);
    // stdin 不需要输入；stdout/stderr 必须 piped 转发到日志，绝不吞掉
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        let msg = i18n::fmt("err_spawn_fail", &[&e.to_string(), &dsh_prog]);
        set_status(app, "error", Some(msg.clone()));
        msg
    })?;
    let pid = child.id();

    // 输出逐行转发到同一个日志面板：完全复用日常的读取器
    // （GBK 无损解码、extract_local_url 解析认证 URL、desktop.log 落盘、dsh-log 事件）。
    // dsh_file 传 None：不在 .dsh-safe 里写任何文件，保持「除凭据外全空」的原厂基线；
    // 完整输出仍全部落 desktop.log。
    if let Some(so) = child.stdout.take() {
        spawn_log_reader(app.clone(), so, "stdout", "dsh-log", false, None, None);
    }
    if let Some(se) = child.stderr.take() {
        spawn_log_reader(app.clone(), se, "stderr", "dsh-log", true, None, None);
    }

    // Job Object 兜底：即使本程序被强杀，Windows 内核也会结束安全模式进程树
    let sstate = app.state::<SafeState>();
    #[cfg(windows)]
    {
        if let Some(j) = process::win::create_kill_on_close_job(pid) {
            *sstate.job.lock().unwrap() = Some(j);
        }
    }

    *sstate.child.lock().unwrap() = Some(child);
    *sstate.pid.lock().unwrap() = Some(pid);
    // 先置激活再发状态：set_status 的 overlay 会把 pid/port 换成安全实例的
    sstate.active.store(true, Ordering::SeqCst);
    set_status(app, "starting", None);

    // 窗口布局记忆：冻结日常布局（快照 + 落盘）并把窗口重置为 tauri.conf.json 的
    // 默认几何。放在 spawn 成功之后 —— 失败路径不该动用户的窗口。
    window_state::on_enter_safe_mode(app);
    emit_log(app, "launcher", i18n::t("log_safe_layout_frozen").to_string());

    let report = SafeReport {
        safe_home: safe_home.clone(),
        daily_home: daily_home.clone(),
        port: SAFE_PORT,
        archived_to,
        credential,
        credential_message,
        reset_baseline: cfg.safe_reset_baseline,
    };
    *sstate.report.lock().unwrap() = Some(report.clone());

    emit_log(
        app,
        "launcher",
        i18n::fmt(
            "log_safe_start_cmd",
            &[&dsh_prog, &SAFE_PORT, &cwd, &safe_home, &daily_home, &pid],
        ),
    );
    emit_log(
        app,
        "launcher",
        i18n::fmt("log_safe_entered", &[&safe_home, &SAFE_PORT, &daily_home]),
    );

    // 就绪等待：与日常模式同一个 wait_ready_and_embed（safe=true），
    // 就绪后把解析到的认证 URL（含 ?token=）内嵌进主窗口
    let app2 = app.clone();
    let timeout_secs = cfg.health_timeout_secs;
    let timeout = if timeout_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(timeout_secs.clamp(5, 3600)))
    };
    std::thread::spawn(move || {
        process::wait_ready_and_embed(&app2, SAFE_PORT, timeout, true);
    });
    Ok(report)
}

// ---------- 修复验证闭环 ----------

/// 退出安全模式后监控日常实例能否就绪：
/// - running / running-external → 验证通过；
/// - error / port-busy → 立即判失败；
/// - 超过 safe_verify_secs 仍未就绪 → 判失败；
/// - 用户已返回安全模式（或正在更新 DSH）→ 验证失去意义，静默退出。
/// 失败时发 safe-verify 事件，前端提示「修复可能未成功」并提供「返回安全模式」入口。
fn spawn_verify_thread(app: AppHandle, secs: u64) {
    std::thread::spawn(move || {
        let timeout = Duration::from_secs(secs.clamp(5, 3600));
        emit_log(
            &app,
            "launcher",
            i18n::fmt("log_safe_verify_start", &[&secs]),
        );
        let started = Instant::now();
        let fail = |app: &AppHandle, waited: u64| {
            emit_log(
                app,
                "launcher",
                i18n::fmt("log_safe_verify_fail", &[&waited]),
            );
            let _ = app.emit(
                "safe-verify",
                SafeVerifyEvent {
                    success: false,
                    message: i18n::fmt("msg_safe_verify_fail", &[&waited]),
                },
            );
        };
        loop {
            if is_active(&app) {
                return; // 用户已经返回安全模式
            }
            match current_status(&app).as_str() {
                "running" | "running-external" => {
                    emit_log(&app, "launcher", i18n::t("log_safe_verify_ok").to_string());
                    let _ = app.emit(
                        "safe-verify",
                        SafeVerifyEvent { success: true, message: String::new() },
                    );
                    return;
                }
                "error" | "port-busy" => {
                    fail(&app, started.elapsed().as_secs());
                    return;
                }
                "updating" => return, // 用户选择了更新 DSH：不打扰
                _ => {}
            }
            if started.elapsed() >= timeout {
                fail(&app, timeout.as_secs());
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

// ---------- Tauri Commands ----------

/// 进入安全模式（前端「安全模式」按钮）。
/// 同步只做互斥与状态检查；重活（停日常 → 归档 → 借凭据 → spawn）在独立线程执行，
/// 过程与结果通过 dsh-status / dsh-log / safe-mode-change 事件推给前端。
#[tauri::command]
pub async fn enter_safe_mode(app: AppHandle) -> Result<(), String> {
    {
        let s = app.state::<SafeState>();
        if s.active.load(Ordering::SeqCst) {
            return Err(i18n::t("err_safe_already").to_string());
        }
        if s.busy.swap(true, Ordering::SeqCst) {
            return Err(i18n::t("err_safe_busy").to_string());
        }
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        let result = enter_safe_blocking(&app2);
        app2.state::<SafeState>().busy.store(false, Ordering::SeqCst);
        match result {
            Ok(report) => {
                emit_safe_change(&app2, true, "entered", Some(report), None);
            }
            Err(e) => {
                emit_log(&app2, "launcher", i18n::fmt("log_safe_enter_fail", &[&e]));
                emit_safe_change(&app2, false, "failed", None, Some(e));
            }
        }
    });
    Ok(())
}

/// 退出安全模式并切回日常（前端「退出安全模式」按钮 / 验证失败弹窗的「返回」除外）。
/// kill 安全实例（含 Job Object 兜底）→ 按日常模式既有路径（start_internal）重启 →
/// 修复验证闭环（safe_verify_secs 内未就绪则提示「修复可能未成功」）。
#[tauri::command]
pub async fn exit_safe_mode(app: AppHandle) -> Result<(), String> {
    {
        let s = app.state::<SafeState>();
        if !s.active.load(Ordering::SeqCst) {
            return Err(i18n::t("err_safe_not_active").to_string());
        }
        if s.busy.swap(true, Ordering::SeqCst) {
            return Err(i18n::t("err_safe_busy").to_string());
        }
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        emit_log(
            &app2,
            "launcher",
            i18n::t("log_safe_exit_restart").to_string(),
        );
        // 1) 停止安全实例（taskkill 进程树 → 回收句柄 → 关 Job → 销毁内嵌页面）
        if let Err(e) = stop_safe_internal(&app2) {
            emit_log(&app2, "launcher", i18n::fmt("log_taskkill_fail", &[&e]));
        }
        emit_safe_change(&app2, false, "exited", None, None);
        // 窗口布局记忆：把日常布局写回窗口并落盘（安全模式期间的拖动/缩放到此作废）
        emit_log(
            &app2,
            "launcher",
            i18n::t("log_safe_layout_restored").to_string(),
        );
        window_state::on_exit_safe_mode(&app2);
        app2.state::<SafeState>().busy.store(false, Ordering::SeqCst);

        // 2) 等 3081 释放后再起日常实例（端口/文件交接避免竞态）
        let _ = wait_port_free(SAFE_PORT, Duration::from_secs(5));

        // 3) 按日常模式既有路径正常重启（含它自己的全部预检与端口检查）
        let cfg = config::load(&app2);
        let verify_secs = cfg.safe_verify_secs;
        match process::start_internal(&app2) {
            Ok(()) => {
                if verify_secs == 0 {
                    emit_log(
                        &app2,
                        "launcher",
                        i18n::t("log_safe_verify_skip").to_string(),
                    );
                } else {
                    spawn_verify_thread(app2.clone(), verify_secs);
                }
            }
            Err(e) => {
                // 日常实例连启动都失败：直接按「修复可能未成功」处理，
                // 前端弹窗提供「返回安全模式」入口
                emit_log(&app2, "launcher", i18n::fmt("log_safe_restart_fail", &[&e]));
                let _ = app2.emit(
                    "safe-verify",
                    SafeVerifyEvent { success: false, message: e },
                );
            }
        }
    });
    Ok(())
}

/// 查询安全模式当前状态（前端初始化时恢复工具栏/徽标/横幅内容）
#[tauri::command]
pub fn get_safe_status(app: AppHandle) -> SafeStatusReport {
    let Some(s) = app.try_state::<SafeState>() else {
        return SafeStatusReport { active: false, busy: false, report: None };
    };
    // 必须先取值、再在末尾构造返回值：tail expression 里的 MutexGuard 临时量会活到
    // 整个块结束，晚于 `s`（State 借用）被 drop 的时刻，直接写成 `report: s.report…`
    // 会触发 E0597（借用的值活得比 s 长），CI 上实测报错。
    let active = s.active.load(Ordering::SeqCst);
    let busy = s.busy.load(Ordering::SeqCst);
    let report = s.report.lock().unwrap().clone();
    SafeStatusReport { active, busy, report }
}
