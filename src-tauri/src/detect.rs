//! Node.js / npm / DSH 自动检测。
//!
//! 设计原则：
//! - 检测结果按进程缓存（避免 config::load 高频调用时反复 spawn `where`）；
//!   安装类操作（setup 向导装完 Node/DSH 后）用 `invalidate_cache()` 强制刷新。
//! - 只做"发现"，绝不修改用户配置；用户在设置里手动填写的有效路径优先。
//!   （两处例外，都是**由用户在设置里明确触发**的写入，且都不动 DSH 自己的配置：
//!   自定义安装位置写用户 PATH；「写入用户环境变量 DSH_HOME」写 `HKCU\Environment`。）
//! - 不下载、不内置任何运行时，只探测本机已有的安装。

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::process::{apply_no_window, command_for, decode_console_output};

/// 检测结果快照
#[derive(Debug, Clone, Default)]
pub struct EnvPaths {
    pub node: Option<PathBuf>,
    pub npm: Option<PathBuf>,
    pub dsh: Option<PathBuf>,
    /// pnpm（`npm install -g pnpm` 的产物；首装向导据此提示/代装，
    /// 以便 DSH 装好后能用它安装插件）
    pub pnpm: Option<PathBuf>,
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
    // 用户改 `.npmrc`（prefix）之后也要重新问一次 npm
    invalidate_npm_prefix_memo();
}

fn scan() -> EnvPaths {
    EnvPaths {
        node: find_node_exe(),
        npm: find_npm_cmd(),
        dsh: find_dsh_cmd(),
        pnpm: find_pnpm_cmd(),
    }
}

/// `where <name>`：返回**全部**确实存在的路径（按 PATH 顺序，只留真文件）。
///
/// 为什么不能只取第一个：`%LOCALAPPDATA%\Microsoft\WindowsApps\python.exe` 那个
/// 执行别名几乎总排在 PATH 最前，而它**确实是一个文件** —— 首个命中就被它占住时，
/// 排在后面的真实解释器会被整个丢掉，只剩注册表扫描与写死的目录兜底
/// （D 盘便携版、公司下发的自定义目录就会被误报成「未检测到」）。
pub(crate) fn where_all(name: &str) -> Vec<PathBuf> {
    let mut cmd = Command::new("where");
    cmd.arg(name);
    apply_no_window(&mut cmd);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let Ok(out) = cmd.output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = decode_console_output(&out.stdout);
    text.lines()
        .map(|l| PathBuf::from(l.trim()))
        .filter(|p| p.is_file())
        .collect()
}

/// `where <name>`：第一个确实存在的路径（= `where_all` 的首个命中）。
pub(crate) fn where_lookup(name: &str) -> Option<PathBuf> {
    where_all(name).into_iter().next()
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from)
}

/// 定位 node.exe
///
/// 查找顺序有意如此：
///   1. `where`（= 当前进程环境里的 PATH，与用户在同一时刻的终端所见一致）；
///   2. **注册表 PATH 里的目录** —— 补上「进程环境是旧快照」这一类：程序从资源管理器
///      启动时继承的是资源管理器自己的环境块，安装器刚写好的目录不在里面，于是
///      `where` 找不到、而硬编码候选又不认识自定义目录（实测症状：文件装好在
///      `D:\Programs\Node.js`，程序却说「未检测到 node.exe」）；
///   3. 最后才是硬编码的常见位置（`%ProgramFiles%\nodejs`、nvm/fnm/volta/scoop）。
///      注意它们只是**兜底猜测**，过去因为默认目录正好在这里，掩盖了第 2 条缺失。
pub fn find_node_exe() -> Option<PathBuf> {
    if let Some(p) = where_lookup("node.exe") {
        return Some(p);
    }
    if let Some(p) = find_in_dirs(&registry_path_dirs(), "node.exe") {
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

/// 在这些目录里找某个文件（按给定顺序，返回第一个确实存在的）。
fn find_in_dirs(dirs: &[PathBuf], file: &str) -> Option<PathBuf> {
    dirs.iter()
        .map(|d| d.join(file))
        .find(|p| p.is_file())
}

/// 同 `find_in_dirs`，但返回**全部**命中的路径。同一份 PATH 里两处都有 python 时
/// （Program Files 与便携版并存），只取第一个会让另一个连 `--version` 验证的机会都没有。
fn find_all_in_dirs(dirs: &[PathBuf], file: &str) -> Vec<PathBuf> {
    dirs.iter().map(|d| d.join(file)).filter(|p| p.is_file()).collect()
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

/// 定位 pnpm（`pnpm.cmd` / `pnpm.exe`）。
///
/// 查找顺序与 `find_dsh_cmd` 完全同款，理由也相同：
///   1. `where`（当前进程 PATH，= 用户同一时刻的终端所见）；
///   2. 注册表 PATH 里的目录 —— 补上「本进程环境块是旧快照」这一类
///      （刚装完 Node / 刚装完 pnpm 的同一次运行里，`where` 还看不见新目录）；
///   3. npm 的全局 bin 目录兜底（`%APPDATA%\npm`、向导登记的自定义目录、node 同目录）。
///
/// 为什么同时认 `pnpm.exe`：除 `npm install -g pnpm`（写 `pnpm.cmd` 进 npm 全局目录）
/// 之外，pnpm 官方独立安装版装的是 `pnpm.exe`（默认 `%LOCALAPPDATA%\pnpm`），
/// 那种情况下 `where pnpm.cmd` 必然落空。
pub fn find_pnpm_cmd() -> Option<PathBuf> {
    for name in ["pnpm.cmd", "pnpm.exe"] {
        if let Some(p) = where_lookup(name) {
            return Some(p);
        }
    }
    for name in ["pnpm.cmd", "pnpm.exe"] {
        if let Some(p) = find_in_dirs(&registry_path_dirs(), name) {
            return Some(p);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for dir in npm_global_bin_dirs() {
        candidates.push(dir.join("pnpm.cmd"));
        candidates.push(dir.join("pnpm.exe"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// 用户在向导里指定的**自定义安装位置**（npm 的全局目录），进程内登记在这里。
/// 两个来源：DSH 的自定义安装位置、pnpm 落在的 npm 自定义全局目录（见 add_extra_bin_dir）。
///
/// 为什么需要这份登记：npm 的 `--prefix <目录>` 只负责把 dsh.cmd 写进那个目录，
/// **不会**把它加进 PATH，而本进程的路径探测（`where dsh`）与 `%APPDATA%\npm`
/// 这两条现有线索都指不到它。装完立刻登记，配合 `invalidate_cache()` 就能当场检测到。
/// （长期生效靠的是把目录写进用户 PATH，见 `append_to_user_path`；这份登记负责的是
/// 「装完到重启之间」这段窗口。）
static EXTRA_BIN_DIRS: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();

fn extra_bin_dirs_slot() -> &'static Mutex<Vec<PathBuf>> {
    EXTRA_BIN_DIRS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 登记一组额外的 bin 目录（DSH 自定义安装位置）。内容真的变了才丢缓存。
pub fn set_extra_bin_dirs(dirs: Vec<PathBuf>) {
    let wanted = dedupe_dirs(dirs);
    {
        let mut guard = extra_bin_dirs_slot().lock().unwrap();
        if dir_keys(guard.as_slice()) == dir_keys(&wanted) {
            return;
        }
        *guard = wanted;
    }
    invalidate_cache();
}

/// **追加**一个 bin 目录（不动已登记的那些）。
///
/// 与 `set_extra_bin_dirs` 的区别就是「追加 vs 覆盖」：pnpm 装进 npm 的自定义全局目录
/// （`.npmrc` 里配过 `prefix`）时用它登记 —— 那个目录通常不在 PATH 里，不登记的话
/// 向导会一直显示「缺少 pnpm」，即便我们刚刚亲手把它装上。而 DSH 那边可能同时登记着
/// 另一个自定义目录，覆盖过去会把 DSH 的登记抹掉。
pub fn add_extra_bin_dir(dir: PathBuf) {
    {
        let mut guard = extra_bin_dirs_slot().lock().unwrap();
        if guard.iter().any(|d| dir_key(d) == dir_key(&dir)) {
            return; // 已在列表里：什么都不用改，也不必丢缓存
        }
        guard.push(dir);
    }
    invalidate_cache();
}

fn extra_bin_dirs() -> Vec<PathBuf> {
    extra_bin_dirs_slot().lock().unwrap().clone()
}

/// 目录列表的「判等键」：按 dir_key（去尾分隔符 + 小写）比较，避免只是大小写/末尾
/// 分隔符不同就当作「变了」而白白丢一次检测缓存。
fn dir_keys(dirs: &[PathBuf]) -> Vec<String> {
    dirs.iter().map(|d| dir_key(d)).collect()
}

/// npm 全局 bin 目录 —— 找 `dsh` / `pnpm` 这类「npm 全局装出来的」命令时用的候选目录。
///
/// 顺序即优先级，三条线索各管一件事：
///   ① **npm 自己解析出的 prefix**（`npm config get prefix`）——权威答案。用户在 `~/.npmrc`
///      里写过 `prefix=D:\...` 时，全局命令就装在那里；这个值只能问 npm 拿到。
///      真机故障（2026-10-06 现场）：用户的 `.npmrc` 里是 `prefix=D:\Programs\npm`、`dsh.cmd`
///      也确实在那里，但该目录**不在 PATH 里**（PATH 里只有更早的旧目录），于是
///      `where dsh` 找不到、向导也没登记过这个目录（那是用户自己写的 .npmrc，程序从未写过
///      它，`npm_prefix_claimed=false`）—— 程序因此报「未找到 DSH」，而 dsh 明明装好且可用。
///      这就是把本线索排在第一位的原因：**只要 npm 认得，我们就该认得**。
///      性能：检测结果按次缓存（detect_all），这条最多在每次扫描时多跑一次 npm。
///   ② `%APPDATA%\npm`：Windows 上**没有配置 prefix 时**的默认全局目录。
///      问不出 prefix 时（npm 缺失 / 超时 / 输出认不出）它是正确答案。
///   ③ 向导登记的自定义目录（`--prefix` 装过东西的那次）与 node 自身目录。
fn npm_global_bin_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |dirs: &mut Vec<PathBuf>, d: PathBuf| {
        let key = dir_key(&d);
        if !key.is_empty() && !dirs.iter().any(|x| dir_key(x) == key) {
            dirs.push(d);
        }
    };
    let npm_prefix = npm_prefix_for_global_bins();
    if !npm_prefix.is_empty() {
        push(&mut dirs, PathBuf::from(&npm_prefix));
    }
    if let Some(appdata) = env_path("APPDATA") {
        push(&mut dirs, appdata.join("npm"));
    }
    // 向导登记的自定义安装位置：它可能比 npm 当前配置更新（刚装完、npm 还没反应过来）
    for d in extra_bin_dirs() {
        push(&mut dirs, d);
    }
    if let Some(node) = find_node_exe() {
        if let Some(dir) = node.parent() {
            push(&mut dirs, dir.to_path_buf());
        }
    }
    dirs
}

/// 给 `npm_global_bin_dirs` 用的 npm prefix：**优先不要走 `default_npm_prefix` 的回落**。
///
/// 记住「问过 npm 之后拿到的全局目录」。
///
/// 为什么必须记住 —— 这是一次**真机崩溃**的教训（0xc00000fd 栈溢出，程序双击后没有窗口）：
///
/// ```text
/// npm_global_bin_dirs()            ← 想拿 npm 的 prefix 作为候选目录
///   → npm_prefix_for_global_bins() → find_npm_cmd()
///     → 启动 npm（run_capture_timeout_in）
///       → child_path_for()          ← 给子进程拼 PATH
///         → npm_global_bin_dirs()   ← 回到起点
///           → …                      ← 无限递归，栈打穿
/// ```
///
/// `child_path_for` 是「**启动任何子进程**都要走」的函数，而它内部要 `npm_global_bin_dirs`；
/// 于是任何一次「为了问 npm 而启动 npm」都会绕回自己。加上这层记忆之后：
///   - 第一次问（由 find_dsh_cmd / find_pnpm_cmd → npm_global_bin_dirs 发起，**不在 spawn 链上**）
///     真去跑一次 npm 并记住结果；
///   - 之后（尤其是 spawn 链里的 `child_path_for`）直接读记住的值，不再启动任何进程 → 环断掉。
///
/// 记忆只在本进程内、不落盘：它只是「这一次运行里 npm 说它装在哪」，用户改完 `.npmrc` 后
/// 由 `invalidate_cache()` 清掉（PATH 变更同源），下一次扫描重新问。
static NPM_PREFIX_MEMO: OnceLock<Mutex<Option<String>>> = OnceLock::new();
/// 正在问 npm 的标记（见下：重入必须立刻返回，否则又会绕回 spawn 链）。
static NPM_PREFIX_RESOLVING: AtomicBool = AtomicBool::new(false);

fn npm_prefix_memo_slot() -> &'static Mutex<Option<String>> {
    NPM_PREFIX_MEMO.get_or_init(|| Mutex::new(None))
}

fn npm_prefix_for_global_bins() -> String {
    // ① 已问过就直接返回（空串也照样记住：问不到 npm 时不必每次都重试一遍）
    if let Some(v) = npm_prefix_memo_slot().lock().unwrap().clone() {
        return v;
    }
    // ② 正在问（= 我们已经在这个函数里面，正通过 spawn 链绕回来）→ 立刻返回空串。
    //    这一道保险不依赖「谁先调用」的顺序：即便将来 `child_path_for` 又重新用上
    //    `npm_global_bin_dirs`，也只会少一条候选目录，不会再把栈打穿。
    if NPM_PREFIX_RESOLVING.swap(true, Ordering::SeqCst) {
        return String::new();
    }
    let prefix = match find_npm_cmd() {
        // 只有**确实问到 npm 的回答**才用；问不到就返回空串，让调用方走自己的默认分支
        Some(npm) => npm_reported_prefix(&npm).unwrap_or_default(),
        None => String::new(),
    };
    *npm_prefix_memo_slot().lock().unwrap() = Some(prefix.clone());
    NPM_PREFIX_RESOLVING.store(false, Ordering::SeqCst);
    prefix
}

/// 丢掉「问过 npm 的全局目录」这份记忆（PATH / `.npmrc` 变更后由 `invalidate_cache` 调用）。
fn invalidate_npm_prefix_memo() {
    *npm_prefix_memo_slot().lock().unwrap() = None;
}

/// 在**指定目录**里找 dsh 启动脚本（dsh.cmd / dsh.exe / dsh.bat）。
/// 装后核对专用：不靠 PATH 认自己刚装的东西（见 process.rs 的成功分支）。
pub fn dsh_cmd_in_dir(dir: &Path) -> Option<PathBuf> {
    ["dsh.cmd", "dsh.exe", "dsh.bat"]
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.is_file())
}

/// 定位 dsh 启动脚本：dsh.cmd / dsh.exe / dsh.bat
pub fn find_dsh_cmd() -> Option<PathBuf> {
    for name in ["dsh.cmd", "dsh.exe", "dsh.bat"] {
        if let Some(p) = where_lookup(name) {
            return Some(p);
        }
    }
    // 注册表 PATH 里的目录：与 find_node_exe 同一条理由 —— 进程环境可能是**旧快照**，
    // 而「自定义 DSH 安装位置」正是靠写用户 PATH 生效的；刚写完 PATH 的同一次运行里，
    // 注册表已经更新、本进程的环境块却还是旧的，只有走注册表才认得出。
    for name in ["dsh.cmd", "dsh.exe", "dsh.bat"] {
        if let Some(p) = find_in_dirs(&registry_path_dirs(), name) {
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

// ---------- Python 检测（首选项「Python 环境」块 + 安装后核对） ----------

/// 最低可用的 Python：3.8（再老的解释器装不上今天这批办公 / 数据分析包）。
/// 「系统已装 Python 就跳过本体安装」只对**够新**的安装成立 —— 装着 Python 2.7 的机器
/// 必须继续往下装，否则后面每一条 pip install 都会在用户看不懂的地方失败。
pub const PYTHON_MIN_VERSION: &str = "3.8.0";

/// 引导安装回退用的固定版本：python.org 下载页解析失败（离线、改版、被墙）时用它。
/// 与 `NODE_LTS_VERSION` 同一角色 —— 探测失败不该让安装整体卡住。
pub const PYTHON_FALLBACK_VERSION: &str = "3.14.7";

/// 官方下载页：既是「最新稳定版」版本号的来源，也是解析失败时的手动安装入口。
pub const PYTHON_DOWNLOAD_PAGE: &str = "https://www.python.org/downloads/";

/// 版本号白名单（与 `is_safe_node_version` 同尺度）：至少两段、每段非空纯数字。
/// 版本号会被拼进下载 URL、发布页 URL 与落盘文件名，必须挡住 `..`、空段与任何路径片段。
pub fn is_safe_python_version(v: &str) -> bool {
    let mut parts = 0usize;
    for p in v.split('.') {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        parts += 1;
    }
    parts >= 2
}

/// 版本串是否 ≥ `PYTHON_MIN_VERSION`（解析不出来 → false，判「不够用」）。
pub fn python_version_usable(version: &str) -> bool {
    let (Some(v), Some(min)) = (
        parse_version_numbers(version),
        parse_version_numbers(PYTHON_MIN_VERSION),
    ) else {
        return false;
    };
    cmp_versions(&v, &min) != std::cmp::Ordering::Less
}

/// 从 `--version` 的输出（`Python 3.14.7`、`Python 3.14.7+ heads/...`、或直接 `3.14.7`）
/// 里取出 `X.Y.Z`。取不出（空输出、Python 2 的报错、执行别名那类「什么都不说」）
/// 一律返回 None —— 调用方据此判「没有可用的 Python」，而不是猜一个版本。
pub fn parse_python_version(out: &str) -> Option<String> {
    let first = out.lines().map(str::trim).find(|l| !l.is_empty())?;
    // 大小写不敏感地剥掉 `Python ` 前缀；剥不掉也无妨，下面按「开头那段数字+点」取值
    let head = match first.get(..6) {
        Some(p) if p.eq_ignore_ascii_case("Python") => first[6..].trim(),
        _ => first,
    };
    let token = head.split_whitespace().next().unwrap_or("");
    // 只取开头那段「数字 + 点」：`3.14.7+`、`3.14.7 (tags/…)`、`3.15.0rc1` 分别取到
    // 3.14.7 / 3.14.7 / 3.15.0 —— 后缀里的构建信息对「这个解释器能不能用」毫无意义
    let end = token
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(token.len());
    let v = token[..end].trim_end_matches('.');
    if v.matches('.').count() < 2 {
        return None;
    }
    if !v.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    Some(v.to_string())
}

/// 一个可用的 Python 解释器。`prefix` 是紧跟在程序名之后的固定参数：
/// 直接找到 `python.exe` 时为空；只剩 `py.exe` 启动器时是 `["-3"]`（明确要 3.x）。
#[derive(Debug, Clone)]
pub struct PythonExe {
    pub program: PathBuf,
    pub prefix: Vec<String>,
}

impl PythonExe {
    /// 拼出完整参数表：`<prefix...> <rest...>`
    pub fn full_args(&self, rest: &[&str]) -> Vec<String> {
        let mut v = self.prefix.clone();
        v.extend(rest.iter().map(|s| (*s).to_string()));
        v
    }

    /// 跑一次 `--version` 拿版本号；读不出返回 None。
    pub fn version(&self) -> Option<String> {
        let args = self.full_args(&["--version"]);
        let out = run_capture_timeout(&self.program.to_string_lossy(), &args, 10)?;
        parse_python_version(&out)
    }
}

/// Microsoft Store 的执行别名（`%LOCALAPPDATA%\Microsoft\WindowsApps\python.exe`）
/// **文件确实存在**，但点开会去应用商店；它的 `--version` 也读不出正经输出。
/// 这里既按路径显式排除，也要求文件不是 0 字节，两道一起挡。
fn usable_python_file(p: &Path) -> bool {
    if !p.is_file() {
        return false;
    }
    let s = p.to_string_lossy().to_lowercase();
    if s.contains("\\windowsapps\\") {
        return false;
    }
    std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
}

/// 常见安装根目录下的 Python 目录（`%LocalAppData%\Programs\Python\Python3xx`、
/// `%ProgramFiles%\Python3xx`…），按「路径长的优先、再按字典序」排：
/// 同一个根下公共前缀等长，路径长 = 目录名长，`Python314` 因此排在 `Python39` 前
/// （纯字典序会把 3.10+ 排到 3.9 后面）。
fn python_dirs_under(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return v;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.to_ascii_lowercase().starts_with("python") {
            v.push(p);
        }
    }
    v.sort_by(|a, b| {
        let ka = a.to_string_lossy().to_lowercase();
        let kb = b.to_string_lossy().to_lowercase();
        kb.len().cmp(&ka.len()).then(kb.cmp(&ka))
    });
    v
}

/// 定位一个**可用**的 Python（≥ 3.8）。查找顺序与 find_node_exe 同一套理由：
///   1. `where python.exe` / `python3.exe`（= 本进程环境的 PATH，与用户终端一致）；
///   2. 注册表 PATH 里的目录 —— Python 安装器刚写完 PATH 时，本进程环境还是旧快照；
///   3. 常见安装根目录（上一步的 2 还没覆盖到的自定义 / 未写 PATH 的安装）；
///   4. 最后才是 `py.exe` 启动器（`py -3`）。
///
/// 每个候选都要**真跑一次 `--version` 并核对 ≥ 3.8** 才算数 —— 这一步同时挡掉三类
/// 「文件在但用不了」：Store 执行别名、Python 2、半残安装。所以这里会有 1~N 次
/// 子进程调用，最坏情况（每步都失败）也就几秒，而它只在打开首选项 / 装完之后跑。
pub fn find_python() -> Option<PythonExe> {
    // 第 1 步收**全部**命中而不是第一个：PATH 里的 WindowsApps 执行别名是个真文件，
    // 取首个命中会把排在它后面的真实解释器整个丢掉（见 where_all 的注释）。
    let mut direct: Vec<PathBuf> = Vec::new();
    for name in ["python.exe", "python3.exe"] {
        for p in where_all(name) {
            if !direct.contains(&p) {
                direct.push(p);
            }
        }
    }
    for name in ["python.exe", "python3.exe"] {
        for p in find_all_in_dirs(&registry_path_dirs(), name) {
            if !direct.contains(&p) {
                direct.push(p);
            }
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(la) = env_path("LOCALAPPDATA") {
        roots.push(la.join("Programs").join("Python"));
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = env_path(var) {
            roots.push(base);
        }
    }
    for root in &roots {
        for dir in python_dirs_under(root) {
            if let Some(p) = python_exe_in_dir(&dir) {
                direct.push(p);
            }
        }
    }

    let mut candidates: Vec<PythonExe> = Vec::new();
    for p in direct {
        if usable_python_file(&p) {
            candidates.push(PythonExe { program: p, prefix: Vec::new() });
        }
    }
    if let Some(p) = where_lookup("py.exe") {
        if usable_python_file(&p) {
            candidates.push(PythonExe { program: p, prefix: vec!["-3".to_string()] });
        }
    }

    for cand in candidates {
        if let Some(v) = cand.version() {
            if python_version_usable(&v) {
                return Some(cand);
            }
        }
    }
    None
}

/// 在**指定目录**里找 python.exe（装后核对专用：不靠 PATH 认自己刚装的东西，
/// 与 dsh_cmd_in_dir 同一条理由）。
pub fn python_exe_in_dir(dir: &Path) -> Option<PathBuf> {
    let p = dir.join("python.exe");
    if p.is_file() { Some(p) } else { None }
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
///
/// 目录名是 `latest-v24.x`（**带 `.x`**），不是 `latest-v24`：少了 `.x` 直接 404，
/// 而后果不止「探测失败」——它会悄悄退化成固定版本（装一个旧补丁版），并且让下载器
/// 走 PowerShell 回退，用户会看到一个「要不要允许打开 PowerShell」的弹窗，
/// 看着像程序在做不该做的事。
/// 实测：`dist/latest-v24.x/SHASUMS256.txt` = 200；`dist/latest-v24/...` = 404 File not found。
pub fn node_shasums_url() -> String {
    format!(
        "https://nodejs.org/dist/latest-v{}.x/SHASUMS256.txt",
        NODE_LTS_LINE
    )
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

/// pnpm 的最低版本（用户拍板：**pnpm 必须 ≥ 10，低了就必须更新** —— DSH 装插件要走 pnpm，
/// 旧版在新版 lockfile / 协议上会出问题）。比较用 `10.0.0`，展示用下面的 LABEL
/// （界面说「≥ v10」比「≥ v10.0.0」更贴合需求原话）。
pub const PNPM_MIN_VERSION: &str = "10.0.0";
/// 上面那个常量的展示形态（用于界面文案）
pub const PNPM_MIN_VERSION_LABEL: &str = "v10";

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

/// 版本串是否 ≥ `PNPM_MIN_VERSION`。与 `node_version_at_least_min` 同一条纪律：
/// 解析不出来返回 None —— 由调用方判「未知」，**读不出版本不等于「版本过低」**，
/// 后者要拦下用户点「完成」，把读不出当成过低会把人堵死在一个修不了的状态里。
pub fn pnpm_version_at_least_min(version: &str) -> Option<bool> {
    let v = parse_version_numbers(version)?;
    let min = parse_version_numbers(PNPM_MIN_VERSION)?;
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

// ---------- Python 官方安装包的地址（与 node_msi_url_for 同一角色：一处来源，处处同源） ----------

/// 官方安装包的架构后缀。python.org 对 Windows 只发三种 exe：
/// `-amd64`（x64）、`-arm64`、以及**不带后缀**的 x86。按机器（不是按本进程）判断：
/// 32 位进程跑在 64 位 Windows 上时 `PROCESSOR_ARCHITECTURE` 是 `x86`，
/// 真实架构只写在 `PROCESSOR_ARCHITEW6432` 里 —— 只看前者会把一台 x64 机器
/// 判成要装 32 位 Python。两者都认不出来时给空串 = x86 那个无后缀文件名
/// （宁可装 32 位也不拼出一个不存在的地址）。
fn python_installer_arch() -> &'static str {
    let arch = std::env::var("PROCESSOR_ARCHITEW6432")
        .or_else(|_| std::env::var("PROCESSOR_ARCHITECTURE"))
        .unwrap_or_default()
        .to_ascii_uppercase();
    match arch.as_str() {
        "AMD64" => "amd64",
        "ARM64" => "arm64",
        _ => "",
    }
}

/// 某个版本的官方安装包文件名（如 `python-3.14.7-amd64.exe`）。
pub fn python_installer_file_name_for(version: &str) -> String {
    let arch = python_installer_arch();
    if arch.is_empty() {
        format!("python-{}.exe", version)
    } else {
        format!("python-{}-{}.exe", version, arch)
    }
}

/// 安装包直链（`https://www.python.org/ftp/python/<版本>/<文件名>`）
pub fn python_installer_url_for(version: &str) -> String {
    format!(
        "https://www.python.org/ftp/python/{}/{}",
        version,
        python_installer_file_name_for(version)
    )
}

/// 该版本的官方发布页 —— 页面表格里挂着每个文件的 SHA-256（取哈希用）。
/// 路径里的版本号是**去掉点**的 `python-3147` 形式，与 python.org 的链接一致。
pub fn python_release_page_url_for(version: &str) -> String {
    format!(
        "https://www.python.org/downloads/release/python-{}/",
        version.replace('.', "")
    )
}

/// 官方 Node.js MSI 在本机的**默认安装目录**（`%ProgramFiles%\nodejs`）。
///
/// 为什么由后端算、而不是前端写死 `C:\Program Files\nodejs`：Windows 不一定装在
/// C 盘（Windows 装在 D 盘时 ProgramFiles 就是 `D:\Program Files`），写死会让
/// 预填值变成一句谎话，用户照着它装反而落到一个非默认的位置。
/// 取自环境变量，取不到时才回退到最常见的 `C:\Program Files\nodejs`。
///
/// 注意这个值与 MSI 自己算出的默认目录是一致的（MSI 同样以 ProgramFiles 为基准），
/// 所以「预填、用户不改」= 传过去的 INSTALLDIR 恰好等于 MSI 默认值，行为与不传无异。
pub fn default_node_install_dir() -> String {
    let base = std::env::var("ProgramFiles")
        .ok()
        .map(|v| v.trim().trim_end_matches(|c| c == '\\' || c == '/').to_string())
        .filter(|v| !v.is_empty());
    match base {
        Some(b) => format!("{}\\nodejs", b),
        None => r"C:\Program Files\nodejs".to_string(),
    }
}

/// Windows 上 npm 未做任何配置时的默认**全局目录**：`%APPDATA%\npm`
/// （APPDATA 缺失时退到 `%USERPROFILE%\AppData\Roaming\npm`；两者都没有返回 None）。
fn appdata_npm_dir() -> Option<PathBuf> {
    if let Some(base) = env_path("APPDATA") {
        return Some(base.join("npm"));
    }
    env_path("USERPROFILE").map(|h| h.join("AppData").join("Roaming").join("npm"))
}

/// npm 在本机的全局目录 —— 也就是「不传 `--prefix` 时 DSH 会被装到哪里」。
///
/// 为什么要问 npm、而不是直接拼 `%APPDATA%\npm`：用户在 `.npmrc`（或环境变量）里配过
/// `prefix=` 时，npm 装到的是那里；那个值只存在于 npm 自己的配置解析里，从外面猜不出来。
/// 而向导里的「安装位置」是**预填**给用户看的 —— 预填一个错的位置，比留空更糟。
/// 所以先问一次 `npm config get prefix`，拿不到（npm 缺失 / 超时 / 输出认不出）才回落到
/// Windows 上的官方默认值。
pub fn default_npm_prefix() -> String {
    default_npm_prefix_for(find_npm_cmd().as_deref())
}

/// 同上，但复用调用方**已经**探测到的 npm 路径（`full_detect` 手上就有），
/// 免得为了一行配置再跑一遍 `where` / 注册表查找。
fn default_npm_prefix_for(npm: Option<&Path>) -> String {
    if let Some(npm) = npm {
        if let Some(p) = npm_reported_prefix(npm) {
            return p;
        }
    }
    appdata_npm_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// `npm config get prefix` → 绝对路径（不成立就 None，不猜）。
fn npm_reported_prefix(npm: &Path) -> Option<String> {
    let npm_s = npm.to_string_lossy().to_string();
    let out = run_capture_timeout(
        &npm_s,
        &[
            "config".to_string(),
            "get".to_string(),
            "prefix".to_string(),
        ],
        // 只值这么点时间：这是个「预填更好」的锦上添花，不该让环境检测卡住。
        // 超时就回落 %APPDATA%\npm（对本机绝大多数用户就是正确答案）。
        5,
    )?;
    // 从**后往前**找第一行「看起来是绝对路径、且不是 npm 自己的告警」的内容。
    // 取最后一行是因为 `npm config get` 的值总是打在最后。
    pick_path_line(&out)
}

/// `npm config get <键>` 输出的取值规则（纯函数，便于离线单测）：
/// 最后一行形如绝对路径、且不是 npm 自己的告警/日志行才作数；认不出返回 None（不猜）。
fn pick_path_line(out: &str) -> Option<String> {
    out.lines()
        .rev()
        .map(str::trim)
        .find(|l| {
            !l.is_empty() && !l.starts_with("npm ") && l.contains(':') && Path::new(l).is_absolute()
        })
        .map(str::to_string)
}

/// npm 的**用户级**配置文件路径（`~/.npmrc`）。
///
/// 问 `npm config get userconfig` 而不是直接拼 `%USERPROFILE%\.npmrc`：环境变量
/// `npm_config_userconfig` 之类会改变它的位置，而我们要动的是**npm 真正在读的那个文件**。
/// 问不到（npm 缺失 / 超时）才回落 `%USERPROFILE%\.npmrc`；连家目录都没有则返回 None，
/// 调用方必须当作「不能改」处理（绝不乱猜一个路径去写）。
pub fn npm_userconfig_path() -> Option<PathBuf> {
    if let Some(npm) = find_npm_cmd() {
        let npm_s = npm.to_string_lossy().to_string();
        let args = [
            "config".to_string(),
            "get".to_string(),
            "userconfig".to_string(),
        ];
        if let Some(out) = run_capture_timeout(&npm_s, &args, 5) {
            if let Some(p) = pick_path_line(&out) {
                return Some(PathBuf::from(p));
            }
        }
    }
    env_path("USERPROFILE").map(|h| h.join(".npmrc"))
}

/// 问**裸 npm**（不带 `--prefix`、不带任何环境覆盖）现在会把「全局目录」解析到哪里。
///
/// 与 [`default_npm_prefix`] 的区别只在**什么时候问**：那个是给向导预填用的（进程刚起来、
/// 还没写任何配置），这个是在我们把 `prefix=<目录>` 写进 `~/.npmrc`（config::apply_npm_prefix）
/// 之后回来核对 —— npm 真的按新配置走了吗？被 `npm_config_prefix` 环境变量或项目级
/// `.npmrc` 压过时，答案会是别处，而那种情况下用户裸 `npm uninstall -g` 仍然删不掉 DSH，
/// 必须当场说出来（见 process.rs::setup_install_dsh 装后那段）。
///
/// 与 `npm_effective_cache_dir` 一样：显式补 PATH（进程环境可能是装 Node 之前的旧快照），
/// 问不到（npm 缺失 / 超时 / 输出认不出）返回 None —— 调用方按「核对不了」处理，不猜。
pub fn effective_npm_prefix() -> Option<String> {
    let npm = find_npm_cmd()?;
    let npm_s = npm.to_string_lossy().to_string();
    let args = [
        "config".to_string(),
        "get".to_string(),
        "prefix".to_string(),
    ];
    let out = run_capture_timeout(&npm_s, &args, 10)?;
    pick_path_line(&out)
}

/// 本机**实际生效**的 npm 缓存目录（含环境变量 / 项目级 `.npmrc` 的覆盖），
/// 只用于首选项里那行「当前生效」提示 —— 它和用户填的值不一致时，光看我们的设置页
/// 是看不出来的。问不到才回落 Windows 的默认位置 `%LOCALAPPDATA%\npm-cache`。
pub fn npm_effective_cache_dir() -> String {
    if let Some(npm) = find_npm_cmd() {
        let npm_s = npm.to_string_lossy().to_string();
        let args = [
            "config".to_string(),
            "get".to_string(),
            "cache".to_string(),
        ];
        if let Some(out) = run_capture_timeout(&npm_s, &args, 5) {
            if let Some(p) = pick_path_line(&out) {
                return p;
            }
        }
    }
    env_path("LOCALAPPDATA")
        .map(|p| p.join("npm-cache").to_string_lossy().to_string())
        .unwrap_or_default()
}

/// `npm/pnpm config get registry` 输出的取值规则（纯函数，便于离线单测）：
/// 取**最后一行**以 `http://` 或 `https://` 开头的内容 —— 值总是打在最后，
/// 而告警/错误行（`npm warn …`、`Error: × …`）永远不会长这样，所以认不出就返回 None。
fn pick_registry_line(out: &str) -> Option<String> {
    out.lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with("http://") || l.starts_with("https://"))
        .map(str::to_string)
}

/// 在 `cwd` 下**npm 实际会用**的源（含项目级 `.npmrc` 与环境变量的覆盖）。
/// 读不到返回 None —— 调用方据此报错，不能猜一个默认值当"目标"写进 pnpm。
pub fn npm_effective_registry(cwd: &str) -> Option<String> {
    let npm = find_npm_cmd()?;
    let npm_s = npm.to_string_lossy().to_string();
    let args = [
        "config".to_string(),
        "get".to_string(),
        "registry".to_string(),
    ];
    let out = run_capture_timeout_in(&npm_s, &args, 15, cwd)?;
    pick_registry_line(&out)
}

/// 在 `cwd` 下 **pnpm 实际会用**的源。与 [`npm_effective_registry`] 同一个 cwd 才可比：
/// 项目级 `.npmrc` 对两者同样生效，所以「读出来不一致」只可能来自 pnpm 自己那份配置
/// （或它的内置默认）—— 也正是「对齐」按钮要动的那一处。
pub fn pnpm_effective_registry(cwd: &str) -> Option<String> {
    let pnpm = find_pnpm_cmd()?;
    let pnpm_s = pnpm.to_string_lossy().to_string();
    let args = [
        "config".to_string(),
        "get".to_string(),
        "registry".to_string(),
    ];
    let out = run_capture_timeout_in(&pnpm_s, &args, 15, cwd)?;
    pick_registry_line(&out)
}

/// 带超时地跑一个小命令并合并捕获 stdout + stderr（非零退出或拿不到输出返回 None）。
///
/// 与 process.rs 的 `run_cmd_capture` 同款，但那边要求 cwd、且不对外；这里只有
/// 「读一行 npm 配置」这种最小需求，所以留一个更小的本地版本，避免为它放宽那边的可见性。
fn run_capture_timeout(program: &str, args: &[String], timeout_secs: u64) -> Option<String> {
    run_capture_timeout_in(program, args, timeout_secs, "")
}

/// 同上，但**指定工作目录**。
///
/// 存在的理由是 registry：`npm/pnpm config get registry` 的答案随 cwd 变（项目级
/// `.npmrc` 只对它所在的那条目录链生效），而我们要比的正是「两个工具在**同一个**
/// 目录下各自会用哪个源」—— 用本进程自己的 cwd 去问，比出来的不是 DSH 工作目录的现实。
fn run_capture_timeout_in(
    program: &str,
    args: &[String],
    timeout_secs: u64,
    cwd: &str,
) -> Option<String> {
    let mut cmd = command_for(program, args).ok()?;
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    apply_no_window(&mut cmd);
    // 子进程 PATH 显式补全（可能刚装完 Node，本进程环境还是旧快照）
    cmd.env("PATH", child_path_for(&[program]));
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut text = String::new();
                if let Some(mut so) = stdout.take() {
                    use std::io::Read;
                    let _ = so.read_to_string(&mut text);
                }
                if let Some(mut se) = stderr.take() {
                    use std::io::Read;
                    let _ = se.read_to_string(&mut text);
                }
                if !status.success() {
                    return None;
                }
                return Some(decode_console_output(text.as_bytes()));
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

/// 把 `dir` 插到**本进程** PATH 的最前面（已存在则不动），返回是否真的插入。
///
/// 为什么需要它：引导安装把 Node 装到自定义目录后，「安装器什么时候、有没有把目录写进
/// 注册表 PATH」不在我们的控制范围内（实测遇到过：文件全都装好在 `D:\Programs\Node.js`、
/// 卸载登记也在，但本进程与终端都找不到 `node`）。而目录是**我们自己指定的**，
/// 所以直接并进本进程 PATH —— 随后同一进程里的 npm/dsh 检测、npm 全局安装以及派生的
/// 子进程都不再受「PATH 何时生效」影响。（`child_path_for` 会把它带给子进程。）
pub fn prepend_process_path(dir: &Path) -> bool {
    let cur = current_path_dirs();
    let merged = match path_with_dir_first(&cur, dir) {
        Some(v) => v,
        None => return false, // 空路径或已在里面，不必改
    };
    match std::env::join_paths(&merged) {
        Ok(joined) => {
            std::env::set_var("PATH", joined);
            true
        }
        Err(_) => false,
    }
}

/// `prepend_process_path` 的纯函数内核：把 dir 放到最前；已在列表里（或 dir 为空）
/// 返回 None。单独抽出来是为了能单测 —— 它要改进程全局状态，不适合在测试里反复调用。
fn path_with_dir_first(cur: &[PathBuf], dir: &Path) -> Option<Vec<PathBuf>> {
    let k = dir_key(dir);
    if k.is_empty() {
        return None;
    }
    if cur.iter().any(|d| dir_key(d) == k) {
        return None;
    }
    let mut out: Vec<PathBuf> = Vec::with_capacity(cur.len() + 1);
    out.push(dir.to_path_buf());
    out.extend(cur.iter().cloned());
    Some(out)
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

/// 读某个注册表键下指定值名的数据（值不存在 / 读不到一律 None）。
fn registry_value_raw(key: &str, name: &str) -> Option<String> {
    let mut cmd = Command::new("reg");
    cmd.arg("query").arg(key).arg("/v").arg(name);
    apply_no_window(&mut cmd);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_reg_value(&decode_console_output(&out.stdout), name)
}

/// 读某个注册表键下的 `Path` 值（失败返回空）。
fn registry_path_raw(key: &str) -> Option<String> {
    registry_value_raw(key, "Path")
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
///
/// ⚠ 这里有两条**不能破的纪律**（都是 0xc00000fd 栈溢出的教训，见 npm_prefix_for_global_bins 的长注释）：
///   1. **不调 `npm_global_bin_dirs()`** —— 它现在会为了拿 npm 的 prefix 而启动 npm，而本函数
///      正是「启动任何子进程」都要走的路径，一调就成环（npm 启 npm 启 npm …）。
///      它提供的 `%APPDATA%\npm` 由下面的 `appdata_npm_dir()` 直接补上；
///      「自定义 prefix」不需要在这里出现 —— 子进程要用的 node / npm 都在别的目录。
///   2. **不用 `detect_all(false)`** —— 那会在**首次扫描**（`scan()` 持锁期间）自锁：
///      首次扫描里可能 spawn 子进程 → child_path_for → detect_all → 同一个锁 → 死锁。
///      用 `try_peek_cached()`：扫过一次就有结果可用，扫描进行中则安全跳过（少几条候选而已）。
pub fn child_path_for(exes: &[&str]) -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for e in exes {
        if let Some(d) = Path::new(e).parent() {
            if !d.as_os_str().is_empty() {
                dirs.push(d.to_path_buf());
            }
        }
    }
    if let Some(appdata) = appdata_npm_dir() {
        dirs.push(appdata);
    }
    if let Some(cached) = try_peek_cached() {
        for p in [&cached.node, &cached.npm, &cached.dsh, &cached.pnpm] {
            if let Some(p) = p {
                if let Some(d) = p.parent() {
                    dirs.push(d.to_path_buf());
                }
            }
        }
    }
    dirs.extend(current_path_dirs());
    dirs.extend(registry_path_dirs());
    let dirs = dedupe_dirs(dirs);
    std::env::join_paths(&dirs)
        .unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

/// 已有的检测结果（**不触发扫描、加锁失败也不等**）。
/// 与 `detect_all(false)` 的区别：后者在首次调用时会**持锁扫描**，而本函数只「看一眼」。
/// 用途是那些可能在扫描过程中被调到的函数（如 `child_path_for`）——它们拿不到旧结果没关系，
/// 扫描完成后自然就拿得到了。
fn try_peek_cached() -> Option<Arc<EnvPaths>> {
    cache_slot().try_lock().ok().map(|g| g.clone())
}

// ---------- 把「自定义 DSH 安装位置」写进**用户** PATH ----------
//
// 为什么必须做：npm 的 `--prefix <目录>` 只把文件写到那个目录，**不会**把它加进 PATH。
// 而用户装完最自然的期待是「终端里也能直接敲 dsh」，本程序的路径探测也依赖 PATH。
//
// 只写 HKCU\Environment（当前用户），不碰系统 PATH：不需要管理员，影响面最小，
// 且与「这是用户为自己装的工具」这件事相称。写完之后：
//   ① `refresh_process_path()` —— 本进程与它派生的子进程立刻能看到；
//   ② 广播 WM_SETTINGCHANGE —— Explorer 刷新自己的环境块，用户**新开**的终端才有它，
//      否则要等重新登录（这正是「加了 PATH 却不生效」的常见来源）。

/// 读取用户 PATH 的**原始值**（未展开 `%VAR%`；REG_SZ 与 REG_EXPAND_SZ 都能读到）。
pub fn user_path_raw() -> Option<String> {
    registry_path_raw(USER_PATH_KEY)
}

/// 用户 PATH 里是不是**已经有**这个目录（注册表真值，展开 `%VAR%`、忽略大小写与尾分隔符）。
///
/// 实现上直接复用 [`user_path_with_dir`] 的判定内核：它「已经在里面」时返回 `None` 且一个
/// 字节都不写，所以「返回 None」就是「已存在」。**刻意不另写一套字符串比较** —— 两份
/// 比较规则迟早会分叉，那时就会出现「界面说不在、点了按钮又说已存在」这种自相矛盾。
/// PATH 读不到（注册表异常）时返回 false：让用户能点按钮去补，而不是被一个读不到的值卡住。
pub fn user_path_contains(dir: &Path) -> bool {
    // 空目录不是「已存在」，而是「别问了」——与 user_path_with_dir 对空值的处理保持一致
    if dir_key(dir).is_empty() {
        return false;
    }
    match user_path_raw() {
        Some(cur) => user_path_with_dir(&cur, dir).is_none(),
        None => false,
    }
}

/// 把 `dir` 追加到用户 PATH 末尾。返回 true = 真的改了注册表；
/// false = 已经在里面（展开 `%VAR%` 后比较，忽略大小写与结尾分隔符），什么都没动。
///
/// 追加到**末尾**而不是最前：不覆盖用户已有的解析顺序。真出现同名 `dsh.cmd` 时，
/// 优先命中的仍是他原来那个（各自都还能用绝对路径启动）。
pub fn append_to_user_path(dir: &Path) -> Result<bool, String> {
    if dir_key(dir).is_empty() {
        return Err("empty dir".to_string());
    }
    let current = user_path_raw().unwrap_or_default();
    let Some(value) = user_path_with_dir(&current, dir) else {
        return Ok(false);
    };
    write_user_path(&value)?;
    // 顺序有讲究：先让本进程/子进程生效，再广播给 Explorer。
    refresh_process_path();
    invalidate_cache();
    broadcast_env_change();
    Ok(true)
}

/// `append_to_user_path` 的纯函数内核：返回追加后的用户 PATH 值；
/// `dir` 已经在里面（展开 `%VAR%` 后比较，忽略大小写与结尾分隔符）时返回 None。
/// 单独抽出来单测，是因为这里最容易出「多一个分号」「把已有项写坏」这类
/// 只有在真机上才看得见的错 —— 而用户 PATH 写坏一次的代价很大。
fn user_path_with_dir(cur: &str, dir: &Path) -> Option<String> {
    let key = dir_key(dir);
    if key.is_empty() {
        return None;
    }
    if split_path_list(&expand_percent(cur))
        .iter()
        .any(|d| dir_key(d) == key)
    {
        return None;
    }
    let trimmed = cur.trim().trim_end_matches(';');
    Some(if trimmed.is_empty() {
        dir.to_string_lossy().to_string()
    } else {
        format!("{};{}", trimmed, dir.to_string_lossy())
    })
}

/// 写 `HKCU\Environment` 的 `Path`。写成 **REG_EXPAND_SZ**：用户 PATH 里常常有
/// `%USERPROFILE%` 这类引用，写成 REG_SZ 会让它们变成字面量（等于把那些目录弄坏）。
/// `reg.exe` 直接读参数、不经 cmd.exe，所以值里的 `%` 不会在写入时被展开
/// （真机验证：含空格、`&`、`;`、`%USERPROFILE%` 的值能原样写回且类型仍是 REG_EXPAND_SZ）。
///
/// 顺带一个容易被忽略的前提：**拼进来的目录不以 `\` 结尾**（path_shape 会去掉），
/// 否则值会以 `…\"` 结束，CreateProcess 的转义规则会把结尾的反斜杠翻倍，
/// reg.exe 收到的是一个带尾反斜杠的怪值。
fn write_user_path(value: &str) -> Result<(), String> {
    let mut cmd = Command::new("reg");
    cmd.arg("add")
        .arg(USER_PATH_KEY)
        .arg("/v")
        .arg("Path")
        .arg("/t")
        .arg("REG_EXPAND_SZ")
        .arg("/d")
        .arg(value)
        .arg("/f");
    apply_no_window(&mut cmd);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = cmd
        .output()
        .map_err(|e| format!("reg add: {}", e))?;
    if !out.status.success() {
        let msg = decode_console_output(&out.stderr);
        let msg = msg.trim();
        return Err(if msg.is_empty() {
            format!("reg add 退出码 {:?}", out.status.code())
        } else {
            msg.to_string()
        });
    }
    Ok(())
}

/// 广播 `WM_SETTINGCHANGE`（Windows 专有）。失败无所谓：用户重新登录后一样生效，
/// 所以这里不返回错误、也不阻断安装结果。
pub(crate) fn broadcast_env_change() {
    #[cfg(windows)]
    crate::process::win::broadcast_environment_change();
}

// ---------- 把「DSH 家目录」写进**用户**环境变量 DSH_HOME ----------
//
// 为什么提供它：本程序启动 DSH 时是**显式注入** DSH_HOME 的（process.rs 的日常启动、
// safe.rs 的安全模式启动都用 `cmd.env("DSH_HOME", …)` 覆盖继承值），所以这里写不写都
// **不改变本程序自己的行为** —— 受影响的只有「用户另外打开的终端」。不写的话
// `%DSH_HOME%` 在那个终端里根本未定义（cmd 会把 `%DSH_HOME%` 原样回显出来），
// 于是那里敲的 `dsh` 沿「显式配置 > $DSH_HOME > ~/.dsh」回退到**另一个家目录**：
// 设置、凭据、会话与本程序用的那份各存一份、互不相通，而且没有任何提示。
//
// 只写 `HKCU\Environment`（当前用户），不碰系统环境：不需要管理员、影响面最小，
// 与上面写用户 PATH 同一套手法 —— 写完广播 WM_SETTINGCHANGE，用户**新开**的终端
// 才拿得到；已经开着的终端要新开（或重新登录）才生效。

/// 用户环境变量里的 DSH_HOME 值名（读 / 写 / 删共用，免得三处拼写漂移）。
pub const HOME_ENV_NAME: &str = "DSH_HOME";

/// 读 `HKCU\Environment` 下某个值的**持久值**（未展开 `%VAR%`；REG_SZ 与
/// REG_EXPAND_SZ 都能读到）。没有这个值或读不到一律 `None` —— 调用方据此区分
/// 「没设置」与「设置成了别的值」，这两种在提示里必须分开说。
pub fn user_env_raw(name: &str) -> Option<String> {
    registry_value_raw(USER_PATH_KEY, name)
}

/// 写 `HKCU\Environment` 的一个值。类型固定 **REG_SZ**：写进去的是家目录的**字面**
/// 路径，新终端里的 `dsh` 就该拿到这个字面量（REG_SZ 不会把 `%…%` 展开成别的东西）。
/// 这与上面用户 PATH 必须用 REG_EXPAND_SZ 正好相反 —— 那里存的本来就是引用形式，
/// 展开才对；这里存的是一个已经由 validate_home_dir 规范化过的绝对路径。
pub fn set_user_env_value(name: &str, value: &str) -> Result<(), String> {
    let mut cmd = Command::new("reg");
    cmd.arg("add")
        .arg(USER_PATH_KEY)
        .arg("/v")
        .arg(name)
        .arg("/t")
        .arg("REG_SZ")
        .arg("/d")
        .arg(value)
        .arg("/f");
    apply_no_window(&mut cmd);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = cmd
        .output()
        .map_err(|e| format!("reg add: {}", e))?;
    if !out.status.success() {
        let msg = decode_console_output(&out.stderr);
        let msg = msg.trim();
        return Err(if msg.is_empty() {
            format!("reg add 退出码 {:?}", out.status.code())
        } else {
            msg.to_string()
        });
    }
    Ok(())
}

/// 删除 `HKCU\Environment` 里的一个值。值不存在时 `reg delete` 同样会失败 —— 调用方
/// 先用 [`plan_home_env`] 判断过「确实有值且归我们管」，所以这里的失败都是真失败
/// （策略锁注册表、权限不足），必须如实报出来而不是假装值已经被删掉了。
pub fn delete_user_env_value(name: &str) -> Result<(), String> {
    let mut cmd = Command::new("reg");
    cmd.arg("delete")
        .arg(USER_PATH_KEY)
        .arg("/v")
        .arg(name)
        .arg("/f");
    apply_no_window(&mut cmd);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = cmd
        .output()
        .map_err(|e| format!("reg delete: {}", e))?;
    if !out.status.success() {
        let msg = decode_console_output(&out.stderr);
        let msg = msg.trim();
        return Err(if msg.is_empty() {
            format!("reg delete 退出码 {:?}", out.status.code())
        } else {
            msg.to_string()
        });
    }
    Ok(())
}

/// [`apply_dsh_home_env`]（process.rs）的动作计划 —— 纯函数，单独单测。
/// 判错的两种代价分别是「终端继续用错家目录」与「删掉用户自己设的值」，都得钉住。
pub enum HomeEnvAction {
    /// 已经是想要的状态，一个字节都不动
    Unchanged,
    /// 把这个值写进注册表
    Write(String),
    /// 删掉注册表里这个值（携带将被删除的当前值，用于提示「原值是什么」）
    Remove(String),
    /// 家目录改回默认、准备删值时发现已有值、但它不指向本程序配置过的任何家目录：原样保留并说明
    KeepForeign(String),
}

/// `apply_dsh_home_env` 的决策内核（**自动规则，没有开关**）：
/// - `desired` = 该写入的家目录（`None` = 家目录就是默认值，此时没有可同步的值、
///   走"删值"这条路 —— 由 config::is_default_home_dir 判定后传进来）；
/// - `current` = 注册表现值（`None` = 这个值不存在）；
/// - `owned`   = 「可以由本程序删掉」的家目录集合（当前配置的家目录 + 本次保存
///   **之前**的家目录 —— 后者用来认领「同一时刻既改家目录又回到默认」时留下的旧值）。
///
/// 归属规则是这里唯一要紧的判断：**只删指向本程序配置过的家目录的值**。
/// 用户可能自己 `setx DSH_HOME` 指向别的地方，家目录回到默认时不该顺手把它删掉 ——
/// 那个值不是我们写进去的。
pub fn plan_home_env(
    desired: Option<&str>,
    current: Option<&str>,
    owned: &[String],
) -> HomeEnvAction {
    let cur = current.map(str::trim).filter(|s| !s.is_empty());
    match desired {
        Some(d) => {
            let want = home_key(d);
            match cur {
                // 已经是它：大小写 / 结尾分隔符的差异不算「变了」，别白广播一次
                Some(c) if home_key(c) == want => HomeEnvAction::Unchanged,
                _ => HomeEnvAction::Write(d.to_string()),
            }
        }
        None => match cur {
            None => HomeEnvAction::Unchanged,
            Some(c) => {
                if owned.iter().any(|o| home_key(o) == home_key(c)) {
                    HomeEnvAction::Remove(c.to_string())
                } else {
                    HomeEnvAction::KeepForeign(c.to_string())
                }
            }
        },
    }
}

/// 家目录比较键：去首尾空白 + 去尾部分隔符 + 小写（Windows 路径大小写不敏感）。
/// 复用 PATH 那边的 `dir_key`，免得两处对「是不是同一个目录」的标准不一致。
/// `pub(crate)` 给 config::is_default_home_dir 用：它必须拿同一把尺子去比
/// 「当前家目录是不是默认值」，否则会在大小写 / 尾斜杠上误判、白写一次注册表。
pub(crate) fn home_key(s: &str) -> String {
    dir_key(Path::new(s))
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
    /// pnpm 是否可用（首装向导据此在「Node 已就绪」时提示/代装 pnpm ——
    /// 它是之后安装 DSH 插件要用的包管理器）
    pub pnpm_found: bool,
    pub pnpm_path: String,
    /// pnpm 版本（`pnpm --version` 读到才有值）。
    /// 本程序对 pnpm 有**硬下限**：必须 ≥ PNPM_MIN_VERSION（用户拍板），低于它就要更新。
    pub pnpm_version: Option<String>,
    /// pnpm 版本相对下限的状态："supported"（≥ 下限）/ "old"（低于下限，必须更新 ——
    /// 前端据此把「完成」按钮拦住）/ "unknown"（没装，或版本串读不到/解析不了；
    /// 读不出**不判**「过低」，只做警示）。
    pub pnpm_min_state: String,
    /// pnpm 最低版本（展示用，如 `v10`），文案里的阈值不写死在前端
    pub pnpm_min_version: String,
    pub dsh_found: bool,
    pub dsh_path: String,
    /// 引导安装将下载的官方 Node.js LTS MSI 地址（展示用）
    pub node_msi_url: String,
    /// 官方手动下载页
    pub node_download_page: String,
    /// 引导安装 Node.js 的**默认安装目录**（`%ProgramFiles%\nodejs`）。
    /// 向导用它预填「安装位置」输入框 —— 预填而不是留空，是因为大多数用户要的就是
    /// 默认位置；由后端按本机 ProgramFiles 算，才不会在 Windows 装在 D 盘时写错。
    pub node_default_dir: String,
    /// 引导安装 DSH 的**默认位置** = npm 的全局目录（问 `npm config get prefix`，
    /// 问不到才回落 `%APPDATA%\npm`）。同样用于预填向导里的输入框：预填一个**正确的**
    /// 默认值，用户不动它时，行为与「不传 --prefix」完全一致。
    pub npm_default_prefix: String,
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
    let pnpm_version = paths.pnpm.as_deref().and_then(|p| quick_version(p, 10));
    let pnpm_min_state = match pnpm_version.as_deref().and_then(pnpm_version_at_least_min) {
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
        pnpm_found: paths.pnpm.is_some(),
        pnpm_path: opt_to_string(&paths.pnpm),
        pnpm_version,
        pnpm_min_state: pnpm_min_state.to_string(),
        pnpm_min_version: PNPM_MIN_VERSION_LABEL.to_string(),
        dsh_found: paths.dsh.is_some(),
        dsh_path: opt_to_string(&paths.dsh),
        node_msi_url: node_msi_url(),
        node_download_page: NODE_DOWNLOAD_PAGE.to_string(),
        node_default_dir: default_node_install_dir(),
        npm_default_prefix: default_npm_prefix_for(paths.npm.as_deref()),
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
    fn parses_python_version_and_its_floor() {
        // `python --version` 的三种常见形态
        assert_eq!(parse_python_version("Python 3.14.7").as_deref(), Some("3.14.7"));
        assert_eq!(parse_python_version("Python 3.14.7+ heads/main").as_deref(), Some("3.14.7"));
        assert_eq!(parse_python_version("3.14.7\n").as_deref(), Some("3.14.7"));
        // 读不出版本 = 没有可用的 Python（Store 执行别名、Python 2 的报错都在这里被挡掉）
        assert_eq!(parse_python_version(""), None);
        assert_eq!(parse_python_version("python is not recognized"), None);
        assert_eq!(parse_python_version("Python 2.7.18"), Some("2.7.18".to_string()));
        // 但「读得出来」不等于「够用」：版本下限是独立判断
        assert!(python_version_usable("3.8.0"));
        assert!(python_version_usable("3.14.7"));
        assert!(python_version_usable("3.14"));
        assert!(!python_version_usable("2.7.18"));
        assert!(!python_version_usable("3.7.9"));
        assert!(!python_version_usable("junk"));
        // 版本号会拼进 URL，白名单必须挡住路径片段与空段
        assert!(is_safe_python_version("3.14.7"));
        assert!(!is_safe_python_version("3.14.7/../../x"));
        assert!(!is_safe_python_version("3..7"));
        assert!(!is_safe_python_version("3.14.7 "));
        assert!(!is_safe_python_version(""));
    }

    /// pnpm 的硬下限（用户拍板：**pnpm 必须 ≥ 10，低了就必须更新**），与 Node 下限
    /// 同一套解析/比较，只是阈值不同。
    /// 关键在最后几条：「读不出版本」必须返回 None 而不是 false —— 前端拿 false 会把
    /// 「完成」按钮锁死，那台机器上 pnpm 可能明明是够用的，却修不掉、也走不出向导。
    #[test]
    fn pnpm_floor_is_v10_and_unknown_is_not_too_old() {
        assert_eq!(parse_version_numbers(PNPM_MIN_VERSION).unwrap(), vec![10, 0, 0]);
        assert_eq!(PNPM_MIN_VERSION_LABEL, "v10");
        for old in ["9.12.3", "9.0.0", "8.15.4", "1.0.0"] {
            assert_eq!(pnpm_version_at_least_min(old), Some(false), "{old} 应低于下限");
        }
        for ok in ["10.0.0", "10.5.1", "11.0.0", "v10.1.2", "10"] {
            assert_eq!(pnpm_version_at_least_min(ok), Some(true), "{ok} 应满足下限");
        }
        for junk in ["", "junk", "pnpm: command not found"] {
            assert_eq!(pnpm_version_at_least_min(junk), None, "{junk} 应判未知而非过低");
        }
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

    /// 向导里预填的默认安装目录必须是「用户不改也能装成功」的值：
    /// 形状得像 `…\nodejs`，而且**必须能通过安装目录校验** —— 否则用户会看到一个
    /// 自己从没输入过的值被判非法（预填值被自己的校验器拒掉，是最难解释的那种报错）。
    #[cfg(windows)]
    #[test]
    fn default_node_install_dir_passes_its_own_validation() {
        let d = default_node_install_dir();
        assert!(d.ends_with("\\nodejs"), "应当以 \\nodejs 结尾: {d}");
        let norm = crate::config::validate_node_install_dir(&d)
            .unwrap_or_else(|e| panic!("预填的默认目录必须合法，却被拒绝: {e}"))
            .expect("预填值非空，应当返回 Some");
        assert_eq!(norm, d.trim_end_matches('\\'));
    }

    /// 预置目录的纯函数内核：插到最前、按 Windows 语义去重（尾部分隔符 + 大小写）。
    /// 它本身不进 PATH（改进程全局状态的那层不在这里测），但判断逻辑全在这儿。
    #[test]
    fn path_with_dir_first_prepends_and_dedupes() {
        let cur = vec![
            PathBuf::from(r"C:\Windows"),
            PathBuf::from(r"D:\Programs\Node.js\"),
        ];
        // 已在列表里（尾部 `\` 与大小写差异不算新目录）→ 返回 None，不重复插
        assert!(path_with_dir_first(&cur, Path::new(r"d:\programs\node.js")).is_none());
        let out = path_with_dir_first(&cur, Path::new(r"D:\nodejs")).expect("应当插入");
        assert_eq!(out[0], PathBuf::from(r"D:\nodejs"), "必须插在最前面");
        assert_eq!(out.len(), cur.len() + 1);
        // 空/纯空白路径不动 PATH
        assert!(path_with_dir_first(&cur, Path::new("   ")).is_none());
        assert!(path_with_dir_first(&[], Path::new("")).is_none());
    }

    /// 注册表 PATH 目录扫描：只在文件确实存在时命中，且按给定顺序取第一个。
    #[test]
    fn find_in_dirs_only_returns_existing_files() {
        let dir = std::env::temp_dir().join(format!("dsh-find-in-dirs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let hit = dir.join("node.exe");
        std::fs::write(&hit, b"x").unwrap();

        // 第一个目录里没有，第二个里有 → 命中第二个
        let dirs = vec![dir.join("nope"), dir.clone()];
        assert_eq!(find_in_dirs(&dirs, "node.exe"), Some(hit));
        // 找不到就是找不到（不能凭目录存在就返回路径）
        assert_eq!(find_in_dirs(&dirs, "npm.cmd"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- DSH 自定义安装位置（npm 全局目录） ----------

    /// `npm config get prefix` 的输出取值：认得出才用，认不出返回 None（回落到
    /// `%APPDATA%\npm`），绝不把 npm 的告警行当成路径预填给用户。
    /// （`npm config get userconfig` / `cache` 走的是同一个取值函数。）
    #[cfg(windows)]
    #[test]
    fn picks_path_line_from_noisy_npm_output() {
        // 正常输出（CRLF）
        assert_eq!(
            pick_path_line("C:\\Users\\me\\AppData\\Roaming\\npm\r\n").as_deref(),
            Some("C:\\Users\\me\\AppData\\Roaming\\npm")
        );
        // 告警混在输出里（stdout/stderr 被合并）：值总是在最后一行
        assert_eq!(
            pick_path_line("npm warn config production Use `--omit=dev` instead.\nD:\\npm-global\n")
                .as_deref(),
            Some("D:\\npm-global")
        );
        // 认不出的输出（undefined / 空）→ None，让调用方回落，不猜
        assert_eq!(pick_path_line("undefined\n"), None);
        assert_eq!(pick_path_line(""), None);
        // npm 失败时那行日志里也有路径，但它以 "npm " 开头，不能被当成值
        assert_eq!(
            pick_path_line("npm error Log files were not written to C:\\tmp\n"),
            None
        );
    }

    /// 问不到 npm（缺失/超时/输出认不出）时必须回落到 Windows 的官方默认全局目录，
    /// 而不是返回空串 —— 空串会让向导里的「安装位置」变成空白框，用户照它装就会
    /// 落到一个我们自己都不知道的地方。
    #[cfg(windows)]
    #[test]
    fn default_npm_prefix_falls_back_to_appdata_npm() {
        if appdata_npm_dir().is_none() {
            return; // 连 APPDATA / USERPROFILE 都没有的极端环境：这条无从断言，跳过
        }
        let p = default_npm_prefix_for(None);
        assert!(
            p.to_ascii_lowercase().ends_with(r"\npm"),
            "应当回落到 %APPDATA%\\npm，实际得到 {p:?}"
        );
    }

    /// 用户 PATH 的追加规则（纯函数内核）：空 PATH、已有项去重、不产生空分隔符。
    /// 这条路径直接改用户的注册表 PATH，写坏一次的代价很大，所以规则单独钉住。
    #[test]
    fn user_path_with_dir_appends_once_without_breaking_entries() {
        let dir = Path::new(r"D:\dsh-global");
        // 空 PATH：直接就是它
        assert_eq!(
            user_path_with_dir("", dir).as_deref(),
            Some(r"D:\dsh-global")
        );
        // 已有内容：追加到末尾，原有项一字不改
        assert_eq!(
            user_path_with_dir(r"C:\Windows;C:\Tools", dir).as_deref(),
            Some(r"C:\Windows;C:\Tools;D:\dsh-global")
        );
        // 结尾已有分号：不能多写一个（`…;;D:\…` 里的空项等于把当前目录塞进 PATH）
        assert_eq!(
            user_path_with_dir(r"C:\Windows;", dir).as_deref(),
            Some(r"C:\Windows;D:\dsh-global")
        );
        // 已在里面：大小写与结尾分隔符差异都算「已有」→ None（不重复追加）
        assert!(user_path_with_dir(r"C:\Windows;d:\DSH-Global\", dir).is_none());
        // 展开 %VAR% 之后再比较：注册表里存的是引用形式，不能用字面量骗过判断
        if let Ok(prof) = std::env::var("USERPROFILE") {
            let with_ref = format!(r"{}\AppData\Roaming\npm;%USERPROFILE%\npm", prof);
            assert!(user_path_with_dir(&with_ref, Path::new(&format!(r"{}\npm", prof))).is_none());
        }
    }

    /// `config get registry` 输出的取值：只认最后一行 http(s) 地址，
    /// 告警 / 错误行一律不作数（认不出返回 None，绝不拿告警当源）。
    #[test]
    fn pick_registry_line_takes_last_http_line_only() {
        assert_eq!(
            pick_registry_line("https://registry.npmjs.org/\n"),
            Some("https://registry.npmjs.org/".to_string())
        );
        // 值在最后、告警在前：取值
        assert_eq!(
            pick_registry_line("npm warn old\nhttps://a/\n"),
            Some("https://a/".to_string())
        );
        // 错误输出里没有地址 → None（调用方据此报"读不到"，而不是猜一个源）
        assert_eq!(pick_registry_line("Error: × boom\n"), None);
        assert_eq!(pick_registry_line(""), None);
        // 非 http 形态（比如打印了相对路径/键名）不作数
        assert_eq!(pick_registry_line("registry=https://a/\n"), None);
    }

    /// 用户环境变量 DSH_HOME 的写 / 删决策（plan_home_env 纯函数内核；**自动规则**：
    /// 非默认家目录 → 写，默认家目录 → 删，没有开关）。
    /// 判错的两种代价分别是「终端继续用错家目录」与「删掉用户自己 setx 的值」，
    /// 所以每条分支都单独钉住 —— 这段跑在 CI 上（开发机没有 Rust 工具链）。
    #[test]
    fn plan_home_env_writes_removes_and_protects_foreign_values() {
        // ---------- 非默认家目录（desired = Some）：要写入 ----------
        // 值不同 → 写
        assert!(matches!(
            plan_home_env(Some(r"D:\DSH\AppData"), Some(r"C:\Users\me\.dsh"), &[]),
            HomeEnvAction::Write(v) if v == r"D:\DSH\AppData"
        ));
        // 原来没有这个值 → 写
        assert!(matches!(
            plan_home_env(Some(r"D:\DSH\AppData"), None, &[]),
            HomeEnvAction::Write(_)
        ));
        // 已经是它（大小写 / 结尾反斜杠差异）→ 不动，别白广播一次 WM_SETTINGCHANGE
        assert!(matches!(
            plan_home_env(Some(r"D:\DSH\AppData"), Some(r"d:\dsh\appdata\"), &[]),
            HomeEnvAction::Unchanged
        ));
        // 空白值等同于「没设置」，不能被当成「已经是它」而漏写
        assert!(matches!(
            plan_home_env(Some(r"D:\DSH\AppData"), Some("   "), &[]),
            HomeEnvAction::Write(_)
        ));

        // ---------- 默认家目录（desired = None）：没有可同步的值，走删值 ----------
        let configured = vec![r"D:\DSH\AppData".to_string()];
        // 没有这个值 → 什么都不做
        assert!(matches!(
            plan_home_env(None, None, &configured),
            HomeEnvAction::Unchanged
        ));
        // 指向当前配置的家目录 → 删，并带回将被删掉的原值
        assert!(matches!(
            plan_home_env(None, Some(r"D:\DSH\AppData\"), &configured),
            HomeEnvAction::Remove(v) if v == r"D:\DSH\AppData\"
        ));
        // 指向**本次保存之前**的家目录 → 也要删：
        // 「同一时刻既改家目录、又把它改回默认值」时，注册表里躺着的是旧值，
        // 只比对新值会漏清
        let before_and_now = vec![
            r"D:\DSH\NewHome".to_string(),
            r"D:\DSH\AppData".to_string(),
        ];
        assert!(matches!(
            plan_home_env(None, Some(r"D:\DSH\AppData"), &before_and_now),
            HomeEnvAction::Remove(_)
        ));
        // 指向别处（用户自己 setx 的）→ 原样保留：那不是我们写进去的东西
        assert!(matches!(
            plan_home_env(None, Some(r"C:\elsewhere\.dsh"), &configured),
            HomeEnvAction::KeepForeign(v) if v == r"C:\elsewhere\.dsh"
        ));
        // ownedList 为空（无从认领）时更要保留，绝不能无条件删
        assert!(matches!(
            plan_home_env(None, Some(r"D:\DSH\AppData"), &[]),
            HomeEnvAction::KeepForeign(_)
        ));
    }
}
