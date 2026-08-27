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

/// 引导安装时使用的 Node.js LTS 版本（来自 nodejs.org 官方 dist）
pub const NODE_LTS_VERSION: &str = "20.18.1";
/// 官方下载页（备选手动安装入口）
pub const NODE_DOWNLOAD_PAGE: &str = "https://nodejs.org/en/download";

pub fn node_msi_url() -> String {
    format!(
        "https://nodejs.org/dist/v{}/node-v{}-x64.msi",
        NODE_LTS_VERSION, NODE_LTS_VERSION
    )
}

// ---------- Tauri 命令层使用的数据结构 ----------

#[derive(Serialize, Clone)]
pub struct EnvDetection {
    pub node_found: bool,
    pub node_path: String,
    pub node_version: Option<String>,
    pub npm_found: bool,
    pub npm_path: String,
    pub dsh_found: bool,
    pub dsh_path: String,
    /// 引导安装将下载的官方 Node.js LTS MSI 地址（展示用）
    pub node_msi_url: String,
    /// 官方手动下载页
    pub node_download_page: String,
}

/// 完整环境检测（强制刷新缓存）。供 setup 向导与「设置 → 自动检测」使用。
pub fn full_detect() -> EnvDetection {
    let paths = detect_all(true);
    let node_version = paths.node.as_deref().and_then(|p| quick_version(p, 10));
    EnvDetection {
        node_found: paths.node.is_some(),
        node_path: opt_to_string(&paths.node),
        node_version,
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
