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

/// 「更新 DSH」进行中的实时进度（每秒回报一次，供页面进度区显示）
#[derive(Clone, Serialize)]
pub struct UpdateProgress {
    pub fetched: usize, // 已获取的 npm 包文件数（按 npm http fetch 日志行计数）
    pub secs: u64,      // 已用时（秒）
    pub message: String, // 已本地化的完整进度文案
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

/// 版本检测结果（来自 `npm view <pkg> dist-tags`，不是单个版本号）
#[derive(Serialize, Clone)]
pub struct VersionInfo {
    /// 本地 `dsh --version` 读到的版本
    pub local: Option<String>,
    /// dist-tag `latest`：稳定版频道
    pub latest: Option<String>,
    /// dist-tag `next`：比 latest 更新的预览频道；注册表里没有该标签时为 None
    pub next: Option<String>,
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
/// next 频道（0.1.2+）打印的地址带 `?token=<base64url>`（浏览器会话认证），
/// 必须整串保留，否则加载裸地址会收到 401。base64url 不含空白/引号，
/// 现有截断逻辑天然安全。
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
                // 保留 DSH 打印的完整地址（可能带 ?token=...），不要丢掉查询串
                return Some((format!("http://{}", host), p));
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
            // 从 DSH 输出中解析实际监听地址（如 "dsh web: http://127.0.0.1:3080/?token=..."），
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
                    // 兜底重导航：若这行输出晚于 HTTP 就绪判定（next 频道 loader
                    // 就绪后才打印 URL 行，可能超过就绪后的宽限期），页面已按裸地址
                    // 内嵌并显示 401——用带令牌的完整地址重新导航一次。
                    // last_url 在 open_dsh_webview 每次加载时更新，地址相同则跳过。
                    if matches!(current_status(&app).as_str(), "running" | "running-external") {
                        if config::last_url(&app) != url {
                            open_dsh_webview(&app, &url);
                        }
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

// ---------- 派生子进程的安全包装（HIGH-1：配置文本不得成为 shell 令牌） ----------
//
// 根因：Rust std 拼 Windows 命令行时遵循 MSVC 规则——只在令牌含空白 / `"` / `\` 时
// 才加引号，`& | < > ^` 这类「无空白的 cmd 元字符」会原样写进命令行；而 cmd.exe
// 又会对整条命令行二次解析，于是 `extra_args` 填 `a&calc.exe` 就成了第二条命令。
//
// 修法分三层：
// 1) 非批处理目标（.exe / 无扩展名，如 curl.exe、powershell）直接 CreateProcess，
//    链路上完全没有 cmd.exe；
// 2) .cmd/.bat 只能经 cmd.exe，改由我们自己掌控引号：每个令牌成对加引号，
//    外层再用 `/d`（跳过 AutoRun 钩子）+ `/s /c "…"`（cmd 无条件剥掉最外层引号）；
// 3) 会进命令行的配置字段先过校验：参数类走白名单，路径类拒绝引号与 `% !`。
//    （cmd 即使在双引号内也会展开 `%VAR%`，所以 `%` 只能拒绝、无法转义。）

/// 允许出现在「传给 npm / DSH 的参数」里的字符。
///
/// 合法的 dsh / npm 参数只需要字母数字，以及 `- _ . , : = + @ / ~` 和路径分隔符；
/// `& | < > ^ % ! " '` 等一概拒绝 —— 宁可报清晰错误，也不让这类值进入执行路径。
fn is_safe_arg_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ',' | ':' | '=' | '+' | '@' | '/' | '\\' | '~')
}

/// 校验单个参数令牌（已按空白切分，故不含空格）。
pub(crate) fn validate_arg_token(field: &str, token: &str) -> Result<(), String> {
    if let Some(c) = token.chars().find(|c| !is_safe_arg_char(*c)) {
        return Err(i18n::fmt("err_cfg_arg_danger", &[&field, &token, &c]));
    }
    Ok(())
}

/// 校验一整串参数字段（按空白切分后逐令牌校验）。空串合法 = 没有附加参数。
pub(crate) fn validate_arg_field(field: &str, value: &str) -> Result<(), String> {
    for tok in value.split_whitespace() {
        validate_arg_token(field, tok)?;
    }
    Ok(())
}

/// 校验要交给 cmd.exe 的可执行文件路径。
/// 只拒绝会破坏引号配对 / 触发批处理二次展开的字符（`"` `%` `!` 与控制字符）；
/// `&`、空格、括号等在成对双引号内是字面量，允许，避免误伤
/// `C:\Program Files\A & B\npm.cmd` 这类合法目录名。
pub(crate) fn validate_program_path(path: &str) -> Result<(), String> {
    for c in path.chars() {
        if c == '"' || c == '%' || c == '!' || (c as u32) < 0x20 {
            return Err(i18n::fmt("err_cfg_prog_danger", &[&path, &c]));
        }
    }
    Ok(())
}

/// 我们自己掌控引号的安全形式（仅 Windows 需要）。
#[cfg(windows)]
fn quote_token(tok: &str) -> String {
    // 裁掉尾部反斜杠：否则 `"…\"` 会让引号配对失效，后面的内容被 cmd 重新解析
    format!("\"{}\"", tok.trim_end_matches('\\'))
}

#[cfg(windows)]
fn build_cmd_line(program: &str, args: &[String]) -> String {
    let mut inner = quote_token(program);
    for a in args {
        inner.push(' ');
        inner.push_str(&quote_token(a));
    }
    format!("/d /s /c \"{inner}\"")
}

/// 只有批处理 shim（npm.cmd / dsh.cmd / *.bat）在 Windows 上必须经 cmd.exe 执行。
#[cfg(windows)]
fn needs_cmd_shim(program: &str) -> bool {
    matches!(
        Path::new(program).extension().and_then(|e| e.to_str()),
        Some(ext) if ext.eq_ignore_ascii_case("bat") || ext.eq_ignore_ascii_case("cmd")
    )
}

/// 派生「调用 npm.cmd / dsh.cmd / curl.exe 等本机 CLI」的 Command。
///
/// `.exe` 与无扩展名（如 `powershell`）直接 CreateProcess，链路上根本没有 cmd.exe；
/// 只有 `.cmd`/`.bat` 才经 cmd.exe，此时命令行由我们自己加引号（见上方注释）。
#[cfg(windows)]
pub(crate) fn command_for(program: &str, args: &[String]) -> Result<Command, String> {
    validate_program_path(program)?;
    if needs_cmd_shim(program) {
        use std::os::windows::process::CommandExt;
        let mut cmd = Command::new("cmd");
        cmd.raw_arg(build_cmd_line(program, args));
        Ok(cmd)
    } else {
        // 没有 cmd.exe 二次解析，交给 std 的标准转义（它正确处理引号与尾部反斜杠）
        let mut cmd = Command::new(program);
        cmd.args(args);
        Ok(cmd)
    }
}

#[cfg(not(windows))]
pub(crate) fn command_for(program: &str, args: &[String]) -> Result<Command, String> {
    validate_program_path(program)?;
    let mut cmd = Command::new(program);
    cmd.args(args);
    Ok(cmd)
}

/// 带超时地运行 <program> <args...> 并捕获输出（用于版本查询等小输出命令）
fn run_cmd_capture(
    program: &str,
    args: &[String],
    cwd: &str,
    timeout: Duration,
) -> Result<(bool, String), String> {
    let mut cmd = command_for(program, args)?;
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
///
/// 安全边界（MEDIUM-4）：这个 webview 的 label 是 `dsh`，而 capabilities/default.json
/// 刻意只绑定 `webviews: ["main"]`，所以它拿不到任何 core/plugin 权限；同时它加载的是
/// `http://127.0.0.1:<port>`，在 Tauri 眼里属于 remote origin（`Webview::is_local_url`
/// 只认 tauri 自定义协议与 app 自身的 devUrl/frontendDist），而 remote origin 默认无法
/// 触达 invoke_handler 里的自定义命令。两条独立机制都依赖「不给 dsh 配 capability」这一
/// 前提：绝不要把这里的 label 加进任何 capability 的 `windows` 列表，也不要写 `remote.urls`。
fn open_dsh_webview(app: &AppHandle, url: &str) {
    // 记录最近一次交给 WebView 的地址（next 频道带 ?token=... 会话令牌），
    // 供没有进程输出的场景（连接现有服务 / 页面重开）复用；只写 last_url 一个键。
    if let Ok(parsed) = url.parse::<tauri::Url>() {
        if parsed.scheme() == "http" {
            config::set_last_url(app, parsed.as_str());
        }
    }
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

/// 取上次记录的 DSH 页面地址，仅当形状合法且端口与给定端口一致时才返回。
/// 形状：`http://127.0.0.1:<port>` 或 `http://localhost:<port>`（可带路径/查询串）。
/// 被手改、损坏或指向别的端口的记录一律忽略，调用方回退到按配置端口构造的裸地址。
fn remembered_url_for(app: &AppHandle, port: u16) -> Option<String> {
    let raw = config::last_url(app);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed = tauri::Url::parse(trimmed).ok()?;
    if parsed.scheme() != "http" {
        return None;
    }
    if !matches!(parsed.host_str(), Some("127.0.0.1") | Some("localhost")) {
        return None;
    }
    if parsed.port() != Some(port) {
        return None;
    }
    Some(parsed.as_str().to_string())
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

    // 服务在运行但页面不存在（例如之前被销毁）：优先复用上次记录的完整地址
    // （可能带会话令牌，next 频道裸地址会 401），否则按配置端口重新打开页面
    let cfg = config::load(&app);
    let remembered = remembered_url_for(&app, cfg.port);
    if remembered.is_some() {
        emit_log(&app, "launcher", i18n::fmt("log_using_last_url", &[]));
    }
    open_dsh_webview(&app, &remembered.unwrap_or_else(|| local_url(cfg.port)));
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
    // 完整的执行目标策略（绝对路径 / 非 UNC / 无 `..` / 存在 / 扩展名白名单 / 不在临时目录）。
    // 只判 is_file() 是不够的：config.json 是明文且开机自启会静默执行它，
    // 把 dsh_path 指到任何用户可写的 .exe/.cmd 就已经是「以本用户身份执行任意程序」。
    let dsh_prog = match config::validate_program_file("dsh_path", &cfg.dsh_path) {
        Ok(p) => p,
        Err(e) => {
            set_status(app, "error", Some(e.clone()));
            return Err(e);
        }
    };

    // 3) npm 缺失只影响更新 / 版本查询，不阻止启动。但它的目录会被 child_path_for
    //    拿去拼子进程 PATH（见 detect.rs）：一个被改坏的 npm_path 等于往 DSH 的 PATH
    //    前面插入攻击者可控的目录，从而劫持 shim 里那个裸 `node`。
    //    所以校验不通过时不是「照旧用」，而是让它彻底不参与 PATH 组装。
    let npm_for_path = match config::validate_program_file("npm_path", &cfg.npm_path) {
        Ok(p) => p,
        Err(_) => {
            emit_log(app, "launcher", i18n::t("log_npm_missing_hint").to_string());
            String::new()
        }
    };

    // 4) 家目录：DSH_HOME、日志镜像目录都落在它里面，其上一级还是本进程的 cwd，
    //    所以同样在使用点校验并改用规范化后的值
    let home_dir = match config::validate_home_dir(&cfg.dsh_home_dir) {
        Ok(h) => h,
        Err(e) => {
            set_status(app, "error", Some(e.clone()));
            return Err(e);
        }
    };
    let cwd = match config::cwd_of_home(&home_dir) {
        Ok(c) => c,
        Err(e) => {
            set_status(app, "error", Some(e.clone()));
            return Err(e);
        }
    };
    if !Path::new(&cwd).is_dir() {
        let msg = i18n::fmt("err_cwd_invalid", &[&cwd, &home_dir]);
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

    // 启动命令等价于："<dsh_path>" web --port <port> --no-open [extra_args]
    // --no-open：DSH 官方参数，禁止其自动打开默认浏览器（Edge）
    //
    // extra_args 来自配置文件（%APPDATA%\com.dsh.desktop\config.json）——那是任何
    // 同用户进程都能写的明文 JSON，且本程序会在开机自启时静默执行它，
    // 所以必须在「使用点」再校验一次（保存时的校验挡不住手改/被改的文件）。
    let port_str = cfg.port.to_string();
    let mut launch_args: Vec<String> =
        vec!["web".to_string(), "--port".to_string(), port_str, "--no-open".to_string()];
    for a in cfg.extra_args.split_whitespace() {
        if let Err(e) = validate_arg_token("extra_args", a) {
            set_status(app, "error", Some(e.clone()));
            return Err(e);
        }
        launch_args.push(a.to_string());
    }
    let mut cmd = match command_for(&dsh_prog, &launch_args) {
        Ok(c) => c,
        Err(e) => {
            set_status(app, "error", Some(e.clone()));
            return Err(e);
        }
    };
    // 工作目录：DSH 家目录的上一级；DSH_HOME 指向家目录（DSH 在此读取配置）
    cmd.current_dir(&cwd);
    cmd.env("DSH_HOME", &home_dir);
    // dsh.cmd 是 npm 生成的 shim，回退分支同样依赖 PATH 里的 node
    cmd.env(
        "PATH",
        detect::child_path_for(&[dsh_prog.as_str(), npm_for_path.as_str()]),
    );
    apply_no_window(&mut cmd);
    // stdin 不需要输入；stdout/stderr 必须 piped 转发到日志，绝不吞掉
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let _ = state.take_last_stderr();
    let mut child = cmd
        .spawn()
        .map_err(|e| i18n::fmt("err_spawn_fail", &[&e.to_string(), &dsh_prog]))?;
    let pid = child.id();

    // DSH 输出同时镜像写入 <DSH 家目录>\logs\dsh.log（要求四.2）
    let dsh_log_file = logger::dsh_log_path(&home_dir);
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
                &dsh_prog,
                &cfg.port,
                &if cfg.extra_args.trim().is_empty() {
                    String::new()
                } else {
                    format!(" {}", cfg.extra_args)
                },
                &cwd,
                &home_dir,
                &pid,
                &logger::dsh_log_path(&home_dir).display().to_string(),
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
            // DSH（next 频道 0.1.2+）在 loader 就绪后才打印带 token 的实际地址行，
            // 会晚于 HTTP 就绪判定（那时裸地址返回 401 也算「就绪」）。就绪后先给
            // 一小段宽限期等这行输出：拿到就按完整地址内嵌；拿不到（旧版/不打印）
            // 按当前目标内嵌；宽限期内进程退出也立即结束等待。
            if state.detected_url.lock().unwrap().is_none() {
                let grace_deadline = Instant::now() + Duration::from_secs(6);
                loop {
                    if state.detected_url.lock().unwrap().is_some() {
                        break;
                    }
                    let dead = {
                        let mut guard = state.child.lock().unwrap();
                        match guard.as_mut() {
                            Some(child) => matches!(child.try_wait(), Ok(Some(_))),
                            None => true,
                        }
                    };
                    if dead || Instant::now() >= grace_deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            // 重新取一次目标：宽限期内可能刚拿到实际地址（含 token）
            let detected = state.detected_url.lock().unwrap().clone();
            let (target_url, _) = match &detected {
                Some((url, dp)) => (url.clone(), *dp),
                None => (local_url(port), port),
            };
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
    // 这两个字段最终会拼成子进程的命令行，在保存入口就把 cmd 元字符挡掉，
    // 用户能立刻在设置页看到原因（使用点还有第二道校验，手改的配置文件绕不过去）。
    // 更新命令本身不再有可配置字段：它固定由 package_name + 页面上选定的
    // dist-tag（latest / next）拼成 `install -g <包名>@<标签>`，见 update_dsh。
    validate_arg_field("extra_args", &config.extra_args)?;
    validate_arg_field("package_name", &config.package_name)?;
    validate_program_path(&config.dsh_path)?;
    validate_program_path(&config.npm_path)?;
    // 路径策略（MEDIUM-3）：绝对路径 / 非 UNC / 无 `..` / 不落系统目录与临时目录 /
    // 扩展名白名单。保存入口刻意**不要求文件已存在**（允许先填路径后装程序），
    // 存在性由执行点的 validate_program_file 把关。
    // 顺手把规范化后的路径落盘：磁盘上留的就是安全形态，也避免同一路径多种写法。
    let mut config = config;
    config.dsh_home_dir = config::validate_home_dir(&config.dsh_home_dir)?;
    config.dsh_path = config::validate_program_shape("dsh_path", &config.dsh_path)?;
    config.npm_path = config::validate_program_shape("npm_path", &config.npm_path)?;
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
    // 复用上次记录的完整地址（可能带会话令牌）；没有则按配置端口加载裸地址
    let remembered = remembered_url_for(&app, cfg.port);
    if remembered.is_some() {
        emit_log(&app, "launcher", i18n::fmt("log_using_last_url", &[]));
    }
    open_dsh_webview(&app, &remembered.unwrap_or_else(|| local_url(cfg.port)));
    Ok(())
}

/// 解析 `npm view <pkg> dist-tags` 的输出，取出 latest / next 两个标签的版本号。
///
/// npm 在这里打印的是 Node `util.inspect` 形态，**不是 JSON**，所以不能交给 JSON 解析器：
/// 单行时是 `{ latest: '0.1.0', next: '0.2.0-rc.1' }`，标签一多就会折成多行：
///
/// ```text
/// {
///   alpha: '0.2.0-alpha.1',
///   latest: '0.1.0',
///   next: '0.2.0-rc.1'
/// }
/// ```
///
/// 于是按 `,` `{` `}` 切成片段，每片段取 `key: 'value'`（键也可能被引号包住，
/// 那种形态就是标准 JSON）；只认 latest / next 两个键，其余频道忽略。
/// 值要求「非空、不含空白、不太长」，避免把 npm 顺带打到 stderr 的告警文字当成版本号。
fn parse_dist_tags(raw: &str) -> (Option<String>, Option<String>) {
    let mut latest: Option<String> = None;
    let mut next: Option<String> = None;
    let unquote = |s: &str| -> String {
        s.trim().trim_matches(|c| c == '\'' || c == '"').trim().to_string()
    };
    for chunk in raw.split(|c| c == ',' || c == '{' || c == '}') {
        let Some((k, v)) = chunk.split_once(':') else {
            continue;
        };
        let key = unquote(k);
        let val = unquote(v);
        if val.is_empty() || val.len() >= 64 || val.chars().any(char::is_whitespace) {
            continue;
        }
        if key.eq_ignore_ascii_case("latest") {
            if latest.is_none() {
                latest = Some(val);
            }
        } else if key.eq_ignore_ascii_case("next") {
            if next.is_none() {
                next = Some(val);
            }
        }
    }
    (latest, next)
}

/// 版本检测：本地依次尝试 --version / -v / -V；远端用 `npm view <pkg> dist-tags`
/// 一次拿齐 latest（稳定频道）与 next（预览频道，比 latest 更新）。
#[tauri::command]
pub async fn check_versions(app: AppHandle) -> Result<VersionInfo, String> {
    let cfg = config::load(&app);
    // package_name 会作为参数交给 `npm view`：先挡掉任何 cmd 元字符（宁可报错也不执行）
    validate_arg_field("package_name", &cfg.package_name)?;
    let cwd = config::workspace_cwd(&cfg)?;
    let mut info = VersionInfo {
        local: None,
        latest: None,
        next: None,
        error: None,
    };

    // 执行前先过同一套路径策略（绝对 / 非 UNC / 无 `..` / 扩展名白名单 / 不在临时目录）。
    // 这里刻意不硬失败：版本检测是只读功能，路径非法时走原有的「未安装」提示分支，
    // 让本地版本仍能在只有一项坏掉时显示出来。
    let dsh_prog = config::validate_program_file("dsh_path", &cfg.dsh_path).ok();
    if let Some(dsh_prog) = dsh_prog {
        for flag in ["--version", "-v", "-V"] {
            match run_cmd_capture(
                &dsh_prog,
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

    let npm_prog = config::validate_program_file("npm_path", &cfg.npm_path).ok();
    if let Some(npm_prog) = npm_prog {
        match run_cmd_capture(
            &npm_prog,
            &[
                "view".to_string(),
                cfg.package_name.clone(),
                "dist-tags".to_string(),
            ],
            &cwd,
            Duration::from_secs(60),
        ) {
            Ok((true, out)) => {
                let (latest, next) = parse_dist_tags(&out);
                if latest.is_some() || next.is_some() {
                    info.latest = latest;
                    info.next = next;
                } else {
                    // 一个标签都没解析出来：多半是这个 npm 版本改了输出形态。
                    // 退一步把整段输出当 latest 版本号（老形态本来就是单个版本号串）；
                    // 连单个 token 都不像的（空白 / 带空格 / 超长）就报解析失败，不硬猜。
                    let v = out.trim();
                    if !v.is_empty() && v.len() < 64 && !v.chars().any(char::is_whitespace) {
                        info.latest = Some(v.trim_matches(|c| c == '\'' || c == '"').to_string());
                    } else {
                        let e = i18n::fmt("err_ver_parse", &[&cfg.package_name, &v]);
                        info.error = Some(join_err(info.error, &e));
                    }
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
    // 路径策略 + 存在性（文件确实在、但被策略拦下时报策略原因；文件本身不在时
    // 沿用原来那句「去设置里配 npm 路径」，更好操作）
    let npm_prog = match config::validate_program_file("npm_path", &cfg.npm_path) {
        Ok(p) => p,
        Err(e) => {
            if Path::new(&cfg.npm_path).is_file() {
                return Err(e);
            }
            return Err(i18n::fmt("err_no_npm_detect", &[&cfg.npm_path]));
        }
    };
    let cwd = config::workspace_cwd(&cfg)?;
    let (ok, out) = run_cmd_capture(
        &npm_prog,
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

/// 更新 / 换频道安装 DSH：先停止本程序启动的 DSH → 执行 npm install -g <pkg>@<tag>
/// → 实时转发输出 → 按退出码报告结果。
///
/// `tag` 是页面上选定的 npm dist-tag，只接受 `latest` / `next` 两个值。
/// 之所以是白名单而不是配置项：它会被拼进 npm 的命令行，交给调用方自由发挥
/// 就等于把「选频道」变成「注入任意参数」的入口。
/// 两个方向都允许（升级或退回稳定版），因此不做任何版本高低判断。
#[tauri::command]
pub async fn update_dsh(app: AppHandle, tag: String) -> Result<(), String> {
    let tag = tag.trim().to_ascii_lowercase();
    if tag != "latest" && tag != "next" {
        return Err(i18n::t("err_update_bad_tag").to_string());
    }

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

    // 执行目标本身也要过策略（绝对 / 非 UNC / 无 `..` / 扩展名白名单 / 不在临时目录）。
    // 上面的 is_file 已经确认文件在，所以这里失败一定是策略拦下，直接报原因。
    let npm_prog = match config::validate_program_file("npm_path", &cfg.npm_path) {
        Ok(p) => p,
        Err(e) => {
            state.updating.store(false, Ordering::SeqCst);
            set_status(&app, "error", Some(e.clone()));
            return Err(e);
        }
    };

    // 先校验将拼进 npm 命令行的参数：避免非法配置已经把 DSH 停掉了才报错
    if let Err(e) = validate_arg_field("package_name", &cfg.package_name) {
        state.updating.store(false, Ordering::SeqCst);
        set_status(&app, "error", Some(e.clone()));
        return Err(e);
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
    // 无论升级还是退回，配置格式的兼容性都可能变化：先把备份提醒写进日志
    // （页面上的确认框里已经写过一次，这里落盘留痕）
    emit_log(
        &app,
        "update",
        i18n::fmt("log_update_backup_remind", &[&cfg.dsh_home_dir]),
    );
    // 注意：workspace_cwd 现在会做路径策略校验并可能失败。
    // 这里不能用 `?` 直接返回 —— updating 已经置为 true，
    // 不显式复位就会把「正在更新」状态永久卡住（后续更新/引导安装全部被 err_update_busy 挡住）。
    let cwd = match config::workspace_cwd(&cfg) {
        Ok(c) => c,
        Err(e) => {
            state.updating.store(false, Ordering::SeqCst);
            set_status(&app, "error", Some(e.clone()));
            return Err(e);
        }
    };
    let args: Vec<String> = vec![
        "install".to_string(),
        "-g".to_string(),
        format!("{}@{}", cfg.package_name, tag),
    ];
    let args_display = args.join(" ");

    let app2 = app.clone();
    std::thread::spawn(move || {
        // 下载进度：计数 npm 的 http fetch 日志行，每秒向页面进度区回报一次
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
                    let fetched = counter.load(Ordering::Relaxed);
                    let secs = started.elapsed().as_secs();
                    let _ = app_tick.emit(
                        "update-progress",
                        UpdateProgress {
                            fetched,
                            secs,
                            message: i18n::fmt("update_npm_progress", &[&fetched, &secs]),
                        },
                    );
                }
            });
        }

        let result: Result<i32, String> = (|| {
            let mut cmd = command_for(&npm_prog, &args)?;
            cmd.current_dir(&cwd);
            // npm.cmd 自身能找到 node，但它派生的安装脚本调的是裸 `node`：必须给出补好的 PATH
            cmd.env("PATH", detect::child_path_for(&[npm_prog.as_str()]));
            // loglevel=http：npm 会把每次 registry 请求打成一行日志，据此统计「已获取 N 个包文件」
            cmd.env("npm_config_loglevel", "http");
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
                    &[&npm_prog, &args_display, &cwd, &pid],
                ),
            );

            if let Some(so) = child.stdout.take() {
                spawn_log_reader(
                    app2.clone(),
                    so,
                    "update",
                    "update-log",
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
                    "update-log",
                    false,
                    None,
                    Some(fetch_counter.clone()),
                );
            }

            child
                .wait()
                .map(|s| s.code().unwrap_or(-1))
                .map_err(|e| i18n::fmt("err_npm_wait", &[&e.to_string()]))
        })();
        // npm 进程已退出：停止上面的进度回报线程
        npm_done.store(true, Ordering::Relaxed);

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

/// Node.js 官方 x64 MSI 实际约 30MB：低于下限说明下载不完整，高于上限按异常处理
/// （上限同时是读入内存前的一道保险，避免用任意大的文件把内存打满）。
const NODE_MSI_MIN_BYTES: u64 = 10 * 1024 * 1024;
const NODE_MSI_MAX_BYTES: u64 = 200 * 1024 * 1024;

// ---------- 官方安装包的完整性校验（MEDIUM-2 修复） ----------
//
// 原逻辑：抓 SHASUMS256.txt 只为读版本号；MSI 下载后只判断「体积 ≥ 10MB」就交给
// msiexec —— 于是中间人 / 镜像投毒 / 临时文件被替换，都会让攻击者的安装包
// 带着一个正经的 UAC 弹窗被执行。现在：先按**最终要装的版本**从官方 dist 目录取
// 清单里该文件的 SHA-256，比对通过才安装；拿不到清单或哈希不符一律中止
// （宁可不装，也绝不做无校验安装）。
//
// 为什么手写 SHA-256，而不是加依赖或调外部工具：
// - 只为算一次哈希就引入 sha2/ring 并不划算，而且要连带重新生成 Cargo.lock（需联网）；
// - certutil / Get-FileHash 会把安全性寄托在可被 PATH 劫持的外部程序 + 本地化文本
//   解析上（中文 Windows 的 certutil 输出行是本地化的），反而更脆。
// 下面这份实现按 FIPS 180-4 的三个标准测试向量（""、"abc"、56 字节串）核对过。

/// SHA-256 轮常量（FIPS 180-4 §4.2.2：前 64 个质数立方根小数部分的前 32 位）
const SHA_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// 标准 SHA-256，返回 64 位小写十六进制。输入是一次性读入的整个文件（约 30MB）。
fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    // 填充：追加 0x80、补零到「56 mod 64」，再拼 64 位大端比特长度
    let bit_len = (data.len() as u64) << 3;
    let mut msg = Vec::with_capacity(data.len() + 72);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 64];
    for block in msg.chunks_exact(64) {
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for x in h.iter() {
        out.push_str(&format!("{:08x}", x));
    }
    out
}

/// 不用 rand 依赖的随机后缀：纳秒时间戳 ^ 进程 ID ^ 计数，再经 xorshift64 打散。
/// 目的是让「攻击者猜不到我们这次的落盘路径」，而不是做密码学用途。
fn random_temp_suffix(counter: u32) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x: u64 = nanos ^ ((std::process::id() as u64) << 32) ^ ((counter as u64) << 57);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    format!("{:016x}", x)
}

/// 建一个一次性随机私有目录存放下载物。
/// 原来直接写 `%TEMP%\node-v<版本>-x64.msi`：文件名完全可预测，同用户进程可以
/// 抢先把该名字做成符号链接（curl 的 CREATE_ALWAYS 会顺着链接覆盖任意可写文件），
/// 或抢先落一个自己的 MSI。随机目录 + create_dir 排他创建能同时挡住这两种。
fn create_private_temp_dir() -> Result<PathBuf, String> {
    let base = std::env::temp_dir();
    for attempt in 0u32..5 {
        let dir = base.join(format!("dsh-node-setup-{}", random_temp_suffix(attempt)));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            // 撞名（几乎不可能）就换个随机值重试；目录已存在说明有人在那儿放了东西，不复用
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(i18n::fmt("setup_tempdir_fail", &[&e.to_string()])),
        }
    }
    Err(i18n::fmt("setup_tempdir_fail", &[&"random-name-collision".to_string()]))
}

/// 取官方清单里 `msi_name` 对应的 SHA-256（小写）。
/// 每一步失败都返回 Err —— 调用方据此中止安装，绝不退化成「不校验继续装」。
fn fetch_expected_sha256(dir: &Path, version: &str, msi_name: &str) -> Result<String, String> {
    if !detect::is_safe_node_version(version) {
        return Err(i18n::fmt("setup_verify_bad_version", &[&version]));
    }
    let page = detect::NODE_DOWNLOAD_PAGE.to_string();
    let url = detect::node_shasums_url_for(version);
    let dest = dir.join("SHASUMS256.txt");

    let mut curl_err = String::new();
    let downloaded = try_download_curl(&url, &dest, &mut curl_err, None)
        || try_download_powershell(&url, &dest).is_ok();
    if !downloaded {
        let detail = if curl_err.trim().is_empty() {
            i18n::t("setup_no_dl_tool").to_string()
        } else {
            curl_err
        };
        return Err(i18n::fmt("setup_verify_dl_fail", &[&detail, &page]));
    }
    let text = std::fs::read_to_string(&dest)
        .map_err(|e| i18n::fmt("setup_verify_dl_fail", &[&e.to_string(), &page]))?;
    // 去掉可能的 UTF-8 BOM：否则它会粘在第一行的哈希令牌前面，让 64 位长度校验误判
    let text = text.strip_prefix('\u{feff}').unwrap_or(text.as_str());

    // 清单每行形如：`<64 位十六进制>  node-v24.20.0-x64.msi`（文件名不含空格，
    // 所以按 (哈希, 名字) 成对取令牌即可；名字必须**整串相等**，不能只配前缀）
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let hash = it.next().unwrap_or("");
        let name = it.next().unwrap_or("");
        if name != msi_name {
            continue;
        }
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(i18n::fmt("setup_verify_bad_digest", &[&msi_name, &page]));
        }
        return Ok(hash.to_ascii_lowercase());
    }
    Err(i18n::fmt("setup_verify_no_entry", &[&msi_name, &page]))
}

/// 读文件并算 SHA-256；先按 metadata 卡上限，避免异常大的文件把内存打满。
fn sha256_hex_of(path: &Path, max_bytes: u64) -> Result<String, String> {
    let len = std::fs::metadata(path)
        .map_err(|e| format!("{}: {}", path.display(), e))?
        .len();
    if len > max_bytes {
        return Err(i18n::fmt("setup_dl_too_large", &[&len, &max_bytes]));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    Ok(sha256_hex(&bytes))
}

/// 从官方 dist 的 SHASUMS256.txt 解析该 LTS 线最新补丁版本号（如 "22.23.2"）。
/// 任何一步失败（无网络、镜像缺文件、格式意外）都返回 None，由调用方回退固定版本。
fn resolve_latest_lts_version(dir: &Path, last_err: &mut String) -> Option<String> {
    let url = detect::node_shasums_url();
    let dest = dir.join("SHASUMS256-latest.txt");

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
        if detect::is_safe_node_version(v) {
            return Some(v.to_string());
        }
    }
    *last_err = i18n::t("setup_node_no_msi_entry").to_string();
    None
}

/// 引导安装 Node.js 的入口：下载物一律放进一次性随机私有目录，返回前整目录删除
/// （校验失败、安装失败、超时、用户取消等任何提前 return 的路径都不会留下安装包）。
fn install_node_blocking(app: &AppHandle) -> Result<String, String> {
    let dir = create_private_temp_dir()?;
    let result = install_node_verified(&dir, app);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn install_node_verified(dir: &Path, app: &AppHandle) -> Result<String, String> {
    // 先解析该 LTS 线的最新补丁版本；失败就用固定回退版本（绝不因为探测失败而卡住安装）
    let mut probe_err = String::new();
    let node_version = match resolve_latest_lts_version(dir, &mut probe_err) {
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

    // 下载地址、清单条目名、落盘文件名都由**最终选定的版本号**推导，三者不会分叉；
    // 之前的写法是「拿全局状态的 node_msi_url 再反解文件名」，一旦两处版本不同步就会
    // 校验一个文件、安装另一个文件。
    let url = detect::node_msi_url_for(&node_version);
    let msi_name = detect::node_msi_file_name_for(&node_version);
    let dest = dir.join(&msi_name);
    let page = detect::NODE_DOWNLOAD_PAGE.to_string();
    // 让向导展示的下载地址与实际安装的版本保持一致
    detect::record_node_version(&node_version);

    // 先向官方要这个文件的 SHA-256（清单只有 2KB）：拿不到就立刻中止，
    // 既省下一次 30MB 的无用下载，也绝不让「无校验安装」成为退路。
    let expected = fetch_expected_sha256(dir, &node_version, &msi_name)?;

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

    // 体积只是一道粗筛（防下载被截断），真正的完整性判断在下面这次 SHA-256 比对。
    // 出错路径不需要单独清理文件：外层 install_node_blocking 会删掉整个私有目录。
    let meta = std::fs::metadata(&dest)
        .map_err(|_| i18n::fmt("setup_dl_missing", &[&dest.display().to_string(), &page]))?;
    if meta.len() < NODE_MSI_MIN_BYTES {
        return Err(i18n::fmt("setup_dl_incomplete", &[&meta.len(), &page]));
    }
    if meta.len() > NODE_MSI_MAX_BYTES {
        return Err(i18n::fmt(
            "setup_dl_too_large",
            &[&meta.len(), &NODE_MSI_MAX_BYTES],
        ));
    }

    // 核心校验：实际哈希与官方清单不一致就中止（不安装、不重试、不降级为无校验）
    let actual = sha256_hex_of(&dest, NODE_MSI_MAX_BYTES)?;
    if actual != expected {
        return Err(i18n::fmt(
            "setup_hash_mismatch",
            &[&msi_name, &expected, &actual],
        ));
    }
    setup_progress(
        app,
        "download",
        &i18n::fmt("setup_hash_ok", &[&node_version, &actual]),
    );

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
    // 执行 npm 之前过路径策略（绝对 / 非 UNC / 无 `..` / 扩展名白名单 / 不在临时目录）；
    // 之后命令本身与子进程 PATH 都只用这个校验后的值
    let npm = match config::validate_program_file("npm_path", &cfg.npm_path) {
        Ok(p) => p,
        Err(e) => {
            app.state::<AppState>().updating.store(false, Ordering::SeqCst);
            return Err(e);
        }
    };
    // 包名会作为参数交给 npm，同样必须是纯字面量
    if let Err(e) = validate_arg_field("package_name", &cfg.package_name) {
        app.state::<AppState>().updating.store(false, Ordering::SeqCst);
        return Err(e);
    }

    let app2 = app.clone();
    let pkg = cfg.package_name.clone();
    // 同 update_dsh：workspace_cwd 可能因路径策略失败，这里必须复位 updating，
    // 否则「正在更新」状态会永久卡住（后续更新与引导安装全部被 busy 挡住）
    let cwd = match config::workspace_cwd(&cfg) {
        Ok(c) => c,
        Err(e) => {
            app.state::<AppState>().updating.store(false, Ordering::SeqCst);
            return Err(e);
        }
    };
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
            let install_args: Vec<String> =
                vec!["install".to_string(), "-g".to_string(), pkg.clone()];
            let mut cmd = command_for(&npm, &install_args)?;
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
