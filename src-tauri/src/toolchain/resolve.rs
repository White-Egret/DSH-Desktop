//! 配置版本 → lock.json → 实际安装状态 → `Resolved`（方案 §4.1），
//! 以及**上游约束解析器**（方案 §1.1b —— 本模块能否"扛住上游变化"的落点）。
//!
//! 上游（DSH）会反复改约束：Node 下限、tag 指向都在动。所以这里**不写死任何
//! 版本号或"某字段一定存在"的假设**，只保留三样稳定的东西：
//! - **去哪里读**：实际安装的 DSH 包目录里的 `package.json`（`engines.node` 等）；
//! - **读不到怎么办**：回退 `detect.rs` 的兜底常量并留一行日志，**不报错、不猜测**；
//! - **读到之后怎么比**：`effective_min = max(运行时值, 兜底常量)`，版本比较复用
//!   `detect::parse_version_numbers` / `cmp_versions`。
//!
//! 判断标准（方案 §1.1b）：上游改一次约束，本程序是否需要发新版？
//! 需要就是设计失败。运行时读到新值、调用方零改动 —— 设计成立。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::managed;
use super::probe::{self, MsvcProbe, RustProbe, Webview2Probe};
use super::store::{self, UpstreamCache};
use super::{Installed, ProbeReq, ProbeResult, Scope, SystemProbe, ToolId, REGISTRY};
use crate::process::sha256_hex;

// ---------- 上游约束（方案 §1.1b） ----------

/// 从「实际安装的 DSH 包」读出的运行时约束。
/// 新增字段只需在这里加一项 + `read_upstream_requirements` 里加一次读取，调用方不用改。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpstreamRequirements {
    /// 来自 `engines.node`（将来也可能换字段），**已经提取出最小版本**。
    pub node_min: Option<String>,
    /// 来自 `engines.pnpm`（当前包可能没有，将来可能有）。
    pub pnpm_min: Option<String>,
    /// 包名 + 版本，用于日志回答「这条下限谁说的」。
    pub source: String,
    /// 原样保留的约束字段（`engines.node` 的原始 range 等），便于排障与新字段灰度。
    pub raw_fields: BTreeMap<String, String>,
    /// 解析过程的兜底说明（字段缺失 / 值认不出）。门面负责落盘成日志。
    pub notes: Vec<String>,
}

impl UpstreamRequirements {
    /// 生效的 Node 下限：`max(运行时值, 兜底常量)`。运行时值**高于**兜底时
    /// `from_runtime = true`（状态面板据此标注"约束来自 dsh 包"）。
    pub fn effective_node_min(&self) -> MinBound {
        effective_min(self.node_min.as_deref(), crate::detect::NODE_MIN_VERSION)
    }

    /// 生效的 pnpm 下限：同上，兜底是 `PNPM_MIN_VERSION`。
    pub fn effective_pnpm_min(&self) -> MinBound {
        effective_min(self.pnpm_min.as_deref(), crate::detect::PNPM_MIN_VERSION)
    }
}

/// 一条生效下限：比较用的版本串 + 它是否被运行时值抬高过。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinBound {
    pub version: String,
    pub from_runtime: bool,
}

/// `max(运行时值, 兜底常量)` 的纯函数内核。两边都解析不出 → 兜底常量
/// （兜底常量解析不出来是程序自身 bug，测试钉住它可解析）。
fn effective_min(runtime: Option<&str>, fallback: &str) -> MinBound {
    let fb = crate::detect::parse_version_numbers(fallback);
    let rt = runtime.and_then(crate::detect::parse_version_numbers);
    match (rt, fb) {
        (Some(r), Some(f)) if crate::detect::cmp_versions(&r, &f) == std::cmp::Ordering::Greater => {
            MinBound { version: runtime.unwrap_or_default().to_string(), from_runtime: true }
        }
        _ => MinBound { version: fallback.to_string(), from_runtime: false },
    }
}

/// semver range → 最小满足版本（方案 §1.1b 要点 3 的"薄薄的提取层"）。
///
/// 规则：
/// - 按 `||` 拆备选，每个备选取**第一个下限 token**（`>=x` / `^x` / `~x` / 裸 `x`），
///   整体取各备选地板的最小值 —— `^22.19.0 || >=24.0.0` 的最小满足版本是 22.19.0；
/// - `<x` 是上界不是地板；`*` / `x` 是无下限；
/// - 认不出的 token 不作数；整串认不出 → `None`（调用方走兜底，**不猜**）。
pub fn min_version_from_range(range: &str) -> Option<String> {
    let mut best: Option<Vec<u32>> = None;
    for alt in range.split("||") {
        for tok in alt.split_whitespace() {
            if let Some(v) = floor_of_token(tok) {
                best = Some(match best {
                    None => v,
                    Some(b) if crate::detect::cmp_versions(&v, &b) == std::cmp::Ordering::Less => v,
                    Some(b) => b,
                });
                break; // 每个备选只取第一个地板
            }
        }
    }
    best.map(|v| {
        // 补齐到三段（22.19 → 22.19.0），展示与比较都用同一形态
        let mut parts = v.clone();
        while parts.len() < 3 {
            parts.push(0);
        }
        parts.iter().take(3).map(|n| n.to_string()).collect::<Vec<_>>().join(".")
    })
}

/// 单个 token 的地板：上界与通配符返回 None（不是地板），其余交给
/// `parse_version_numbers`（它处理 `v` / `^` / `~` 前缀）。
fn floor_of_token(tok: &str) -> Option<Vec<u32>> {
    let t = tok.trim();
    if t.is_empty() || t.starts_with('<') {
        return None;
    }
    let t = t.trim_start_matches(['>', '=', 'v', 'V']);
    if t.is_empty() || t == "*" || t.eq_ignore_ascii_case("x") {
        return None;
    }
    crate::detect::parse_version_numbers(t)
}

/// 定位「实际安装的 DSH 包」目录：从 `cfg.dsh_path` 反推 prefix，再按
/// `cfg.package_name`（**可配置字段**，绝不写死）拼出
/// `node_modules/<package_name>/package.json`（方案 §1.1b 要点 1）。
pub fn locate_dsh_package_dir(dsh_path: &str, package_name: &str) -> Option<PathBuf> {
    let name = package_name.trim();
    // 包名形状守卫：它会被拼进路径。空、绝对路径、盘符、任何 `..` 一律拒绝。
    if name.is_empty()
        || name.contains("..")
        || name.contains(':')
        || name.starts_with('/')
        || name.starts_with('\\')
        || Path::new(name).is_absolute()
    {
        return None;
    }
    let p = Path::new(dsh_path.trim());
    let prefix = p.parent()?; // dsh.cmd 所在目录 = npm 全局 prefix
    let pkg = prefix.join("node_modules").join(name);
    if pkg.join("package.json").is_file() {
        Some(pkg)
    } else {
        None
    }
}

/// 读一个包目录的 `package.json`，提取运行时约束（方案 §1.1b 要点 2）。
///
/// **字段缺失是一等公民**：`engines` 整个不存在、`engines.node` 不存在、值是无法
/// 解析的 range —— 三种都回退兜底常量并留一行说明（`notes`），不报错、不猜测。
/// 已发布的包并不声明 engines（detect.rs 的注释就是这个判断的实证）。
pub fn read_upstream_requirements(dsh_pkg_dir: &Path) -> UpstreamRequirements {
    let mut up = UpstreamRequirements::default();
    let pj = dsh_pkg_dir.join("package.json");
    let Ok(bytes) = std::fs::read(&pj) else {
        up.source = "(package.json not readable)".to_string();
        up.notes
            .push(format!("upstream: {} 不可读，Node/pnpm 下限走兜底常量", pj.display()));
        return up;
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        up.source = "(package.json not parseable)".to_string();
        up.notes.push("upstream: package.json 不是合法 JSON，下限走兜底常量".to_string());
        return up;
    };
    // source = 包名 + 版本（读不到就退目录名）——出问题时要能回答"这条下限谁说的"
    let name = match v.get("name").and_then(|x| x.as_str()) {
        Some(s) => s.to_string(),
        None => dsh_pkg_dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
    };
    let version = v.get("version").and_then(|x| x.as_str()).unwrap_or("?");
    up.source = format!("{name}@{version}");

    let engines = v.get("engines");
    if engines.map(|e| !e.is_object()).unwrap_or(true) {
        if engines.is_some() {
            up.notes.push("upstream: engines 不是对象，下限走兜底常量".to_string());
        } else {
            up.notes.push("upstream: 没有 engines 字段，下限走兜底常量".to_string());
        }
        return up;
    }
    let engines = engines.unwrap();
    for (field, slot) in [("node", 0u8), ("pnpm", 1u8)] {
        let Some(raw) = engines.get(field).and_then(|x| x.as_str()) else {
            up.notes.push(format!("upstream: engines.{field} 缺失，走兜底常量"));
            continue;
        };
        up.raw_fields.insert(format!("engines.{field}"), raw.to_string());
        match min_version_from_range(raw) {
            Some(m) => {
                if slot == 0 {
                    up.node_min = Some(m);
                } else {
                    up.pnpm_min = Some(m);
                }
            }
            None => {
                up.notes
                    .push(format!("upstream: engines.{field}=\"{raw}\" 无法解析出下限，走兜底常量"));
            }
        }
    }
    up
}

/// 带内容指纹缓存地读上游约束（方案 §1.1b 要点 4）。
///
/// `package.json` 的 SHA-256 记进 `state.json`：没变不重读；变了就重新解析。
/// **上游改一次约束，程序自动跟上，调用方代码零改动** —— 这是本次设计的验收标准。
/// `state_root` 为空时不读写缓存（纯探测路径）。
pub fn upstream_requirements_cached(state_root: &Path, dsh_pkg_dir: &Path) -> UpstreamRequirements {
    let bytes = match std::fs::read(dsh_pkg_dir.join("package.json")) {
        Ok(b) => b,
        Err(_) => return read_upstream_requirements(dsh_pkg_dir),
    };
    let fp = sha256_hex(&bytes);
    let state = store::load_state(state_root);
    if let Some(c) = state.upstream_cache.as_ref() {
        if c.fingerprint == fp {
            return UpstreamRequirements {
                node_min: c.node_min.clone(),
                pnpm_min: c.pnpm_min.clone(),
                source: c.source.clone(),
                raw_fields: BTreeMap::new(),
                notes: Vec::new(), // 缓存命中：内容没变，无需重复说明
            };
        }
    }
    let up = read_upstream_requirements(dsh_pkg_dir);
    let mut st = state;
    st.upstream_cache = Some(UpstreamCache {
        fingerprint: fp,
        node_min: up.node_min.clone(),
        pnpm_min: up.pnpm_min.clone(),
        source: up.source.clone(),
    });
    store::save_state(state_root, &st);
    up
}

// ---------- 精确版本的下限校验（方案 §1.1b 规则 1/3） ----------

/// 精确版本过下限：不满足就拒绝（调用方不写 lock.json）。
/// **复用系统工具也走同一条校验**（不能因为"是系统自带的"就跳过）。
///
/// 版本读不出（`None`）时**不判过低**（与 detect.rs 的"unknown 不判过低"同一条
/// 纪律 —— 把读不出当成过低会把人堵死在一个修不了的状态里）。
pub fn check_min(id: ToolId, version: &str, min: Option<&str>) -> Result<(), String> {
    match id {
        ToolId::Python => {
            // Python 的门槛来自 detect::PYTHON_MIN_VERSION（markitdown 等硬要求 ≥3.10）
            if crate::detect::python_version_usable(version) {
                Ok(())
            } else {
                Err(crate::i18n::fmt(
                    "tool_min_rejected",
                    &[&id.to_string(), &version.to_string(), &crate::detect::PYTHON_MIN_VERSION.to_string()],
                ))
            }
        }
        _ => match min {
            None => Ok(()),
            Some(m) => match probe::version_meets_min(version, m) {
                Some(true) | None => Ok(()),
                Some(false) => Err(crate::i18n::fmt(
                    "tool_min_rejected",
                    &[&id.to_string(), &version.to_string(), &m.to_string()],
                )),
            },
        },
    }
}

/// 两个版本串是否指同一个版本（`v24.22.0` == `24.22.0`；解析不出时退字符串相等）。
fn versions_match(a: &str, b: &str) -> bool {
    match (
        crate::detect::parse_version_numbers(a),
        crate::detect::parse_version_numbers(b),
    ) {
        (Some(x), Some(y)) => crate::detect::cmp_versions(&x, &y) == std::cmp::Ordering::Equal,
        _ => a.trim().eq_ignore_ascii_case(b.trim()),
    }
}

// ---------- resolve（方案 §4.1：每次调用都重新算，没有全局"当前版本"状态） ----------

/// resolve 的输入（调用方从 config.json / toolchain.json 收集）。
pub struct ResolveInput<'a> {
    /// 受管根目录（state.json / lock.json / tools\ 都在它下面）。
    pub state_root: &'a Path,
    /// `cfg.dsh_path`（程序实际启动的 dsh；上游约束也从它旁边的包目录读）。
    pub dsh_path: &'a str,
    /// `cfg.package_name`（可配置字段）。
    pub package_name: &'a str,
    /// `settings.use_system_tools`。
    pub use_system_tools: bool,
    /// `toolchain.json` 的 `managed` 段：tool id → 配置的精确版本。
    pub managed_versions: &'a BTreeMap<String, String>,
    /// `system.rust.min`（None = 只判存在）。
    pub rust_min: Option<String>,
}

/// 单个工具的解析结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTool {
    pub id: ToolId,
    pub scope: Scope,
    pub source: ToolSource,
    pub version: Option<String>,
    pub usable: bool,
    /// 不适用 / 被拒 / Broken 的具体原因（给状态面板显示，不能是笼统的"未检测到"）。
    pub detail: Option<String>,
    /// 受管安装给 PATH 贡献的前缀（build_env 合并用；系统安装为 None ——
    /// 它本来就在 PATH 上，我们不改变解析顺序，方案 §0.5）。
    pub path_prefix: Option<PathBuf>,
}

/// 当前生效来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSource {
    /// 受管目录里的安装。
    Managed(PathBuf),
    /// 系统里的现成安装（复用，含 cfg.dsh_path 指向的那份 dsh）。
    System(PathBuf),
    /// 只探测的系统组件（rust / msvc / webview2，无路径语义）。
    Probe,
    /// 没有找到。
    Missing,
}

/// resolve 的完整结论：七项工具 + 生效约束。
///
/// 刻意**不含** spawn 环境：环境由 `toolchain::spawn_env` 单独算（那条是热路径，
/// 只扫受管目录 + 读 MSVC 缓存，不跑系统探测，见 mod.rs 的说明）。
#[derive(Debug, Clone)]
pub struct Resolved {
    pub tools: Vec<ResolvedTool>,
    pub upstream: UpstreamRequirements,
    pub node_min: MinBound,
    pub pnpm_min: MinBound,
}

/// 复用 / 受管 / 缺失的三方判定 —— **纯函数内核**（机器探测结果作为参数传入，
/// 单测用临时数据构造，不依赖本机真实环境）。
pub fn decide_tool(
    id: ToolId,
    use_system_tools: bool,
    configured: Option<&str>,
    system_found: Option<&Installed>,
    managed_found: Option<&Installed>,
    min: Option<&str>,
) -> ResolvedTool {
    // ① 复用系统工具：配置了精确版本、系统里真有、版本精确一致，再过下限校验
    //    （方案 §2.3：不能因为"是系统自带的"就跳过校验）
    if use_system_tools {
        let cfg_v = configured.map(str::trim).filter(|s| !s.is_empty());
        if let (Some(cfg_v), Some(sys)) = (cfg_v, system_found) {
            if !sys.version.trim().is_empty() && versions_match(cfg_v, &sys.version) {
                return match check_min(id, &sys.version, min) {
                    Ok(()) => ResolvedTool {
                        id,
                        scope: Scope::Managed,
                        source: ToolSource::System(sys.path.clone()),
                        version: Some(sys.version.clone()),
                        usable: true,
                        detail: None,
                        path_prefix: None, // 系统安装不贡献 PATH（它本来就在）
                    },
                    Err(e) => ResolvedTool {
                        id,
                        scope: Scope::Managed,
                        source: ToolSource::System(sys.path.clone()),
                        version: Some(sys.version.clone()),
                        usable: false,
                        detail: Some(e),
                        path_prefix: None,
                    },
                };
            }
        }
    }
    // ② 受管安装
    if let Some(m) = managed_found {
        return match check_min(id, &m.version, min) {
            Ok(()) => ResolvedTool {
                id,
                scope: Scope::Managed,
                source: ToolSource::Managed(m.path.clone()),
                version: Some(m.version.clone()),
                usable: true,
                detail: None,
                path_prefix: managed::managed_path_prefix(m, id),
            },
            Err(e) => ResolvedTool {
                id,
                scope: Scope::Managed,
                source: ToolSource::Managed(m.path.clone()),
                version: Some(m.version.clone()),
                usable: false,
                detail: Some(e),
                path_prefix: None,
            },
        };
    }
    // ③ 都没有（系统里有但与配置不一致 = 需要装受管版本，不算错误）
    ResolvedTool {
        id,
        scope: Scope::Managed,
        source: ToolSource::Missing,
        version: system_found.map(|s| s.version.clone()).filter(|v| !v.trim().is_empty()),
        usable: false,
        detail: None,
        path_prefix: None,
    }
}

/// 只探测的三项：直接跑 probe，翻译成 ResolvedTool。
fn probe_tool(p: &dyn SystemProbe, min: Option<String>, state_root: &Path) -> ResolvedTool {
    let req = ProbeReq { min_version: min, state_root: state_root.to_path_buf() };
    let r = p.probe(&req);
    let (version, usable, detail, source) = match r {
        ProbeResult::Ok(v) => (Some(v), true, None, ToolSource::Probe),
        ProbeResult::TooOld { current, min } => (
            Some(current),
            false,
            Some(crate::i18n::fmt("tool_probe_too_old", &[&min])),
            ToolSource::Probe,
        ),
        ProbeResult::Broken(d) => (None, false, Some(d), ToolSource::Probe),
        ProbeResult::Missing => (None, false, None, ToolSource::Missing),
    };
    ResolvedTool {
        id: ToolId::Rust, // 由调用方覆盖（见 resolve_tool_probes）
        scope: Scope::ProbeOnly,
        source,
        version,
        usable,
        detail,
        path_prefix: None,
    }
}

/// resolve 的主体（方案 §4.1）。
pub fn resolve(input: &ResolveInput) -> Resolved {
    // ① 上游约束（定位包目录 → 读/缓存 → effective_min）
    let pkg_dir = locate_dsh_package_dir(input.dsh_path, input.package_name);
    let upstream = match pkg_dir.as_deref() {
        Some(d) => upstream_requirements_cached(input.state_root, d),
        None => read_upstream_requirements(Path::new("")),
    };
    let node_min = upstream.effective_node_min();
    let pnpm_min = upstream.effective_pnpm_min();

    let system_of = |id: ToolId| managed::detect_system(id);
    let managed_of = |id: ToolId| managed::detect_managed(input.state_root, id);
    let configured_of = |id: ToolId| input.managed_versions.get(id.as_str()).map(String::as_str);

    let mut tools: Vec<ResolvedTool> = Vec::new();
    for spec in REGISTRY {
        let id = spec.id;
        let t = match id {
            ToolId::Node => decide_tool(
                id,
                input.use_system_tools,
                configured_of(id),
                system_of(id).as_ref(),
                managed_of(id).as_ref(),
                Some(&node_min.version),
            ),
            ToolId::Pnpm => decide_tool(
                id,
                input.use_system_tools,
                configured_of(id),
                system_of(id).as_ref(),
                managed_of(id).as_ref(),
                Some(&pnpm_min.version),
            ),
            // Python 的门槛在 check_min 内部走 detect::PYTHON_MIN_VERSION
            ToolId::Python => decide_tool(
                id,
                input.use_system_tools,
                configured_of(id),
                system_of(id).as_ref(),
                managed_of(id).as_ref(),
                None,
            ),
            ToolId::Dsh => resolve_dsh(input),
            ToolId::Rust => {
                let mut t = probe_tool(&RustProbe, input.rust_min.clone(), input.state_root);
                t.id = ToolId::Rust;
                t
            }
            ToolId::Msvc => {
                let mut t = probe_tool(&MsvcProbe, None, input.state_root);
                t.id = ToolId::Msvc;
                t
            }
            ToolId::Webview2 => {
                let mut t = probe_tool(&Webview2Probe, None, input.state_root);
                t.id = ToolId::Webview2;
                t
            }
        };
        tools.push(t);
    }

    Resolved { tools, upstream, node_min, pnpm_min }
}

/// dsh 的解析：程序实际启动的是 `cfg.dsh_path`（process::resolve_dsh_prog 校验过），
/// 所以 dsh 的"现状"以它为准 —— 有效 → System（复用的主形态）；失效 → 受管目录里
/// 找；都没有 → Missing。
fn resolve_dsh(input: &ResolveInput) -> ResolvedTool {
    let dp = input.dsh_path.trim();
    if !dp.is_empty() && Path::new(dp).is_file() {
        let version = probe::run_probe_version(Path::new(dp))
            .as_deref()
            .and_then(probe::first_version_token)
            .filter(|v| !v.is_empty());
        // dsh 自身没有版本下限（它对 node/pnpm 的约束已经进 upstream；
        // 受管 dsh 的精确版本校验发生在安装侧，方案 §5.2）
        return ResolvedTool {
            id: ToolId::Dsh,
            scope: Scope::Managed,
            source: ToolSource::System(PathBuf::from(dp)),
            version,
            usable: true,
            detail: None,
            path_prefix: None,
        };
    }
    match managed::detect_managed(input.state_root, ToolId::Dsh) {
        Some(m) => ResolvedTool {
            id: ToolId::Dsh,
            scope: Scope::Managed,
            source: ToolSource::Managed(m.path.clone()),
            version: Some(m.version.clone()),
            usable: true,
            detail: None,
            path_prefix: managed::managed_path_prefix(&m, ToolId::Dsh),
        },
        None => ResolvedTool {
            id: ToolId::Dsh,
            scope: Scope::Managed,
            source: ToolSource::Missing,
            version: None,
            usable: false,
            detail: None,
            path_prefix: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toolchain::Installed;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsh-resolve-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_pkg(dir: &Path, engines_node: Option<&str>, engines_pnpm: Option<&str>) -> PathBuf {
        let mut engines = String::from("{");
        if let Some(n) = engines_node {
            engines.push_str(&format!(r#""node": "{}""#, n));
            if engines_pnpm.is_some() {
                engines.push(',');
            }
        }
        if let Some(p) = engines_pnpm {
            engines.push_str(&format!(r#""pnpm": "{}""#, p));
        }
        engines.push('}');
        let engines_field = if engines_node.is_some() || engines_pnpm.is_some() {
            format!(r#""engines": {engines},"#)
        } else {
            String::new()
        };
        let body = format!(
            r#"{{ "name": "@fake/dsh", "version": "9.9.9-fake", {engines_field} "scripts": {{}} }}"#
        );
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("package.json"), body).unwrap();
        dir.to_path_buf()
    }

    // ---------- min_version_from_range ----------

    /// range 提取：`>=24.9.0`、`^22.19.0 || >=24.0.0` 这类真实形态都要能提取出
    /// 最小版本；认不出的返回 None 走兜底（不猜）。
    #[test]
    fn min_version_from_range_extracts_floor() {
        assert_eq!(min_version_from_range(">=24.9.0").as_deref(), Some("24.9.0"));
        assert_eq!(
            min_version_from_range("^22.19.0 || >=24.0.0").as_deref(),
            Some("22.19.0"),
            "多备选取最小地板"
        );
        assert_eq!(min_version_from_range("^22.19.0").as_deref(), Some("22.19.0"));
        assert_eq!(min_version_from_range("~1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(min_version_from_range(">=18").as_deref(), Some("18.0.0"), "残缺段补齐");
        assert_eq!(min_version_from_range("22.19.0").as_deref(), Some("22.19.0"), "裸版本也是地板");
        // 无下限 / 认不出 → None
        assert_eq!(min_version_from_range("*"), None);
        assert_eq!(min_version_from_range(""), None);
        assert_eq!(min_version_from_range("junk"), None);
        assert_eq!(min_version_from_range("<24.0.0"), None, "只有上界 = 没有地板");
    }

    // ---------- read_upstream_requirements：字段缺失是一等公民（判据 g） ----------

    /// 三种异常输入（engines 整个缺失 / engines.node 缺失 / 值无法解析）
    /// 都走兜底并留说明，不报错不猜测。
    #[test]
    fn upstream_read_tolerates_missing_and_unparseable_fields() {
        let root = temp_root("g");
        // ① engines 整个不存在
        let d1 = fake_pkg(&root.join("p1"), None, None);
        let up1 = read_upstream_requirements(&d1);
        assert_eq!(up1.node_min, None);
        assert_eq!(up1.source, "@fake/dsh@9.9.9-fake");
        assert!(up1.notes.iter().any(|n| n.contains("engines")), "应留一行说明：{:?}", up1.notes);
        // ② engines 存在但 node 缺失
        let d2 = fake_pkg(&root.join("p2"), None, Some(">=10.0.0"));
        let up2 = read_upstream_requirements(&d2);
        assert_eq!(up2.node_min, None);
        assert_eq!(up2.pnpm_min.as_deref(), Some("10.0.0"));
        assert!(up2.notes.iter().any(|n| n.contains("engines.node")));
        // ③ 值是无法解析的 range
        let d3 = fake_pkg(&root.join("p3"), Some("not-a-range"), None);
        let up3 = read_upstream_requirements(&d3);
        assert_eq!(up3.node_min, None);
        assert_eq!(up3.raw_fields.get("engines.node").map(String::as_str), Some("not-a-range"),
            "原样保留原始字段便于排障");
        assert!(up3.notes.iter().any(|n| n.contains("无法解析")));
        // ④ 连 package.json 都没有
        let up4 = read_upstream_requirements(&root.join("no-such"));
        assert_eq!(up4.node_min, None);
        assert!(up4.notes.iter().any(|n| n.contains("不可读")));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---------- effective_min：取更严格者（判据 f） ----------

    /// 假包的 engines.node 比常量更高时，effective_min 取更高值并标注来源。
    #[test]
    fn effective_min_takes_stricter_of_runtime_and_fallback() {
        let root = temp_root("f");
        // 运行时 24.9.0 > 兜底 22.19.0 → 取 24.9.0
        let d = fake_pkg(&root.join("p"), Some(">=24.9.0"), None);
        let up = read_upstream_requirements(&d);
        let eff = up.effective_node_min();
        assert_eq!(eff.version, "24.9.0");
        assert!(eff.from_runtime, "被运行时抬高要标注来源");
        // 运行时低于兜底 → 兜底胜出（取更严格者）
        let d2 = fake_pkg(&root.join("p2"), Some(">=18.0.0"), None);
        let up2 = read_upstream_requirements(&d2);
        let eff2 = up2.effective_node_min();
        assert_eq!(eff2.version, crate::detect::NODE_MIN_VERSION);
        assert!(!eff2.from_runtime);
        // 没有运行时值 → 兜底
        let up3 = read_upstream_requirements(&root.join("p3-none"));
        assert_eq!(up3.effective_node_min().version, crate::detect::NODE_MIN_VERSION);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---------- 指纹缓存（判据 h）：上游改一次约束，调用方零改动自动跟上 ----------

    #[test]
    fn upstream_cache_follows_package_json_changes() {
        let root = temp_root("h");
        let state_root = root.join("state");
        std::fs::create_dir_all(&state_root).unwrap();
        let pkg = fake_pkg(&root.join("pkg"), Some(">=24.9.0"), None);

        // 第一次读：走真实解析并落缓存
        let up1 = upstream_requirements_cached(&state_root, &pkg);
        assert_eq!(up1.node_min.as_deref(), Some("24.9.0"));
        assert!(store::load_state(&state_root).upstream_cache.is_some(), "缓存应已落盘");

        // 缓存命中：同内容再读，值一致
        let up2 = upstream_requirements_cached(&state_root, &pkg);
        assert_eq!(up2.node_min.as_deref(), Some("24.9.0"));

        // **模拟上游提门槛**：只改假包的下限数字（调用方代码零改动）
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "@fake/dsh", "version": "9.9.9-fake", "engines": { "node": ">=25.1.0" } }"#,
        )
        .unwrap();
        let up3 = upstream_requirements_cached(&state_root, &pkg);
        assert_eq!(up3.node_min.as_deref(), Some("25.1.0"), "指纹变化应触发重读");
        assert_eq!(
            up3.effective_node_min().version,
            "25.1.0",
            "effective_min 自动跟上，无需发新版"
        );
        // 再次改回旧值：同样自动跟回
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "@fake/dsh", "version": "9.9.9-fake", "engines": { "node": ">=24.9.0" } }"#,
        )
        .unwrap();
        let up4 = upstream_requirements_cached(&state_root, &pkg);
        assert_eq!(up4.node_min.as_deref(), Some("24.9.0"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---------- locate：包名可配置（判据 i） ----------

    /// 把 package_name 换成别的 scoped 包仍能定位；非法包名一律拒绝。
    #[test]
    fn locate_dsh_package_handles_scoped_and_rejects_bad_names() {
        let root = temp_root("i");
        let prefix = root.join("npm-global");
        // scoped 包：node_modules/@scope/other/package.json
        let pkg = prefix.join("node_modules").join("@scope").join("other");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), "{}").unwrap();
        let dsh_cmd = prefix.join("dsh.cmd");
        std::fs::write(&dsh_cmd, b"").unwrap();

        let got = locate_dsh_package_dir(dsh_cmd.to_string_lossy().as_ref(), "@scope/other");
        assert_eq!(got, Some(pkg));
        // 另一个 scoped 名 → 定位到别处（不存在 → None，不猜）
        assert_eq!(
            locate_dsh_package_dir(dsh_cmd.to_string_lossy().as_ref(), "@deepseek-ai/dsh"),
            None
        );
        // 形状守卫：路径穿越 / 绝对路径 / 盘符一律拒绝
        assert_eq!(locate_dsh_package_dir(dsh_cmd.to_string_lossy().as_ref(), "../escape"), None);
        assert_eq!(locate_dsh_package_dir(dsh_cmd.to_string_lossy().as_ref(), r"C:\evil"), None);
        assert_eq!(locate_dsh_package_dir(dsh_cmd.to_string_lossy().as_ref(), ""), None);
        // dsh_path 为空 → None
        assert_eq!(locate_dsh_package_dir("", "@scope/other"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---------- decide_tool（判据 d / e，纯函数内核用假数据） ----------

    fn sys_inst(v: &str) -> Installed {
        Installed { version: v.to_string(), path: PathBuf::from(r"C:\sys\node.exe") }
    }
    fn mng_inst(v: &str) -> Installed {
        Installed { version: v.to_string(), path: PathBuf::from(r"D:\toolchain\tools\node\v") }
    }

    /// 判据 (d)：低于下限的精确版本被拒；恰好等于下限放行。
    #[test]
    fn decide_tool_rejects_version_below_min() {
        let min = "24.9.0";
        // 受管 24.8.0 < 24.9.0 → 拒，detail 说清原因
        let t = decide_tool(ToolId::Node, false, Some("24.8.0"), None, Some(&mng_inst("24.8.0")), Some(min));
        assert!(!t.usable);
        assert!(t.detail.as_deref().unwrap_or("").contains("24.9.0"), "detail 要带上下限：{:?}", t.detail);
        // 恰好等于下限 → 放行（边界）
        let t2 = decide_tool(ToolId::Node, false, None, None, Some(&mng_inst("24.9.0")), Some(min));
        assert!(t2.usable, "恰好等于下限应放行");
        assert!(matches!(t2.source, ToolSource::Managed(_)));
        // 版本读不出 → 不判过低（与 detect 的 unknown 纪律一致）
        let t3 = decide_tool(ToolId::Node, false, None, None, Some(&mng_inst("")), Some(min));
        assert!(t3.usable, "unknown 不判过低");
    }

    /// 判据 (e)：复用系统工具也过同一条下限校验 —— 低了照样拒，不因"系统自带"放行。
    #[test]
    fn decide_tool_validates_system_reuse_against_min() {
        let min = "24.9.0";
        // 系统版本与配置精确一致（含 v 前缀差异）且达标 → 复用
        let t = decide_tool(
            ToolId::Node,
            true,
            Some("24.22.0"),
            Some(&sys_inst("v24.22.0")),
            None,
            Some(min),
        );
        assert!(t.usable);
        assert!(matches!(t.source, ToolSource::System(_)));
        assert!(t.path_prefix.is_none(), "系统复用不贡献 PATH 前缀");
        // 系统版本一致但低于下限 → 拒（不写 lock，不标记可用）
        let t2 = decide_tool(
            ToolId::Node,
            true,
            Some("24.8.0"),
            Some(&sys_inst("24.8.0")),
            Some(&mng_inst("24.9.5")),
            Some(min),
        );
        assert!(!t2.usable, "复用系统也要过下限");
        assert!(t2.detail.is_some());
        // 系统版本与配置不一致 → 不复用，落到受管那份
        let t3 = decide_tool(
            ToolId::Node,
            true,
            Some("24.22.0"),
            Some(&sys_inst("22.19.0")),
            Some(&mng_inst("24.22.0")),
            Some(min),
        );
        assert!(matches!(t3.source, ToolSource::Managed(_)));
        // 没配置精确版本时不复用（可复现要求：配置说了才算）
        let t4 = decide_tool(ToolId::Node, true, None, Some(&sys_inst("24.22.0")), None, Some(min));
        assert!(matches!(t4.source, ToolSource::Missing));
    }

    /// 判据 (c) 的内核：某工具缺失不影响其他 —— decide_tool 对 Missing 输入
    /// 产出明确的 Missing 结论，且其余工具照常判定（resolve 的编排同理）。
    #[test]
    fn decide_tool_reports_missing_without_polluting_others() {
        let t = decide_tool(ToolId::Pnpm, false, Some("10.0.0"), None, None, Some("10.0.0"));
        assert!(matches!(t.source, ToolSource::Missing));
        assert!(!t.usable);
        assert!(t.detail.is_none(), "缺失不是错误，不需要 detail");
        // 另一个工具照常可用
        let ok = decide_tool(ToolId::Pnpm, false, None, None, Some(&mng_inst("10.5.0")), Some("10.0.0"));
        assert!(ok.usable);
    }

    /// check_min：Python 走 detect::PYTHON_MIN_VERSION 门槛；unknown 不判过低。
    #[test]
    fn check_min_covers_python_floor_and_unknown() {
        assert!(check_min(ToolId::Python, "3.14.7", None).is_ok());
        assert!(check_min(ToolId::Python, "3.10.0", None).is_ok(), "恰好等于门槛应放行");
        assert!(check_min(ToolId::Python, "3.8.10", None).is_err(), "低于 3.10 门槛要拒");
        // Node：unknown 不判过低；明确低于才拒
        assert!(check_min(ToolId::Node, "", Some("22.19.0")).is_ok());
        assert!(check_min(ToolId::Node, "21.7.3", Some("22.19.0")).is_err());
        // 没有下限的工具（dsh 自身）恒过
        assert!(check_min(ToolId::Dsh, "0.2.1-rc.5", None).is_ok());
    }

    /// versions_match：v 前缀与残缺段不制造"精确匹配"的假阴性。
    #[test]
    fn versions_match_ignores_v_prefix() {
        assert!(versions_match("v24.22.0", "24.22.0"));
        assert!(versions_match("24.22", "24.22.0"));
        assert!(!versions_match("24.22.0", "24.22.1"));
        assert!(!versions_match("", "24.22.0"));
    }
}
