//! Managed 四项（node / pnpm / dsh / python）的检测与安装。
//!
//! 本文件分两半：
//! - **检测半边**：受管目录里装了什么、系统里有什么可复用的。只读、不碰网络 ——
//!   `resolve()` 每次调用都依赖它（方案 §4.1）。
//! - **安装半边**：下载 → 校验 → 解压/安装 → 试跑 → 原子落位（方案 §1.1 的表
//!   与 §5A.4 的校验链）。
//!
//! ## 三条安全纪律（方案 §5.1 / §5A.4）
//!
//! 1. **信任锚与传输通道分离**：清单 URL 是**常量**，绝不随环境变量变；哈希只从
//!    官方渠道取（nodejs.org 的 SHASUMS256 / GitHub Releases 的 asset digest），
//!    文件可以从任意可达通道下。
//! 2. **拒绝非 HTTPS**：即便哈希固定，plain-http 也能让人拿到「旧的但哈希正确的
//!    版本」（降级攻击）。下载走 `process::try_download_curl` —— 它内部强制
//!    `--proto =https` / `--proto-redir =https`，**安全参数不在这里复制一份**。
//! 3. **解压前校验归档成员**：拒绝 `..`、绝对路径、符号链接、重复项，并限制条目数
//!    （zip-slip 与解压膨胀防护，方案 §5A.4 第 6 条）。
//!
//! ## 措辞边界（方案 §5A.9）
//!
//! SHA-256 校验**不是签名**。官方仓库（nodejs.org / GitHub）仍是共同信任根；这套
//! 机制防的是镜像投毒与传输损坏，**不防上游账号被攻陷**。防后者需要签名验证
//! （sigstore / cosign / GPG），v1 不做。所有文案与注释都不要把它说成签名验证。

use std::path::{Path, PathBuf};

use super::env::EnvOverlay;
use super::probe::{self, DIR_SCAN_MAX_ENTRIES};
use super::{InstallReq, Installed, Progress, ToolId};
use crate::process::{
    command_for, create_private_temp_dir, emit_download_progress, fetch_expected_sha256,
    resolve_latest_lts_version, run_cmd_capture, sha256_hex_of, try_download_curl,
    try_download_powershell,
};

// ---------- 常量：清单一律写死在这里，不随任何环境变量变（方案 §5A.4 第 1 条） ----------

/// Node.js 官方 dist 根（哈希清单与安装包都从这里取）。
/// 传输通道可以换，**这个信任锚不换**。
pub const NODE_DIST_BASE: &str = "https://nodejs.org/dist/";

/// python-build-standalone 的**官方仓库**（清单来源 = 信任锚）。
pub const PYBS_REPO: &str = "astral-sh/python-build-standalone";

/// python-build-standalone 的 Releases API（常量：拿 asset 的 digest 用）。
pub fn pybs_releases_api_url() -> String {
    format!("https://api.github.com/repos/{PYBS_REPO}/releases")
}

/// python-build-standalone 的资产下载根（常量）。
pub fn pybs_download_base() -> String {
    format!("https://github.com/{PYBS_REPO}/releases/download/")
}

/// 允许额外下载的 HTTPS 前缀（传给共享下载器做来源守卫，**不复制安全参数**）。
fn pybs_allowed_prefixes() -> Vec<&'static str> {
    // 注意：`format!` 的结果无法进 `&'static [&'static str]`，所以这里用两个
    // 与函数返回同源的**字面量**前缀；两者必须一致（下方单测钉住）。
    vec![
        "https://github.com/astral-sh/python-build-standalone/releases/download/",
        "https://api.github.com/repos/astral-sh/python-build-standalone/releases",
    ]
}

/// 归档内条目数上限（解压膨胀防护的粗粒度闸；真正的上限是体积）。
pub const ARCHIVE_MAX_MEMBERS: usize = 8192;
/// 下载物体积上限（Node zip ≈ 30MB、Python tar.gz ≈ 50MB；留足冗余但挡住异常大文件，
/// 同时是「读入内存前」的最后一道闸）。
pub const DOWNLOAD_MAX_BYTES: u64 = 400 * 1024 * 1024;

/// dsh 的**默认** npm 包名。真正的包名来自 `config.json` 的 `package_name`
/// （可配置字段）；这里只是 trait 方法拿不到 config 时的兜底，与
/// `config.rs` 的 `Default for Config` 保持一致。
pub const DEFAULT_DSH_PACKAGE: &str = "@deepseek-ai/dsh";

// ---------- 目录布局（方案 §2.1） ----------

/// 受管目录里某个工具的版本目录：`<root>\tools\<tool>\<version>\`。
pub fn tool_dir(root: &Path, id: ToolId, version: &str) -> PathBuf {
    root.join("tools").join(id.as_str()).join(version)
}

/// npm 全局 shim 所在的 bin 目录：Windows 上 npm 把 `dsh.cmd` 直接写进 `--prefix`
/// 目录本身，Linux 上写进 `<prefix>/bin/`。**全模块只有这一个函数知道这个差异**
/// （方案 §1.3 点名「移植时最容易漏的一层」）。
pub fn bin_dir_for(prefix: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        prefix.to_path_buf()
    }
    #[cfg(not(windows))]
    {
        prefix.join("bin")
    }
}

// ---------- 校验工具（纯函数为主，单测直接钉住） ----------

/// 只接受 https 的下载/清单地址。非 HTTPS 一律拒 —— 防降级的第一道闸。
pub fn require_https(url: &str, what: &str) -> Result<(), String> {
    if url.starts_with("https://") {
        Ok(())
    } else {
        Err(crate::i18n::fmt("tool_dl_not_https", &[&what, &url]))
    }
}

/// 清单地址的本地测试覆盖入口。
///
/// **v1 不提供这个入口**：常量是唯一路径，从构造上就没有可被污染的地方 ——
/// 这比"只允许 file:// 绝对路径"更硬。留着这个函数只为说明那条备选方案
/// （方案 §5A.4 第 2 条）曾经被考虑过，不参与任何调用链。
#[allow(dead_code)]
pub fn manifest_override_for(raw: Option<&str>, fallback: &str) -> String {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return fallback.to_string();
    };
    let Some(rest) = raw.strip_prefix("file://") else {
        return fallback.to_string();
    };
    let p = Path::new(rest);
    if p.is_absolute() {
        raw.to_string()
    } else {
        fallback.to_string()
    }
}

/// 归档成员校验（方案 §5A.4 第 6 条 / zip-slip 防护）。纯函数。
///
/// 拒绝：绝对路径、盘符、任何 `..` 段、空段、重复项、超量条目。
/// `tar -tf` 的输出每行一个成员名（目录以 `/` 结尾），这里统一按 `/` 与 `\` 切段判断。
pub fn validate_archive_members(members: &[String]) -> Result<(), String> {
    if members.is_empty() {
        return Err(crate::i18n::t("tool_zip_empty").to_string());
    }
    if members.len() > ARCHIVE_MAX_MEMBERS {
        return Err(crate::i18n::fmt(
            "tool_zip_too_many",
            &[&members.len(), &ARCHIVE_MAX_MEMBERS],
        ));
    }
    let mut seen: Vec<String> = Vec::with_capacity(members.len());
    for m in members {
        let name = m.trim();
        if name.is_empty() {
            return Err(crate::i18n::t("tool_zip_empty_member").to_string());
        }
        // 绝对路径 / UNC / 盘符
        if name.starts_with('/') || name.starts_with('\\') || name.starts_with("//") {
            return Err(crate::i18n::fmt("tool_zip_absolute", &[&name]));
        }
        if name.len() >= 2 && name.as_bytes()[1] == b':' {
            return Err(crate::i18n::fmt("tool_zip_absolute", &[&name]));
        }
        // 任何 `..` 段（zip-slip）
        let segs: Vec<&str> = name
            .split(['/', '\\'])
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        if segs.contains(&"..") {
            return Err(crate::i18n::fmt("tool_zip_traversal", &[&name]));
        }
        // 重复项（同名两份 = 后一份静默覆盖前一份，是投毒与解压歧义的常见手法）
        let key = segs.join("/").to_lowercase();
        if seen.contains(&key) {
            return Err(crate::i18n::fmt("tool_zip_duplicate", &[&name]));
        }
        seen.push(key);
    }
    Ok(())
}

/// 版本目录名白名单：至少两段、每段非空纯数字。
/// 版本名会拼进落盘路径与下载 URL，必须挡住 `..`、空段与任何路径片段。
pub fn is_safe_version_dir_name(v: &str) -> bool {
    let mut parts = 0usize;
    for p in v.split('.') {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        parts += 1;
    }
    parts >= 2
}

/// **拒绝自动降级**（方案 §5A.4 第 5 条）：要装的版本比已装的旧就拒绝。
/// 已装版本读不出时不拦（unknown 不判过高/过低，与全项目同一纪律）。
pub fn check_not_downgrade(id: ToolId, installed: Option<&str>, wanted: &str) -> Result<(), String> {
    let Some(inst) = installed.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    match (
        crate::detect::parse_version_numbers(inst),
        crate::detect::parse_version_numbers(wanted),
    ) {
        (Some(a), Some(b)) => {
            if crate::detect::cmp_versions(&b, &a) == std::cmp::Ordering::Less {
                Err(crate::i18n::fmt("tool_dl_downgrade", &[&id.to_string(), &wanted, &inst]))
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

/// 校验下载物的 SHA-256；**不符就删掉它并返回 Err**（七个受管安装路径共用）。
///
/// 这是「哈希不符不留残留」的唯一落点：无论是 Node zip、Python tar.gz，还是
/// 清单自身，都走这里。删文件放在错误分支里，所以调用方不必记得清理。
pub fn verify_downloaded_sha256(dest: &Path, expected: &str, max_bytes: u64) -> Result<String, String> {
    let actual = match sha256_hex_of(dest, max_bytes) {
        Ok(a) => a,
        Err(e) => {
            let _ = std::fs::remove_file(dest);
            return Err(e);
        }
    };
    let want = expected.trim().to_ascii_lowercase();
    if actual != want {
        let _ = std::fs::remove_file(dest);
        return Err(crate::i18n::fmt(
            "tool_dl_hash_mismatch",
            &[
                &dest.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                &want,
                &actual,
            ],
        ));
    }
    Ok(actual)
}

// ---------- 检测半边 ----------

/// 某工具装好的判据文件（存在 = 这份安装可尝试）。
fn managed_key_file(dir: &Path, id: ToolId) -> Option<PathBuf> {
    match id {
        ToolId::Node => {
            let p = dir.join("node.exe");
            p.is_file().then_some(p)
        }
        ToolId::Python => {
            // python-build-standalone 的 install_only 包解开后是 `python/python.exe`
            // 这一层嵌套；npm 风格的扁平布局也认（两种都试，取先命中的）。
            let nested = dir.join("python").join("python.exe");
            let flat = dir.join("python.exe");
            if nested.is_file() {
                Some(nested)
            } else if flat.is_file() {
                Some(flat)
            } else {
                None
            }
        }
        ToolId::Pnpm => ["pnpm.cmd", "pnpm.exe"]
            .iter()
            .map(|n| bin_dir_for(dir).join(n))
            .find(|p| p.is_file()),
        ToolId::Dsh => crate::detect::dsh_cmd_in_dir(&bin_dir_for(dir)),
        _ => None,
    }
}

/// 扫描受管目录里某工具的全部版本目录（上限 [`DIR_SCAN_MAX_ENTRIES`]），
/// 按版本号**数值降序**返回「判据文件确实存在」的 `(版本名, 目录)`。
/// 多版本并存是设计内的（方案 §2.1），取最大者为「当前」。
pub fn scan_managed_versions(root: &Path, id: ToolId) -> Vec<(String, PathBuf)> {
    let tools_dir = root.join("tools").join(id.as_str());
    let Ok(rd) = std::fs::read_dir(&tools_dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for e in rd.flatten().take(DIR_SCAN_MAX_ENTRIES) {
        let d = e.path();
        if !d.is_dir() {
            continue;
        }
        let Some(name) = d.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        // 目录名必须是版本号形状；`.stage-*` 之类的临时目录天然被排除
        if !is_safe_version_dir_name(&name) {
            continue;
        }
        if managed_key_file(&d, id).is_some() {
            out.push((name, d));
        }
    }
    out.sort_by(|a, b| {
        match (
            crate::detect::parse_version_numbers(&a.0),
            crate::detect::parse_version_numbers(&b.0),
        ) {
            (Some(x), Some(y)) => crate::detect::cmp_versions(&y, &x), // 降序
            _ => b.0.cmp(&a.0),
        }
    });
    out
}

/// 受管目录里检测某工具：取版本号最大的完整安装，并**试跑 `--version`**
/// （文件在 ≠ 能用；方案 §5A.4 第 7 条的试跑纪律）。
pub fn detect_managed(root: &Path, id: ToolId) -> Option<Installed> {
    let (version, dir) = scan_managed_versions(root, id).into_iter().next()?;
    let key = managed_key_file(&dir, id)?;
    let version = probe::run_probe_version(&key)
        .as_deref()
        .and_then(probe::first_version_token)
        .unwrap_or(version);
    Some(Installed { version, path: dir })
}

/// 系统里检测某受管工具（`use_system_tools` 复用判断用）。全部复用现有
/// `detect.rs` 的查找（它们已处理注册表 PATH 旧快照、WindowsApps 假 python 等）。
pub fn detect_system(id: ToolId) -> Option<Installed> {
    let path = match id {
        ToolId::Node => crate::detect::find_node_exe()?,
        ToolId::Pnpm => crate::detect::find_pnpm_cmd()?,
        ToolId::Dsh => crate::detect::find_dsh_cmd()?,
        ToolId::Python => {
            let py = crate::detect::find_python()?;
            let version = py
                .version()
                .as_deref()
                .and_then(probe::first_version_token)
                .unwrap_or_default();
            return Some(Installed { version, path: py.program });
        }
        _ => return None,
    };
    // 试跑读版本（有界探测；`.cmd` 经 command_for 走 cmd.exe，与 detect 同语义）
    let version = probe::run_probe_version(&path)
        .as_deref()
        .and_then(probe::first_version_token)
        .unwrap_or_default();
    Some(Installed { version, path })
}

/// 受管安装给 spawn 环境贡献的 PATH 前缀。系统安装不贡献（它本来就在 PATH 上，
/// 也不该由我们改变解析顺序 —— 方案 §0.5：不接管终端）。
pub fn managed_path_prefix(inst: &Installed, id: ToolId) -> Option<PathBuf> {
    match id {
        ToolId::Node => Some(inst.path.clone()),
        ToolId::Python => {
            // install_only 包的真实 bin 在 `<版本>\python\` 下
            let nested = inst.path.join("python");
            if nested.is_dir() {
                Some(nested)
            } else {
                Some(inst.path.clone())
            }
        }
        ToolId::Pnpm | ToolId::Dsh => Some(bin_dir_for(&inst.path)),
        _ => None,
    }
}

// ---------- 安装半边 ----------

/// ManagedTool 实现。`app` 仅供下载进度事件使用（None = 无进度，单测路径）；
/// 安装器核心不依赖它 —— 这是方案 §3.1「安装器内部不需要知道 AppHandle」的落点。
pub struct ManagedEntry {
    pub id: ToolId,
    pub app: Option<tauri::AppHandle>,
}

impl ManagedEntry {
    pub fn new(id: ToolId, app: Option<tauri::AppHandle>) -> Self {
        ManagedEntry { id, app }
    }
}

impl super::ManagedTool for ManagedEntry {
    fn detect_system(&self) -> Option<Installed> {
        detect_system(self.id)
    }

    fn detect_managed(&self, root: &Path) -> Option<Installed> {
        detect_managed(root, self.id)
    }

    fn env_overlay(&self, inst: &Installed) -> EnvOverlay {
        let mut ov = EnvOverlay::default();
        if let Some(dir) = managed_path_prefix(inst, self.id) {
            ov.path_dirs = vec![dir];
        }
        ov
    }

    fn install(&self, req: &InstallReq, prog: &dyn Progress) -> Result<Installed, String> {
        if !is_safe_version_dir_name(&req.version) {
            return Err(crate::i18n::fmt("tool_bad_version", &[&req.version]));
        }
        // 拒绝自动降级：与已装版本比较，更旧就拒（方案 §5A.4 第 5 条）
        let current = detect_managed(&req.root, self.id).map(|i| i.version);
        check_not_downgrade(self.id, current.as_deref(), &req.version)?;
        match self.id {
            ToolId::Node => install_node(self.app.as_ref(), req, prog),
            ToolId::Python => install_python(self.app.as_ref(), req, prog),
            ToolId::Pnpm => install_npm_pkg(self.app.as_ref(), req, prog, ToolId::Pnpm, "pnpm", "pnpm"),
            // dsh 的 npm 包名可由 config 的 `package_name` 覆盖（那是可配置字段）。
            // trait 方法拿不到 config，所以用默认包名；需要自定义包名的调用方走
            // [`install_dsh_with_pkg`]（mod.rs 的安装命令就是那么做的）。
            ToolId::Dsh => install_npm_pkg(
                self.app.as_ref(),
                req,
                prog,
                ToolId::Dsh,
                DEFAULT_DSH_PACKAGE,
                "dsh",
            ),
            _ => Err(format!("{} is not a managed tool", self.id)),
        }
    }

    fn uninstall(&self, inst: &Installed) -> Result<(), String> {
        // 卸载 = 删该工具的版本目录。受管目录是本程序的私有资产（方案 §2.1），
        // 不涉及系统共享组件；绝不触碰系统安装（方案 §6.2 的卸载分层）。
        std::fs::remove_dir_all(&inst.path)
            .map_err(|e| format!("remove {}: {e}", inst.path.display()))
    }
}

/// 用系统 `tar.exe` 解压（方案 §2.2：零新增 crate，一个命令同时认 zip 与 tar.gz）。
#[cfg(windows)]
fn tar_exe() -> PathBuf {
    // 系统目录从环境变量取，不写死盘符；取不到再退 System32 的默认位置
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    PathBuf::from(sysroot).join("System32").join("tar.exe")
}

fn extract_archive(archive: &Path, dest: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let tar = tar_exe();
        if !tar.is_file() {
            return Err(crate::i18n::t("tool_no_tar").to_string());
        }
        // 先列成员再校验（方案 §5A.4 第 6 条）：绝不先解开再检查
        let list_args: Vec<String> = vec![
            "-tf".to_string(),
            archive.to_string_lossy().to_string(),
        ];
        let (ok, out) = run_cmd_capture(
            &tar.to_string_lossy(),
            &list_args,
            "",
            std::time::Duration::from_secs(120),
        )?;
        if !ok {
            return Err(crate::i18n::fmt("tool_zip_list_fail", &[&out.trim().to_string()]));
        }
        let members: Vec<String> = out.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        validate_archive_members(&members)?;

        let args: Vec<String> = vec![
            "-xf".to_string(),
            archive.to_string_lossy().to_string(),
            "-C".to_string(),
            dest.to_string_lossy().to_string(),
        ];
        let (ok2, out2) = run_cmd_capture(
            &tar.to_string_lossy(),
            &args,
            "",
            std::time::Duration::from_secs(10 * 60),
        )?;
        if !ok2 {
            return Err(crate::i18n::fmt("tool_unzip_fail", &[&out2.trim().to_string()]));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        // v1 只实现 Windows；结构留口子（方案 §1.3）
        let _ = (archive, dest);
        Err("archive extraction is Windows-only in v1".to_string())
    }
}

/// 在解开的树里找一个符号链接 / 重解析点（有就拒绝）。
fn find_link_in_tree(root: &Path) -> Option<String> {
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            visited += 1;
            if visited > ARCHIVE_MAX_MEMBERS * 4 {
                return None; // 超量后不再深挖（已经在成员数上限里挡过，这里只是兜底）
            }
            let p = e.path();
            let Ok(md) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if md.file_type().is_symlink() {
                return Some(p.display().to_string());
            }
            if md.is_dir() {
                stack.push(p);
            }
        }
    }
    None
}

/// 下载一个文件：curl 优先（带进度）、PowerShell 兜底，然后校验哈希。
/// **每一步失败都删掉下载物**，绝不留下一个未校验的文件。
fn download_and_verify(
    app: Option<&tauri::AppHandle>,
    url: &str,
    dest: &Path,
    expected: &str,
    extra_allowed: &[&str],
    page: &str,
    prog: &dyn Progress,
) -> Result<String, String> {
    require_https(url, "download")?;
    let mut last_err = String::new();
    let via_curl = try_download_curl(url, dest, &mut last_err, app, page, extra_allowed);
    if !via_curl {
        prog.phase("download", &crate::i18n::fmt("tool_curl_fallback", &[&last_err]));
        try_download_powershell(url, dest, page, extra_allowed)?;
    }
    if let Some(a) = app {
        let done = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        emit_download_progress(a, done, None);
    }
    verify_downloaded_sha256(dest, expected, DOWNLOAD_MAX_BYTES)
}

/// Node：官方 zip 解压到 `tools\node\<ver>\`（方案 §1.1）。
fn install_node(app: Option<&tauri::AppHandle>, req: &InstallReq, prog: &dyn Progress) -> Result<Installed, String> {
    let tmp = create_private_temp_dir("tc-node")?;
    let result = (|| -> Result<Installed, String> {
        // 版本：配置给了精确版本就用它；没给则解析当前 LTS 线的最新补丁版
        let version = if req.version.trim().is_empty() {
            let mut e = String::new();
            resolve_latest_lts_version(&tmp, &mut e)
                .ok_or_else(|| crate::i18n::fmt("tool_version_resolve_fail", &[&"node", &e]))?
        } else {
            req.version.clone()
        };
        if !is_safe_version_dir_name(&version) {
            return Err(crate::i18n::fmt("tool_bad_version", &[&version]));
        }
        prog.phase("download", &crate::i18n::fmt("tool_node_resolved", &[&version]));

        // 文件名与 URL 由**同一个版本号**推导，不会分叉
        let file = node_zip_name_for(&version);
        let url = format!("{NODE_DIST_BASE}v{version}/{file}");
        let dest = tmp.join(&file);

        // 信任锚：官方 SHASUMS256.txt（文件可以从别处下，哈希只能从这里取）
        prog.phase("download", crate::i18n::t("tool_fetch_digest"));
        let expected = fetch_expected_sha256(&tmp, &version, &file)?;
        prog.phase("download", &crate::i18n::fmt("tool_dl_start", &[&version, &url]));
        download_and_verify(app, &url, &dest, &expected, &[], NODE_DIST_BASE, prog)?;
        prog.phase("verify", &crate::i18n::fmt("tool_hash_ok", &[&version]));

        // 解压 + 试跑 + 原子落位。官方 zip 里顶层是 `node-v<ver>-win-x64\`
        prog.phase("install", crate::i18n::t("tool_extracting"));
        let target = tool_dir(&req.root, ToolId::Node, &version);
        let top = format!("node-v{version}-win-x64");
        let key_rel = Path::new(&top).join("node.exe");
        stage_extract_wrapped(&tmp, &dest, &target, &key_rel, &version)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// Node 官方 zip 的文件名（一处来源，URL 与清单条目名都用它）。
pub fn node_zip_name_for(version: &str) -> String {
    format!("node-v{version}-win-x64.zip")
}

/// 归档里多一层顶层目录时的落位：先解到 staging，把顶层目录里的内容搬出来，
/// 再走 [`stage_extract_and_commit`] 的校验与原子 rename。
///
/// 为什么需要它：官方 Node zip 的成员全在 `node-v<ver>-win-x64/` 下，
/// 解压后多一层。这里用「解到临时树 → 定位顶层 → 整目录移入 staging」处理，
/// 而不是给 tar 传 `--strip-components`（bsdtar 的该选项在不同版本行为不完全一致，
/// 且我们要自己控制成员校验）。
fn stage_extract_wrapped(
    tmp: &Path,
    archive: &Path,
    target: &Path,
    key_rel: &Path,
    version: &str,
) -> Result<Installed, String> {
    let unpack = tmp.join("unpack");
    let _ = std::fs::remove_dir_all(&unpack);
    std::fs::create_dir_all(&unpack)
        .map_err(|e| crate::i18n::fmt("tool_mkdir_fail", &[&unpack.display().to_string(), &e.to_string()]))?;
    extract_archive(archive, &unpack)?;
    if let Some(link) = find_link_in_tree(&unpack) {
        return Err(crate::i18n::fmt("tool_zip_symlink", &[&link]));
    }
    // 单层包装：若 unpack 下只有一个目录且它里面有 key，就把那一层当内容根
    let content_root = {
        let entries: Vec<PathBuf> = std::fs::read_dir(&unpack)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        if entries.len() == 1 && entries[0].is_dir() && entries[0].join(key_rel).is_file() {
            entries[0].clone()
        } else {
            unpack.clone()
        }
    };
    // 重打包成一个不含包装层的 staging，再交给统一的校验/落位
    let parent = target
        .parent()
        .ok_or_else(|| crate::i18n::fmt("tool_bad_target", &[&target.display().to_string()]))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| crate::i18n::fmt("tool_mkdir_fail", &[&parent.display().to_string(), &e.to_string()]))?;
    let stage = parent.join(format!(
        ".stage-{}",
        target.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    ));
    let _ = std::fs::remove_dir_all(&stage);
    move_dir_contents(&content_root, &stage)?;

    // 试跑 + 原子落位（与纯解压那条共用一个收尾函数）
    let key = stage.join(key_rel);
    if !key.is_file() {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(crate::i18n::fmt(
            "tool_key_missing",
            &[&key_rel.display().to_string(), &stage.display().to_string()],
        ));
    }
    match probe::run_probe_version(&key) {
        Some(v) => {
            if let Some(got) = probe::first_version_token(&v) {
                if let (Some(g), Some(w)) = (
                    crate::detect::parse_version_numbers(&got),
                    crate::detect::parse_version_numbers(version),
                ) {
                    if crate::detect::cmp_versions(&g, &w) != std::cmp::Ordering::Equal {
                        let _ = std::fs::remove_dir_all(&stage);
                        return Err(crate::i18n::fmt("tool_version_mismatch", &[&version, &got]));
                    }
                }
            }
        }
        None => {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(crate::i18n::fmt("tool_selftest_fail", &[&key.display().to_string()]));
        }
    }
    if target.exists() {
        let _ = std::fs::remove_dir_all(target);
    }
    std::fs::rename(&stage, target).map_err(|e| {
        let _ = std::fs::remove_dir_all(&stage);
        crate::i18n::fmt(
            "tool_commit_fail",
            &[&stage.display().to_string(), &target.display().to_string(), &e.to_string()],
        )
    })?;
    Ok(Installed { version: version.to_string(), path: target.to_path_buf() })
}

/// 把 `src` 下的内容（不含 src 本身这一层）搬进 `dst`。
fn move_dir_contents(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst)
        .map_err(|e| crate::i18n::fmt("tool_mkdir_fail", &[&dst.display().to_string(), &e.to_string()]))?;
    let rd = std::fs::read_dir(src)
        .map_err(|e| crate::i18n::fmt("tool_mkdir_fail", &[&src.display().to_string(), &e.to_string()]))?;
    for e in rd.flatten() {
        let from = e.path();
        let Some(name) = from.file_name() else { continue };
        let to = dst.join(name);
        // 同卷 rename 最快；跨卷（极少见）退回复制
        if std::fs::rename(&from, &to).is_err() {
            if from.is_dir() {
                move_dir_contents(&from, &to)?;
            } else {
                std::fs::copy(&from, &to).map_err(|e| {
                    crate::i18n::fmt(
                        "tool_commit_fail",
                        &[&from.display().to_string(), &to.display().to_string(), &e.to_string()],
                    )
                })?;
            }
        }
    }
    Ok(())
}

// ---------- Python（python-build-standalone） ----------

/// 从 releases JSON 里挑出目标版本的 asset：`(asset 名, 下载 URL, sha256, 版本)`。
///
/// 纯函数（输入是 API 返回的 JSON 文本），单测用构造的 JSON 覆盖：
/// 找不到 → Err；JSON 非法 → Err；digest 缺失 → Err（**绝不在没有哈希时安装**）。
pub fn pick_python_asset(
    releases_json: &str,
    py_version: &str,
) -> Result<(String, String, String, String), String> {
    let v: serde_json::Value = serde_json::from_str(releases_json)
        .map_err(|e| crate::i18n::fmt("tool_manifest_invalid", &[&e.to_string()]))?;
    let arr = v
        .as_array()
        .ok_or_else(|| crate::i18n::t("tool_manifest_not_array").to_string())?;
    // 目标资产名形如 cpython-<ver>+<tag>-x86_64-pc-windows-msvc-install_only.tar.gz
    let want_prefix = format!("cpython-{py_version}+");
    let want_suffix = "-x86_64-pc-windows-msvc-install_only.tar.gz";
    for rel in arr {
        let Some(assets) = rel.get("assets").and_then(|a| a.as_array()) else {
            continue;
        };
        for a in assets {
            let Some(name) = a.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            if !name.starts_with(&want_prefix) || !name.ends_with(want_suffix) {
                continue;
            }
            // 版本必须**精确**等于目标。`want_prefix` 自带结尾的 `+`，所以
            // `3.11.1` 不会命中 `3.11.18`（"3.11.1" 后面是 "8" 而不是 "+"）——
            // 这个前缀本身就完成了精确匹配，不需要再切一遍 `+`。
            let middle = &name[want_prefix.len()..name.len() - want_suffix.len()];
            if middle.is_empty() || middle.contains('-') {
                continue; // 中间那段是构建标签（如 20261110），不能含分隔符
            }
            let url = a
                .get("browser_download_url")
                .and_then(|u| u.as_str())
                .ok_or_else(|| crate::i18n::t("tool_manifest_no_url").to_string())?;
            let digest = a
                .get("digest")
                .and_then(|d| d.as_str())
                .ok_or_else(|| crate::i18n::fmt("tool_manifest_no_digest", &[&name]))?;
            let Some(hex) = digest.strip_prefix("sha256:") else {
                return Err(crate::i18n::fmt("tool_manifest_bad_digest", &[&digest.to_string()]));
            };
            if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(crate::i18n::fmt("tool_manifest_bad_digest", &[&digest.to_string()]));
            }
            return Ok((
                name.to_string(),
                url.to_string(),
                hex.to_ascii_lowercase(),
                py_version.to_string(),
            ));
        }
    }
    Err(crate::i18n::fmt("tool_python_asset_missing", &[&py_version]))
}

/// Python：python-build-standalone 的 `install_only` tarball 解压到 `tools\python\<ver>\`。
///
/// 哈希来源 = GitHub Releases 的 asset digest（**同一信任根**：官方仓库仍是共同
/// 信任根，这套机制防镜像投毒与传输损坏，不防上游账号被攻陷 —— 方案 §5A.9）。
fn install_python(app: Option<&tauri::AppHandle>, req: &InstallReq, prog: &dyn Progress) -> Result<Installed, String> {
    let tmp = create_private_temp_dir("tc-py")?;
    let result = (|| -> Result<Installed, String> {
        let version = req.version.trim().to_string();
        if !is_safe_version_dir_name(&version) {
            return Err(crate::i18n::fmt("tool_bad_version", &[&version]));
        }
        // ① 清单（常量 URL；只允许 file:// 绝对路径的本地覆盖，远程一律忽略）
        let api = pybs_releases_api_url();
        prog.phase("download", &crate::i18n::fmt("tool_manifest_fetch", &[&api]));
        let manifest_dest = tmp.join("pybs-releases.json");
        let extra = pybs_allowed_prefixes();
        let mut err = String::new();
        require_https(&api, "manifest")?;
        if !try_download_curl(&api, &manifest_dest, &mut err, None, &api, &extra) {
            try_download_powershell(&api, &manifest_dest, &api, &extra)?;
        }
        let text = std::fs::read_to_string(&manifest_dest).map_err(|e| {
            let _ = std::fs::remove_file(&manifest_dest);
            format!("read manifest: {e}")
        })?;
        let _ = std::fs::remove_file(&manifest_dest);

        // ② 选资产 + 取 digest（没有 digest 就不装）
        let (name, url, sha, _ver) = pick_python_asset(&text, &version)?;
        prog.phase("download", &crate::i18n::fmt("tool_python_resolved", &[&name]));

        // ③ 下载 + 校验
        let dest = tmp.join(&name);
        let page = pybs_download_base();
        download_and_verify(app, &url, &dest, &sha, &extra, &page, prog)?;
        prog.phase("verify", &crate::i18n::fmt("tool_hash_ok", &[&version]));

        // ④ 解压 + 试跑 + 原子落位。install_only 包的顶层是 `python\`
        prog.phase("install", crate::i18n::t("tool_extracting"));
        let target = tool_dir(&req.root, ToolId::Python, &version);
        let key_rel = Path::new("python").join("python.exe");
        stage_extract_wrapped(&tmp, &dest, &target, &key_rel, &version)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

// ---------- pnpm / dsh（走 npm --prefix） ----------

/// `npm install -g <spec> --prefix <目录>`，并核对 shim 真的落在 `<prefix>` 里。
///
/// 与现有 `install_pnpm_blocking` 的区别只有一个：**带精确版本与受管 prefix**
/// （方案 §1.1 的表 / §5.2 的可复现要求）。安装完必须核对 —— npm 返回 0 不等于
/// 东西落到了我们指定的目录（`--prefix` 被环境变量覆盖时它会装到别处）。
fn install_npm_pkg(
    app: Option<&tauri::AppHandle>,
    req: &InstallReq,
    prog: &dyn Progress,
    id: ToolId,
    pkg: &str,
    shim: &str,
) -> Result<Installed, String> {
    let version = req.version.trim().to_string();
    if !is_safe_version_dir_name(&version) {
        return Err(crate::i18n::fmt("tool_bad_version", &[&version]));
    }
    let Some(npm) = crate::detect::find_npm_cmd() else {
        return Err(crate::i18n::t("tool_no_npm").to_string());
    };
    // 落点：`tools\<tool>\<版本>\`，npm 把 shim 写进 bin_dir_for(该目录)
    let target = tool_dir(&req.root, id, &version);
    std::fs::create_dir_all(&target).map_err(|e| {
        crate::i18n::fmt("tool_mkdir_fail", &[&target.display().to_string(), &e.to_string()])
    })?;

    let spec = format!("{pkg}@{version}");
    let args: Vec<String> = vec![
        "install".to_string(),
        "-g".to_string(),
        format!("--prefix={}", target.display()),
        spec.clone(),
    ];
    prog.phase("install", &crate::i18n::fmt("tool_npm_install", &[&spec]));

    let npm_s = npm.to_string_lossy().to_string();
    let mut cmd = command_for(&npm_s, &args)?;
    // 全局目录钉死在受管目录：环境变量优先级高于用户配置与项目级 .npmrc，
    // 所以即便本机有别的覆盖，这次安装也一定落在受管目录里
    cmd.env("npm_config_prefix", target.to_string_lossy().to_string());
    cmd.env("PATH", crate::detect::child_path_for(&[npm_s.as_str()]));
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::process::apply_no_window(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| crate::i18n::fmt("tool_npm_spawn_fail", &[&e.to_string()]))?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                if !st.success() {
                    let _ = std::fs::remove_dir_all(&target); // 失败不留半成品
                    return Err(crate::i18n::fmt("tool_npm_fail", &[&spec, &st.code().unwrap_or(-1)]));
                }
                break;
            }
            Ok(None) => {
                if started.elapsed() >= std::time::Duration::from_secs(15 * 60) {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_dir_all(&target);
                    return Err(crate::i18n::fmt("tool_npm_timeout", &[&spec]));
                }
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&target);
                return Err(crate::i18n::fmt("tool_npm_spawn_fail", &[&e.to_string()]));
            }
        }
    }

    // 核对落点：shim 必须真在 bin_dir_for(target) 里（npm 的退出码看不出这件事）
    let bin = bin_dir_for(&target);
    let shim_path = [format!("{shim}.cmd"), format!("{shim}.exe")]
        .iter()
        .map(|n| bin.join(n))
        .find(|p| p.is_file());
    let Some(shim_path) = shim_path else {
        let _ = std::fs::remove_dir_all(&target);
        return Err(crate::i18n::fmt(
            "tool_npm_mismatch",
            &[&spec, &bin.display().to_string()],
        ));
    };

    // 试跑（`.cmd` 经 cmd.exe，与 detect 同一条路；有字节上限）
    let got = probe::run_probe_version(&shim_path)
        .as_deref()
        .and_then(probe::first_version_token)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| version.clone());

    if let Some(a) = app {
        prog.phase("verify", &crate::i18n::fmt("tool_installed_at", &[&spec, &shim_path.display().to_string()]));
        let _ = a;
    }
    Ok(Installed { version: got, path: target })
}

/// 装 dsh 到受管目录，**包名由调用方给**（来自 `cfg.package_name`，可配置字段）。
///
/// 这是 `ManagedTool::install` 之上的一层薄包装：trait 方法拿不到 config，
/// 而包名恰恰必须来自 config（方案 §1.1b 要点 1：包名写死会挡住自定义包名的场景）。
pub fn install_dsh_with_pkg(
    app: Option<&tauri::AppHandle>,
    req: &InstallReq,
    pkg: &str,
    prog: &dyn Progress,
) -> Result<Installed, String> {
    if !is_safe_version_dir_name(&req.version) {
        return Err(crate::i18n::fmt("tool_bad_version", &[&req.version]));
    }
    let current = detect_managed(&req.root, ToolId::Dsh).map(|i| i.version);
    check_not_downgrade(ToolId::Dsh, current.as_deref(), &req.version)?;
    let name = {
        let n = pkg.trim();
        if n.is_empty() { DEFAULT_DSH_PACKAGE } else { n }
    };
    install_npm_pkg(app, req, prog, ToolId::Dsh, name, "dsh")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsh-managed-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---------- 目录布局 ----------

    /// bin_dir_for：Windows 上 shim 直接在 prefix 下（方案 §1.3 的单点收口）。
    #[test]
    #[cfg(windows)]
    fn bin_dir_is_prefix_on_windows() {
        let p = Path::new(r"D:\toolchain\tools\dsh\0.2.0");
        assert_eq!(bin_dir_for(p), PathBuf::from(p));
        assert_eq!(bin_dir_for(p).join("dsh.cmd"), p.join("dsh.cmd"));
    }

    /// tool_dir：`<root>\tools\<id>\<version>\`。
    #[test]
    fn tool_dir_layout() {
        let root = Path::new(r"D:\tc");
        assert_eq!(
            tool_dir(root, ToolId::Node, "24.22.0"),
            PathBuf::from(r"D:\tc\tools\node\24.22.0")
        );
    }

    // ---------- 下载安全（判据：五种输入都被拒） ----------

    /// 非 HTTPS 被拒（判据：非 HTTPS base 被拒）。
    #[test]
    fn non_https_download_url_is_rejected() {
        assert!(require_https("http://example.com/x.zip", "download").is_err());
        assert!(require_https("ftp://example.com/x.zip", "download").is_err());
        assert!(require_https("https://example.com/x.zip", "download").is_ok());
        // 大小写敏感地按字面判：`HTTPS://` 不是我们认识的形式（宁可拒，不放宽）
        assert!(require_https("HTTPS://example.com/x", "d").is_err());
        // 相对 / 空 / 无 scheme
        assert!(require_https("example.com/x", "d").is_err());
        assert!(require_https("", "d").is_err());
        // 前缀伪装：http 里嵌 https 不算
        assert!(require_https("http://https://x", "d").is_err());
    }

    /// 清单覆盖入口只在 `file://` 且绝对路径时生效，其余一律回退常量
    /// （方案 §5A.4 第 2 条：远程环境变量不能把信任锚指走）。
    #[test]
    fn manifest_override_only_honours_absolute_file_urls() {
        let fallback = "https://official.example/manifest.json";
        // 远程覆盖：忽略（这正是防线本身）
        assert_eq!(
            manifest_override_for(Some("https://evil.example/m.json"), fallback),
            fallback
        );
        assert_eq!(manifest_override_for(Some("http://evil/m.json"), fallback), fallback);
        // 相对 file://：忽略
        assert_eq!(manifest_override_for(Some("file://relative/m.json"), fallback), fallback);
        // 绝对 file://：生效
        #[cfg(windows)]
        let ok = "file://C:/tmp/m.json";
        #[cfg(not(windows))]
        let ok = "file:///tmp/m.json";
        assert_eq!(manifest_override_for(Some(ok), fallback), ok);
        // 空 / None：回退
        assert_eq!(manifest_override_for(None, fallback), fallback);
        assert_eq!(manifest_override_for(Some("   "), fallback), fallback);
    }

    /// 归档成员校验：`..`、绝对路径、盘符、重复项、超量全部被拒（判据：含 `..` 被拒）。
    #[test]
    fn archive_member_validation_rejects_traversal_absolute_and_dupes() {
        // 正常成员
        assert!(validate_archive_members(&[
            "node-v24.22.0-win-x64/".to_string(),
            "node-v24.22.0-win-x64/node.exe".to_string(),
            "node-v24.22.0-win-x64/npm.cmd".to_string(),
        ])
        .is_ok());

        // `..`（三种写法）
        for bad in ["../evil", "a/../../b", "..\\evil", "a\\..\\..\\b"] {
            assert!(
                validate_archive_members(&[bad.to_string()]).is_err(),
                "{bad} 应被拒"
            );
        }
        // 绝对路径 / UNC / 盘符
        for bad in ["/etc/passwd", "\\\\server\\share\\x", r"C:\Windows\x", "D:evil"] {
            assert!(
                validate_archive_members(&[bad.to_string()]).is_err(),
                "{bad} 应被拒"
            );
        }
        // 重复项（大小写 / 分隔符差异也算重复）
        assert!(validate_archive_members(&[
            "a/b".to_string(),
            "A\\B".to_string(),
        ])
        .is_err());
        // 空 / 超量
        assert!(validate_archive_members(&[]).is_err());
        let many: Vec<String> = (0..ARCHIVE_MAX_MEMBERS + 1).map(|i| format!("f{i}")).collect();
        assert!(validate_archive_members(&many).is_err());
        // `./` 前缀是合法写法（不算穿越）
        assert!(validate_archive_members(&["./a/b".to_string()]).is_ok());
    }

    /// 降级被拒（判据：降级被拒）：要装的比已装的旧就报错；同版/升级都放行。
    #[test]
    fn downgrade_is_rejected() {
        // 已装 24.22.0 → 要装 24.9.0：更旧，拒
        assert!(check_not_downgrade(ToolId::Node, Some("24.22.0"), "24.9.0").is_err());
        // 升级：放行
        assert!(check_not_downgrade(ToolId::Node, Some("22.19.0"), "24.22.0").is_ok());
        // 同版（含 v 前缀差异）：放行（重装同一版本是合法操作）
        assert!(check_not_downgrade(ToolId::Node, Some("v24.22.0"), "24.22.0").is_ok());
        // 没装过：放行
        assert!(check_not_downgrade(ToolId::Node, None, "24.22.0").is_ok());
        // 已装版本读不出：不拦（unknown 不判过高过低）
        assert!(check_not_downgrade(ToolId::Node, Some(""), "24.22.0").is_ok());
        assert!(check_not_downgrade(ToolId::Node, Some("junk"), "24.22.0").is_ok());
    }

    /// 哈希不符：删掉下载物并返回 Err —— **不留残留**（判据：哈希不符不留残留目录/文件）。
    #[test]
    fn hash_mismatch_removes_the_file_and_errors() {
        let root = temp_root("hash");
        let f = root.join("payload.bin");
        std::fs::write(&f, b"the actual content").unwrap();
        let wrong = "0".repeat(64);
        let err = verify_downloaded_sha256(&f, &wrong, DOWNLOAD_MAX_BYTES)
            .expect_err("哈希不符必须报错");
        assert!(!err.trim().is_empty());
        assert!(!f.exists(), "哈希不符的文件必须被删掉（不留残留）");

        // 哈希正确：保留文件并返回实际值
        std::fs::write(&f, b"abc").unwrap();
        let good = crate::process::sha256_hex(b"abc");
        let got = verify_downloaded_sha256(&f, &good, DOWNLOAD_MAX_BYTES).expect("哈希正确应通过");
        assert_eq!(got, good);
        assert!(f.exists(), "校验通过的文件应保留");
        // 大小写不敏感：清单里可能是大写
        let upper = good.to_ascii_uppercase();
        assert!(verify_downloaded_sha256(&f, &upper, DOWNLOAD_MAX_BYTES).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 超限文件：读入前就拦下（同时删掉），不会把内存打满。
    #[test]
    fn oversized_download_is_rejected_and_removed() {
        let root = temp_root("oversize");
        let f = root.join("big.bin");
        std::fs::write(&f, vec![0u8; 2048]).unwrap();
        let err = verify_downloaded_sha256(&f, &"a".repeat(64), 1024).expect_err("超限应拒");
        assert!(err.contains("1024") || !err.is_empty());
        assert!(!f.exists(), "超限文件应被删除");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---------- 清单 schema（判据：清单格式非法时拒绝） ----------

    fn releases_json(assets: &str) -> String {
        format!(r#"[{{"tag_name":"20261110","assets":[{assets}]}}]"#)
    }

    /// 正常资产：选中并取出 digest + URL。
    #[test]
    fn pick_python_asset_finds_matching_asset() {
        let digest = "a".repeat(64);
        let j = releases_json(&format!(
            r#"{{"name":"cpython-3.14.7+20261110-x86_64-pc-windows-msvc-install_only.tar.gz","browser_download_url":"https://github.com/astral-sh/python-build-standalone/releases/download/20261110/x.tar.gz","digest":"sha256:{digest}"}}"#
        ));
        let (name, url, sha, ver) = pick_python_asset(&j, "3.14.7").expect("应选中");
        assert!(name.starts_with("cpython-3.14.7+"));
        assert!(url.starts_with("https://github.com/"));
        assert_eq!(sha, digest);
        assert_eq!(ver, "3.14.7");
    }

    /// 清单格式非法时一律拒绝（判据：清单格式非法时拒绝）：
    /// 非 JSON、非数组、**没有 digest**、digest 不是 sha256:64hex、找不到目标版本。
    #[test]
    fn pick_python_asset_rejects_invalid_manifests() {
        // 非 JSON
        assert!(pick_python_asset("{ not json", "3.14.7").is_err());
        // 非数组
        assert!(pick_python_asset(r#"{"assets":[]}"#, "3.14.7").is_err());
        // 没有 digest：绝不在没有哈希时安装
        let no_digest = releases_json(
            r#"{"name":"cpython-3.14.7+20261110-x86_64-pc-windows-msvc-install_only.tar.gz","browser_download_url":"https://github.com/x"}"#,
        );
        assert!(pick_python_asset(&no_digest, "3.14.7").is_err(), "缺 digest 必须拒");
        // digest 格式不对（不是 sha256: + 64 hex）
        for bad in ["md5:abc", "sha256:xyz", "sha256:abc"] {
            let j = releases_json(&format!(
                r#"{{"name":"cpython-3.14.7+20261110-x86_64-pc-windows-msvc-install_only.tar.gz","browser_download_url":"https://github.com/x","digest":"{bad}"}}"#
            ));
            assert!(pick_python_asset(&j, "3.14.7").is_err(), "{bad} 应被拒");
        }
        // 目标版本不存在
        let other = releases_json(&format!(
            r#"{{"name":"cpython-3.11.18+20261110-x86_64-pc-windows-msvc-install_only.tar.gz","browser_download_url":"https://github.com/x","digest":"sha256:{}"}}"#,
            "b".repeat(64)
        ));
        assert!(pick_python_asset(&other, "3.14.7").is_err());
        // 版本前缀陷阱：3.11.1 不能匹配 3.11.18
        assert!(pick_python_asset(&other, "3.11.1").is_err(), "3.11.1 不应匹配 3.11.18");
    }

    /// pybs 白名单前缀必须真在共享守卫接受的范围里（防"白名单写错但没人发现"）。
    #[test]
    fn pybs_prefixes_match_the_constants() {
        let v = pybs_allowed_prefixes();
        assert!(v.iter().all(|p| p.starts_with("https://")));
        let dl = pybs_download_base();
        assert!(v.iter().any(|p| dl.starts_with(p) || *p == dl), "下载根应在白名单内：{dl}");
        let api = pybs_releases_api_url();
        assert!(v.iter().any(|p| api.starts_with(p)), "清单 URL 应在白名单内：{api}");
        assert!(dl.starts_with("https://"));
    }

    // ---------- 版本目录与检测 ----------

    /// 版本目录名白名单：挡 `..`、空段、路径片段。
    #[test]
    fn version_dir_name_whitelist() {
        assert!(is_safe_version_dir_name("24.22.0"));
        assert!(is_safe_version_dir_name("3.14.7"));
        for bad in ["..", "24..0", "latest", "v24.22.0", "24.22.0/../x", ""] {
            assert!(!is_safe_version_dir_name(bad), "{bad} 不应算合法版本名");
        }
    }

    /// 多版本并存取数值降序最大者；非版本书形状的目录（含 `.stage-*`）一律不算安装。
    #[test]
    fn scan_managed_versions_sorts_desc_and_skips_staging() {
        let root = temp_root("scan");
        let node_tools = root.join("tools").join("node");
        for v in ["24.9.0", "24.22.0", "9.18.0"] {
            std::fs::create_dir_all(node_tools.join(v)).unwrap();
            std::fs::write(node_tools.join(v).join("node.exe"), b"x").unwrap();
        }
        // staging 与非法名：即便里面有 node.exe 也不算安装
        for bad in [".stage-24.23.0", "..", "latest"] {
            std::fs::create_dir_all(node_tools.join(bad)).unwrap();
            std::fs::write(node_tools.join(bad).join("node.exe"), b"x").unwrap();
        }
        let got = scan_managed_versions(&root, ToolId::Node);
        let names: Vec<&str> = got.iter().map(|(v, _)| v.as_str()).collect();
        assert_eq!(names, vec!["24.22.0", "24.9.0", "9.18.0"], "数值降序且排除 staging");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 空壳目录（目录在、关键文件缺）不算安装；dsh 的 shim 要在 bin_dir_for 下找。
    #[test]
    fn detect_managed_requires_key_file() {
        let root = temp_root("keyfile");
        std::fs::create_dir_all(root.join("tools").join("node").join("24.22.0")).unwrap();
        assert_eq!(detect_managed(&root, ToolId::Node), None);

        let dsh_dir = root.join("tools").join("dsh").join("0.2.0");
        std::fs::create_dir_all(&dsh_dir).unwrap();
        std::fs::write(dsh_dir.join("dsh.cmd"), b"@echo off\r\n").unwrap();
        let got = detect_managed(&root, ToolId::Dsh);
        assert!(got.is_some(), "dsh.cmd 存在应判安装");
        assert_eq!(got.unwrap().path, dsh_dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Python 的 install_only 包是 `python\python.exe` 嵌套布局：
    /// 判据文件与 PATH 前缀都要认这一层（否则装好了却"检测不到"）。
    #[test]
    fn python_nested_layout_is_detected() {
        let root = temp_root("pynest");
        let dir = root.join("tools").join("python").join("3.14.7");
        std::fs::create_dir_all(dir.join("python")).unwrap();
        std::fs::write(dir.join("python").join("python.exe"), b"x").unwrap();
        assert!(managed_key_file(&dir, ToolId::Python).is_some(), "嵌套布局应认得 python.exe");
        let inst = Installed { version: "3.14.7".into(), path: dir.clone() };
        assert_eq!(
            managed_path_prefix(&inst, ToolId::Python),
            Some(dir.join("python")),
            "PATH 前缀应指向真实 bin 目录"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}