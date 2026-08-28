use crate::config::{self, Config};
use crate::{detect, i18n, logger};
use serde::Serialize;
use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 主窗口顶部工具栏高度（逻辑像素），必须与前端 CSS 中 header 的 height 保持一致
pub const TOOLBAR_H: f64 = 43.2;

/// Windows Job Object：程序退出（含崩溃）时由内核结束整个 DSH 进程树，兜底防残留。
#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    pub struct JobHandle(pub HANDLE);
    // HANDLE 为裸指针，仅在本进程内使用，跨线程移动是安全的。
    unsafe impl Send for JobHandle {}

    pub fn create_kill_on_close_job(pid: u32) -> Option<JobHandle> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(job);
                return None;
            }
            let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if proc.is_null() {
                CloseHandle(job);
                return None;
            }
            let ok2 = AssignProcessToJobObject(job, proc);
            CloseHandle(proc);
            if ok2 == 0 {
                CloseHandle(job);
                return None;
            }
            Some(JobHandle(job))
        }
    }

    pub fn close_job(h: &JobHandle) {
        unsafe { CloseHandle(h.0) };
    }
}

/// 全局运行状态（由 Tauri manage 注入）
pub struct AppState {
    child: Mutex<Option<Child>>,
    pid: Mutex<Option<u32>>,
    status: Mutex<String>,
    pub updating: AtomicBool,
    /// DSH 崩溃前最后一条 stderr 输出（用于在状态区显示单行错误原因）
    last_stderr: Mutex<Option<String>>,
    /// 从 DSH 输出中解析出的实际监听地址 (url, port)；就绪后优先按它加载页面
    pub detected_url: Mutex<Option<(String, u16)>>,
    /// 首次运行引导安装互斥标志（同一时间只允许一个引导任务）
    pub setup_busy: AtomicBool,
    /// 当前进程是否由「开机自启」触发（main.rs 检测 --autostart 参数后置 true）。
    /// start_internal 消费此标志后置回 false，避免重复延迟。
    pub launched_by_autostart: AtomicBool,
    #[cfg(windows)]
    job: Mutex<Option<win::JobHandle>>,
    #[cfg(windows)]
    update_job: Mutex<Option<win::JobHandle>>,
}

impl AppState {
    pub fn new() -> Self {
        Self::with_autostart(false)
    }

    pub fn with_autostart(launched_by_autostart: bool) -> Self {
        Self {
            child: Mutex::new(None),
            pid: Mutex::new(None),
            status: Mutex::new("idle".to_string()),
            updating: AtomicBool::new(false),
            last_stderr: Mutex::new(None),
            detected_url: Mutex::new(None),
            setup_busy: AtomicBool::new(false),
            launched_by_autostart: AtomicBool::new(launched_by_autostart),
            #[cfg(windows)]
            job: Mutex::new(None),
            #[cfg(windows)]
            update_job: Mutex::new(None),
        }
    }

    fn close_jobs(&self) {
        #[cfg(windows)]
        {
            if let Some(j) = self.job.lock().unwrap().take() {
                win::close_job(&j);
            }
        }
        #[cfg(not(windows))]
        {
            let _ = self;
        }
    }

    fn close_update_job(&self) {
        #[cfg(windows)]
        {
            if let Some(j) = self.update_job.lock().unwrap().take() {
                win::close_job(&j);
            }
        }
        #[cfg(not(windows))]
        {
            let _ = self;
        }
    }

    fn set_last_stderr(&self, line: &str) {
        *self.last_stderr.lock().unwrap() = Some(line.to_string());
    }

    fn take_last_stderr(&self) -> Option<String> {
        self.last_stderr.lock().unwrap().take()
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------- 事件 payload ----------

#[derive(Clone, Serialize)]
pub struct LogEvent {
    pub stream: String, // stdout | stderr | launcher | update
    pub line: String,
}

#[derive(Clone, Serialize)]
pub struct StatusEvent {
    pub status: String, // idle | starting | running | running-external | stopping | error | port-busy | updating
    pub pid: Option<u32>,
    pub port: u16,
    pub message: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct UpdateFinished {
    pub success: bool,
    pub message: String,
}

/// 首次运行引导安装：阶段性进度（download | install | verify）
#[derive(Clone, Serialize)]
pub struct SetupStatus {
    pub phase: String,
    pub message: String,
}

/// 首次运行引导安装：最终结果（target = node | dsh）
#[derive(Clone, Serialize)]
pub struct SetupResult {
    pub target: String,
    pub success: bool,
    pub message: String,
}

/// 端口可用性检查结果
#[derive(Serialize)]
pub struct PortCheck {
    pub port: u16,
    pub in_use: bool,
}

#[derive(Serialize, Clone)]
pub struct VersionInfo {
    pub local: Option<String>,
    pub latest: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct ConfigReport {
    #[serde(flatten)]
    pub config: Config,
    pub dsh_exists: bool,
    pub npm_exists: bool,
    pub home_exists: bool,
    pub config_path: String,
    /// 首次运行（%APPDATA%\com.dsh.desktop\config.json 尚不存在）：前端据此显示安装引导
    pub first_run: bool,
}

// ---------- 工具函数 ----------

/// Windows 控制台输出可能是 UTF-8（node）或 GBK（taskkill 等系统命令），做无损解码。
pub(crate) fn decode_console_output(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let (cow, _, _) = encoding_rs::GBK.decode(bytes);
    cow.into_owned()
}

/// 统一日志出口：先镜像写入 %APPDATA%\com.dsh.desktop\desktop.log，再发到前端「日志」面板
pub fn emit_log(app: &AppHandle, stream: &str, line: String) {
    logger::append_line(&logger::desktop_log_path(app), &line);
    let _ = app.emit("dsh-log", LogEvent { stream: stream.to_string(), line });
}

/// 供 lib.rs（托盘回调）等其他模块写 launcher 日志，同样落盘
pub fn log_launcher(app: &AppHandle, line: &str) {
    emit_log(app, "launcher", line.to_string());
}

fn set_status(app: &AppHandle, status: &str, message: Option<String>) {
    let state = app.state::<AppState>();
    *state.status.lock().unwrap() = status.to_string();
    let cfg = config::load(app);
    let pid = *state.pid.lock().unwrap();
    let _ = app.emit(
        "dsh-status",
        StatusEvent { status: status.to_string(), pid, port: cfg.port, message },
    );
}

fn current_status(app: &AppHandle) -> String {
    app.state::<AppState>().status.lock().unwrap().clone()
}

fn port_in_use(port: u16) -> bool {
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// 规范化的本机服务地址
fn local_url(port: u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

/// 从 DSH 输出行中提取本机监听地址，如：
///   `dsh web: http://127.0.0.1:3080`、`Local: http://localhost:3000/`
/// 返回 (规范化URL, 端口)。只接受 127.0.0.1 / localhost。
fn extract_local_url(line: &str) -> Option<(String, u16)> {
    const NEEDLE: &str = "http://";
    let mut rest = line;
    while let Some(i) = rest.find(NEEDLE) {
        let s = &rest[i..];
        let end = s
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(s.len());
        let url = &s[..end];
        let host = url.trim_start_matches("http://");
        let authority = host.split('/').next().unwrap_or("");
        let mut it = authority.splitn(2, ':');
        let hostname = it.next().unwrap_or("");
        let port_str = it.next().unwrap_or("");
        if let Ok(p) = port_str.parse::<u16>() {
            if p > 0 && (hostname == "127.0.0.1" || hostname.eq_ignore_ascii_case("localhost")) {
                return Some((local_url(p), p));
            }
        }
        rest = &rest[i + NEEDLE.len()..];
    }
    None
}

/// 端口可连接后，再发一个轻量 HTTP GET 确认服务真正能响应（任意响应码都算就绪）。
fn http_ready(addr: &SocketAddr) -> bool {
    use std::io::Write;
    let Ok(mut stream) = TcpStream::connect_timeout(addr, Duration::from_millis(900)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let req = format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUser-Agent: DSH-Desktop/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 128];
    matches!(stream.read(&mut buf), Ok(n) if n > 0)
}

/// 结束整个进程树：taskkill /PID <pid> /T /F（绝不使用 /IM 误杀其他程序）
fn run_taskkill(pid: u32) -> Result<String, String> {
    let mut cmd = Command::new("taskkill");
    cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
    apply_no_window(&mut cmd);
    cmd.stdin(Stdio::null());
    match cmd.output() {
        Ok(out) => {
            let mut s = decode_console_output(&out.stdout);
            let se = decode_console_output(&out.stderr);
            if !se.trim().is_empty() {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(&se);
            }
            if out.status.success() {
                Ok(s.trim().to_string())
            } else {
                Err(s.trim().to_string())
            }
        }
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(windows)]
pub(crate) fn apply_no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub(crate) fn apply_no_window(_cmd: &mut Command) {}

/// 子进程输出逐行读取并转发到前端日志面板（绝不吞掉 stdout/stderr）。
/// 同时落盘：所有行 → desktop.log；DSH 进程的 stdout/stderr 额外 → <DSH家目录>\logs\dsh.log
/// `fetch_counter`：可选，每读到一行 npm 的 http fetch 记录就 +1，
/// 供引导安装 DSH 时在界面上显示「已下载 N 个包文件」的进度。
fn spawn_log_reader(
    app: AppHandle,
    out: impl Read + Send + 'static,
    stream: &'static str,
    event: &'static str,
    track_stderr: bool,
    dsh_file: Option<PathBuf>,
    fetch_counter: Option<std::sync::Arc<AtomicUsize>>,
) {
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        let desktop_log = logger::desktop_log_path(&app);
        let mut reader = BufReader::new(out);
        // 必须按字节切行再解码：中文 Windows 上 cmd.exe / npm 会把子命令的错误
        // 信息（如「'node' 不是内部或外部命令」）以 GBK 写进 stderr。
        // BufRead::lines() 要求整行是合法 UTF-8，遇到 GBK 字节返回 Err —— 原来的
        // `Err(_) => break` 会让整条日志流在最关键的那一行**直接中断**，
        // 用户只看到半截 npm error、看不到真正原因。这里统一走 decode_console_output。
        loop {
            let mut raw: Vec<u8> = Vec::new();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) => break,
                Ok(_) => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
            while matches!(raw.last(), Some(b'\n') | Some(b'\r')) {
                raw.pop();
            }
            if raw.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            let l = decode_console_output(&raw);

            // npm 的 http 日志行（loglevel=http 时形如 "npm http fetch GET 200 …"），
            // 每行代表一次 registry 请求 → 作为「已下载包文件数」的进度依据
            if let Some(c) = fetch_counter.as_ref() {
                if l.contains("http fetch") {
                    c.fetch_add(1, Ordering::Relaxed);
                }
            }

            if track_stderr && stream == "stderr" {
                state.set_last_stderr(&l);
            }
            // 从 DSH 输出中解析实际监听地址（如 "dsh web: http://127.0.0.1:3080"），
            // 就绪后优先按实际地址加载页面（要求一.8）
            if matches!(stream, "stdout" | "stderr") {
                if let Some((url, port)) = extract_local_url(&l) {
                    // 先更新并释放锁，再发日志（避免持锁期间的跨线程操作）
                    let changed = {
                        let mut guard = state.detected_url.lock().unwrap();
                        let changed = guard.as_ref().map(|(_, p)| *p) != Some(port);
                        *guard = Some((url.clone(), port));
                        changed
                    };
                    if changed {
                        emit_log(
                            &app,
                            "launcher",
                            i18n::fmt("log_detected_url", &[&url]),
                        );
                    }
                }
            }
            // 落盘（失败忽略，不影响主流程）
            if let Some(f) = &dsh_file {
                logger::append_line(f, &l);
            }
            logger::append_line(&desktop_log, &l);
            let _ = app.emit(event, LogEvent { stream: stream.to_string(), line: l });
        }
    });
}

/// 读取整个管道（进程退出后调用）
fn drain_pipe(r: &mut Option<impl Read>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(r) = r.as_mut() {
        let _ = r.read_to_end(&mut buf);
    }
    buf
}

/// 带超时地运行 cmd /C <program> <args...> 并捕获输出（用于版本查询等小输出命令）
fn run_cmd_capture(
    program: &str,
    args: &[String],
    cwd: &str,
    timeout: Duration,
) -> Result<(bool, String), String> {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(program);
    for a in args {
        cmd.arg(a);
    }
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    apply_no_window(&mut cmd);
    // 显式补全 PATH：本进程 PATH 可能是装 Node 之前的旧快照，
    // 否则 dsh.cmd / npm.cmd 派生的脚本里的裸 `node` 会找不到。
    cmd.env("PATH", detect::child_path_for(&[program]));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| i18n::fmt("err_cmd_spawn", &[&program.to_string(), &e.to_string()]))?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut text = decode_console_output(&drain_pipe(&mut stdout));
                let err_text = decode_console_output(&drain_pipe(&mut stderr));
                if !err_text.trim().is_empty() {
                    if !text.trim().is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&err_text);
                }
                return Ok((status.success(), text));
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = drain_pipe(&mut stdout);
                    let _ = drain_pipe(&mut stderr);
                    let _ = child.wait();
                    return Err(i18n::fmt("err_cmd_timeout", &[&timeout.as_secs()]));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(i18n::fmt("err_cmd_wait", &[&e.to_string()])),
        }
    }
}

// ---------- DSH Webview（内嵌在主窗口工具栏下方） ----------

/// 主窗口内容区的逻辑尺寸（宽, 高-工具栏）
fn main_content_size(app: &AppHandle) -> (f64, f64) {
    if let Some(win) = app.get_webview_window("main") {
        if let (Ok(scale), Ok(size)) = (win.scale_factor(), win.inner_size()) {
            let logical: tauri::LogicalSize<f64> = size.to_logical(scale);
            return (logical.width, (logical.height - TOOLBAR_H).max(0.0));
        }
    }
    (1024.0, 640.0)
}

/// 在主窗口内创建（或刷新）内嵌的 DSH Webview。
/// `url` 由调用方决定：优先用从 DSH 输出解析到的实际地址，否则用配置端口构造。
fn open_dsh_webview(app: &AppHandle, url: &str) {
    // 已存在：直接刷新到当前 URL（端口可能已变更）
    if let Some(wv) = app.get_webview("dsh") {
        sync_dsh_webview_size(app);
        let _ = wv.show();
        let _ = wv.eval(&format!("window.location.replace('{}')", url));
        return;
    }

    // add_child 是 Window 的方法：取主窗口的 Window 句柄（unstable API）
    let Some(win) = app.get_window("main") else {
        emit_log(app, "launcher", i18n::t("log_no_main_window").to_string());
        return;
    };
    let (w, h) = main_content_size(app);
    let Ok(parsed) = url.parse::<tauri::Url>() else {
        emit_log(app, "launcher", i18n::fmt("log_invalid_url", &[&url.to_string()]));
        return;
    };
    let result = win.add_child(
        tauri::WebviewBuilder::new(
            "dsh",
            tauri::WebviewUrl::External(parsed),
        ),
        tauri::LogicalPosition::new(0.0, TOOLBAR_H),
        tauri::LogicalSize::new(w, h),
    );
    if let Err(e) = result {
        emit_log(app, "launcher", i18n::fmt("log_embed_fail", &[&e.to_string()]));
    }
}

/// 窗口尺寸变化时同步内嵌 webview 大小（由主窗口 Resized 事件调用）
pub fn sync_dsh_webview_size(app: &AppHandle) {
    let Some(wv) = app.get_webview("dsh") else { return };
    let Some(win) = app.get_webview_window("main") else { return };
    let Ok(scale) = win.scale_factor() else { return };
    if let Ok(size) = win.inner_size() {
        let logical: tauri::LogicalSize<f64> = size.to_logical(scale);
        let _ = wv.set_size(tauri::LogicalSize::new(
            logical.width,
            (logical.height - TOOLBAR_H).max(0.0),
        ));
    }
}

/// 销毁内嵌的 DSH Webview（服务停止后露出状态区）
fn destroy_dsh_webview(app: &AppHandle) {
    if let Some(wv) = app.get_webview("dsh") {
        let _ = wv.close();
    }
}

/// 模态框（设置/日志等）打开时隐藏内嵌 webview，避免被遮挡
#[tauri::command]
pub fn set_dsh_webview_visible(app: AppHandle, visible: bool) -> Result<(), String> {
    if let Some(wv) = app.get_webview("dsh") {
        let _ = if visible { wv.show() } else { wv.hide() };
    }
    Ok(())
}

/// 只刷新内嵌的 DSH 页面，不停止/重启 DSH 服务。
/// 保留当前地址（http://127.0.0.1:<配置端口>），不会重新走启动流程。
#[tauri::command]
pub fn refresh_dsh_page(app: AppHandle) -> Result<(), String> {
    let st = current_status(&app);
    if st != "running" && st != "running-external" {
        return Err(i18n::t("err_not_running_refresh").to_string());
    }

    if let Some(wv) = app.get_webview("dsh") {
        // 优先走 WebView 的 reload：window.location.reload() 保留当前 URL
        wv.eval("window.location.reload()")
            .map_err(|e| format!("Failed to reload DSH page: {}", e))?;
        emit_log(&app, "launcher", i18n::t("log_refreshed_page").to_string());
        return Ok(());
    }

    // 服务在运行但页面不存在（例如之前被销毁）：按当前配置端口重新打开页面（不重启服务）
    let cfg = config::load(&app);
    open_dsh_webview(&app, &local_url(cfg.port));
    emit_log(&app, "launcher", i18n::t("log_reopened_page").to_string());
    Ok(())
}

// ---------- 启动 / 停止核心 ----------

fn start_internal(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let st = current_status(app);
    if matches!(
        st.as_str(),
        "starting" | "running" | "running-external" | "stopping" | "updating"
    ) {
        return Err(i18n::fmt("err_status_locked", &[&st]));
    }

    // 开机自启触发：先延迟 12 秒错开系统冷启动高峰（IO 拥堵、Node/网络未就绪极易超时）
    if state.launched_by_autostart.swap(false, Ordering::SeqCst) {
        const AUTOSTART_DELAY_SECS: u64 = 12;
        emit_log(
            app,
            "launcher",
            i18n::fmt("log_autostart_wait", &[&AUTOSTART_DELAY_SECS]),
        );
        set_status(
            app,
            "starting",
            Some(i18n::fmt("status_autostart_delay", &[&AUTOSTART_DELAY_SECS])),
        );
        // 分段 sleep，期间允许用户点击「停止」取消延迟
        let total = Duration::from_secs(AUTOSTART_DELAY_SECS);
        let step = Duration::from_millis(500);
        let mut elapsed = Duration::ZERO;
        while elapsed < total {
            std::thread::sleep(step);
            elapsed += step;
            let cur = current_status(app);
            if cur != "starting" {
                emit_log(app, "launcher", i18n::t("log_autostart_cancelled").to_string());
                return Ok(());
            }
        }
        emit_log(app, "launcher", i18n::t("log_autostart_resume").to_string());
    }

    let cfg = config::load(app);

    // ---- 前置检查：依赖缺失时立即给出明确错误，绝不无限等待（要求三.3 / 四.5） ----

    // 1) Node.js：DSH 是 Node 程序，缺 node 必然失败
    {
        let env = detect::detect_all(false);
        if env.node.is_none() {
            let msg = i18n::t("err_node_missing").to_string();
            set_status(app, "error", Some(msg.clone()));
            return Err(msg);
        }
    }

    // 2) DSH 可执行文件（自动检测失败时允许用户在设置中手动选择）
    if cfg.dsh_path.trim().is_empty() || !Path::new(&cfg.dsh_path).is_file() {
        let msg = if cfg.dsh_path.trim().is_empty() {
            i18n::fmt("err_dsh_missing_auto", &[&cfg.package_name])
        } else {
            i18n::fmt("err_dsh_missing", &[&cfg.dsh_path, &cfg.package_name])
        };
        set_status(app, "error", Some(msg.clone()));
        return Err(msg);
    }

    // 3) npm 缺失只影响更新 / 版本查询，不阻止启动
    if cfg.npm_path.trim().is_empty() || !Path::new(&cfg.npm_path).is_file() {
        emit_log(
            app,
            "launcher",
            i18n::t("log_npm_missing_hint").to_string(),
        );
    }

    let cwd = config::workspace_cwd(&cfg);
    if !Path::new(&cwd).is_dir() {
        let msg = i18n::fmt("err_cwd_invalid", &[&cwd, &cfg.dsh_home_dir]);
        set_status(app, "error", Some(msg.clone()));
        return Err(msg);
    }

    // 端口占用检查：绝不强杀未知进程，交给用户决策（可改端口 / 重检 / 连接现有服务）
    if port_in_use(cfg.port) {
        let msg = i18n::fmt("err_port_busy", &[&cfg.port]);
        set_status(app, "port-busy", Some(msg.clone()));
        return Err(msg);
    }

    // 本轮启动前清空上一次解析到的实际地址
    *state.detected_url.lock().unwrap() = None;

    // 启动命令等价于：cmd /C "<dsh_path>" web --port <port> --no-open [extra_args]
    // --no-open：DSH 官方参数，禁止其自动打开默认浏览器（Edge）
    let mut cmd = Command::new("cmd");
    cmd.arg("/C")
        .arg(&cfg.dsh_path)
        .arg("web")
        .arg("--port")
        .arg(cfg.port.to_string())
        .arg("--no-open");
    for a in cfg.extra_args.split_whitespace() {
        cmd.arg(a);
    }
    // 工作目录：DSH 家目录的上一级；DSH_HOME 指向家目录（DSH 在此读取配置）
    cmd.current_dir(&cwd);
    cmd.env("DSH_HOME", &cfg.dsh_home_dir);
    // dsh.cmd 是 npm 生成的 shim，回退分支同样依赖 PATH 里的 node
    cmd.env(
        "PATH",
        detect::child_path_for(&[cfg.dsh_path.as_str(), cfg.npm_path.as_str()]),
    );
    apply_no_window(&mut cmd);
    // stdin 不需要输入；stdout/stderr 必须 piped 转发到日志，绝不吞掉
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let _ = state.take_last_stderr();
    let mut child = cmd
        .spawn()
        .map_err(|e| i18n::fmt("err_spawn_fail", &[&e.to_string(), &cfg.dsh_path]))?;
    let pid = child.id();

    // DSH 输出同时镜像写入 <DSH 家目录>\logs\dsh.log（要求四.2）
    let dsh_log_file = logger::dsh_log_path(&cfg.dsh_home_dir);
    if let Some(so) = child.stdout.take() {
        spawn_log_reader(app.clone(), so, "stdout", "dsh-log", false, Some(dsh_log_file.clone()), None);
    }
    if let Some(se) = child.stderr.take() {
        spawn_log_reader(app.clone(), se, "stderr", "dsh-log", true, Some(dsh_log_file), None);
    }

    // Job Object 兜底：即使本程序异常退出，Windows 内核也会结束 DSH 进程树
    #[cfg(windows)]
    {
        if let Some(j) = win::create_kill_on_close_job(pid) {
            *state.job.lock().unwrap() = Some(j);
        }
    }

    *state.child.lock().unwrap() = Some(child);
    *state.pid.lock().unwrap() = Some(pid);
    set_status(app, "starting", None);

    emit_log(
        app,
        "launcher",
        i18n::fmt(
            "log_start_cmd",
            &[
                &cfg.dsh_path,
                &cfg.port,
                &if cfg.extra_args.trim().is_empty() {
                    String::new()
                } else {
                    format!(" {}", cfg.extra_args)
                },
                &cwd,
                &cfg.dsh_home_dir,
                &pid,
                &logger::dsh_log_path(&cfg.dsh_home_dir).display().to_string(),
            ],
        ),
    );

    let app2 = app.clone();
    let port = cfg.port;
    // 0 = 一直等待（只要 DSH 进程还活着）；否则限制在 5 秒 ~ 1 小时之间
    let timeout_secs = cfg.health_timeout_secs;
    let timeout = if timeout_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(timeout_secs.clamp(5, 3600)))
    };
    std::thread::spawn(move || {
        wait_ready_and_embed(&app2, port, timeout);
    });
    Ok(())
}

/// 等待 DSH 就绪：同时监控子进程存活 + 轮询 HTTP；就绪后才把 DSH 页面内嵌进主窗口。
/// 若 DSH 输出中解析到实际监听地址（如 `dsh web: http://127.0.0.1:3080`），
/// 则以实际地址为准轮询并加载。
fn wait_ready_and_embed(app: &AppHandle, port: u16, timeout: Option<Duration>) {
    let state = app.state::<AppState>();
    let started = Instant::now();
    emit_log(
        app,
        "launcher",
        i18n::fmt(
            "log_wait_ready",
            &[
                &port,
                &match timeout {
                    Some(t) => i18n::fmt("wait_suffix_timeout", &[&t.as_secs()]),
                    None => i18n::t("wait_suffix_infinite").to_string(),
                },
            ],
        ),
    );
    loop {
        // 0) 实际监听地址优先（要求一.8）：输出中出现则切换轮询/加载目标
        let detected = state.detected_url.lock().unwrap().clone();
        let (target_url, poll_port) = match &detected {
            Some((url, dp)) => (url.clone(), *dp),
            None => (local_url(port), port),
        };
        let addr: SocketAddr = format!("127.0.0.1:{}", poll_port).parse().unwrap();

        // 1) 监控子进程存活：DSH 若在端口就绪前闪退，立即终止等待并显示错误
        {
            let mut guard = state.child.lock().unwrap();
            if let Some(child) = guard.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        guard.take();
                        drop(guard);
                        *state.pid.lock().unwrap() = None;
                        state.close_jobs();
                        let last_err = state.take_last_stderr();
                        let code = format!("{:?}", status.code());
                        emit_log(
                            app,
                            "launcher",
                            i18n::fmt("log_exit_before_ready", &[&code]),
                        );
                        let mut msg = i18n::fmt("err_exit_before_ready", &[&code]);
                        if let Some(e) = last_err {
                            msg.push_str("：");
                            msg.push_str(&e);
                        }
                        set_status(app, "error", Some(msg));
                        return;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        emit_log(app, "launcher", i18n::fmt("log_proc_check_fail", &[&e.to_string()]));
                    }
                }
            } else {
                // 进程句柄已被停止操作清空，无需继续轮询
                return;
            }
        }

        // 2) HTTP 就绪检查
        if http_ready(&addr) {
            emit_log(
                app,
                "launcher",
                i18n::fmt(
                    "log_ready_embed",
                    &[&format!("{:.0}", started.elapsed().as_secs_f64()), &target_url],
                ),
            );
            set_status(app, "running", None);
            open_dsh_webview(app, &target_url);
            return;
        }

        // 3) 超时（仅当配置了超时时间；0 表示无限等待）
        if let Some(t) = timeout {
            if started.elapsed() >= t {
                emit_log(
                    app,
                    "launcher",
                    i18n::fmt("log_timeout_stop", &[&t.as_secs()]),
                );
                let _ = stop_internal(app);
                set_status(
                    app,
                    "error",
                    Some(i18n::fmt("err_timeout", &[&t.as_secs()])),
                );
                return;
            }
        }

        std::thread::sleep(Duration::from_millis(500));
    }
}

/// 停止本次启动的 DSH 进程树（幂等；对"外部进程"模式只重置状态，绝不碰别人的进程）
fn stop_internal(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let pid = state.pid.lock().unwrap().take();
    let child = state.child.lock().unwrap().take();

    if pid.is_none() && child.is_none() {
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

    // 回收子进程句柄（最多等待 3 秒，避免异常情况下卡住退出流程）
    if let Some(mut child) = child {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    state.close_jobs();
    // 服务停止后销毁内嵌页面，露出状态区
    destroy_dsh_webview(app);
    set_status(app, "idle", None);
    emit_log(app, "launcher", i18n::t("log_stopped").to_string());
    Ok(())
}

// ---------- Tauri Commands ----------

#[tauri::command]
pub fn get_config(app: AppHandle) -> ConfigReport {
    let cfg = config::load(&app);
    let first_run = !config::config_path(&app).exists();
    ConfigReport {
        dsh_exists: Path::new(&cfg.dsh_path).is_file(),
        npm_exists: Path::new(&cfg.npm_path).is_file(),
        home_exists: Path::new(&cfg.dsh_home_dir).is_dir(),
        config_path: config::config_path(&app).to_string_lossy().to_string(),
        first_run,
        config: cfg,
    }
}

#[tauri::command]
pub fn save_config(app: AppHandle, config: Config) -> Result<ConfigReport, String> {
    // 端口必须是 1~65535 的数字（要求一.5）
    if !(1..=65535).contains(&config.port) {
        return Err(i18n::t("err_port_invalid").to_string());
    }
    if config.dsh_home_dir.trim().is_empty() {
        return Err(i18n::t("err_home_empty").to_string());
    }
    if config.dsh_path.trim().is_empty() || config.npm_path.trim().is_empty() {
        return Err(i18n::t("err_paths_empty").to_string());
    }
    let old_lang = config::load(&app).language;
    config::save(&app, &config)?;

    // 语言切换即时生效：本会话后续的 launcher 日志、托盘菜单文字（前端自行切换界面文案）。
    // DSH 自身界面语言通过 settings.yaml 联动，需 DSH 重启后变化。
    i18n::set_lang(&config.language);
    crate::refresh_tray_texts(&app);
    // 同步 DSH 家目录 settings.yaml → locale.preference
    match config::sync_dsh_locale(&config.dsh_home_dir, &config.language) {
        Ok(()) => emit_log(
            &app,
            "launcher",
            i18n::fmt("log_locale_synced", &[&config.language]),
        ),
        Err(e) => emit_log(
            &app,
            "launcher",
            i18n::fmt("err_locale_sync_fail", &[&e]),
        ),
    }
    if old_lang != config.language {
        emit_log(
            &app,
            "launcher",
            i18n::fmt("log_lang_changed", &[&config.language]),
        );
    }

    let report = get_config(app.clone());
    emit_log(
        &app,
        "launcher",
        i18n::fmt("log_config_saved", &[&report.config_path]),
    );
    Ok(report)
}

#[tauri::command]
pub fn get_status(app: AppHandle) -> StatusEvent {
    let state = app.state::<AppState>();
    let cfg = config::load(&app);
    let status = state.status.lock().unwrap().clone();
    let pid = *state.pid.lock().unwrap();
    StatusEvent {
        status,
        pid,
        port: cfg.port,
        message: None,
    }
}

#[tauri::command]
pub async fn start_dsh(app: AppHandle) -> Result<(), String> {
    start_internal(&app)
}

#[tauri::command]
pub async fn stop_dsh(app: AppHandle) -> Result<(), String> {
    let st = current_status(&app);
    if st == "running-external" {
        // 外部进程不由本程序管理，只解除连接状态并收起页面
        destroy_dsh_webview(&app);
        set_status(&app, "idle", None);
        emit_log(&app, "launcher", i18n::t("log_disconnected_external").to_string());
        return Ok(());
    }
    stop_internal(&app)
}

#[tauri::command]
pub async fn restart_dsh(app: AppHandle) -> Result<(), String> {
    let st = current_status(&app);
    if st != "running" && st != "running-external" {
        return Err(i18n::t("err_restart_not_running").to_string());
    }
    if st == "running-external" {
        destroy_dsh_webview(&app);
        set_status(&app, "idle", None);
    } else {
        stop_internal(&app)?;
    }
    std::thread::sleep(Duration::from_millis(400));
    start_internal(&app)
}

/// 端口被占用时：连接到现有服务（不接管、不停止该进程）
#[tauri::command]
pub async fn connect_existing(app: AppHandle) -> Result<(), String> {
    let cfg = config::load(&app);
    if !port_in_use(cfg.port) {
        return Err(i18n::fmt("err_connect_no_listener", &[&cfg.port]));
    }
    emit_log(
        &app,
        "launcher",
        i18n::fmt("log_connect_existing", &[&cfg.port]),
    );
    set_status(&app, "running-external", None);
    open_dsh_webview(&app, &local_url(cfg.port));
    Ok(())
}

/// 版本检测：本地依次尝试 --version / -v / -V；最新版本用 npm view <pkg> version
#[tauri::command]
pub async fn check_versions(app: AppHandle) -> Result<VersionInfo, String> {
    let cfg = config::load(&app);
    let cwd = config::workspace_cwd(&cfg);
    let mut info = VersionInfo { local: None, latest: None, error: None };

    if Path::new(&cfg.dsh_path).is_file() {
        for flag in ["--version", "-v", "-V"] {
            match run_cmd_capture(
                &cfg.dsh_path,
                &[flag.to_string()],
                &cwd,
                Duration::from_secs(20),
            ) {
                Ok((true, out)) => {
                    let v = out
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .map(str::to_string);
                    if let Some(v) = v {
                        info.local = Some(v);
                        break;
                    }
                }
                _ => continue,
            }
        }
        if info.local.is_none() {
            info.error = Some(i18n::t("err_ver_flags").to_string());
        }
    } else {
        info.error = Some(i18n::fmt("err_ver_no_dsh", &[&cfg.dsh_path]));
    }

    if Path::new(&cfg.npm_path).is_file() {
        match run_cmd_capture(
            &cfg.npm_path,
            &[
                "view".to_string(),
                cfg.package_name.clone(),
                "version".to_string(),
            ],
            &cwd,
            Duration::from_secs(60),
        ) {
            Ok((true, out)) => {
                let v = out.trim();
                if !v.is_empty() {
                    info.latest = Some(v.to_string());
                } else {
                    let e = i18n::fmt("err_ver_view_empty", &[&cfg.package_name]);
                    info.error = Some(join_err(info.error, &e));
                }
            }
            Ok((false, out)) => {
                let e = i18n::fmt("err_view_fail", &[&out.trim()]);
                info.error = Some(join_err(info.error, &e));
            }
            Err(e) => {
                info.error = Some(join_err(info.error, &e));
            }
        }
    } else {
        let e = i18n::fmt("err_no_npm_view", &[&cfg.npm_path]);
        info.error = Some(join_err(info.error, &e));
    }

    Ok(info)
}

fn join_err(a: Option<String>, b: &str) -> String {
    let sep = if i18n::is_en() { "; " } else { "；" };
    match a {
        Some(x) => format!("{}{}{}", x, sep, b),
        None => b.to_string(),
    }
}

/// 检测全局安装的 DSH 包名（npm list -g --depth=0）
#[tauri::command]
pub async fn detect_npm_package(app: AppHandle) -> Result<String, String> {
    let cfg = config::load(&app);
    if !Path::new(&cfg.npm_path).is_file() {
        return Err(i18n::fmt("err_no_npm_detect", &[&cfg.npm_path]));
    }
    let cwd = config::workspace_cwd(&cfg);
    let (ok, out) = run_cmd_capture(
        &cfg.npm_path,
        &["list".to_string(), "-g".to_string(), "--depth=0".to_string()],
        &cwd,
        Duration::from_secs(60),
    )?;
    let text = out.trim().to_string();
    let mut found: Vec<String> = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.contains("dsh") && (l.contains("--") || l.starts_with("├") || l.starts_with("└")) {
            found.push(l.to_string());
        }
    }
    if ok {
        if found.is_empty() {
            Ok(i18n::fmt("log_pkg_none", &[&text]))
        } else {
            Ok(i18n::fmt("log_pkg_found", &[&found.join("\n"), &text]))
        }
    } else {
        Ok(i18n::fmt("err_pkg_list_fail", &[&text]))
    }
}

/// 更新 DSH：先停止本程序启动的 DSH → 执行 npm install -g <pkg>@latest → 实时转发输出 → 按退出码报告结果
#[tauri::command]
pub async fn update_dsh(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    if state.updating.swap(true, Ordering::SeqCst) {
        return Err(i18n::t("err_update_busy").to_string());
    }

    let cfg = config::load(&app);
    if !Path::new(&cfg.npm_path).is_file() {
        state.updating.store(false, Ordering::SeqCst);
        let msg = i18n::fmt("err_no_npm_update", &[&cfg.npm_path]);
        set_status(&app, "error", Some(msg.clone()));
        return Err(msg);
    }

    // 停止本程序启动的 DSH（外部进程模式不动）
    let st = current_status(&app);
    if st == "running" || st == "starting" {
        let _ = stop_internal(&app);
    } else if st == "running-external" {
        destroy_dsh_webview(&app);
        set_status(&app, "idle", None);
    }

    set_status(&app, "updating", None);
    let cwd = config::workspace_cwd(&cfg);
    let args: Vec<String> = cfg
        .update_args
        .split_whitespace()
        .map(std::string::ToString::to_string)
        .collect();

    let app2 = app.clone();
    std::thread::spawn(move || {
        let result: Result<i32, String> = (|| {
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(&cfg.npm_path);
            for a in &args {
                cmd.arg(a);
            }
            cmd.current_dir(&cwd);
            // npm.cmd 自身能找到 node，但它派生的安装脚本调的是裸 `node`：必须给出补好的 PATH
            cmd.env("PATH", detect::child_path_for(&[cfg.npm_path.as_str()]));
            apply_no_window(&mut cmd);
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let mut child = cmd
                .spawn()
                .map_err(|e| i18n::fmt("err_npm_spawn", &[&e.to_string()]))?;
            let pid = child.id();

            // npm 进程也放入独立 Job Object：更新期间本程序退出则一并结束，避免残留
            #[cfg(windows)]
            {
                let state = app2.state::<AppState>();
                if let Some(j) = win::create_kill_on_close_job(pid) {
                    *state.update_job.lock().unwrap() = Some(j);
                }
            }

            emit_log(
                &app2,
                "update",
                i18n::fmt(
                    "log_update_cmd",
                    &[&cfg.npm_path, &cfg.update_args, &cwd, &pid],
                ),
            );

            if let Some(so) = child.stdout.take() {
                spawn_log_reader(app2.clone(), so, "update", "update-log", false, None, None);
            }
            if let Some(se) = child.stderr.take() {
                spawn_log_reader(app2.clone(), se, "update", "update-log", false, None, None);
            }

            child
                .wait()
                .map(|s| s.code().unwrap_or(-1))
                .map_err(|e| i18n::fmt("err_npm_wait", &[&e.to_string()]))
        })();

        let state = app2.state::<AppState>();
        state.close_update_job();
        match result {
            Ok(0) => {
                emit_log(&app2, "update", i18n::t("log_update_ok").to_string());
                let _ = app2.emit(
                    "update-finished",
                    UpdateFinished { success: true, message: i18n::t("msg_update_success").to_string() },
                );
            }
            Ok(code) => {
                emit_log(
                    &app2,
                    "update",
                    i18n::fmt("log_update_fail_code", &[&code]),
                );
                let _ = app2.emit(
                    "update-finished",
                    UpdateFinished {
                        success: false,
                        message: i18n::fmt("msg_update_fail_code", &[&code]),
                    },
                );
                set_status(&app2, "idle", None);
            }
            Err(e) => {
                emit_log(&app2, "update", format!("[update] {}", e));
                let _ = app2.emit(
                    "update-finished",
                    UpdateFinished { success: false, message: e },
                );
                set_status(&app2, "idle", None);
            }
        }
        state.updating.store(false, Ordering::SeqCst);
    });

    Ok(())
}

// ---------- 文件 / 目录选择 ----------

#[tauri::command]
pub fn pick_exec_path(app: AppHandle, kind: String) {
    use tauri_plugin_dialog::DialogExt;
    let app2 = app.clone();
    app.dialog()
        .file()
        .add_filter(i18n::t("pick_filter_exec"), &["cmd", "bat", "exe"])
        .pick_file(move |file| {
            if let Some(f) = file {
                if let Ok(p) = f.into_path() {
                    let _ = app2.emit(
                        "path-picked",
                        serde_json::json!({ "kind": kind, "path": p.to_string_lossy() }),
                    );
                }
            }
        });
}

#[tauri::command]
pub fn pick_folder(app: AppHandle, kind: String) {
    use tauri_plugin_dialog::DialogExt;
    let app2 = app.clone();
    app.dialog().file().pick_folder(move |file| {
        if let Some(f) = file {
            if let Ok(p) = f.into_path() {
                let _ = app2.emit(
                    "path-picked",
                    serde_json::json!({ "kind": kind, "path": p.to_string_lossy() }),
                );
            }
        }
    });
}

/// 退出 / 关窗时的兜底清理（幂等，可安全重复调用）
pub fn cleanup_sync(app: &AppHandle) {
    let _ = stop_internal(app);
}

// ---------- 开机自启 ----------

/// 查询操作系统层面是否已注册开机自启
#[tauri::command]
pub fn is_autostart_enabled(app: AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// 注册 / 取消开机自启；写入位置由官方插件自动选择（Windows: HKCU\...\Run）
#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let mgr = app.autolaunch();
    if enabled {
        mgr.enable().map_err(|e| i18n::fmt("err_autostart_enable", &[&e.to_string()]))?;
        emit_log(&app, "launcher", i18n::t("log_autostart_on").to_string());
    } else {
        mgr.disable().map_err(|e| i18n::fmt("err_autostart_disable", &[&e.to_string()]))?;
        emit_log(&app, "launcher", i18n::t("log_autostart_off").to_string());
    }
    // 同步托盘菜单勾选（通过查找 managed state；lib.rs 定义）
    sync_tray_autostart_checked(&app, enabled);
    Ok(())
}

/// 通知前端托盘菜单勾选变化（前端据此刷新设置开关）
fn sync_tray_autostart_checked(app: &AppHandle, enabled: bool) {
    // 通过 emit 通知前端刷新 UI 开关。结构仅取 stream 字段（line 为空）即可。
    let _ = app.emit(
        "autostart-changed",
        LogEvent {
            stream: "launcher".to_string(),
            line: if enabled { "on".to_string() } else { "off".to_string() },
        },
    );
}

/// 当前进程是否由「开机自启」触发（前端据此判断是否需要显示延迟提示）
#[tauri::command]
pub fn was_launched_by_autostart(app: AppHandle) -> bool {
    app.state::<AppState>()
        .launched_by_autostart
        .load(Ordering::SeqCst)
}

// ---------- 工具命令（日志目录 / 浏览器 / 端口检查 / 环境检测） ----------

/// 打开 Desktop 日志目录（%APPDATA%\com.dsh.desktop）
#[tauri::command]
pub fn open_log_dir(app: AppHandle) -> Result<(), String> {
    let dir = config::config_dir(&app);
    std::fs::create_dir_all(&dir).map_err(|e| {
        i18n::fmt("err_logdir_create", &[&dir.display().to_string(), &e.to_string()])
    })?;
    Command::new("explorer")
        .arg(dir.as_os_str())
        .spawn()
        .map_err(|e| i18n::fmt("err_logdir_open", &[&e.to_string()]))?;
    Ok(())
}

/// 用系统默认浏览器打开链接（仅允许 http/https，用于引导页打开官网等）
#[tauri::command]
pub fn open_in_browser(url: String) -> Result<(), String> {
    let u = url.trim();
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return Err(i18n::fmt("err_bad_url", &[&u.to_string()]));
    }
    Command::new("explorer")
        .arg(u)
        .spawn()
        .map_err(|e| i18n::fmt("err_browser_open", &[&e.to_string()]))?;
    Ok(())
}

/// 检查配置端口当前是否被占用（端口占用面板的「重新检测」按钮使用）
#[tauri::command]
pub async fn check_port(app: AppHandle) -> Result<PortCheck, String> {
    let cfg = config::load(&app);
    Ok(PortCheck { port: cfg.port, in_use: port_in_use(cfg.port) })
}

/// 完整环境检测（强制刷新缓存）：Node.js / npm / DSH 的存在性与路径、Node 版本
#[tauri::command]
pub async fn detect_environment(_app: AppHandle) -> Result<detect::EnvDetection, String> {
    Ok(detect::full_detect())
}

// ---------- 首次运行引导安装（要求七；不内置 Node/DSH，只在线引导官方安装包） ----------

fn setup_progress(app: &AppHandle, phase: &str, msg: &str) {
    let _ = app.emit(
        "setup-status",
        SetupStatus { phase: phase.to_string(), message: msg.to_string() },
    );
    logger::append_line(
        &logger::desktop_log_path(app),
        &format!("[setup][{}] {}", phase, msg),
    );
}

/// 引导安装 Node.js：下载官方 LTS MSI → 启动安装程序 → 等待 → 验证。
/// 失败时给出明确原因（无网络/下载失败/权限不足/用户取消），绝不静默失败。
#[tauri::command]
pub async fn setup_install_node(app: AppHandle) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        if state.setup_busy.swap(true, Ordering::SeqCst) {
            return Err(i18n::t("err_setup_busy").to_string());
        }
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        let outcome = install_node_blocking(&app2);
        let (ok, msg) = match outcome {
            Ok(m) => (true, m),
            Err(e) => (false, e),
        };
        logger::append_line(
            &logger::desktop_log_path(&app2),
            &i18n::fmt(
                "setup_node_result_line",
                &[
                    &i18n::t(if ok { "setup_word_ok" } else { "setup_word_fail" }),
                    &msg,
                ],
            ),
        );
        let _ = app2.emit(
            "setup-result",
            SetupResult { target: "node".to_string(), success: ok, message: msg },
        );
        app2.state::<AppState>().setup_busy.store(false, Ordering::SeqCst);
    });
    Ok(())
}

/// 从官方 dist 的 SHASUMS256.txt 解析该 LTS 线最新补丁版本号（如 "22.23.2"）。
/// 任何一步失败（无网络、镜像缺文件、格式意外）都返回 None，由调用方回退固定版本。
fn resolve_latest_lts_version(last_err: &mut String) -> Option<String> {
    let url = detect::node_shasums_url();
    let dest = std::env::temp_dir().join("dsh-node-shasums256.txt");
    let _ = std::fs::remove_file(&dest);

    let mut curl_err = String::new();
    let downloaded =
        try_download_curl(&url, &dest, &mut curl_err, None)
            || try_download_powershell(&url, &dest).is_ok();
    if !downloaded {
        *last_err = if curl_err.trim().is_empty() {
            i18n::t("setup_no_dl_tool").to_string()
        } else {
            curl_err
        };
        return None;
    }
    let text = std::fs::read_to_string(&dest).unwrap_or_default();
    let _ = std::fs::remove_file(&dest);

    // 清单每行形如：`<sha256>  node-v24.20.0-x64.msi`
    // 注意：必须是「node-v<完整版本>-x64.msi」，先剥掉前后缀再校验主版本线，
    // 否则会把 "node-v24." 当前缀吃掉主版本号，解析出 "20.0" 这种残缺值。
    let line_prefix = format!("{}.", detect::NODE_LTS_LINE);
    for token in text.split_whitespace() {
        let name = token.rsplit(['/', '\\']).next().unwrap_or(token);
        let Some(rest) = name.strip_prefix("node-v") else {
            continue;
        };
        let Some(v) = rest.strip_suffix("-x64.msi") else {
            continue;
        };
        if !v.starts_with(line_prefix.as_str()) {
            continue; // 其它 LTS 线的条目，跳过
        }
        if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.') {
            return Some(v.to_string());
        }
    }
    *last_err = i18n::t("setup_node_no_msi_entry").to_string();
    None
}

fn install_node_blocking(app: &AppHandle) -> Result<String, String> {
    // 先解析该 LTS 线的最新补丁版本；失败就用固定回退版本（绝不因为探测失败而卡住安装）
    let mut probe_err = String::new();
    let node_version = match resolve_latest_lts_version(&mut probe_err) {
        Some(v) => {
            detect::record_node_version(&v);
            setup_progress(
                app,
                "download",
                &i18n::fmt("setup_lts_resolved", &[&detect::NODE_LTS_LINE, &v]),
            );
            v
        }
        None => {
            setup_progress(
                app,
                "download",
                &i18n::fmt(
                    "setup_lts_fallback",
                    &[&probe_err, &detect::NODE_LTS_VERSION.to_string()],
                ),
            );
            detect::NODE_LTS_VERSION.to_string()
        }
    };

    let url = detect::node_msi_url();
    let file_name = url.rsplit('/').next().unwrap_or("node-lts-x64.msi").to_string();
    let dest = std::env::temp_dir().join(&file_name);
    let _ = std::fs::remove_file(&dest);

    setup_progress(
        app,
        "download",
        &i18n::fmt("setup_dl_start", &[&node_version, &url]),
    );

    // 方式一：curl.exe（Windows 10 1803+ 自带）；失败则回退 PowerShell Invoke-WebRequest
    // curl 下载时实时回报进度（MB / 百分比）；PS 回退路径拿不到流式落盘，只有提示行
    let mut last_err = i18n::t("setup_no_dl_tool").to_string();
    let via_curl = try_download_curl(&url, &dest, &mut last_err, Some(app));
    if !via_curl {
        setup_progress(
            app,
            "download",
            &i18n::fmt("setup_curl_fallback", &[&last_err]),
        );
        try_download_powershell(&url, &dest)?;
    }

    // 校验文件已落盘且大小合理（MSI 一般 ~30MB）
    match std::fs::metadata(&dest) {
        Ok(meta) if meta.len() >= 10 * 1024 * 1024 => {}
        Ok(meta) => {
            let _ = std::fs::remove_file(&dest);
            return Err(i18n::fmt(
                "setup_dl_incomplete",
                &[&meta.len(), &detect::NODE_DOWNLOAD_PAGE.to_string()],
            ));
        }
        Err(_) => {
            return Err(i18n::fmt(
                "setup_dl_missing",
                &[&dest.display().to_string(), &detect::NODE_DOWNLOAD_PAGE.to_string()],
            ));
        }
    }

    setup_progress(app, "install", i18n::t("setup_install_launch"));

    // 运行官方 MSI：/passive 显示进度条但无需逐页点击；UAC 由 Windows 弹出（权限提升交给系统）
    let mut cmd = Command::new("msiexec");
    cmd.arg("/i").arg(&dest).args(["/passive", "/norestart"]);
    apply_no_window(&mut cmd); // 只是不创建控制台窗口；MSI 本身是 GUI 程序不受影响
    let mut child = cmd.spawn().map_err(|e| {
        i18n::fmt(
            "setup_msi_launch_fail",
            &[&e.to_string(), &detect::NODE_DOWNLOAD_PAGE.to_string()],
        )
    })?;

    let started = Instant::now();
    let code;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                code = st.code().unwrap_or(-1);
                break;
            }
            Ok(None) => {
                if started.elapsed() >= Duration::from_secs(30 * 60) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(i18n::t("setup_install_timeout").to_string());
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => return Err(i18n::fmt("setup_wait_exit_fail", &[&e.to_string()])),
        }
    }
    let _ = std::fs::remove_file(&dest); // 清理下载的安装包

    detect::invalidate_cache();
    setup_progress(app, "verify", i18n::t("setup_verifying"));
    match code {
        0 | 3010 => {
            // 关键修复：MSI 只改了注册表里的 PATH，本进程 PATH 仍是启动时的旧快照。
            // 不刷新它，随后 npm install -g 里 koffi 的生命周期脚本会报
            // 「'node' 不是内部或外部命令」而 npm error code 1。
            let added = detect::refresh_process_path();
            if added > 0 {
                setup_progress(app, "verify", &i18n::fmt("setup_path_refreshed", &[&added]));
            }
            let env = detect::full_detect();
            if env.node_found {
                let ver = env.node_version.clone().unwrap_or_default();
                setup_progress(
                    app,
                    "verify",
                    &i18n::fmt("setup_node_detected", &[&env.node_path, &ver]),
                );
                Ok(format!("{} {}", env.node_path, ver))
            } else {
                Err(i18n::fmt(
                    "setup_node_not_detected",
                    &[&code, &detect::NODE_DOWNLOAD_PAGE.to_string()],
                ))
            }
        }
        1602 => Err(i18n::t("setup_node_cancelled").to_string()),
        c => Err(i18n::fmt("setup_node_fail_code", &[&c])),
    }
}

/// 把字节数格式化成 MB（保留 1 位小数）。
fn mb_str(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1048576.0)
}

/// 按 `Content-Length` + 已落盘字节数，向前端/日志发一次下载进度。
fn emit_download_progress(app: &AppHandle, done: u64, total: Option<u64>) {
    let msg = match total {
        Some(t) if t > 0 => {
            let pct = (((done as f64) / (t as f64)) * 100.0).round().max(0.0) as u64;
            i18n::fmt("setup_dl_progress", &[&pct, &mb_str(done), &mb_str(t)])
        }
        _ => i18n::fmt("setup_dl_progress_unknown", &[&mb_str(done)]),
    };
    setup_progress(app, "download", &msg);
}

/// 用 curl HEAD 探测下载总大小（失败只影响百分比显示，不阻断下载）。
/// `-sIL`：静默 + HEAD + 跟随重定向；取最后一个 content-length（重定向后的生效值）。
fn probe_content_length(curl: &Path, url: &str) -> Option<u64> {
    let args: Vec<String> = vec![
        "-sIL".into(),
        "--max-time".into(),
        "20".into(),
        url.to_string(),
    ];
    let out = run_cmd_capture(&curl.to_string_lossy(), &args, "", Duration::from_secs(25)).ok()?.1;
    let mut len: Option<u64> = None;
    for line in out.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            if let Ok(v) = rest.trim().parse::<u64>() {
                len = Some(v); // 多次重定向时保留最后一个
            }
        }
    }
    len
}

/// curl 下载官方 Node.js 安装包：spawn 后轮询落盘文件大小，
/// 通过 setup-status 事件实时回报「已下载字节 / 百分比」（约 0.4s 一次）。
/// 返回 true 表示下载成功；失败原因写入 last_err。
fn try_download_curl(
    url: &str,
    dest: &Path,
    last_err: &mut String,
    app: Option<&AppHandle>,
) -> bool {
    let Some(curl) = detect::where_lookup("curl.exe") else {
        *last_err = i18n::t("setup_no_curl").to_string();
        return false;
    };
    let total = probe_content_length(&curl, url);

    let args: Vec<String> = vec![
        "-fL".into(),
        "--retry".into(),
        "2".into(),
        "--connect-timeout".into(),
        "15".into(),
        "-sS".into(), // 静默（进度由我们自己按文件大小计算），但保留错误输出
        "-o".into(),
        dest.to_string_lossy().to_string(),
        url.to_string(),
    ];
    let mut cmd = Command::new(&curl);
    for a in &args {
        cmd.arg(a);
    }
    apply_no_window(&mut cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            *last_err = e.to_string();
            return false;
        }
    };

    let started = Instant::now();
    let mut last_report = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                let mut err_pipe = child.stderr.take();
                let err_text = decode_console_output(&drain_pipe(&mut err_pipe));
                if st.success() {
                    // 收尾：无论是否发过，都补一次 100% 的进度
                    if let Some(app) = app {
                        let done = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
                        emit_download_progress(app, done, total);
                    }
                    return true;
                }
                *last_err = i18n::fmt("setup_curl_fail", &[&err_text.trim()]);
                return false;
            }
            Ok(None) => {
                if let Some(app) = app {
                    if last_report.elapsed() >= Duration::from_millis(400) {
                        last_report = Instant::now();
                        let done = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
                        emit_download_progress(app, done, total);
                    }
                }
                if started.elapsed() >= Duration::from_secs(15 * 60) {
                    let _ = child.kill();
                    let _ = child.wait();
                    *last_err = i18n::fmt(
                        "setup_dl_timeout",
                        &[&"900".to_string(), &detect::NODE_DOWNLOAD_PAGE.to_string()],
                    );
                    return false;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => {
                *last_err = e.to_string();
                return false;
            }
        }
    }
}

fn try_download_powershell(url: &str, dest: &Path) -> Result<(), String> {
    let script = format!(
        "$ProgressPreference='SilentlyContinue'; [Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; Invoke-WebRequest -Uri '{}' -OutFile '{}'",
        url,
        dest.display()
    );
    let args: Vec<String> = vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-ExecutionPolicy".into(),
        "Bypass".into(),
        "-Command".into(),
        script,
    ];
    match run_cmd_capture("powershell", &args, "", Duration::from_secs(15 * 60)) {
        Ok((true, _)) => Ok(()),
        Ok((false, out)) => {
            let o = out.trim();
            let hint = if o.contains("Unable to connect")
                || o.contains("远程名称")
                || o.contains("resolve")
                || o.contains("denied")
            {
                i18n::t("setup_net_denied")
            } else {
                i18n::t("setup_ps_fail")
            };
            Err(i18n::fmt(
                "setup_ps_dl_fail",
                &[
                    &hint.to_string(),
                    &o.chars().take(300).collect::<String>(),
                    &detect::NODE_DOWNLOAD_PAGE.to_string(),
                ],
            ))
        }
        Err(e) => Err(i18n::fmt(
            "setup_dl_timeout",
            &[&e, &detect::NODE_DOWNLOAD_PAGE.to_string()],
        )),
    }
}

/// 引导安装 DSH：执行 `cmd /C <npm> install -g @deepseek-ai/dsh`，
/// 输出实时转发到日志面板，结束后自动重新检测。
#[tauri::command]
pub async fn setup_install_dsh(app: AppHandle) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        if state.updating.swap(true, Ordering::SeqCst) {
            return Err(i18n::t("err_task_busy").to_string());
        }
    }

    let cfg = config::load(&app);
    if cfg.npm_path.trim().is_empty() || !Path::new(&cfg.npm_path).is_file() {
        app.state::<AppState>().updating.store(false, Ordering::SeqCst);
        return Err(i18n::fmt("setup_npm_missing", &[&cfg.npm_path]));
    }

    let app2 = app.clone();
    let npm = cfg.npm_path.clone();
    let pkg = cfg.package_name.clone();
    let cwd = config::workspace_cwd(&cfg);
    std::thread::spawn(move || {
        emit_log(
            &app2,
            "launcher",
            i18n::fmt("setup_dsh_executing", &[&npm, &pkg]),
        );

        // 下载进度：计数 npm http fetch 行，每秒向向导进度区回报一次。
        // 放在安装闭包外层：进度标志要在 npm 进程结束后仍可置位。
        let fetch_counter = Arc::new(AtomicUsize::new(0));
        let npm_done = Arc::new(AtomicBool::new(false));
        {
            let app_tick = app2.clone();
            let counter = fetch_counter.clone();
            let done_flag = npm_done.clone();
            let started = Instant::now();
            std::thread::spawn(move || {
                while !done_flag.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(1000));
                    if done_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    let n = counter.load(Ordering::Relaxed);
                    let secs = started.elapsed().as_secs();
                    setup_progress(&app_tick, "install", &i18n::fmt("setup_npm_progress", &[&n, &secs]));
                }
            });
        }

        let outcome: Result<i32, String> = (|| {
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(&npm).arg("install").arg("-g").arg(&pkg);
            cmd.current_dir(&cwd);
            // 同上：koffi 等原生依赖的 prebuild 脚本用裸 `node`，PATH 必须包含 node 目录
            cmd.env("PATH", detect::child_path_for(&[npm.as_str()]));
            // loglevel=http：npm 会把每次 registry 请求打成一行日志，
            // 日志读取线程据此给界面进度（「已下载 N 个包文件」）计数
            cmd.env("npm_config_loglevel", "http");
            apply_no_window(&mut cmd);
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd
                .spawn()
                .map_err(|e| i18n::fmt("setup_npm_spawn_fail", &[&e.to_string()]))?;
            let pid = child.id();

            // 放入 Job Object：引导期间本程序退出则一并结束，避免残留
            #[cfg(windows)]
            {
                let st = app2.state::<AppState>();
                if let Some(j) = win::create_kill_on_close_job(pid) {
                    *st.update_job.lock().unwrap() = Some(j);
                }
            }

            // 下载进度：日志读取线程会对每行 http fetch 计数（计数器在外层）

            if let Some(so) = child.stdout.take() {
                spawn_log_reader(
                    app2.clone(),
                    so,
                    "update",
                    "dsh-log",
                    false,
                    None,
                    Some(fetch_counter.clone()),
                );
            }
            if let Some(se) = child.stderr.take() {
                spawn_log_reader(
                    app2.clone(),
                    se,
                    "update",
                    "dsh-log",
                    false,
                    None,
                    Some(fetch_counter.clone()),
                );
            }

            let deadline = Instant::now() + Duration::from_secs(15 * 60);
            loop {
                match child.try_wait() {
                    Ok(Some(st)) => return Ok(st.code().unwrap_or(-1)),
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err(i18n::t("setup_npm_timeout").to_string());
                        }
                        std::thread::sleep(Duration::from_millis(400));
                    }
                    Err(e) => return Err(i18n::fmt("setup_npm_wait_fail", &[&e.to_string()])),
                }
            }
        })();
        npm_done.store(true, Ordering::Relaxed);

        {
            let st = app2.state::<AppState>();
            st.close_update_job();
            st.updating.store(false, Ordering::SeqCst);
        }

        match outcome {
            Ok(0) => {
                detect::invalidate_cache();
                let env = detect::full_detect();
                if env.dsh_found {
                    let msg = i18n::fmt("setup_dsh_success_msg", &[&env.dsh_path]);
                    logger::append_line(&logger::desktop_log_path(&app2), i18n::t("setup_dsh_ok_log"));
                    let _ = app2.emit(
                        "setup-result",
                        SetupResult { target: "dsh".to_string(), success: true, message: msg },
                    );
                } else {
                    let _ = app2.emit(
                        "setup-result",
                        SetupResult {
                            target: "dsh".to_string(),
                            success: false,
                            message: i18n::t("setup_dsh_notfound").to_string(),
                        },
                    );
                }
            }
            Ok(c) => {
                let msg = i18n::fmt("setup_dsh_fail_code", &[&pkg, &c]);
                logger::append_line(&logger::desktop_log_path(&app2), &msg);
                let _ = app2.emit(
                    "setup-result",
                    SetupResult { target: "dsh".to_string(), success: false, message: msg },
                );
            }
            Err(e) => {
                logger::append_line(&logger::desktop_log_path(&app2), &e);
                let _ = app2.emit(
                    "setup-result",
                    SetupResult { target: "dsh".to_string(), success: false, message: e },
                );
            }
        }
    });
    Ok(())
}

/// 完成首次运行引导：把当前（含自动检测补全的）配置写入 %APPDATA%\com.dsh.desktop\config.json。
/// 写入成功后 get_config.first_run 变为 false，向导不再出现。
#[tauri::command]
pub fn finish_setup(app: AppHandle) -> Result<ConfigReport, String> {
    let cfg = config::load(&app);
    config::save(&app, &cfg)?;
    // 引导完成后按用户配置同步一次 DSH 界面语言（默认 zh）
    let _ = config::sync_dsh_locale(&cfg.dsh_home_dir, &cfg.language);
    log_launcher(&app, i18n::t("log_setup_done"));
    Ok(get_config(app))
}

/// 向导第一步「选择语言」：立即切换本进程与托盘文案，同步 DSH 的
/// settings.yaml，并把选择持久化到 ui-language sidecar（config.json 生成前
/// 的唯一载体；finish_setup 之后以 config.json 为准）。
/// 不写 config.json —— 否则 first_run 语义被破坏，用户中途中断重开后向导会消失。
#[tauri::command]
pub fn set_language(app: AppHandle, lang: String) -> Result<(), String> {
    let lang = if lang.eq_ignore_ascii_case("en") { "en".to_string() } else { "zh".to_string() };
    let cfg = config::load(&app);
    let old = cfg.language.clone();

    if let Err(e) = config::set_ui_language_override(&app, &lang) {
        log_launcher(&app, &i18n::fmt("err_lang_persist_fail", &[&e]));
    }
    // 家目录可能还不存在：sync_dsh_locale 内部会创建目录与文件，best-effort
    if let Err(e) = config::sync_dsh_locale(&cfg.dsh_home_dir, &lang) {
        log_launcher(&app, &i18n::fmt("err_locale_sync_fail", &[&e]));
    }
    i18n::set_lang(&lang);
    crate::refresh_tray_texts(&app);
    if old != lang {
        log_launcher(&app, &i18n::fmt("log_lang_changed", &[&lang]));
    }
    Ok(())
}
