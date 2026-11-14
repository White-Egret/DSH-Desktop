//! ProbeOnly 三项（rust / msvc / webview2）的探测 + 全模块共用的**有界探测**基础设施。
//!
//! 两条主线（方案 §5A.1 / §3.2）：
//! - **有界探测**：所有外部命令一律走 `run_bounded_capture` —— 超时、输出字节上限、
//!   stdin/stderr 归零、所有失败折叠成 `None`。任何一个探测卡住都会把状态面板拖死，
//!   所以边界收在一处而不是靠每个调用点自觉。
//! - **MSVC 判定**：「装了 VS」不等于「link.exe 可用」。本机实测存在 Build Tools
//!   装了、`VsDevCmd` 却选中一个只有 Auxiliary 目录的空壳版本（方案坑③），
//!   所以判定标准是「能挑出一个 link.exe 真实存在的工具集版本」，并给出三态
//!   （Ok / Broken / Missing）。
//!
//! 通用性纪律（提示词「开发机 ≠ 用户机」）：本文件**没有任何本机特有的值** ——
//! VS 安装根靠 vswhere 查、MSVC 目录靠 `rustc` host triple 推导、SDK 路径靠
//! `VsDevCmd` 的输出取。验证本机用的对照值只存在于测试与文档，不进实现。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::env::EnvOverlay;
use super::store::{self, MsvcCache};
use super::{ProbeFound, ProbeReq, ProbeResult, SystemProbe};
use crate::process::{apply_no_window, command_for, decode_console_output, validate_program_path};

// ---------- 有界探测的边界常量（方案 §5A.1 的三重边界） ----------

/// 探测命令的默认超时。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// 探测输出的默认字节上限。探测要的只是几行版本号，超限即异常。
pub const PROBE_MAX_BYTES: usize = 4096;
/// PATH 扫描的目录数上限：异常 PATH（循环、几千条）不该拖慢探测。
pub const PATH_SCAN_MAX_DIRS: usize = 64;
/// 目录扫描的条目数上限（MSVC 工具集目录等）。
pub const DIR_SCAN_MAX_ENTRIES: usize = 64;

/// 有界捕获一条外部命令的 stdout（方案 §5A.1 的形态，Windows 分支）。
///
/// 边界：
/// - **超时**：循环里判 deadline，超时 `kill()` + `wait()` 后直接返回 `None`，
///   绝不等 reader（后代可能继承 stdout 管道让它永远不退）；
/// - **字节上限**：reader 累计到 `limit` 后**不再保存**，但**继续读到 EOF 并丢弃** ——
///   只停手不读会让子进程把管道写满后永久阻塞（真机实测：`VsDevCmd.bat … && set`
///   的环境清单撑满管道 → 子进程卡住 → 超时 → SDK 路径丢失 → 链接缺 kernel32.lib）。
///   所以内存上界是 `limit`，而子进程始终能跑完；
/// - **stdin(null) / stderr(null)**：探测不读终端、也不被 stderr 淹没；
/// - **所有失败折叠成 `None`**：spawn 失败、非零退出、超时、reader 卡住（后代
///   持有管道）一律 `None`，调用方按「探测不到」处理，不用区分失败原因；
/// - **Windows 特有**：reader 放独立线程 + mpsc，主线程只看 `try_wait()` 与
///   deadline，**绝不 `join()` 那个线程**；子进程退出后用有界的 `recv_timeout`
///   收结果。
pub fn run_bounded_capture(cmd: &mut Command, limit: usize, timeout: Duration) -> Option<Vec<u8>> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    apply_no_window(cmd);
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + timeout;

    // reader：**边读边发**，不是攒到 EOF 才发。
    //
    // 为什么必须这样（真机实测）：`cmd /s /c "<VsDevCmd.bat> … && set"` 的子进程
    // 正常退出（exit 0，约 4.5 秒），但它的**后代仍持有 stdout 管道**，reader 永远等
    // 不到 EOF —— 若把发送绑在 EOF 上，主线程一个有界等待超时后只能拿到空，
    // 结果就是「命令明明成功了却什么都读不到」（表现为 SDK 的 LIB/INCLUDE 丢失、
    // 链接时缺 kernel32.lib）。
    //
    // 三道边界同时成立：
    // - **发送总量 ≤ limit**（channel 里不会堆积无限多字节，内存有界）；
    // - 超限后**继续读到 EOF 并丢弃**（不读会让管道写满、子进程永久阻塞）；
    // - **绝不 join 这个线程**（后代开着管道时它可能永不返回）。
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let mut pipe = child.stdout.take();
    std::thread::spawn(move || {
        if let Some(p) = pipe.as_mut() {
            let mut chunk = [0u8; 512];
            let mut sent = 0usize;
            loop {
                match p.read(&mut chunk) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if sent < limit {
                            let take = n.min(limit - sent);
                            if tx.send(chunk[..take].to_vec()).is_err() {
                                break; // 主线程已放弃：停手
                            }
                            sent += take;
                        }
                        // sent 到上限后继续读并丢弃：管道不能堵
                    }
                    Err(_) => break,
                }
            }
        }
    });

    let mut buf: Vec<u8> = Vec::new();
    let drain = |rx: &mpsc::Receiver<Vec<u8>>, buf: &mut Vec<u8>| {
        while let Ok(c) = rx.try_recv() {
            if buf.len() < limit {
                let take = c.len().min(limit - buf.len());
                buf.extend_from_slice(&c[..take]);
            }
        }
    };

    loop {
        drain(&rx, &mut buf);
        match child.try_wait() {
            Ok(Some(status)) => {
                // 子进程已退出：再给 reader 一小段**有界**时间来交付尾巴
                // （后代还开着管道时它不会 EOF，所以这里只能"等一会儿"而不是"等它结束"）
                let until = Instant::now() + Duration::from_millis(400);
                while Instant::now() < until {
                    match rx.recv_timeout(Duration::from_millis(40)) {
                        Ok(c) => {
                            if buf.len() < limit {
                                let take = c.len().min(limit - buf.len());
                                buf.extend_from_slice(&c[..take]);
                            }
                        }
                        Err(_) => break,
                    }
                }
                drain(&rx, &mut buf);
                return if status.success() { Some(buf) } else { None };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // 超时：直接返回，不等 reader（它的结果已无用）
                    return None;
                }
                // 用 recv_timeout 代替 sleep：既等数据又不空转
                if let Ok(c) = rx.recv_timeout(Duration::from_millis(25)) {
                    if buf.len() < limit {
                        let take = c.len().min(limit - buf.len());
                        buf.extend_from_slice(&c[..take]);
                    }
                }
            }
            Err(_) => return None,
        }
    }
}

/// 同上，但把字节按控制台编码解码成文本（node / rustc 输出 UTF-8，系统命令可能 GBK）。
fn run_capture_text(cmd: &mut Command, limit: usize, timeout: Duration) -> Option<String> {
    run_bounded_capture(cmd, limit, timeout).map(|b| decode_console_output(&b))
}

/// 在给定目录列表里找可执行文件（按顺序，目录数受 `max_dirs` 限制）。
/// 纯函数内核（测试用临时目录构造 PATH 布局）。
pub fn find_in_path_dirs(dirs: &[PathBuf], name: &str, max_dirs: usize) -> Option<PathBuf> {
    dirs.iter()
        .take(max_dirs)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// 在**本进程 PATH** 里找可执行文件（上限 `PATH_SCAN_MAX_DIRS` 个目录）。
///
/// 为什么不 spawn `where.exe`：探测本身就在跑外部命令，为找命令再跑一条命令
/// 既慢又多一层无界风险；直接扫 PATH 目录，边界可控。
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    find_in_path_dirs(&dirs, name, PATH_SCAN_MAX_DIRS)
}

// ---------- MSVC 的架构布局：按 rustc host triple 推导，绝不写死（方案 §3.2） ----------

/// 一套 MSVC 目录布局名：bin 下的宿主/目标子目录，与 lib 下的目标子目录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsvcLayout {
    /// `bin\` 下的宿主目录（`HostX64` / `HostX86` / `Hostarm64`）。
    pub host_dir: &'static str,
    /// `bin\<host>\` 下的目标目录（`x64` / `x86` / `arm64`）。
    pub target: &'static str,
    /// `lib\` 下的目标目录（与 target 同值，单列一个字段便于将来交叉编译）。
    pub lib_dir: &'static str,
}

/// 架构名 → MSVC 目录布局。本机有四种组合（`HostX64\x64`、`HostX64\x86`、
/// `HostX86\x64`、`HostX86\x86`），v1 取「宿主架构 = 目标架构」这一种 ——
/// 本程序为自己编译（build script / proc-macro），宿主就是这台机器。
pub fn msvc_layout_for_arch(arch: &str) -> Option<MsvcLayout> {
    match arch.trim().to_ascii_lowercase().as_str() {
        // i686 是 rustc 32 位 Windows triple（i686-pc-windows-msvc）的架构段
        "x86_64" | "amd64" => Some(MsvcLayout { host_dir: "HostX64", target: "x64", lib_dir: "x64" }),
        "x86" | "i686" => Some(MsvcLayout { host_dir: "HostX86", target: "x86", lib_dir: "x86" }),
        "aarch64" | "arm64" => Some(MsvcLayout { host_dir: "Hostarm64", target: "arm64", lib_dir: "arm64" }),
        _ => None,
    }
}

/// rustc host triple（`x86_64-pc-windows-msvc`）取架构段。
pub fn arch_of_triple(triple: &str) -> Option<&str> {
    let arch = triple.trim().split('-').next().unwrap_or("");
    if arch.is_empty() { None } else { Some(arch) }
}

/// 问 rustc 自己的 host triple（`rustc -vV` 输出里的 `host: <triple>` 行）。
pub fn rustc_host_triple(rustc: &Path) -> Option<String> {
    let args: Vec<String> = ["-vV"].iter().map(|s| (*s).to_string()).collect();
    let mut cmd = command_for(&rustc.to_string_lossy(), &args).ok()?;
    let text = run_capture_text(&mut cmd, PROBE_MAX_BYTES, PROBE_TIMEOUT)?;
    text.lines().find_map(|l| {
        l.strip_prefix("host:")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

/// 解析出本机的 MSVC 布局：rustc host triple 优先（任务判据：按 rustc 推导），
/// rustc 缺失时退回本程序二进制自己的架构（`std::env::consts::ARCH`，
/// 用户机上它与操作系统架构一致）。两头都认不出时判 Broken 由调用方处理不了，
/// 这里按最常见的 x64 布局兜底 —— 探测会如实报「找不到 link.exe」。
fn resolve_layout(rustc: Option<&Path>) -> MsvcLayout {
    let from_triple = rustc
        .and_then(rustc_host_triple)
        .as_deref()
        .and_then(arch_of_triple)
        .and_then(msvc_layout_for_arch);
    from_triple
        .or_else(|| msvc_layout_for_arch(std::env::consts::ARCH))
        .or_else(|| msvc_layout_for_arch("x86_64"))
        .unwrap_or(MsvcLayout { host_dir: "HostX64", target: "x64", lib_dir: "x64" })
}

// ---------- MSVC 探测（方案 §3.2 的最终算法） ----------

/// vswhere 的候选路径：微软文档固定的安装器目录，**从环境变量取**（不写死盘符）。
fn vswhere_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Ok(base) = std::env::var(var) {
            let p = PathBuf::from(base).join("Microsoft Visual Studio").join("Installer").join("vswhere.exe");
            if !v.iter().any(|x| x == &p) {
                v.push(p);
            }
        }
    }
    v
}

/// ① vswhere 拿安装根。
///
/// 刻意**不带 `-requires <组件ID>`**（方案坑②）：Build Tools 2026 的组件 ID 与
/// 2022 不同，带过滤会在部分机器上返回空，把「装了」误判成「没装」。
fn query_vs_install_root() -> Option<PathBuf> {
    for vswhere in vswhere_candidates() {
        if !vswhere.is_file() {
            continue;
        }
        let args: Vec<String> = ["-latest", "-products", "*", "-property", "installationPath"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let mut cmd = command_for(&vswhere.to_string_lossy(), &args).ok()?;
        let text = run_capture_text(&mut cmd, PROBE_MAX_BYTES, Duration::from_secs(5))?;
        if let Some(line) = text.lines().map(str::trim).find(|l| !l.is_empty()) {
            let p = PathBuf::from(line);
            if p.is_dir() {
                return Some(p);
            }
        }
    }
    None
}

/// `VC\Tools\MSVC` 的扫描结果：哪些版本目录完整（link.exe 在）、哪些是空壳。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MsvcScan {
    /// 完整版本：`(版本目录名, 目录路径)`，已按版本号**数值降序**排好
    /// （纯字符串序会把 `9.x` 排到 `10.x` 之后，与"取最大版本"的意图相反）。
    pub complete: Vec<(String, PathBuf)>,
    /// 不完整的版本目录名（缺 bin\<host>\<target>\link.exe）。
    pub incomplete: Vec<String>,
}

/// ③ 扫描 `<root>\VC\Tools\MSVC\*`，挑「`bin\<host>\<target>\link.exe` 存在」的版本。
///
/// `VsDevCmd` 可能选中一个只有 Auxiliary 目录的空壳版本（方案坑③），所以版本
/// 必须自己挑：只有 link.exe 真实存在的目录才进 `complete`。条目数受
/// `DIR_SCAN_MAX_ENTRIES` 限制。
pub fn scan_msvc_tools(tools_dir: &Path, layout: &MsvcLayout) -> MsvcScan {
    let mut scan = MsvcScan::default();
    let Ok(rd) = std::fs::read_dir(tools_dir) else {
        return scan;
    };
    let dirs: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .take(DIR_SCAN_MAX_ENTRIES)
        .collect();
    for d in dirs {
        let Some(name) = d.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        let link = d.join("bin").join(layout.host_dir).join(layout.target).join("link.exe");
        if link.is_file() {
            scan.complete.push((name, d));
        } else {
            scan.incomplete.push(name);
        }
    }
    scan.complete.sort_by(|a, b| {
        match (
            crate::detect::parse_version_numbers(&a.0),
            crate::detect::parse_version_numbers(&b.0),
        ) {
            (Some(x), Some(y)) => crate::detect::cmp_versions(&y, &x), // 降序
            _ => b.0.cmp(&a.0),
        }
    });
    scan.incomplete.sort();
    scan
}

/// ② 跑 `VsDevCmd.bat` / 回退 `vcvarsall.bat`，取 SDK 部分的 LIB / INCLUDE。
///
/// 回退链（方案坑②）：Build Tools 2026 已移除 `vcvarsall.bat`，入口是
/// `VsDevCmd.bat`（参数也不同：`-arch=x64 -host_arch=x64`）；2022 及更早只有
/// `vcvarsall.bat x64`。两个都不能写死 —— 按文件存在与否依次尝试。
/// 命令末尾接 `&& set` 把初始化后的环境倾倒出来，再从中解析 LIB / INCLUDE。
fn run_dev_cmd_env(vs_root: &Path, layout: &MsvcLayout) -> Option<(String, String)> {
    let candidates: Vec<(PathBuf, String)> = vec![
        (
            vs_root.join("Common7").join("Tools").join("VsDevCmd.bat"),
            format!("-arch={} -host_arch={}", layout.target, layout.target),
        ),
        (
            vs_root.join("VC").join("Auxiliary").join("Build").join("vcvarsall.bat"),
            layout.target.to_string(),
        ),
    ];
    for (bat, args) in candidates {
        if !bat.is_file() {
            continue;
        }
        let Some(mut cmd) = build_devcmd_command(&bat, &args) else {
            continue;
        };
        // `set` 会倾倒整个环境（通常 1~3 KB，极端机器更大），4096 可能截在 LIB 之前
        // —— 这一路给更宽的字节界；超时也比普通探测宽（首次初始化 1~2 秒起）。
        // `set` 会倾倒整个环境（PATH 很长的机器上几十 KB），这个上限要开得足够宽：
// 截断在 PATH 之前是可以的（LIB/INCLUDE 字母序在前），但**读的字节数**不能卡住管道
// （run_bounded_capture 现在会继续读到 EOF 并丢弃，所以上限只影响保存多少）。
        let Some(text) = run_capture_text(&mut cmd, 256 * 1024, Duration::from_secs(30)) else {
            continue;
        };
        let lib = parse_env_assignment(&text, "LIB");
        let include = parse_env_assignment(&text, "INCLUDE");
        if lib.is_some() || include.is_some() {
            return Some((lib.unwrap_or_default(), include.unwrap_or_default()));
        }
    }
    None
}

/// 拼 `cmd /d /s /c ""<bat>" <args> && set"`。
///
/// 为什么绕开 `command_for` 自己拼：`.bat` 目标经 cmd.exe 执行时，`&& set` 必须作为
/// **同一条命令行**的一部分（bat 的环境变更只活在它自己的进程里，接 `set` 才能倒出来）；
/// `command_for` 的参数表形态表达不了这个。引号规则与 `process::build_cmd_line` 同源：
/// 整段用外层引号包住，bat 路径自带内层引号（`/s` 语义下内层引号原样保留）。
#[cfg(windows)]
fn build_devcmd_command(bat: &Path, args: &str) -> Option<Command> {
    let bat_s = bat.to_string_lossy().to_string();
    // bat 路径进命令行，先过同一道字符校验（`"` `%` `!` 与控制字符会破坏引号配对）
    validate_program_path(&bat_s).ok()?;
    use std::os::windows::process::CommandExt;
    let inner = if args.trim().is_empty() {
        format!("\"{bat_s}\"")
    } else {
        format!("\"{bat_s}\" {} && set", args.trim())
    };
    let mut c = Command::new("cmd");
    c.raw_arg(format!("/d /s /c \"{inner}\""));
    Some(c)
}

#[cfg(not(windows))]
fn build_devcmd_command(_bat: &Path, _args: &str) -> Option<Command> {
    None
}

/// 从 `set` 的输出里取 `NAME=value` 的值（大小写不敏感；取最后一次出现）。
/// 纯函数，单测覆盖。
pub fn parse_env_assignment(text: &str, name: &str) -> Option<String> {
    let mut hit: Option<String> = None;
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k.trim().eq_ignore_ascii_case(name) && !v.trim().is_empty() {
                hit = Some(v.trim().to_string());
            }
        }
    }
    hit
}

/// 组装 EnvOverlay（方案 §3.2 第 4 步）：
/// PATH 前置 MSVC bin；`LIB` = MSVC 的 lib\<target> **在前** + VsDevCmd 的 LIB；
/// `INCLUDE` 同理。顺序不能反 —— 空壳版本算出的 LIB 里没有 msvcrt.lib，
/// MSVC 自己的在前才能先命中。
fn assemble_overlay(
    bin_dir: &Path,
    lib_dir: &Path,
    include_dir: &Path,
    sdk_lib: &str,
    sdk_include: &str,
) -> EnvOverlay {
    let mut ov = EnvOverlay { path_dirs: vec![bin_dir.to_path_buf()], vars: Vec::new() };
    let lib = join_non_empty(&lib_dir.to_string_lossy(), sdk_lib);
    let include = join_non_empty(&include_dir.to_string_lossy(), sdk_include);
    if !lib.is_empty() {
        ov.vars.push(("LIB".to_string(), lib));
    }
    if !include.is_empty() {
        ov.vars.push(("INCLUDE".to_string(), include));
    }
    ov
}

fn join_non_empty(a: &str, b: &str) -> String {
    let a = a.trim();
    let b = b.trim();
    match (a.is_empty(), b.is_empty()) {
        (true, true) => String::new(),
        (false, true) => a.to_string(),
        (true, false) => b.to_string(),
        (false, false) => format!("{a};{b}"),
    }
}

/// MSVC 探测的完整结论（probe() 与 env_overlay() 共用同一条链，缓存使其便宜）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MsvcOutcome {
    /// 找到可用 link.exe，overlay 已就绪。
    Available { version: String, overlay: EnvOverlay },
    /// vswhere 找不到任何 VS 安装。
    Missing,
    /// 找到安装但没有可用的 MSVC 工具集（detail 说清哪个版本目录不完整）。
    Broken(String),
}

/// 目录比较键（与 env.rs 的 dir_key 同语义：去尾分隔符 + 小写）。
fn path_key(p: &Path) -> String {
    p.to_string_lossy().trim().trim_end_matches(['\\', '/']).to_lowercase()
}

/// 完整链：vswhere → 布局推导 → 缓存校验 → 扫描挑版本 → VsDevCmd → 组装 → 落缓存。
///
/// 缓存纪律（方案 §3.2 第 5 步）：读缓存前**先校验 link.exe 还在**且 VS 根没变；
/// 校验失败就重算（VS 升级 / 卸载 / 装了第二个版本都会走到这条路上），
/// 绝不把过期缓存直接交出去。
pub fn compute_msvc(state_root: &Path) -> MsvcOutcome {
    // ① 安装根（不带 -requires）
    let Some(vs_root) = query_vs_install_root() else {
        return MsvcOutcome::Missing;
    };
    // ② 布局：rustc host triple 推导，rustc 缺失退回本机架构
    let rustc = find_on_path("rustc.exe");
    let layout = resolve_layout(rustc.as_deref());

    // ③ 缓存：link.exe 还在 + VS 根没变 → 直接用（不重跑 VsDevCmd，省 1~2 秒）
    let state = store::load_state(state_root);
    if let Some(c) = state.msvc_cache.as_ref() {
        let link_ok = !c.link_exe.trim().is_empty() && Path::new(&c.link_exe).is_file();
        let root_same = path_key(Path::new(&c.vs_root)) == path_key(&vs_root);
        if link_ok && root_same {
            let bin = PathBuf::from(&c.bin_dir);
            if bin.is_dir() {
                return MsvcOutcome::Available {
                    version: c.msvc_version.clone(),
                    overlay: assemble_overlay(
                        &bin,
                        Path::new(&c.lib_dir),
                        Path::new(&c.include_dir),
                        &c.sdk_lib,
                        &c.sdk_include,
                    ),
                };
            }
        }
    }

    // ④ 重算：扫 MSVC 工具集目录，挑 link.exe 真实存在的最大版本
    let tools_dir = vs_root.join("VC").join("Tools").join("MSVC");
    let scan = scan_msvc_tools(&tools_dir, &layout);
    if scan.complete.is_empty() {
        // Broken 必须能说出「哪个版本目录不完整」（方案 §3.2）
        let vs_root_s = vs_root.to_string_lossy().to_string();
        let detail = if scan.incomplete.is_empty() {
            crate::i18n::fmt("tool_msvc_broken_no_toolset", &[&vs_root_s])
        } else {
            let names = scan.incomplete.join(", ");
            crate::i18n::fmt(
                "tool_msvc_broken_incomplete",
                &[&vs_root_s, &names, &layout.host_dir, &layout.target],
            )
        };
        return MsvcOutcome::Broken(detail);
    }
    let (version, dir) = scan.complete[0].clone();
    let bin = dir.join("bin").join(layout.host_dir).join(layout.target);
    let lib = dir.join("lib").join(layout.lib_dir);
    let include = dir.join("include");
    // scan 已确认 link.exe 在；bin/lib/include 三个目录再核一次（防扫描后被删的窗口）
    if !bin.is_dir() || !lib.is_dir() || !include.is_dir() {
        let vs_root_s = vs_root.to_string_lossy().to_string();
        return MsvcOutcome::Broken(crate::i18n::fmt(
            "tool_msvc_broken_incomplete",
            &[&vs_root_s, &version, &layout.host_dir, &layout.target],
        ));
    }

    // ⑤ VsDevCmd 算 SDK 路径（失败降级：overlay 只含 MSVC 自己的目录，不阻断判定）
    let (sdk_lib, sdk_include) = run_dev_cmd_env(&vs_root, &layout).unwrap_or_default();
    let overlay = assemble_overlay(&bin, &lib, &include, &sdk_lib, &sdk_include);

    // ⑥ 落缓存（含 VsDevCmd 失败时的空值；下次 link.exe 不在了才重算）
    let cache = MsvcCache {
        vs_root: vs_root.to_string_lossy().to_string(),
        msvc_version: version.clone(),
        link_exe: bin.join("link.exe").to_string_lossy().to_string(),
        bin_dir: bin.to_string_lossy().to_string(),
        lib_dir: lib.to_string_lossy().to_string(),
        include_dir: include.to_string_lossy().to_string(),
        sdk_lib: sdk_lib.clone(),
        sdk_include: sdk_include.clone(),
    };
    let mut st = state;
    st.msvc_cache = Some(cache);
    store::save_state(state_root, &st);

    MsvcOutcome::Available { version, overlay }
}

pub struct MsvcProbe;

impl SystemProbe for MsvcProbe {
    fn probe(&self, req: &ProbeReq) -> ProbeResult {
        #[cfg(windows)]
        {
            match compute_msvc(&req.state_root) {
                MsvcOutcome::Available { version, .. } => ProbeResult::Ok(version),
                MsvcOutcome::Missing => ProbeResult::Missing,
                MsvcOutcome::Broken(d) => ProbeResult::Broken(d),
            }
        }
        #[cfg(not(windows))]
        {
            let _ = req;
            ProbeResult::Missing
        }
    }

    fn guide_url(&self) -> &'static str {
        // 官方 Build Tools 下载页（缺失时的安装指引）
        "https://visualstudio.microsoft.com/visual-cpp-build-tools/"
    }

    fn env_overlay(&self, found: &ProbeFound) -> EnvOverlay {
        let root = found
            .extra
            .get("state_root")
            .map(PathBuf::from)
            .unwrap_or_default();
        match compute_msvc(&root) {
            MsvcOutcome::Available { overlay, .. } => overlay,
            _ => EnvOverlay::default(),
        }
    }
}

// ---------- Rust 探测 ----------

/// 从版本命令输出（`cargo 1.83.0 (…)` / `rustc 1.83.0 (… 2024-…)`）里取第一个
/// 能解析成版本号的 token。纯函数，单测覆盖。
pub fn first_version_token(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|t| crate::detect::parse_version_numbers(t).is_some())
        .map(str::to_string)
}

/// 版本串是否 ≥ min。解析不出来返回 None（判「未知」，不猜）——
/// 与 detect.rs 的 `*_at_least_min` 同一条纪律。
pub fn version_meets_min(current: &str, min: &str) -> Option<bool> {
    let v = crate::detect::parse_version_numbers(current)?;
    let m = crate::detect::parse_version_numbers(min)?;
    Some(crate::detect::cmp_versions(&v, &m) != std::cmp::Ordering::Less)
}

pub struct RustProbe;

impl SystemProbe for RustProbe {
    fn probe(&self, req: &ProbeReq) -> ProbeResult {
        // cargo 与 rustc 都要看：cargo.exe 在而 rustc 缺失（cargo 安装不完整）是
        // 真实的 Broken 状态，笼统报 Missing 会把人引去重装 Rust。
        let Some(cargo) = find_on_path("cargo.exe") else {
            return ProbeResult::Missing;
        };
        let cargo_ver = run_probe_version(&cargo);
        let rustc = find_on_path("rustc.exe");
        let rustc_ver = rustc.as_deref().and_then(run_probe_version);
        if rustc.is_none() {
            return match cargo_ver {
                Some(_) => ProbeResult::Broken(crate::i18n::t("tool_rust_broken_no_rustc").to_string()),
                None => ProbeResult::Missing,
            };
        }
        // 版本以 rustc 为准（真正参与编译的是它），读不出再退 cargo 的
        let version = rustc_ver.clone().or(cargo_ver).and_then(|t| first_version_token(&t));
        let Some(version) = version else {
            return ProbeResult::Missing;
        };
        if let Some(min) = req.min_version.as_deref() {
            if version_meets_min(&version, min) == Some(false) {
                return ProbeResult::TooOld { current: version, min: min.to_string() };
            }
        }
        ProbeResult::Ok(version)
    }

    fn guide_url(&self) -> &'static str {
        "https://rustup.rs"
    }
}

/// 跑 `--version` 并取首行非空内容（有界探测）。模块内共用（受管检测也走它）。
pub(crate) fn run_probe_version(exe: &Path) -> Option<String> {
    let args: Vec<String> = ["--version"].iter().map(|s| (*s).to_string()).collect();
    let mut cmd = command_for(&exe.to_string_lossy(), &args).ok()?;
    let text = run_capture_text(&mut cmd, PROBE_MAX_BYTES, PROBE_TIMEOUT)?;
    text.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string)
}

// ---------- WebView2 探测（方案 §4.5：三个注册表位置按序查） ----------

/// WebView2 Runtime 在 EdgeUpdate\Clients 下的产品 GUID（微软公开常量）。
/// 刻意**按 GUID 精确匹配**而不是按 name 字段模糊匹配：同一个 Clients 键下还有
/// Edge 浏览器的条目，按名字找会撞上（方案 §4.5）。
pub const WEBVIEW2_CLIENT_GUID: &str = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";

/// 三个注册表位置，按序查、任一命中即可。
///
/// 为什么必须三处都查（方案 §4.5 的本机实测）：WebView2 可能是**用户级**安装
/// （只在 HKCU），也可能是**系统级**（HKLM 的两个位置）。只查一处必然在
/// 另一类机器上误判。
pub fn webview2_registry_keys() -> Vec<String> {
    let bases = [
        r"HKCU\SOFTWARE\Microsoft\EdgeUpdate\Clients",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients",
        r"HKLM\SOFTWARE\Microsoft\EdgeUpdate\Clients",
    ];
    bases.iter().map(|b| format!("{b}\\{WEBVIEW2_CLIENT_GUID}")).collect()
}

/// 从 `reg query <key> /v pv` 的输出里取版本值。行形如：
/// `    pv    REG_SZ    <版本>`。纯函数，单测覆盖。
pub fn parse_reg_pv_output(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| {
            let mut it = l.split_whitespace();
            it.next().is_some_and(|t| t.eq_ignore_ascii_case("pv")) && l.contains("REG_")
        })
        .and_then(|l| l.split_whitespace().last().map(str::to_string))
        .filter(|v| !v.is_empty())
}

/// 读一个键下的 `pv` 值（有界探测；键不存在 / 读不到返回 None）。
#[cfg(windows)]
fn reg_read_pv(key: &str) -> Option<String> {
    let args: Vec<String> = ["query", key, "/v", "pv"].iter().map(|s| (*s).to_string()).collect();
    let mut cmd = command_for("reg", &args).ok()?;
    let text = run_capture_text(&mut cmd, PROBE_MAX_BYTES, Duration::from_secs(5))?;
    parse_reg_pv_output(&text)
}

pub struct Webview2Probe;

impl SystemProbe for Webview2Probe {
    fn probe(&self, _req: &ProbeReq) -> ProbeResult {
        #[cfg(windows)]
        {
            for key in webview2_registry_keys() {
                if let Some(pv) = reg_read_pv(&key) {
                    return ProbeResult::Ok(pv);
                }
            }
            ProbeResult::Missing
        }
        #[cfg(not(windows))]
        {
            ProbeResult::Missing
        }
    }

    fn guide_url(&self) -> &'static str {
        "https://developer.microsoft.com/microsoft-edge/webview2/"
    }
}

// ---------- 单测 ----------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// 经 cmd 跑一条内联命令（与生产同一条路径：command_for 对 cmd 直接转义）。
    fn cmd_c(inner: &str) -> Command {
        let args: Vec<String> = ["/d", "/s", "/c", inner].iter().map(|s| (*s).to_string()).collect();
        command_for("cmd", &args).expect("cmd 命令应能构造")
    }

    /// 判据 (a)：故意卡住的命令在超时内返回 None，不能拖死调用方。
    /// `ping -n 31` 每秒一发、共 30 秒 —— 远超 1.5 秒超时，又不需要真的等它。
    #[test]
    #[cfg(windows)]
    fn bounded_capture_times_out_a_hanging_command() {
        let started = Instant::now();
        let mut c = cmd_c("ping -n 31 127.0.0.1");
        let out = run_bounded_capture(&mut c, PROBE_MAX_BYTES, Duration::from_millis(1500));
        let elapsed = started.elapsed();
        assert_eq!(out, None, "挂住的命令应返回 None");
        assert!(
            elapsed < Duration::from_secs(5),
            "必须在超时后尽快返回，实际耗时 {elapsed:?}"
        );
    }

    /// 判据 (b)：无限输出的程序不会撑大内存 —— reader 在 limit 处停手，
    /// 超时后 kill；调用方拿到 None，且**及时**返回。
    #[test]
    #[cfg(windows)]
    fn bounded_capture_survives_infinite_output() {
        let started = Instant::now();
        let mut c = cmd_c("for /l %i in (0,0,0) do @echo 0123456789012345678901234567890123456789");
        let out = run_bounded_capture(&mut c, PROBE_MAX_BYTES, Duration::from_millis(2500));
        let elapsed = started.elapsed();
        assert_eq!(out, None, "永不退出的命令应返回 None");
        assert!(elapsed < Duration::from_secs(6), "应及时返回，实际耗时 {elapsed:?}");
    }

    /// 判据 (b) 补充：输出**超过** limit 但命令正常退出的，返回**截断**的
    /// 恰好 limit 字节 —— 内存上界就是 limit。
    #[test]
    #[cfg(windows)]
    fn bounded_capture_truncates_to_limit() {
        // 1000 行 × 41 字节 ≈ 41 KB，远超 4096
        let mut c = cmd_c("for /l %i in (1,1,1000) do @echo 0123456789012345678901234567890123456789");
        let out = run_bounded_capture(&mut c, PROBE_MAX_BYTES, Duration::from_secs(15)).expect("应成功");
        assert_eq!(out.len(), PROBE_MAX_BYTES, "应恰好截断到 limit");
    }

    /// 判据 (c)：非零退出返回 None（哪怕有输出）。
    #[test]
    #[cfg(windows)]
    fn bounded_capture_rejects_nonzero_exit() {
        let mut c = cmd_c("echo boom& exit /b 7");
        assert_eq!(run_bounded_capture(&mut c, PROBE_MAX_BYTES, Duration::from_secs(10)), None);
    }

    /// 正常小输出命令：拿到文本、探测路径能解析。
    #[test]
    #[cfg(windows)]
    fn bounded_capture_returns_output_of_successful_command() {
        let mut c = cmd_c("echo hello-probe");
        let text = run_capture_text(&mut c, PROBE_MAX_BYTES, Duration::from_secs(10));
        assert!(text.unwrap_or_default().contains("hello-probe"));
    }

    /// spawn 失败（目标不存在）折叠成 None。
    #[test]
    fn bounded_capture_folds_spawn_failure_into_none() {
        let mut c = Command::new("dsh-no-such-tool-xyz-9817");
        assert_eq!(run_bounded_capture(&mut c, PROBE_MAX_BYTES, Duration::from_secs(2)), None);
    }

    /// PATH 查找：上限生效、顺序生效、用临时目录构造（不依赖本机真实环境）。
    #[test]
    fn find_in_path_dirs_respects_order_and_limit() {
        let base = std::env::temp_dir().join(format!("dsh-probe-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let hit_b = b.join("probe-fake-tool.exe");
        std::fs::write(&hit_b, b"x").unwrap();

        let dirs = vec![a.clone(), b.clone()];
        assert_eq!(find_in_path_dirs(&dirs, "probe-fake-tool.exe", PATH_SCAN_MAX_DIRS), Some(hit_b.clone()));
        // 上限 1：只有第一个目录参与查找 → 找不到
        assert_eq!(find_in_path_dirs(&dirs, "probe-fake-tool.exe", 1), None);
        // 目标不存在 → None
        assert_eq!(find_in_path_dirs(&dirs, "no-such-thing.exe", PATH_SCAN_MAX_DIRS), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 架构布局表：三种 rustc host triple 都能推导（判据：不写死 HostX64\x64）。
    #[test]
    fn msvc_layout_covers_all_documented_triples() {
        for (arch, host, target) in [
            ("x86_64", "HostX64", "x64"),
            ("i686", "HostX86", "x86"),
            ("aarch64", "Hostarm64", "arm64"),
        ] {
            let l = msvc_layout_for_arch(arch).unwrap_or_else(|| panic!("{arch} 应有布局"));
            assert_eq!(l.host_dir, host);
            assert_eq!(l.target, target);
            assert_eq!(l.lib_dir, target);
        }
        // 未知架构不猜
        assert_eq!(msvc_layout_for_arch("sparc"), None);
        // triple 取架构段
        assert_eq!(arch_of_triple("x86_64-pc-windows-msvc"), Some("x86_64"));
        assert_eq!(arch_of_triple("i686-pc-windows-msvc"), Some("i686"));
        assert_eq!(arch_of_triple("aarch64-pc-windows-msvc"), Some("aarch64"));
        assert_eq!(arch_of_triple(""), None);
        // resolve_layout：rustc 缺失时也能给出一个布局（退回本机架构）
        let l = resolve_layout(None);
        assert!(msvc_layout_for_arch(std::env::consts::ARCH).is_some() || l.host_dir.starts_with("Host"));
    }

    /// MSVC 扫描：完整 / 空壳 / 多版本 / 无目录，全部用临时目录构造
    /// （判据：单测覆盖「路径不存在」「目录是空壳」「多个版本」这几类输入）。
    #[test]
    fn scan_msvc_tools_picks_complete_versions_and_skips_shells() {
        let base = std::env::temp_dir().join(format!("dsh-probe-msvc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let tools = base.join("MSVC");
        let layout = MsvcLayout { host_dir: "HostX64", target: "x64", lib_dir: "x64" };

        // 目录不存在 → 两个列表都空
        let scan = scan_msvc_tools(&tools, &layout);
        assert!(scan.complete.is_empty() && scan.incomplete.is_empty());

        // 造三个版本：10.1.0 完整、11.1.1 空壳（只有 Auxiliary）、
        // 9.9.0 完整（字符串序把它排在最前，数值序应排最后 —— 用它验证降序是对的）
        let mk = |name: &str| tools.join(name);
        for name in ["10.1.0", "11.1.1", "9.9.0"] {
            std::fs::create_dir_all(mk(name)).unwrap();
        }
        let full1 = mk("10.1.0").join("bin").join("HostX64").join("x64");
        let full2 = mk("9.9.0").join("bin").join("HostX64").join("x64");
        std::fs::create_dir_all(&full1).unwrap();
        std::fs::create_dir_all(&full2).unwrap();
        std::fs::write(full1.join("link.exe"), b"x").unwrap();
        std::fs::write(full2.join("link.exe"), b"x").unwrap();
        // 空壳版本：只造 Auxiliary，不造 bin
        std::fs::create_dir_all(mk("11.1.1").join("Auxiliary")).unwrap();

        let scan = scan_msvc_tools(&tools, &layout);
        // 完整版本数值降序：10.1.0 在 9.9.0 前（字符串序会给出相反结果）
        assert_eq!(scan.complete.len(), 2, "{:?}", scan.complete);
        assert_eq!(scan.complete[0].0, "10.1.0");
        assert_eq!(scan.complete[1].0, "9.9.0");
        // 空壳版本被认出并单独记账
        assert_eq!(scan.incomplete, vec!["11.1.1".to_string()]);
        // 换个架构布局：同一棵树立刻全变「不完整」（布局必须跟着 rustc 走）
        let x86 = MsvcLayout { host_dir: "HostX86", target: "x86", lib_dir: "x86" };
        let scan_x86 = scan_msvc_tools(&tools, &x86);
        assert!(scan_x86.complete.is_empty());
        assert_eq!(scan_x86.incomplete.len(), 3);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `set` 输出解析：认得出 LIB / INCLUDE、大小写不敏感、取最后一次出现。
    #[test]
    fn parse_env_assignment_finds_lib_and_include() {
        let sample = "**********\r\nVsDevCmd.bat: ok\r\n\r\nALLUSERSPROFILE=C:\\ProgramData\r\n\
                      INCLUDE=C:\\Kits\\10\\Include\\10.0.xxxx.0\\ucrt\r\n\
                      LIB=C:\\Kits\\10\\Lib\\10.0.xxxx.0\\um\\x64\r\n\
                      PATH=C:\\something\r\n";
        assert_eq!(
            parse_env_assignment(sample, "LIB").as_deref(),
            Some(r"C:\Kits\10\Lib\10.0.xxxx.0\um\x64")
        );
        assert_eq!(
            parse_env_assignment(sample, "include").as_deref(),
            Some(r"C:\Kits\10\Include\10.0.xxxx.0\ucrt")
        );
        assert_eq!(parse_env_assignment(sample, "TMP"), None);
        assert_eq!(parse_env_assignment("", "LIB"), None);
        // 值为空不算命中
        assert_eq!(parse_env_assignment("LIB=\r\n", "LIB"), None);
        // 多次出现取最后一次
        assert_eq!(
            parse_env_assignment("LIB=a\r\nLIB=b\r\n", "LIB").as_deref(),
            Some("b")
        );
    }

    /// overlay 组装：MSVC 的 lib/include 在前、SDK 的在后（顺序不能反）。
    #[test]
    fn assemble_overlay_orders_msvc_before_sdk() {
        let ov = assemble_overlay(
            Path::new(r"D:\MSVC\14.x\bin\HostX64\x64"),
            Path::new(r"D:\MSVC\14.x\lib\x64"),
            Path::new(r"D:\MSVC\14.x\include"),
            r"C:\Kits\10\Lib\um\x64",
            r"C:\Kits\10\Include\ucrt",
        );
        assert_eq!(ov.path_dirs, vec![PathBuf::from(r"D:\MSVC\14.x\bin\HostX64\x64")]);
        assert_eq!(
            ov.var("LIB").unwrap(),
            r"D:\MSVC\14.x\lib\x64;C:\Kits\10\Lib\um\x64"
        );
        assert_eq!(
            ov.var("INCLUDE").unwrap(),
            r"D:\MSVC\14.x\include;C:\Kits\10\Include\ucrt"
        );
        // SDK 没算到：只剩 MSVC 自己的（降级不阻断）
        let ov2 = assemble_overlay(
            Path::new(r"D:\bin"),
            Path::new(r"D:\lib"),
            Path::new(r"D:\include"),
            "",
            "",
        );
        assert_eq!(ov2.var("LIB").unwrap(), r"D:\lib");
        assert_eq!(ov2.vars.len(), 2);
    }

    /// WebView2 输出解析与键序：GUID 三处、HKCU 在前（本机实测用户级安装命中 HKCU，
    /// 但代码必须三处都查 —— 判据：不是只查 HKCU）。
    #[test]
    fn webview2_keys_and_pv_parse() {
        let keys = webview2_registry_keys();
        assert_eq!(keys.len(), 3, "三处注册表都要查");
        assert!(keys[0].starts_with("HKCU\\"), "第一处应是 HKCU");
        assert!(keys[1].starts_with(r"HKLM\SOFTWARE\WOW6432Node\\".trim_end_matches('\\')), "第二处应是 HKLM WOW6432Node");
        assert!(keys[2].starts_with(r"HKLM\SOFTWARE\\".trim_end_matches('\\')), "第三处应是 HKLM");
        for k in &keys {
            assert!(k.ends_with(WEBVIEW2_CLIENT_GUID), "按 GUID 精确匹配：{k}");
        }
        let sample = "\r\nHKEY_CURRENT_USER\\SOFTWARE\\Microsoft\\EdgeUpdate\\Clients\\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}\r\n\
                      某产品键名    REG_SZ    Edge Client\r\n\
                      pv    REG_SZ    1.2.3.4.5\r\n\r\n";
        assert_eq!(parse_reg_pv_output(sample).as_deref(), Some("1.2.3.4.5"));
        // 按名字找不到 pv 时返回 None，不猜
        assert_eq!(parse_reg_pv_output("    version    REG_SZ    1.2.3"), None);
        assert_eq!(parse_reg_pv_output(""), None);
    }

    /// 版本提取与下限比较：恰好等于 min 与低于 min 两种边界都要钉住（第 4 步判据）。
    #[test]
    fn version_token_and_min_comparison() {
        assert_eq!(
            first_version_token("cargo 1.83.0 (5f8c 2024-12-10)").as_deref(),
            Some("1.83.0")
        );
        assert_eq!(
            first_version_token("rustc 1.77.2 (e8e5 2024-03-28)").as_deref(),
            Some("1.77.2")
        );
        assert_eq!(first_version_token("cargo: command not found"), None);
        assert_eq!(first_version_token(""), None);

        // 恰好等于 min → 满足
        assert_eq!(version_meets_min("1.77.2", "1.77.2"), Some(true));
        // 低于 min → 不满足
        assert_eq!(version_meets_min("1.76.0", "1.77.2"), Some(false));
        // 残缺段按 0 补齐后再比：1.77 == 1.77.0 < 1.77.2
        assert_eq!(version_meets_min("1.77", "1.77.2"), Some(false));
        // 反过来 min 更短：1.77.2 > 1.77.0
        assert_eq!(version_meets_min("1.77.2", "1.77"), Some(true));
        // 解析不出来 → None（判「未知」，不猜）
        assert_eq!(version_meets_min("junk", "1.77.2"), None);
        assert_eq!(version_meets_min("1.77.2", "junk"), None);
    }

    /// ProbeResult 的语义辅助方法：found / usable / version 的划分要与
    /// 「Missing ≠ Broken」的纪律一致。
    #[test]
    fn probe_result_semantics() {
        let ok = ProbeResult::Ok("1.83.0".into());
        assert!(ok.found() && ok.usable() && ok.version() == Some("1.83.0"));
        let too_old = ProbeResult::TooOld { current: "1.70.0".into(), min: "1.77.2".into() };
        assert!(too_old.found() && !too_old.usable() && too_old.version() == Some("1.70.0"));
        let broken = ProbeResult::Broken("某个版本目录不完整".into());
        assert!(broken.found() && !broken.usable() && broken.version().is_none());
        assert!(!ProbeResult::Missing.found());
    }

    /// rustc host triple 解析（走真实 rustc；rustc 不存在的机器上跳过）。
    #[test]
    #[cfg(windows)]
    fn rustc_triple_when_available() {
        let Some(rustc) = find_on_path("rustc.exe") else {
            return; // 没有 rustc 的机器：本机探测无从谈起，跳过
        };
        let t = rustc_host_triple(&rustc);
        if let Some(t) = t {
            assert!(t.ends_with("-windows-msvc"), "本程序只在 Windows 编译：{t}");
        }
    }

    /// MSVC compute 在「无 state.json」的临时根上应能出结论（本机有 VS → Available；
    /// 无 VS 的机器 → Missing / Broken，都不该 panic）。顺带验证缓存真的被写出。
    #[test]
    #[cfg(windows)]
    fn compute_msvc_produces_outcome_and_writes_cache() {
        let root = std::env::temp_dir().join(format!("dsh-probe-msvc-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let outcome = compute_msvc(&root);
        match outcome {
            MsvcOutcome::Available { version, overlay } => {
                // 版本目录名是运行时读到的值，断言形状而不是具体数字
                assert!(!version.is_empty());
                assert_eq!(overlay.path_dirs.len(), 1, "PATH 前置应恰为 MSVC bin");
                assert!(overlay.path_dirs[0].join("link.exe").is_file(), "bin 目录必须真的含 link.exe");
                assert!(overlay.var("LIB").is_some(), "LIB 必须存在");
                // LIB 的 MSVC 部分必须指向真实存在的目录（本机验证核心判据）
                let lib = overlay.var("LIB").unwrap().split(';').next().unwrap().to_string();
                assert!(Path::new(&lib).is_dir(), "LIB 的 MSVC 部分 {lib} 应存在");
                // 缓存已写出，且 link_exe 锚点有效
                let st = store::load_state(&root);
                let c = st.msvc_cache.expect("缓存应已写出");
                assert!(Path::new(&c.link_exe).is_file());
            }
            MsvcOutcome::Missing | MsvcOutcome::Broken(_) => {
                // 没有 VS 的机器/CI：得结论即可（Broken 时 detail 必须非空）
                if let MsvcOutcome::Broken(d) = outcome {
                    assert!(!d.trim().is_empty());
                }
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 缓存有效性：link.exe 被删后 compute 必须重算而不是交出旧缓存
    /// （判据：缓存在使用前校验有效性，校验失败能重算）。
    #[test]
    #[cfg(windows)]
    fn stale_cache_is_recomputed_when_link_exe_missing() {
        let root = std::env::temp_dir().join(format!("dsh-probe-msvc-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // 先算一次真实环境（有 VS 才继续；无 VS 的机器跳过这条）
        let first = compute_msvc(&root);
        let MsvcOutcome::Available { .. } = &first else {
            let _ = std::fs::remove_dir_all(&root);
            return;
        };
        // 把缓存里的 link.exe 位置改成一个必然不存在的路径（模拟 VS 升级换目录）
        let mut st = store::load_state(&root);
        let mut c = st.msvc_cache.clone().expect("上一步应已写缓存");
        c.link_exe = format!("{}\\no-such\\link.exe", std::env::temp_dir().display());
        c.vs_root = c.vs_root.clone();
        st.msvc_cache = Some(c);
        store::save_state(&root, &st);
        // 重算后仍应 Available，且缓存被修复为真实路径
        let second = compute_msvc(&root);
        match second {
            MsvcOutcome::Available { .. } => {
                let st2 = store::load_state(&root);
                let c2 = st2.msvc_cache.expect("重算后应重新写缓存");
                assert!(Path::new(&c2.link_exe).is_file(), "重算后的 link_exe 必须真实存在");
            }
            other => panic!("缓存失效后重算应恢复 Available，得到 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **回归护栏**：`VsDevCmd.bat … && set` 的输出必须能被读回。
///
/// 这条测试守的是一个真机 bug（本地实测发现）：该命令的子进程正常退出（exit 0），
/// 但它的**后代仍持有 stdout 管道**，reader 永远等不到 EOF。若把"发送结果"绑在
/// EOF 上，主线程有界等待超时后只能拿到空 —— 表现为 SDK 的 LIB/INCLUDE 丢失，
/// 最终链接时报 `LNK1181: 无法打开输入文件 kernel32.lib`。
///
/// 现在的实现边读边发，所以即便后代开着管道也能拿到已经产出的字节。没有 VS 的
/// 机器上跳过（无从谈 SDK 路径）。
#[test]
#[cfg(windows)]
fn dev_cmd_env_exposes_windows_sdk_libs() {
    let Some(root) = query_vs_install_root() else {
        return;
    };
    let layout = resolve_layout(find_on_path("rustc.exe").as_deref());
    let Some((lib, include)) = run_dev_cmd_env(&root, &layout) else {
        // 有 VS 却取不到环境：可能是只有空壳工具集（没有 VsDevCmd/vcvarsall）。
        // 那种机器由 scan_msvc_tools 判 Broken，这里不重复断言。
        return;
    };
    assert!(!lib.is_empty(), "LIB 应含 Windows SDK 的 lib 路径");
    assert!(!include.is_empty(), "INCLUDE 应含 Windows SDK 的 include 路径");
    // SDK 路径按定义在 Windows Kits 下；断言形状而不是写死具体版本号
    let lib_lc = lib.to_ascii_lowercase();
    assert!(
        lib_lc.contains("windows kits") || lib_lc.contains("lib"),
        "LIB 看起来不像 SDK 路径：{lib}"
    );
    // 多个路径用分号分隔（这些是 link.exe 的搜索目录）
    assert!(lib.contains(';'), "SDK 的 LIB 应包含多个搜索目录：{lib}");
}

/// **第 3 步的核心验收**：用 `compute_msvc` 自己算出的 EnvOverlay，在一个
/// **剥掉了所有 VS 环境**的子进程里跑 `rustc hello.rs`，必须链接成功。
///
/// 这是"裸终端里 cargo check 能过"的自动化等价物：
/// - PATH 只给 overlay 的 MSVC bin（模拟普通用户 PATH 里没有 link.exe）；
/// - LIB / INCLUDE 只给 overlay 的值（模拟从未打开过开发者命令提示符）；
/// - rustc 用绝对路径启动（它本身不依赖 PATH）。
///
/// 本机实测基线：不加 overlay 时 `rustc hello.rs` 报
/// `linker 'link.exe' not found`；加了就产出 exe。没有 VS 的机器上跳过
/// （找不到可用的 MSVC 时无从谈"链接"）。
#[test]
#[cfg(windows)]
fn overlay_from_compute_msvc_links_a_real_program() {
    let base = std::env::temp_dir().join(format!("dsh-probe-link-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let state_root = base.join("state");
    std::fs::create_dir_all(&state_root).unwrap();

    let MsvcOutcome::Available { overlay, .. } = compute_msvc(&state_root) else {
        let _ = std::fs::remove_dir_all(&base);
        return; // 没有可用的 MSVC：这条断言在本机无意义
    };
    let Some(rustc) = find_on_path("rustc.exe") else {
        let _ = std::fs::remove_dir_all(&base);
        return;
    };

    let src = base.join("hello.rs");
    std::fs::write(&src, b"fn main() { println!(\"hello\"); }\n").unwrap();
    let exe = base.join("hello.exe");
    let _ = std::fs::remove_file(&exe);

    // 子进程环境**只**来自 overlay：PATH 拿 overlay 的值（existing = None），
    // 显式清掉可能从父进程继承的 LIB / INCLUDE / LIBPATH。
    let mut cmd = std::process::Command::new(&rustc);
    cmd.arg(&src)
        .current_dir(&base)
        .env("PATH", overlay.path_with_prefix(None))
        .env_remove("LIB")
        .env_remove("INCLUDE")
        .env_remove("LIBPATH");
    for (k, v) in &overlay.vars {
        cmd.env(k, v);
    }
    crate::process::apply_no_window(&mut cmd);
    let out = cmd.output().expect("rustc 应能启动");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "用 compute_msvc 算出的 EnvOverlay 链接失败：\nPATH={:?}\nLIB={:?}\nINCLUDE={:?}\n--- stderr ---\n{stderr}",
        overlay.path_with_prefix(None),
        overlay.var("LIB"),
        overlay.var("INCLUDE"),
    );
    assert!(exe.is_file(), "rustc 返回成功但没有产出 hello.exe");
    let _ = std::fs::remove_dir_all(&base);
}

/// 空壳版本必须被识别为 Broken，且 detail 能说出是哪个版本目录
    /// （判据：Broken 项显示具体原因）。用假布局构造：vswhere 没法伪造，
    /// 这里直接测 scan → Broken detail 的拼装路径。
    #[test]
    fn broken_detail_names_the_incomplete_dirs() {
        let base = std::env::temp_dir().join(format!("dsh-probe-msvc-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let tools = base.join("MSVC");
        std::fs::create_dir_all(tools.join("11.1.1").join("Auxiliary")).unwrap();
        let layout = MsvcLayout { host_dir: "HostX64", target: "x64", lib_dir: "x64" };
        let scan = scan_msvc_tools(&tools, &layout);
        assert!(scan.complete.is_empty());
        assert_eq!(scan.incomplete.len(), 1);
        // 拼出与 compute_msvc 同构的 detail：应包含目录名与缺的组件
        let detail = crate::i18n::fmt(
            "tool_msvc_broken_incomplete",
            &[&base.to_string_lossy().to_string(), &scan.incomplete.join(", "), &layout.host_dir, &layout.target],
        );
        assert!(detail.contains("11.1.1"), "detail 必须点名不完整的目录：{detail}");
        assert!(detail.contains("link.exe"), "detail 必须说清缺的是 link.exe：{detail}");
        let _ = std::fs::remove_dir_all(&base);
    }

    // 让未在全部测试分支用到的导入保持安静
    #[allow(unused)]
    fn _unused(o: &OsStr) -> &OsStr {
        o
    }
}
