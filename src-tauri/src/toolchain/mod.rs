//! 工具链模块：node / pnpm / dsh / python 四样**受管工具**的安装与卸载，
//! rust / MSVC / WebView2 三样**系统工具**的探测。
//!
//! 设计依据：`plans/toolchain-module-plan.md`（下称「方案」）。三条主线纪律：
//! - **编排而非重写**：下载 / 校验 / 安装的现成实现尽量复用，本模块只做统一门面；
//! - **同步代码**：本仓库 22 处后台任务全是 `std::thread::spawn` + 阻塞 IO，
//!   trait 方法全部同步，Tauri command 层负责 spawn 线程；
//! - **不新增依赖**：下载用系统 curl，解压用 `System32\tar.exe`，哈希用手写实现。
//!
//! 模块边界（方案 §0.5 / §1.4）：不做 PATH 接管、不做 shim 转发、不做版本管理器；
//! OnlyOffice 等需提权的系统服务归 `office.rs`，本模块不重复探测。

pub mod env;
pub mod managed;
pub mod probe;
pub mod registry;
pub mod resolve;
pub mod store;

pub use env::EnvOverlay;
pub use registry::{Scope, ToolId, REGISTRY};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tauri::Emitter;

use probe::{MsvcProbe, RustProbe, Webview2Probe};

// ---------- 共享类型（方案 §3.1） ----------

/// 受管工具的一处安装记录：版本 + 落盘目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    pub path: PathBuf,
}

/// 受管安装请求：装哪个**精确版本**、受管根目录在哪。
///
/// 版本必须是精确版本号（方案 §5.2：不能把 `latest` 这类 tag 直接传给 npm），
/// 由调用方先解析成具体版本再进来 —— 可复现是硬要求。
#[derive(Debug, Clone)]
pub struct InstallReq {
    pub version: String,
    /// 受管根目录（`...\toolchain\`），工具自己的版本目录由实现决定
    /// （`tools\node\<ver>\` 等，方案 §2.1）。
    pub root: PathBuf,
}

/// 系统工具探测请求。`min_version` 来自 `toolchain.json` 的 `system` 段；
/// None = 只判「存在 / 可用」，不做下限判断（默认配置，见方案 §2.3 的说明）。
/// `state_root` 是受管根目录（state.json 所在），探测用它存取缓存；
/// 留空 = 不读写缓存（纯探测）。
#[derive(Debug, Clone, Default)]
pub struct ProbeReq {
    pub min_version: Option<String>,
    pub state_root: PathBuf,
}

/// 系统工具探测结果。
///
/// 为什么 `Broken` 是独立状态而不是折叠进 `Missing`（方案 §3.2 / §5A.2）：
/// 「装了但坏了」与「没装」给用户的建议完全不同 —— 前者该去修 VS 安装，
/// 后者才是装新的。rustdsh 刻意折叠两者，我们不学这一条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    /// 探测到且可用（携带版本串；MSVC 携带的是 MSVC 工具集版本）。
    Ok(String),
    /// 版本低于配置下限（仅对声明了 min 的探测有意义）。
    TooOld { current: String, min: String },
    /// 探测到但不可用。detail 必须说清**哪个环节**坏了（如哪个版本目录不完整），
    /// 不能给一句笼统的「未检测到」。
    Broken(String),
    /// 没有探测到。
    Missing,
}

impl ProbeResult {
    /// 探测到了吗（Ok / TooOld / Broken 都算「找到了东西」）。
    ///
    /// 这三个辅助方法是 `ProbeResult` 的**语义契约**（方案 §5A.2：「探测到」与
    /// 「能用」是两个字段，消费方据此分流）。当前命令层直接 match 变体，所以暂时
    /// 没有内部调用点；留着是为了让「found ≠ usable」这件事有一个可读的名字，
    /// 而不是每个消费方各写一遍 `!matches!(.., Missing)`。
    #[allow(dead_code)]
    pub fn found(&self) -> bool {
        !matches!(self, ProbeResult::Missing)
    }

    /// 可用吗（只有 Ok）。
    #[allow(dead_code)]
    pub fn usable(&self) -> bool {
        matches!(self, ProbeResult::Ok(_))
    }

    /// 版本串（Ok / TooOld 有，Broken / Missing 无）。
    #[allow(dead_code)]
    pub fn version(&self) -> Option<&str> {
        match self {
            ProbeResult::Ok(v) => Some(v),
            ProbeResult::TooOld { current, .. } => Some(current),
            _ => None,
        }
    }
}

/// `SystemProbe::env_overlay` 的输入：探测到的原始事实。
///
/// 刻意宽泛（`extra` 不解读）：只有 MSVC 覆盖这个方法，它的实现要拿安装根与
/// 挑中的工具集版本；用键值对传递避免把 MSVC 专属概念漏进共享类型。
///
/// `version` / `detail` 目前没有消费方（MSVC 的实现只需要 `state_root`），
/// 留着是因为它们描述的是「探测到了什么」这件事本身，将来别的探测覆盖
/// `env_overlay` 时会用到；删掉会让这个入参退化成只有一个键的字典。
#[derive(Debug, Clone, Default)]
pub struct ProbeFound {
    #[allow(dead_code)]
    pub version: Option<String>,
    /// Broken 时的具体原因。
    #[allow(dead_code)]
    pub detail: Option<String>,
    pub extra: std::collections::BTreeMap<String, String>,
}

/// 安装进度出口：把现有 `setup_progress(app, phase, msg)` 事件包一层，
/// 安装器内部不需要知道 `AppHandle`（方案 §3.1）。
pub trait Progress: Send + Sync {
    fn phase(&self, phase: &str, msg: &str);
}

// ---------- 两个 trait（方案 §3.1，同步版） ----------

/// 受管工具：能装、能卸、能给环境。
///
/// 为什么拆成两个 trait 而不是一个带 `scope` 判断的：给 Rust 调 `install()`
/// 应该编译不过，而不是运行时返回「不支持」。
///
/// `detect_system` / `detect_managed` / `env_overlay` 当前由 `resolve()` 通过
/// free function 直接调用（那条路径每次都要跑，绕开 trait object 省一次分发）。
/// 它们仍是契约的一部分：命令层按 trait 编程时（`install` / `uninstall` 就在用）
/// 不该被迫知道"哪个工具用哪个 free function"。
pub trait ManagedTool: Send + Sync {
    /// 系统里有没有现成的（`use_system_tools` 复用判断用）。版本读不出返回 None ——
    /// 「有文件但读不出版本」交给调用方按 unknown 处理，不在这里猜。
    #[allow(dead_code)]
    fn detect_system(&self) -> Option<Installed>;
    /// 受管目录里有没有装好的（目录存在 + 试跑通过才算）。
    #[allow(dead_code)]
    fn detect_managed(&self, root: &Path) -> Option<Installed>;
    /// 安装。失败时**不留半成品**（临时目录自己清理）；成功返回落盘位置。
    fn install(&self, req: &InstallReq, prog: &dyn Progress) -> Result<Installed, String>;
    /// 卸载受管目录内的这份安装（删目录）。绝不触碰系统安装。
    fn uninstall(&self, inst: &Installed) -> Result<(), String>;
    /// 这个安装给 spawn 环境贡献什么（PATH 前缀等）。
    #[allow(dead_code)]
    fn env_overlay(&self, inst: &Installed) -> EnvOverlay;
}

/// 系统工具：只探测，无 install/uninstall。
pub trait SystemProbe: Send + Sync {
    fn probe(&self, req: &ProbeReq) -> ProbeResult;
    /// 缺失时给用户的官方安装指引深链（toolchain_open_guide 用）。
    fn guide_url(&self) -> &'static str;
    /// 系统工具也可能需要注入环境。默认空实现，目前只有 MSVC 覆盖它
    /// （原因见方案 §3.2：装了 VS 不等于 link.exe 在 PATH 里）。
    fn env_overlay(&self, found: &ProbeFound) -> EnvOverlay {
        let _ = found;
        EnvOverlay::default()
    }
}

// ---------- 门面：配置 / spawn 环境 / 状态报告（方案 §2.3 / §4.2 / §7） ----------

/// `toolchain.json`（与 config.json 同级，方案 §2.3）。
///
/// 刻意薄：`managed` 只存「要装的精确版本」（§5.2 的可复现要求），`rust_min`
/// 是探测下限（None = 只判存在）。**不在这里存任何"当前 latest 是什么"的快照**
/// —— 上游一变它就过期，那类值一律运行时解析（方案 §1.1b）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ToolchainConfig {
    /// tool id → 要安装/使用的精确版本（如 "24.22.0"）。
    pub managed: BTreeMap<String, String>,
    /// `system.rust.min`：Rust 版本下限（None = 只判存在）。
    pub rust_min: Option<String>,
    /// 受管工具若在系统里已装且版本精确匹配则复用，不重复下载（默认 true）。
    /// 复用前必过下限校验（方案 §2.3）。
    pub use_system_tools: bool,
}

impl Default for ToolchainConfig {
    fn default() -> Self {
        ToolchainConfig {
            managed: BTreeMap::new(),
            rust_min: None,
            use_system_tools: true,
        }
    }
}

/// `toolchain.json` 的路径（`app_config_dir()\toolchain.json`，方案 §2.3）。
pub fn config_path(app: &tauri::AppHandle) -> PathBuf {
    crate::config::config_dir(app).join("toolchain.json")
}

/// 读 toolchain.json；缺失 / 损坏 → 默认值（设置文件不是关键路径，坏了不阻断）。
pub fn load_config(app: &tauri::AppHandle) -> ToolchainConfig {
    match std::fs::read(config_path(app)) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => ToolchainConfig::default(),
    }
}

/// 写 toolchain.json（写失败只影响设置持久化，返回原因给调用方）。
pub fn save_config(app: &tauri::AppHandle, cfg: &ToolchainConfig) -> Result<(), String> {
    let path = config_path(app);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// spawn 的**快路径**环境：受管工具的 PATH 前缀 + MSVC 的 overlay。
///
/// 与完整 [`status_report`] 的区别刻意记录在这里：spawn 环境不跑系统工具探测、
/// 不解析上游约束 —— 系统工具本来就在 PATH 上（不贡献 overlay），约束只影响
/// 版本校验（不影响环境）。受管部分只扫目录（无子进程），MSVC 走 state.json
/// 缓存（`link.exe` 校验），所以典型机器上这条路径近乎零开销。
pub fn spawn_env(state_root: &Path) -> EnvOverlay {
    let mut tools: Vec<resolve::ResolvedTool> = Vec::new();
    for id in [ToolId::Node, ToolId::Pnpm, ToolId::Dsh, ToolId::Python] {
        if let Some(inst) = managed::detect_managed(state_root, id) {
            let prefix = managed::managed_path_prefix(&inst, id);
            tools.push(resolve::ResolvedTool {
                id,
                scope: Scope::Managed,
                source: resolve::ToolSource::Managed(inst.path.clone()),
                version: Some(inst.version),
                usable: prefix.is_some(),
                detail: None,
                path_prefix: prefix,
            });
        }
    }
    // MSVC 的增量走 trait 方法（方案 §4.4：cargo build 需要 link.exe 与 LIB ——
    // 这正是 `SystemProbe::env_overlay` 存在的理由）。缓存命中时近乎零开销。
    let mut extra = std::collections::BTreeMap::new();
    extra.insert("state_root".to_string(), state_root.to_string_lossy().to_string());
    let found = ProbeFound { version: None, detail: None, extra };
    env::build_env(&tools, MsvcProbe.env_overlay(&found))
}

/// 单个工具的状态报告（Tauri 命令边界上的 JSON 形态）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolStatusJson {
    pub id: String,
    pub scope: String,
    /// managed / system / probe / missing
    pub source: String,
    pub version: Option<String>,
    pub usable: bool,
    /// Broken / 被拒的具体原因（"未检测到"不占这个字段 —— Missing 就是 Missing）
    pub detail: Option<String>,
    pub path: String,
    /// 系统工具缺失时的官方安装指引（toolchain_open_guide 用）
    pub guide_url: Option<String>,
}

/// 全量状态报告。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolchainReport {
    pub tools: Vec<ToolStatusJson>,
    /// 生效的 Node / pnpm 下限与约束来源（"这条下限谁说的"）
    pub node_min: String,
    pub node_min_from_runtime: bool,
    pub pnpm_min: String,
    pub constraints_source: String,
    /// 约束解析的兜底说明（原样透传给前端 + 落盘日志）
    pub upstream_notes: Vec<String>,
    /// 受管目录里那份 dsh 的路径（空 = 没有）。前端据此决定是否显示
    /// 「切换为受管副本」入口 —— 只有受管副本真的存在时才给用户这个选项。
    pub managed_dsh: String,
}

/// 全量检测报告（方案 §7 的 toolchain_status）。每次调用都重新 resolve
/// （方案 §4.1：没有全局"当前版本"状态），探测全部有界、MSVC 有缓存。
pub fn status_report(app: &tauri::AppHandle) -> ToolchainReport {
    let cfg = load_config(app);
    let root = store::toolchain_root(app);
    let dsh_cfg = crate::config::load(app);
    let input = resolve::ResolveInput {
        state_root: &root,
        dsh_path: &dsh_cfg.dsh_path,
        package_name: &dsh_cfg.package_name,
        use_system_tools: cfg.use_system_tools,
        managed_versions: &cfg.managed,
        rust_min: cfg.rust_min.clone(),
    };
    let r = resolve::resolve(&input);
    // 约束解析的兜底说明落盘（方案 §1.1b："在日志留一行"）
    for n in &r.upstream.notes {
        crate::process::log_launcher(app, n);
    }
    let tools = r
        .tools
        .iter()
        .map(|t| {
            let guide = match t.id {
                ToolId::Rust => Some(RustProbe.guide_url()),
                ToolId::Msvc => Some(MsvcProbe.guide_url()),
                ToolId::Webview2 => Some(Webview2Probe.guide_url()),
                _ => None,
            };
            let (source, path) = match &t.source {
                resolve::ToolSource::Managed(p) => ("managed", p.to_string_lossy().to_string()),
                resolve::ToolSource::System(p) => ("system", p.to_string_lossy().to_string()),
                resolve::ToolSource::Probe => ("probe", String::new()),
                resolve::ToolSource::Missing => ("missing", String::new()),
            };
            ToolStatusJson {
                id: t.id.as_str().to_string(),
                scope: match t.scope {
                    Scope::Managed => "managed",
                    Scope::ProbeOnly => "probe",
                }
                .to_string(),
                source: source.to_string(),
                version: t.version.clone(),
                usable: t.usable,
                detail: t.detail.clone(),
                path,
                guide_url: guide.map(str::to_string),
            }
        })
        .collect();
    ToolchainReport {
        tools,
        node_min: r.node_min.version,
        node_min_from_runtime: r.node_min.from_runtime,
        pnpm_min: r.pnpm_min.version,
        constraints_source: r.upstream.source.clone(),
        upstream_notes: r.upstream.notes.clone(),
        // 受管 dsh 的 shim 路径（没有就空串）
        managed_dsh: managed::detect_managed(&root, ToolId::Dsh)
            .and_then(|i| crate::detect::dsh_cmd_in_dir(&managed::bin_dir_for(&i.path)))
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
    }
}

/// `update_dsh` 完成后重读上游约束（方案 §1.1b 触发时机表里**最关键**的一条：
/// 通道变动真正发生在这里）。
///
/// 读不到包目录（tag 装到了别的 prefix 之类）只留一行日志走兜底，绝不报错；
/// 新下限高于当前 Node 版本时给出**明确告警** —— 不阻断（包已装好，用户需要的是
/// 知道接下来会发生什么），但绝不让它带着旧下限无声运行。
pub fn recheck_after_dsh_update(app: &tauri::AppHandle) {
    // npm 刚改写过全局目录与本进程 PATH 的记忆，检测缓存全部作废
    crate::detect::invalidate_cache();
    let cfg = crate::config::load(app);
    let root = store::toolchain_root(app);
    let Some(pkg) = resolve::locate_dsh_package_dir(&cfg.dsh_path, &cfg.package_name) else {
        crate::process::log_launcher(
            app,
            &crate::i18n::fmt("log_update_constraint_unlocated", &[&cfg.package_name]),
        );
        return;
    };
    let up = resolve::upstream_requirements_cached(&root, &pkg);
    for n in &up.notes {
        crate::process::log_launcher(app, n);
    }
    let eff = up.effective_node_min();
    let node_ver = crate::detect::find_node_exe()
        .as_deref()
        .and_then(probe::run_probe_version)
        .as_deref()
        .and_then(probe::first_version_token);
    match node_ver {
        Some(v) if probe::version_meets_min(&v, &eff.version) == Some(false) => {
            crate::process::log_launcher(
                app,
                &crate::i18n::fmt(
                    "log_update_node_below_new_min",
                    &[&v, &eff.version, &up.source],
                ),
            );
        }
        Some(_) => {
            // 合规：留一行说明当前生效的约束，便于事后回答"这条下限谁说的"
            crate::process::log_launcher(
                app,
                &crate::i18n::fmt("log_update_constraint_ok", &[&eff.version, &up.source]),
            );
        }
        None => {
            // 读不出版本 → 不判过低（与 detect.rs 的 unknown 纪律一致）
            crate::process::log_launcher(
                app,
                &crate::i18n::fmt("log_update_constraint_unknown", &[&eff.version]),
            );
        }
    }
}

// ---------- Tauri 命令（方案 §7；命名沿用 <域>_<动作>） ----------

/// 全量检测报告（含每项状态、版本、约束来源）。探测全走有界通道，
/// 用 spawn_blocking 避免卡运行时工作线程（与 detect_environment 同一条纪律）。
#[tauri::command]
pub async fn toolchain_status(app: tauri::AppHandle) -> Result<ToolchainReport, String> {
    tauri::async_runtime::spawn_blocking(move || status_report(&app))
        .await
        .map_err(|e| e.to_string())
}

/// 打开系统工具的官方安装指引（方案 §7）。受管四项没有"外部指引"——
/// 它们的处置是"用引导向导装"，给外链反而制造第二个安装入口。
#[tauri::command]
pub fn toolchain_open_guide(tool: String) -> Result<(), String> {
    let id = ToolId::parse(&tool).ok_or_else(|| format!("unknown tool id: {tool}"))?;
    let url = match id {
        ToolId::Rust => RustProbe.guide_url(),
        ToolId::Msvc => MsvcProbe.guide_url(),
        ToolId::Webview2 => Webview2Probe.guide_url(),
        _ => return Err(crate::i18n::t("tool_guide_unavailable").to_string()),
    };
    crate::process::open_in_browser(url.to_string())
}

// ---------- 安装 / 卸载 / 清理（方案 §7） ----------

/// 同一时刻只允许一个工具链任务（与既有 `setup_busy` 的分工：那个管首装向导的
/// Node/DSH/Python，这个管工具链面板 —— 两条链都改受管目录，串行才安全）。
static TOOLCHAIN_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 把 `AppHandle` 包成 `Progress`（方案 §3.1：安装器内部不需要知道 AppHandle）。
struct AppProgress<'a> {
    app: &'a tauri::AppHandle,
}

impl Progress for AppProgress<'_> {
    fn phase(&self, phase: &str, msg: &str) {
        crate::process::setup_progress(self.app, phase, msg);
    }
}

/// 解析某工具要装的**精确版本**（方案 §5.2：绝不把 `latest` 这类 tag 直接交给安装器）。
///
/// 顺序：`toolchain.json` 的 `managed.<id>` → 各工具的"当前版本"解析：
/// - Node：官方 dist 的当前 LTS 补丁版（`resolve_latest_lts_version`）；
/// - pnpm / dsh：`npm view <pkg> version` 拿**实际解析到的确切版本号**；
/// - Python：没有可自动解析的"最新"，必须由配置给出精确版本（否则报错）。
fn resolve_install_version(
    app: &tauri::AppHandle,
    cfg: &ToolchainConfig,
    id: ToolId,
) -> Result<String, String> {
    if let Some(v) = cfg.managed.get(id.as_str()).map(|s| s.trim().to_string()) {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    match id {
        ToolId::Node => {
            let tmp = crate::process::create_private_temp_dir("tc-ver")?;
            let mut err = String::new();
            let got = crate::process::resolve_latest_lts_version(&tmp, &mut err);
            let _ = std::fs::remove_dir_all(&tmp);
            got.ok_or_else(|| crate::i18n::fmt("tool_version_resolve_fail", &[&"node", &err]))
        }
        ToolId::Pnpm | ToolId::Dsh => {
            let pkg = if id == ToolId::Pnpm {
                "pnpm".to_string()
            } else {
                let c = crate::config::load(app);
                let n = c.package_name.trim().to_string();
                if n.is_empty() { managed::DEFAULT_DSH_PACKAGE.to_string() } else { n }
            };
            npm_latest_version(&pkg).ok_or_else(|| {
                crate::i18n::fmt("tool_version_resolve_fail", &[&id.to_string(), &pkg])
            })
        }
        ToolId::Python => Err(crate::i18n::fmt("tool_python_version_required", &[])),
        _ => Err(format!("{} is not a managed tool", id)),
    }
}

/// `npm view <pkg> version` → 确切版本号（纯文本单行）。
fn npm_latest_version(pkg: &str) -> Option<String> {
    let npm = crate::detect::find_npm_cmd()?;
    let args: Vec<String> = vec!["view".to_string(), pkg.to_string(), "version".to_string()];
    let (ok, out) = crate::process::run_cmd_capture(
        &npm.to_string_lossy(),
        &args,
        "",
        std::time::Duration::from_secs(60),
    )
    .ok()?;
    if !ok {
        return None;
    }
    // 版本号必须在最后一行且形如 x.y.z（前面可能有 npm warn）
    out.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.starts_with("npm "))
        .filter(|l| crate::detect::parse_version_numbers(l).is_some())
        .map(str::to_string)
}

/// 安装指定工具（`toolchain_install`）。进度走既有 `setup-progress` 事件，
/// 结果走 `setup-result`（`target` = 工具 id），**不新开事件通道**（方案 §7）。
#[tauri::command]
pub async fn toolchain_install(app: tauri::AppHandle, ids: Vec<String>) -> Result<(), String> {
    let targets: Vec<ToolId> = ids
        .iter()
        .map(|s| ToolId::parse(s).ok_or_else(|| format!("unknown tool id: {s}")))
        .collect::<Result<_, _>>()?;
    if targets.is_empty() {
        return Err("no tools given".to_string());
    }
    // 系统组件只探测，绝不代装（方案 §1.4 / §6.2）
    for t in &targets {
        if REGISTRY.iter().any(|s| s.id == *t && s.scope == Scope::ProbeOnly) {
            return Err(crate::i18n::fmt("tool_probe_not_installable", &[&t.to_string()]));
        }
    }
    if TOOLCHAIN_BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err(crate::i18n::t("tool_busy").to_string());
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        let root = store::toolchain_root(&app2);
        let cfg = load_config(&app2);
        let dsh_cfg = crate::config::load(&app2);
        // 安装时刻的约束（写进 lock.json，方案 §1.1b 规则 2）
        let up = {
            let pkg_dir = resolve::locate_dsh_package_dir(&dsh_cfg.dsh_path, &dsh_cfg.package_name);
            match pkg_dir {
                Some(d) => resolve::upstream_requirements_cached(&root, &d),
                None => resolve::read_upstream_requirements(Path::new("")),
            }
        };
        let node_min = up.effective_node_min();
        let pnpm_min = up.effective_pnpm_min();
        let prog = AppProgress { app: &app2 };

        for id in targets {
            let outcome = (|| -> Result<Installed, String> {
                let version = resolve_install_version(&app2, &cfg, id)?;
                // 把**解析到的精确版本**写回 toolchain.json（方案 §5.2 的可复现要求：
                // 这次装的是哪个版本要留下痕迹，下次就按它装，而不是重新跟 upstream 猜）。
                // 写失败不影响本次安装，只影响"下次是否可复现"，所以只记日志。
                let mut cfg2 = cfg.clone();
                cfg2.managed.insert(id.to_string(), version.clone());
                if let Err(e) = save_config(&app2, &cfg2) {
                    crate::process::log_launcher(
                        &app2,
                        &format!("[toolchain] persist version for {id}: {e}"),
                    );
                }
                crate::process::setup_progress(
                    &app2,
                    "install",
                    &crate::i18n::fmt("tool_install_start", &[&id.to_string(), &version]),
                );
                let req = InstallReq { version: version.clone(), root: root.clone() };
                let entry = managed::ManagedEntry::new(id, Some(app2.clone()));
                if id == ToolId::Dsh {
                    // dsh 的包名来自 config（可配置字段），必须走这一层
                    managed::install_dsh_with_pkg(
                        Some(&app2),
                        &req,
                        &dsh_cfg.package_name,
                        &prog,
                    )
                } else {
                    use ManagedTool;
                    entry.install(&req, &prog)
                }
            })();

            match outcome {
                Ok(inst) => {
                    // 版本下限校验：装完的精确版本必须过 effective_min（方案 §1.1b 规则 1）
                    let min = match id {
                        ToolId::Node => Some(node_min.version.as_str()),
                        ToolId::Pnpm => Some(pnpm_min.version.as_str()),
                        _ => None,
                    };
                    let verdict = resolve::check_min(id, &inst.version, min);
                    // 写 lock.json + state.json（成功与"装了但低于下限"都记账）
                    let mut lock = store::load_lock(&root);
                    lock.entries.insert(
                        id.to_string(),
                        store::LockEntry {
                            version: inst.version.clone(),
                            url: String::new(),
                            sha256: String::new(),
                            min_version_at_install: min.unwrap_or_default().to_string(),
                            constraints_source: up.source.clone(),
                            installed_at: crate::logger::utc_now_string(),
                        },
                    );
                    store::save_lock(&root, &lock);
                    let mut st = store::load_state(&root);
                    st.tools.insert(
                        id.to_string(),
                        store::ToolState {
                            state: if verdict.is_ok() { "installed" } else { "outdated" }.to_string(),
                            version: inst.version.clone(),
                            min_version_at_install: min.unwrap_or_default().to_string(),
                            constraints_source: up.source.clone(),
                            path: inst.path.to_string_lossy().to_string(),
                            installed_at: crate::logger::utc_now_string(),
                            last_error: verdict.clone().err(),
                        },
                    );
                    store::save_state(&root, &st);
                    crate::detect::invalidate_cache();

                    let ok = verdict.is_ok();
                    let msg = match &verdict {
                        Ok(()) => crate::i18n::fmt(
                            "tool_installed_at",
                            &[&format!("{} {}", id, inst.version), &inst.path.to_string_lossy().to_string()],
                        ),
                        Err(e) => e.clone(),
                    };
                    let _ = app2.emit(
                        "setup-result",
                        crate::process::SetupResult {
                            target: id.to_string(),
                            success: ok,
                            message: msg,
                        },
                    );
                }
                Err(e) => {
                    // 失败现场落 state.json（方案 §6.1：重启后能看到上次为什么失败）
                    let mut st = store::load_state(&root);
                    let cur = st.tools.entry(id.to_string()).or_default();
                    cur.state = "failed".to_string();
                    cur.last_error = Some(e.clone());
                    store::save_state(&root, &st);
                    let _ = app2.emit(
                        "setup-result",
                        crate::process::SetupResult {
                            target: id.to_string(),
                            success: false,
                            message: e,
                        },
                    );
                }
            }
        }

        // 方案 §1.1b 触发时机：装 / 换 Node 后重读约束并与当前 Node 比对
        recheck_after_dsh_update(&app2);
        TOOLCHAIN_BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
    });
    Ok(())
}

/// 卸载指定工具（只删受管目录内的那份；**绝不触碰系统安装** —— 方案 §6.2）。
#[tauri::command]
pub async fn toolchain_uninstall(app: tauri::AppHandle, ids: Vec<String>) -> Result<(), String> {
    let targets: Vec<ToolId> = ids
        .iter()
        .map(|s| ToolId::parse(s).ok_or_else(|| format!("unknown tool id: {s}")))
        .collect::<Result<_, _>>()?;
    for t in &targets {
        if REGISTRY.iter().any(|s| s.id == *t && s.scope == Scope::ProbeOnly) {
            return Err(crate::i18n::fmt("tool_probe_not_uninstallable", &[&t.to_string()]));
        }
    }
    let root = store::toolchain_root(&app);
    for id in targets {
        let Some(inst) = managed::detect_managed(&root, id) else {
            let _ = app.emit(
                "setup-result",
                crate::process::SetupResult {
                    target: id.to_string(),
                    success: true,
                    message: crate::i18n::fmt("tool_uninstall_none", &[&id.to_string()]),
                },
            );
            continue;
        };
        let res = {
            use ManagedTool;
            managed::ManagedEntry::new(id, Some(app.clone())).uninstall(&inst)
        };
        // lock.json 里那条记录一并清掉（它描述的安装已经不存在了）
        let mut lock = store::load_lock(&root);
        lock.entries.remove(id.as_str());
        store::save_lock(&root, &lock);
        let mut st = store::load_state(&root);
        st.tools.remove(id.as_str());
        store::save_state(&root, &st);
        crate::detect::invalidate_cache();
        let (ok, msg) = match res {
            Ok(()) => (
                true,
                crate::i18n::fmt("tool_uninstall_ok", &[&id.to_string(), &inst.path.to_string_lossy().to_string()]),
            ),
            Err(e) => (false, e),
        };
        let _ = app.emit(
            "setup-result",
            crate::process::SetupResult { target: id.to_string(), success: ok, message: msg },
        );
    }
    Ok(())
}

/// 可清理的版本条目。
#[derive(Debug, Clone, serde::Serialize)]
pub struct PruneEntry {
    pub id: String,
    pub version: String,
    pub path: String,
    pub bytes: u64,
    /// 当前是否就是"生效中"的那一版（生效的默认不勾选）
    pub current: bool,
}

/// 列出受管目录里**没有被当前配置引用**的版本（方案 §6.3：v1 只列不删）。
#[tauri::command]
pub async fn toolchain_prune(app: tauri::AppHandle) -> Result<Vec<PruneEntry>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let root = store::toolchain_root(&app);
        let cfg = load_config(&app);
        let mut out: Vec<PruneEntry> = Vec::new();
        for id in [ToolId::Node, ToolId::Pnpm, ToolId::Dsh, ToolId::Python] {
            let current = managed::detect_managed(&root, id).map(|i| i.version);
            let want = cfg.managed.get(id.as_str()).map(|s| s.trim().to_string());
            for (version, dir) in managed::scan_managed_versions(&root, id) {
                let is_current = current.as_deref() == Some(version.as_str());
                let is_wanted = want.as_deref() == Some(version.as_str());
                if is_current || is_wanted {
                    continue; // 生效中 / 配置指定的：不算"未使用"
                }
                out.push(PruneEntry {
                    id: id.to_string(),
                    version,
                    path: dir.to_string_lossy().to_string(),
                    bytes: dir_size_capped(&dir, 512 * 1024 * 1024),
                    current: false,
                });
            }
        }
        out
    })
    .await
    .map_err(|e| e.to_string())
}

/// 目录大小（有上限，避免为了显示一个数字把几 GB 全走一遍）。
fn dir_size_capped(dir: &Path, cap: u64) -> u64 {
    let mut total: u64 = 0;
    let mut stack = vec![dir.to_path_buf()];
    let mut visited = 0usize;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            visited += 1;
            if visited > 200_000 || total >= cap {
                return total;
            }
            let p = e.path();
            let Ok(md) = std::fs::symlink_metadata(&p) else { continue };
            if md.is_dir() {
                stack.push(p);
            } else {
                total = total.saturating_add(md.len());
            }
        }
    }
    total
}

/// **显式把 DSH 切换到受管副本**（用户主动动作，Q3 的 UI 落点）。
///
/// 刻意不做成"装完自动改写"（那会抢用户已有的 `dsh_path`）；只有用户在这里点了
/// 按钮才改。改完 `invalidate_cache()` 让后续检测立刻看到新路径。
#[tauri::command]
pub fn toolchain_use_managed(app: tauri::AppHandle, tool: String) -> Result<(), String> {
    let id = ToolId::parse(&tool).ok_or_else(|| format!("unknown tool id: {tool}"))?;
    if id != ToolId::Dsh {
        return Err(crate::i18n::fmt("tool_switch_only_dsh", &[&id.to_string()]));
    }
    let root = store::toolchain_root(&app);
    let Some(inst) = managed::detect_managed(&root, id) else {
        return Err(crate::i18n::t("tool_no_managed_dsh").to_string());
    };
    let shim = crate::detect::dsh_cmd_in_dir(&managed::bin_dir_for(&inst.path))
        .ok_or_else(|| crate::i18n::t("tool_no_managed_dsh").to_string())?;
    let mut cfg = crate::config::load(&app);
    cfg.dsh_path = shim.to_string_lossy().to_string();
    crate::config::save(&app, &cfg)?;
    crate::detect::invalidate_cache();
    crate::process::log_launcher(
        &app,
        &crate::i18n::fmt("tool_switch_managed_ok", &[&cfg.dsh_path]),
    );
    Ok(())
}
