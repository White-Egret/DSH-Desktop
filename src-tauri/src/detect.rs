//! Node.js / npm / DSH 自动检测。
//!
//! 设计原则：
//! - 检测结果按进程缓存（避免 config::load 高频调用时反复 spawn `where`）；
//!   安装类操作（setup 向导装完 Node/DSH 后）用 `invalidate_cache()` 强制刷新。
//! - 只做"发现"，绝不修改用户配置；用户在设置里手动填写的有效路径优先。
//! - 不下载、不内置任何运行时，只探测本机已有的安装。

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::process::{apply_no_window, decode_console_output};

/// 检测结果快照
#[derive(Debug, Clone, Default)]
pub struct EnvPaths {
    pub node: Option<PathBuf>,
    pub npm: Option<PathBuf>,
    pub dsh: Option<PathBuf>,
}

static CACHE: OnceLock<Mutex<Arc<EnvPaths>>> = OnceLock::new();
static SCANNED: AtomicBool = AtomicBool::new(false);

fn cache_slot() -> &'static Mutex<Arc<EnvPaths>> {
    CACHE.get_or_init(|| Mutex::new(Arc::new(EnvPaths::default())))
}

/// 读取缓存的检测结果；进程首次调用或 `force = true` 时重新扫描。
/// 之后即使结果为空也不再重复扫描（避免高频 config::load 反复拉起子进程），
/// 安装类操作完成后用 `invalidate_cache()` 允许重新检测。
pub fn detect_all(force: bool) -> Arc<EnvPaths> {
    let slot = cache_slot();
    let mut guard = slot.lock().unwrap();
    if force || !SCANNED.load(Ordering::SeqCst) {
        *guard = Arc::new(scan());
        SCANNED.store(true, Ordering::SeqCst);
    }
    guard.clone()
}

/// 安装器完成后调用：丢弃缓存，下一次 detect_all 会重新扫描本机环境。
pub fn invalidate_cache() {
    *cache_slot().lock().unwrap() = Arc::new(EnvPaths::default());
    SCANNED.store(false, Ordering::SeqCst);
    // 安装器同样会改写注册表里的 PATH，注册表快照必须一起丢弃，否则又会拿到旧的。
    invalidate_reg_path_cache();
}

fn scan() -> EnvPaths {
    EnvPaths {
        node: find_node_exe(),
        npm: find_npm_cmd(),
        dsh: find_dsh_cmd(),
    }
}

/// `where <name>`：返回第一个确实存在的路径。
pub(crate) fn where_lookup(name: &str) -> Option<PathBuf> {
    let mut cmd = Command::new("where");
    cmd.arg(name);
    apply_no_window(&mut cmd);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = decode_console_output(&out.stdout);
    for line in text.lines() {
        let p = PathBuf::from(line.trim());
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from)
}

/// 定位 node.exe
pub fn find_node_exe() -> Option<PathBuf> {
    if let Some(p) = where_lookup("node.exe") {
        return Some(p);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = env_path(var) {
            candidates.push(base.join("nodejs").join("node.exe"));
        }
    }
    if let Some(la) = env_path("LOCALAPPDATA") {
        // nvm-windows / fnm / volta 等常见软链位置
        candidates.push(la.join("..").join(".nvm").join("node.exe"));
        candidates.push(la.join("fnm_multishells").join("node.exe"));
        candidates.push(la.join("Volta").join("tools").join("image").join("node.exe"));
    }
    if let Some(home) = env_path("USERPROFILE") {
        candidates.push(home.join("scoop").join("apps").join("nodejs-lts").join("current").join("node.exe"));
        candidates.push(home.join("scoop").join("apps").join("nodejs").join("current").join("node.exe"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// 定位 npm.cmd（npm 与 node 通常同目录）
pub fn find_npm_cmd() -> Option<PathBuf> {
    if let Some(p) = where_lookup("npm.cmd") {
        return Some(p);
    }
    // node 同目录优先（覆盖 PATH 未包含 node 的场景）
    if let Some(node) = find_node_exe() {
        if let Some(dir) = node.parent() {
            let cand = dir.join("npm.cmd");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = env_path(var) {
            candidates.push(base.join("nodejs").join("npm.cmd"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// npm 全局 bin 目录（%APPDATA%\npm 是 npm 在 Windows 的默认 prefix）
fn npm_global_bin_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(appdata) = env_path("APPDATA") {
        dirs.push(appdata.join("npm"));
    }
    if let Some(node) = find_node_exe() {
        if let Some(dir) = node.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    dirs
}

/// 定位 dsh 启动脚本：dsh.cmd / dsh.exe / dsh.bat
pub fn find_dsh_cmd() -> Option<PathBuf> {
    for name in ["dsh.cmd", "dsh.exe", "dsh.bat"] {
        if let Some(p) = where_lookup(name) {
            return Some(p);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for dir in npm_global_bin_dirs() {
        candidates.push(dir.join("dsh.cmd"));
        candidates.push(dir.join("dsh.exe"));
        candidates.push(dir.join("dsh.bat"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// 运行 `<exe> --version` 并截取首行（带超时的简化版：依赖调用方控制场景）
pub fn quick_version(exe: &Path, timeout_secs: u64) -> Option<String> {
    let mut cmd = Command::new(exe);
    cmd.arg("--version");
    apply_no_window(&mut cmd);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut text = String::new();
                if let Some(mut so) = child.stdout.take() {
                    use std::io::Read;
                    let _ = so.read_to_string(&mut text);
                }
                if text.trim().is_empty() {
                    if let Some(mut se) = child.stderr.take() {
                        use std::io::Read;
                        let _ = se.read_to_string(&mut text);
                    }
                }
                return text.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string);
            }
            Ok(None) => {
                if started.elapsed() >= std::time::Duration::from_secs(timeout_secs) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

// ---------- 官方 Node.js LTS 安装引导常量（不内置，仅引导在线下载官方安装包） ----------

/// 引导安装跟随的 Node.js **LTS 线**（主版本号）。补丁版本在下载前从官方 dist
/// 的 SHASUMS256.txt 动态解析（见 process.rs::resolve_latest_lts_version），
/// 因此不必每次 Node 发新版就改代码。
/// 选 24：当前 active LTS（支持窗口到 2028-04；22 线 2027-04 就结束）。
/// 可行性依据：DSH 未声明 engines 限制，且其原生依赖 koffi 走 N-API
/// （自带 node-api-headers，跨 Node 大版本 ABI 稳定），无需为版本重编译。
pub const NODE_LTS_LINE: &str = "24";
/// 解析失败（离线、镜像缺该文件、网络被墙）时回退使用的固定版本。
pub const NODE_LTS_VERSION: &str = "24.20.0";
/// 官方下载页（备选手动安装入口）
pub const NODE_DOWNLOAD_PAGE: &str = "https://nodejs.org/en/download";

/// 该 LTS 线的滚动目录校验清单（约 2 KB，用来查最新补丁版本号）
pub fn node_shasums_url() -> String {
    format!("https://nodejs.org/dist/latest-v{}/SHASUMS256.txt", NODE_LTS_LINE)
}

/// 某个**具体版本**目录下的官方清单（与 MSI 同目录，安装前取 SHA-256 用它）
pub fn node_shasums_url_for(version: &str) -> String {
    format!("https://nodejs.org/dist/v{}/SHASUMS256.txt", version)
}

// ---------- Node.js 最低版本（DSH 的运行下限） ----------
//
// 为什么需要这一层：DSH（deepseek-harness）的运行下限是 **22.19.0**
// （仓库根 engines.node = `^22.19.0 || >=24.0.0`）。三个来源共同定出这条线：
//   - `node:sqlite` 的 `DatabaseSync` 在 22.13 才去掉 `--experimental-sqlite`；
//   - 原生 TypeScript type-stripping 在 22.18 才默认开启；
//   - 依赖 `@earendil-works/pi-ai` 自己声明 `engines.node >=22.19.0`。
// 而**已发布的 `@deepseek-ai/dsh` 包并不声明 engines**，所以 npm 安装阶段不会拦，
// 低于下限只会在运行时炸（实测 Node 21.7.3 起不来）。以前本程序只判断
// 「node.exe 在不在」，于是用户看到的是 DSH 闪退 + 一段看不懂的 stderr；
// 现在把这条线显式判定出来，好让界面能提前告警并提供一键升级。
/// DSH 认可的 Node.js 最低版本（比较用，三段式）
pub const NODE_MIN_VERSION: &str = "22.19.0";
/// 上面那个常量的展示形态（带 v，用于界面文案）
pub const NODE_MIN_VERSION_LABEL: &str = "v22.19.0";

/// 把 `node --version` / `npm` 输出的版本串解析成可比较的数字序列。
///
/// 宽容但**不含糊**：允许 `v` 前缀、`-rc.1` 之类的预发布后缀、`^`/`~` 前缀
/// 与残缺段（`"22.19"` 视作 `22.19.0`）；一旦出现无法解释的内容就返回 None ——
/// 调用方据此判「未知」，而不是把它当成通过或失败。
pub fn parse_version_numbers(s: &str) -> Option<Vec<u32>> {
    let t = s.trim();
    let t = t.strip_prefix('v').or_else(|| t.strip_prefix('V')).unwrap_or(t);
    let t = t.strip_prefix(['^', '~']).unwrap_or(t);
    // npm 也可能在 stderr 里带上 "npm warn EBADENGINE" 之类的告警文字，
    // 所以只取第一段（到空白或预发布记号为止）
    let head = t
        .split(|c: char| c.is_whitespace() || c == '-' || c == '+')
        .next()
        .unwrap_or("");
    if head.is_empty() {
        return None;
    }
    let mut out: Vec<u32> = Vec::new();
    for p in head.split('.') {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        out.push(p.parse::<u32>().ok()?);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 语义化版本比较：补零对齐后逐段比大小（`22.19` == `22.19.0`）。
pub fn cmp_versions(a: &[u32], b: &[u32]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

/// 版本串是否 ≥ `NODE_MIN_VERSION`。解析不出来时返回 None（判「未知」，不猜）。
pub fn node_version_at_least_min(version: &str) -> Option<bool> {
    let v = parse_version_numbers(version)?;
    let min = parse_version_numbers(NODE_MIN_VERSION)?;
    Some(cmp_versions(&v, &min) != std::cmp::Ordering::Less)
}

/// 版本号白名单：形如 `24.20.0`（至少两段、每段非空纯数字）。
/// 版本号会被拼进下载 URL、清单匹配名和落盘文件名，必须挡住 `..`、空段与任何路径片段。
pub fn is_safe_node_version(v: &str) -> bool {
    let mut parts = 0usize;
    for p in v.split('.') {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        parts += 1;
    }
    parts >= 2
}

/// 官方 MSI 的文件名（如 `node-v24.20.0-x64.msi`）。
/// 下载地址、清单条目匹配、落盘文件名三处共用这一个来源，避免任何一处拼歪。
pub fn node_msi_file_name_for(version: &str) -> String {
    format!("node-v{}-x64.msi", version)
}

/// 本次运行内实际解析到的 Node 版本；未解析成功时用固定回退版本。
static RESOLVED_NODE_VERSION: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn resolved_slot() -> &'static Mutex<Option<String>> {
    RESOLVED_NODE_VERSION.get_or_init(|| Mutex::new(None))
}

/// 记下解析到的最新 LTS 版本，让下载地址与向导展示保持一致。
pub fn record_node_version(v: &str) {
    *resolved_slot().lock().unwrap() = Some(v.to_string());
}

/// 当前应使用的 Node 版本号（已解析 → 最新补丁版；否则 → 回退版本）。
pub fn current_node_version() -> String {
    resolved_slot()
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| NODE_LTS_VERSION.to_string())
}

/// 某个具体版本的官方 MSI 下载地址（文件名由 node_msi_file_name_for 决定，两者不会分叉）
pub fn node_msi_url_for(version: &str) -> String {
    format!(
        "https://nodejs.org/dist/v{0}/{1}",
        version,
        node_msi_file_name_for(version)
    )
}

pub fn node_msi_url() -> String {
    node_msi_url_for(&current_node_version())
}

// ---------- PATH 环境：注册表刷新与子进程 PATH 组装 ----------
//
// 为什么需要这一段：引导安装 Node.js 发生在「本程序已经在运行」的时候。
// 官方 MSI 会把 C:\Program Files\nodejs 写进注册表的系统 PATH，但本进程持有的
// PATH 仍是启动时的旧快照 —— 所有由本程序派生的子进程都继承这个旧快照。
// 后果：npm.cmd 自身能用（它优先调用同目录的 node.exe），但它为原生依赖
// 派生的生命周期脚本是 `cmd /d /s /c node ./cnoke.cjs …`，这个**裸 node**
// 要查 PATH，于是报「'node' 不是内部或外部命令」（GBK 输出），
// npm 以 `npm error code 1` 失败。修复 = 派子进程时显式给出补好的 PATH。

/// 系统 PATH 所在注册表键（MSI 安装写这里）
const MACHINE_PATH_KEY: &str = r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
/// 当前用户 PATH 所在注册表键
const USER_PATH_KEY: &str = r"HKCU\Environment";

/// 注册表 PATH（Machine + User）的进程内缓存，避免每次派子进程都 reg query。
static REG_PATH: OnceLock<Mutex<Option<Vec<PathBuf>>>> = OnceLock::new();

fn reg_path_slot() -> &'static Mutex<Option<Vec<PathBuf>>> {
    REG_PATH.get_or_init(|| Mutex::new(None))
}

/// 丢弃注册表 PATH 缓存（安装完成后调用，强制重新读取）。
pub fn invalidate_reg_path_cache() {
    *reg_path_slot().lock().unwrap() = None;
}

/// 目录去重键：去尾部分隔符 + 小写（Windows 路径大小写不敏感）。
fn dir_key(d: &Path) -> String {
    d.to_string_lossy()
        .trim()
        .trim_end_matches(|c: char| c == '\\' || c == '/')
        .to_lowercase()
}

fn split_path_list(s: &str) -> Vec<PathBuf> {
    s.split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn current_path_dirs() -> Vec<PathBuf> {
    match std::env::var_os("PATH") {
        Some(v) => split_path_list(&v.to_string_lossy()),
        None => Vec::new(),
    }
}

fn dedupe_dirs(dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<PathBuf> = Vec::new();
    for d in dirs {
        let k = dir_key(&d);
        if k.is_empty() || seen.iter().any(|s| *s == k) {
            continue;
        }
        seen.push(k);
        out.push(d);
    }
    out
}

/// 从 `reg query` 的输出里取出指定值名对应的数据。
/// 输出形如：`    Path    REG_EXPAND_SZ    C:\Windows\system32;...`
fn parse_reg_value(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim_start();
        let head = match t.as_bytes().get(..name.len()) {
            Some(h) => h,
            None => continue,
        };
        if !head.eq_ignore_ascii_case(name.as_bytes()) {
            continue;
        }
        let rest = &t[name.len()..];
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        // 去掉类型列（REG_SZ / REG_EXPAND_SZ），剩下的才是值
        let Some((_, value)) = rest.trim_start().split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// 展开 `%SystemRoot%` 之类的引用；未知变量原样保留（不猜测）。
fn expand_percent(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) if j > 0 => {
                let name = &after[..j];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => out.push_str(&rest[i..i + j + 2]),
                }
                rest = &after[j + 1..];
            }
            _ => {
                out.push('%');
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 读某个注册表键下的 `Path` 值（失败返回空）。
fn registry_path_raw(key: &str) -> Option<String> {
    let mut cmd = Command::new("reg");
    cmd.arg("query").arg(key).arg("/v").arg("Path");
    apply_no_window(&mut cmd);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_reg_value(&decode_console_output(&out.stdout), "Path")
}

/// 注册表里的 PATH 目录（Machine 在前、User 在后），带缓存。
fn registry_path_dirs() -> Vec<PathBuf> {
    let slot = reg_path_slot();
    let mut guard = slot.lock().unwrap();
    if let Some(cached) = guard.as_ref() {
        return cached.clone();
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    for key in [MACHINE_PATH_KEY, USER_PATH_KEY] {
        if let Some(raw) = registry_path_raw(key) {
            dirs.extend(split_path_list(&expand_percent(&raw)));
        }
    }
    let dirs = dedupe_dirs(dirs);
    *guard = Some(dirs.clone());
    dirs
}

/// 用注册表里最新的 PATH 重建**本进程**的 PATH，返回本次新增的目录数。
/// 引导装完 Node.js 后调用：之后所有派生的子进程（含 detect 的 `where` 查询）
/// 都能看到新装的 node，不需要重启本程序。
pub fn refresh_process_path() -> usize {
    invalidate_reg_path_cache();
    let cur = current_path_dirs();
    let mut seen: Vec<String> = cur.iter().map(|d| dir_key(d)).collect();
    let mut merged = cur.clone();
    let mut added = 0usize;
    for d in registry_path_dirs() {
        let k = dir_key(&d);
        if seen.iter().any(|s| *s == k) {
            continue;
        }
        seen.push(k);
        merged.push(d);
        added += 1;
    }
    if added > 0 {
        if let Ok(joined) = std::env::join_paths(&merged) {
            std::env::set_var("PATH", joined);
        }
    }
    added
}

/// 为 npm / dsh 等子进程组装 PATH：把 node、npm、npm 全局 bin、dsh 所在目录放到最前面，
/// 再接本进程 PATH 与注册表 PATH（去重）。即使本进程 PATH 还是旧快照，
/// 子进程里的 `node`、npm 生命周期脚本也一定能被解析到。
pub fn child_path_for(exes: &[&str]) -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for e in exes {
        if let Some(d) = Path::new(e).parent() {
            if !d.as_os_str().is_empty() {
                dirs.push(d.to_path_buf());
            }
        }
    }
    dirs.extend(npm_global_bin_dirs());
    let cached = detect_all(false);
    for p in [&cached.node, &cached.npm, &cached.dsh] {
        if let Some(p) = p {
            if let Some(d) = p.parent() {
                dirs.push(d.to_path_buf());
            }
        }
    }
    dirs.extend(current_path_dirs());
    dirs.extend(registry_path_dirs());
    let dirs = dedupe_dirs(dirs);
    std::env::join_paths(&dirs)
        .unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

// ---------- Tauri 命令层使用的数据结构 ----------

#[derive(Serialize, Clone)]
pub struct EnvDetection {
    pub node_found: bool,
    pub node_path: String,
    pub node_version: Option<String>,
    /// Node 版本相对最低下限的状态："supported"（≥ 下限）/ "old"（低于下限）
    /// / "unknown"（没装，或版本串读不到/解析不了）。前端据此决定是否告警。
    pub node_min_state: String,
    /// 最低版本（展示用，如 `v22.19.0`），文案里的阈值不写死在前端
    pub node_min_version: String,
    pub npm_found: bool,
    pub npm_path: String,
    pub dsh_found: bool,
    pub dsh_path: String,
    /// 引导安装将下载的官方 Node.js LTS MSI 地址（展示用）
    pub node_msi_url: String,
    /// 官方手动下载页
    pub node_download_page: String,
}

impl EnvDetection {
    /// Node 版本够不够：Some(false) 明确低于下限，None 表示没法判定。
    /// 目前前端是直接读 `node_min_state` 字段判定的（同一份判定逻辑在前端也有一份），
    /// 这个方法留给需要「一次问清」的调用点用。
    ///
    /// 有意保留、当前无调用点：它是 `EnvDetection` 上「把版本串直接翻译成是与否」的
    /// 唯一入口，调用点出现时不该被迫在别处重写一遍判下限的逻辑。加 allow 只是让构建日志
    /// 保持干净（真正的使用点出现时这个属性可以删掉，编译器会重新盯着它）。
    #[allow(dead_code)]
    pub fn node_too_old(&self) -> Option<bool> {
        self.node_version
            .as_deref()
            .and_then(node_version_at_least_min)
            .map(|ok| !ok)
    }
}

/// 完整环境检测（强制刷新缓存）。供 setup 向导与「设置 → 自动检测」使用。
pub fn full_detect() -> EnvDetection {
    let paths = detect_all(true);
    let node_version = paths.node.as_deref().and_then(|p| quick_version(p, 10));
    let node_min_state = match node_version.as_deref().and_then(node_version_at_least_min) {
        Some(true) => "supported",
        Some(false) => "old",
        None => "unknown",
    };
    EnvDetection {
        node_found: paths.node.is_some(),
        node_path: opt_to_string(&paths.node),
        node_version,
        node_min_state: node_min_state.to_string(),
        node_min_version: NODE_MIN_VERSION_LABEL.to_string(),
        npm_found: paths.npm.is_some(),
        npm_path: opt_to_string(&paths.npm),
        dsh_found: paths.dsh.is_some(),
        dsh_path: opt_to_string(&paths.dsh),
        node_msi_url: node_msi_url(),
        node_download_page: NODE_DOWNLOAD_PAGE.to_string(),
    }
}

fn opt_to_string(p: &Option<PathBuf>) -> String {
    p.as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn parses_common_version_shapes() {
        assert_eq!(parse_version_numbers("v24.18.0"), Some(vec![24, 18, 0]));
        assert_eq!(parse_version_numbers("22.19.0"), Some(vec![22, 19, 0]));
        assert_eq!(parse_version_numbers(" 21.7.3 \r\n"), Some(vec![21, 7, 3]));
        // 残缺段按 0 补齐（22.19 == 22.19.0）
        assert_eq!(parse_version_numbers("22.19"), Some(vec![22, 19]));
        // 预发布后缀只取数字部分：0.1.5-rc.1 这类 npm 输出不该被误判
        assert_eq!(parse_version_numbers("v0.1.5-rc.1"), Some(vec![0, 1, 5]));
        assert_eq!(parse_version_numbers("^22.19.0"), Some(vec![22, 19, 0]));
        // 读不到 / 解析不了 → None（判「未知」，不猜）
        assert_eq!(parse_version_numbers(""), None);
        assert_eq!(parse_version_numbers("v"), None);
        assert_eq!(parse_version_numbers("node: not found"), None);
        assert_eq!(parse_version_numbers("v1.x.0"), None);
    }

    #[test]
    fn compares_semantically() {
        let min = parse_version_numbers(NODE_MIN_VERSION).unwrap();
        // 明确低于下限：21.x 整条线（含实测起不来的 21.7.3）与 22.18.x
        for old in ["21.7.3", "20.19.5", "22.18.0", "18.20.4", "21.99.99"] {
            let v = parse_version_numbers(old).unwrap();
            assert_eq!(cmp_versions(&v, &min), Ordering::Less, "{old} 应低于下限");
            assert_eq!(node_version_at_least_min(old), Some(false), "{old}");
        }
        // 恰好等于下限：比较结果是 Equal 而不是 Greater —— 这里同时替 node_version_at_least_min
        // 的正确性作证（Equal 必须算「满足」，否则下限本身会被误判为过低）
        let at_min = parse_version_numbers(NODE_MIN_VERSION).unwrap();
        assert_eq!(cmp_versions(&at_min, &min), Ordering::Equal);
        assert_eq!(node_version_at_least_min(NODE_MIN_VERSION), Some(true));

        // 高于下限：严格 Greater；「不低于下限」用 assert_ne!(.., Less) 表达，
        // 与循环里那句文案（应不低于下限）语义一致（原来断言的是 Greater，边界值必然失败）
        for ok in ["22.19", "22.19.1", "22.20.0", "24.18.0", "v24.20.0"] {
            let v = parse_version_numbers(ok).unwrap();
            assert_ne!(cmp_versions(&v, &min), Ordering::Less, "{ok} 应不低于下限");
            assert_eq!(node_version_at_least_min(ok), Some(true), "{ok}");
        }
        assert_eq!(node_version_at_least_min("not-a-version"), None);
    }
}
