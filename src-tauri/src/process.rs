use crate::config::{self, Config};
use serde::Serialize;
use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
    #[cfg(windows)]
    job: Mutex<Option<win::JobHandle>>,
    #[cfg(windows)]
    update_job: Mutex<Option<win::JobHandle>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            child: Mutex::new(None),
            pid: Mutex::new(None),
            status: Mutex::new("idle".to_string()),
            updating: AtomicBool::new(false),
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
}

// ---------- 工具函数 ----------

/// Windows 控制台输出可能是 UTF-8（node）或 GBK（taskkill 等系统命令），做无损解码。
fn decode_console_output(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let (cow, _, _) = encoding_rs::GBK.decode(bytes);
    cow.into_owned()
}

fn emit_log(app: &AppHandle, stream: &str, line: String) {
    let _ = app.emit("dsh-log", LogEvent { stream: stream.to_string(), line });
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

/// 端口可连接后，再发一个轻量 HTTP GET 确认服务真正能响应（任意响应码都算就绪）。
fn http_ready(addr: &SocketAddr) -> bool {
    use std::io::Write;
    let Ok(mut stream) = TcpStream::connect_timeout(addr, Duration::from_millis(900)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let req = format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUser-Agent: DSH-Launcher/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n"
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
fn apply_no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn apply_no_window(_cmd: &mut Command) {}

/// 子进程输出逐行读取并转发到前端日志面板（绝不吞掉 stdout/stderr）
fn spawn_log_reader(app: AppHandle, out: impl Read + Send + 'static, stream: &'static str, event: &'static str) {
    std::thread::spawn(move || {
        let reader = BufReader::new(out);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if l.trim().is_empty() {
                        continue;
                    }
                    let _ = app.emit(event, LogEvent { stream: stream.to_string(), line: l });
                }
                Err(_) => break,
            }
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
    cmd.current_dir(cwd);
    apply_no_window(&mut cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("无法启动 {}: {}", program, e))?;
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
                    return Err(format!("命令执行超时（{} 秒）", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("等待命令退出失败: {}", e)),
        }
    }
}

// ---------- 启动 / 停止核心 ----------

fn start_internal(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let st = current_status(app);
    if matches!(
        st.as_str(),
        "starting" | "running" | "running-external" | "stopping" | "updating"
    ) {
        return Err(format!("当前 DSH 状态为「{}」，无法重复启动", st));
    }

    let cfg = config::load(app);
    if !Path::new(&cfg.dsh_path).is_file() {
        let msg = format!(
            "找不到 DSH 可执行文件：{}\n请在「设置」中手动选择 dsh.cmd 的路径。",
            cfg.dsh_path
        );
        set_status(app, "error", Some(msg.clone()));
        return Err(msg);
    }
    if !Path::new(&cfg.home_dir).is_dir() {
        let msg = format!(
            "DSH 工作目录不存在：{}\n请在「设置」中修改家目录 / 工作目录。",
            cfg.home_dir
        );
        set_status(app, "error", Some(msg.clone()));
        return Err(msg);
    }

    // 端口占用检查：绝不强杀未知进程，交给用户决策
    if port_in_use(cfg.port) {
        let msg = format!(
            "端口 {} 已被其他进程占用（可能是已在运行的 DSH，也可能是其他程序）。\n可选择「连接现有服务」，或到「设置」中修改端口。",
            cfg.port
        );
        set_status(app, "port-busy", Some(msg.clone()));
        return Err(msg);
    }

    // 启动命令等价于：cmd /C "<dsh_path>" web --port <port> [extra_args]
    let mut cmd = Command::new("cmd");
    cmd.arg("/C")
        .arg(&cfg.dsh_path)
        .arg("web")
        .arg("--port")
        .arg(cfg.port.to_string());
    for a in cfg.extra_args.split_whitespace() {
        cmd.arg(a);
    }
    // 关键：DSH 依赖工作目录查找配置，必须设置 current_dir
    cmd.current_dir(&cfg.home_dir);
    apply_no_window(&mut cmd);
    // stdin 不需要输入；stdout/stderr 必须 piped 转发到日志，绝不吞掉
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 DSH 失败: {}（路径: {}）", e, cfg.dsh_path))?;
    let pid = child.id();

    if let Some(so) = child.stdout.take() {
        spawn_log_reader(app.clone(), so, "stdout", "dsh-log");
    }
    if let Some(se) = child.stderr.take() {
        spawn_log_reader(app.clone(), se, "stderr", "dsh-log");
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
        format!(
            "[launcher] 启动命令: cmd /C \"{}\" web --port {}{}（工作目录: {}，PID: {}）",
            cfg.dsh_path,
            cfg.port,
            if cfg.extra_args.trim().is_empty() {
                String::new()
            } else {
                format!(" {}", cfg.extra_args)
            },
            cfg.home_dir,
            pid
        ),
    );

    let app2 = app.clone();
    let port = cfg.port;
    let timeout = Duration::from_secs(cfg.health_timeout_secs.clamp(5, 300));
    std::thread::spawn(move || {
        wait_ready_and_open(&app2, port, timeout);
    });
    Ok(())
}

/// 等待 DSH 就绪：同时监控子进程存活 + 轮询 HTTP；就绪后才打开 DSH 窗口（绝不提前加载网页）
fn wait_ready_and_open(app: &AppHandle, port: u16, timeout: Duration) {
    let state = app.state::<AppState>();
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let started = Instant::now();
    emit_log(
        app,
        "launcher",
        format!(
            "[launcher] 正在等待 DSH 就绪（http://127.0.0.1:{}，超时 {} 秒）...",
            port,
            timeout.as_secs()
        ),
    );
    loop {
        // 1) 监控子进程存活：DSH 若在端口就绪前闪退，立即终止等待并高亮报错
        {
            let mut guard = state.child.lock().unwrap();
            if let Some(child) = guard.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        guard.take();
                        drop(guard);
                        *state.pid.lock().unwrap() = None;
                        state.close_jobs();
                        emit_log(
                            app,
                            "launcher",
                            format!(
                                "[launcher] DSH 进程在服务就绪前已退出（退出码 {:?}），启动失败！请检查上方 DSH 的原始输出。",
                                status.code()
                            ),
                        );
                        set_status(
                            app,
                            "error",
                            Some("DSH 启动失败：进程在端口就绪前退出，详见日志".to_string()),
                        );
                        return;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        emit_log(app, "launcher", format!("[launcher] 检查 DSH 进程状态失败: {}", e));
                    }
                }
            } else {
                // 进程句柄已被停止操作清空，无需继续轮询
                return;
            }
        }

        // 2) HTTP 就绪检查
        if http_ready(&addr) {
            emit_log(app, "launcher", "[launcher] DSH 服务已就绪。".to_string());
            set_status(app, "running", None);
            open_dsh_webview(app, port);
            return;
        }

        // 3) 超时
        if started.elapsed() >= timeout {
            emit_log(
                app,
                "launcher",
                format!(
                    "[launcher] 等待 DSH 就绪超时（{} 秒），正在停止进程树...",
                    timeout.as_secs()
                ),
            );
            let _ = stop_internal(app);
            set_status(
                app,
                "error",
                Some(format!(
                    "DSH 启动超时：服务在 {} 秒内未就绪，已停止进程。请查看日志。",
                    timeout.as_secs()
                )),
            );
            return;
        }

        std::thread::sleep(Duration::from_millis(500));
    }
}

/// 停止本次启动的 DSH 进程树（幂等；对“外部进程”模式只重置状态，绝不碰别人的进程）
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
            format!("[launcher] 正在结束 DSH 进程树: taskkill /PID {} /T /F ...", pid),
        );
        match run_taskkill(pid) {
            Ok(out) => {
                if !out.is_empty() {
                    emit_log(app, "launcher", format!("[launcher] taskkill: {}", out));
                }
            }
            Err(e) => {
                emit_log(app, "launcher", format!("[launcher] taskkill 执行失败: {}（Job Object 会在程序退出时兜底清理）", e));
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
    set_status(app, "idle", None);
    emit_log(app, "launcher", "[launcher] DSH 已停止。".to_string());
    Ok(())
}

/// 打开（或复用）DSH Web 窗口
fn open_dsh_webview(app: &AppHandle, port: u16) {
    let url = format!("http://127.0.0.1:{}", port);
    match app.get_webview_window("dsh") {
        Some(win) => {
            let _ = win.show();
            let _ = win.unminimize();
            let _ = win.set_focus();
            let _ = win.eval(&format!("window.location.replace('{}')", url));
        }
        None => {
            let parsed: tauri::Url = url.parse().expect("valid url");
            let result = tauri::WebviewWindowBuilder::new(
                app,
                "dsh",
                tauri::WebviewUrl::External(parsed),
            )
            .title("DSH")
            .inner_size(1280.0, 860.0)
            .min_inner_size(760.0, 520.0)
            .build();
            if let Err(e) = result {
                emit_log(app, "launcher", format!("[launcher] 打开 DSH 窗口失败: {}", e));
            }
        }
    }
}

// ---------- Tauri Commands ----------

#[tauri::command]
pub fn get_config(app: AppHandle) -> ConfigReport {
    let cfg = config::load(&app);
    ConfigReport {
        dsh_exists: Path::new(&cfg.dsh_path).is_file(),
        npm_exists: Path::new(&cfg.npm_path).is_file(),
        home_exists: Path::new(&cfg.home_dir).is_dir(),
        config_path: config::config_path(&app).to_string_lossy().to_string(),
        config: cfg,
    }
}

#[tauri::command]
pub fn save_config(app: AppHandle, config: Config) -> Result<ConfigReport, String> {
    if config.port == 0 {
        return Err("端口无效".to_string());
    }
    if config.dsh_path.trim().is_empty() || config.npm_path.trim().is_empty() {
        return Err("路径不能为空".to_string());
    }
    config::save(&app, &config)?;
    let report = get_config(app.clone());
    emit_log(
        &app,
        "launcher",
        format!("[launcher] 配置已保存到 {}（端口 / 参数将在下次启动 DSH 时生效）", report.config_path),
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
        // 外部进程不由本程序管理，只解除连接状态
        set_status(&app, "idle", None);
        emit_log(&app, "launcher", "[launcher] 已断开与外部服务的连接（该服务不是本程序启动的，仍在运行）。".to_string());
        return Ok(());
    }
    stop_internal(&app)
}

#[tauri::command]
pub async fn restart_dsh(app: AppHandle) -> Result<(), String> {
    let st = current_status(&app);
    if st != "running" && st != "running-external" {
        return Err("DSH 当前未在运行，无法重启".to_string());
    }
    if st == "running-external" {
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
        return Err(format!("端口 {} 当前没有服务在监听，无法连接。", cfg.port));
    }
    emit_log(
        &app,
        "launcher",
        format!(
            "[launcher] 连接到端口 {} 上的现有服务。注意：该服务不是本程序启动的，关闭本程序不会停止它。",
            cfg.port
        ),
    );
    set_status(&app, "running-external", None);
    open_dsh_webview(&app, cfg.port);
    Ok(())
}

#[tauri::command]
pub async fn open_dsh_window(app: AppHandle) -> Result<(), String> {
    let cfg = config::load(&app);
    open_dsh_webview(&app, cfg.port);
    Ok(())
}

/// 版本检测：本地依次尝试 --version / -v / -V；最新版本用 npm view <pkg> version
#[tauri::command]
pub async fn check_versions(app: AppHandle) -> Result<VersionInfo, String> {
    let cfg = config::load(&app);
    let mut info = VersionInfo { local: None, latest: None, error: None };

    if Path::new(&cfg.dsh_path).is_file() {
        for flag in ["--version", "-v", "-V"] {
            match run_cmd_capture(
                &cfg.dsh_path,
                &[flag.to_string()],
                &cfg.home_dir,
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
            info.error = Some("无法获取本地 DSH 版本（--version / -v / -V 均不可用）".to_string());
        }
    } else {
        info.error = Some(format!("找不到 dsh.cmd（{}），无法获取本地版本", cfg.dsh_path));
    }

    if Path::new(&cfg.npm_path).is_file() {
        match run_cmd_capture(
            &cfg.npm_path,
            &[
                "view".to_string(),
                cfg.package_name.clone(),
                "version".to_string(),
            ],
            &cfg.home_dir,
            Duration::from_secs(60),
        ) {
            Ok((true, out)) => {
                let v = out.trim();
                if !v.is_empty() {
                    info.latest = Some(v.to_string());
                } else {
                    let e = format!("npm view {} version 返回为空", cfg.package_name);
                    info.error = Some(join_err(info.error, &e));
                }
            }
            Ok((false, out)) => {
                let e = format!("npm view 失败: {}", out.trim());
                info.error = Some(join_err(info.error, &e));
            }
            Err(e) => {
                info.error = Some(join_err(info.error, &e));
            }
        }
    } else {
        let e = format!("找不到 npm.cmd（{}），无法查询最新版本", cfg.npm_path);
        info.error = Some(join_err(info.error, &e));
    }

    Ok(info)
}

fn join_err(a: Option<String>, b: &str) -> String {
    match a {
        Some(x) => format!("{}；{}", x, b),
        None => b.to_string(),
    }
}

/// 检测全局安装的 DSH 包名（npm list -g --depth=0）
#[tauri::command]
pub async fn detect_npm_package(app: AppHandle) -> Result<String, String> {
    let cfg = config::load(&app);
    if !Path::new(&cfg.npm_path).is_file() {
        return Err(format!("找不到 npm: {}，请先在设置中配置 npm 路径", cfg.npm_path));
    }
    let (ok, out) = run_cmd_capture(
        &cfg.npm_path,
        &["list".to_string(), "-g".to_string(), "--depth=0".to_string()],
        &cfg.home_dir,
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
            Ok(format!("未在 npm 全局包中发现 dsh 相关包。\n\n完整输出:\n{}", text))
        } else {
            Ok(format!("检测到全局包:\n{}\n\n完整输出:\n{}", found.join("\n"), text))
        }
    } else {
        Ok(format!("npm list 执行失败:\n{}", text))
    }
}

/// 更新 DSH：先停止本程序启动的 DSH → 执行 npm install -g <pkg>@latest → 实时转发输出 → 按退出码报告结果
#[tauri::command]
pub async fn update_dsh(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    if state.updating.swap(true, Ordering::SeqCst) {
        return Err("已有更新任务正在进行，请等待完成".to_string());
    }

    let cfg = config::load(&app);
    if !Path::new(&cfg.npm_path).is_file() {
        state.updating.store(false, Ordering::SeqCst);
        let msg = format!("找不到 npm: {}，请在「设置」中修改 npm.cmd 路径", cfg.npm_path);
        set_status(&app, "error", Some(msg.clone()));
        return Err(msg);
    }

    // 停止本程序启动的 DSH（外部进程模式不动）
    let st = current_status(&app);
    if st == "running" || st == "starting" {
        let _ = stop_internal(&app);
    } else if st == "running-external" {
        set_status(&app, "idle", None);
    }

    set_status(&app, "updating", None);
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
            cmd.current_dir(&cfg.home_dir);
            apply_no_window(&mut cmd);
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let mut child = cmd
                .spawn()
                .map_err(|e| format!("npm 启动失败: {}", e))?;
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
                format!(
                    "[update] 执行: cmd /C \"{}\" {}（工作目录: {}，PID: {}）",
                    cfg.npm_path, cfg.update_args, cfg.home_dir, pid
                ),
            );

            if let Some(so) = child.stdout.take() {
                spawn_log_reader(app2.clone(), so, "update", "update-log");
            }
            if let Some(se) = child.stderr.take() {
                spawn_log_reader(app2.clone(), se, "update", "update-log");
            }

            child
                .wait()
                .map(|s| s.code().unwrap_or(-1))
                .map_err(|e| format!("等待 npm 退出失败: {}", e))
        })();

        let state = app2.state::<AppState>();
        state.close_update_job();
        match result {
            Ok(0) => {
                emit_log(&app2, "update", "[update] npm 更新成功（退出码 0）。".to_string());
                let _ = app2.emit(
                    "update-finished",
                    UpdateFinished { success: true, message: "更新成功".to_string() },
                );
            }
            Ok(code) => {
                emit_log(
                    &app2,
                    "update",
                    format!("[update] npm 更新失败（退出码 {}），不会自动启动 DSH。请查看上方日志。", code),
                );
                let _ = app2.emit(
                    "update-finished",
                    UpdateFinished {
                        success: false,
                        message: format!("更新失败（退出码 {}），详见日志", code),
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
        .add_filter("命令脚本 / 可执行文件", &["cmd", "bat", "exe"])
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
