//! `state.json` / `lock.json` 的读写（方案 §2.1 / §6.1）。
//!
//! - `state.json`：安装状态 + 失败现场 + 探测缓存（MSVC 环境、上游约束指纹）。
//!   它只是**加速**与**记账**，任何缓存值在使用前都必须重新校验有效性
//!   （VS 升级、目录被删都会让它过期，方案「运行期要能自愈」一节）。
//! - `lock.json`：已解析版本 + 来源约束（`min_version_at_install` /
//!   `constraints_source`），供「锁定版本失效可发现」使用（方案 §10）。
//!
//! 两个文件都放在受管根目录（`%LOCALAPPDATA%\com.dsh.desktop\toolchain\`），
//! 刻意与 `~/.dsh` 分离：那是 DSH 自己的家目录，混进去会让备份逻辑把几 GB
//! 工具一起带走（方案 §2.1）。读写失败一律降级为「无缓存」，绝不阻断主流程。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tauri::Manager;

/// `state.json` 的形态。字段都是 Option：旧文件缺字段 = 没缓存，不是错误。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ToolchainState {
    /// MSVC 环境缓存：挑中的 MSVC 工具集版本目录 + VsDevCmd 算出的 LIB/INCLUDE。
    /// 每次使用前必须校验 `link.exe` 还在（方案 §3.2 第 5 步）。
    pub msvc_cache: Option<MsvcCache>,
    /// 上游约束的缓存（按 package.json 内容指纹）。
    pub upstream_cache: Option<UpstreamCache>,
    /// 各工具的安装状态（方案 §6.1 的五态状态机 + 失败现场）。
    pub tools: BTreeMap<String, ToolState>,
}

/// MSVC 环境缓存。
///
/// 每个目录都显式落盘，而不是从 link_exe 路径反推：反推省三个字段，
/// 却让「缓存结构」绑死在「link.exe 恰好在 bin\<host>\<target> 下」这个布局假设上。
/// 有效性校验只看 `link_exe`（VS 升级会换版本目录，link.exe 一不在就整体重算）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MsvcCache {
    /// VS 安装根（vswhere 查到的）。根变了（卸了重装到别处）缓存作废。
    pub vs_root: String,
    /// 挑中的 MSVC 工具集版本目录名（形如 `14.x.yyyyy` —— 缓存里出现它没问题，
    /// 它是**运行时读到的值**，不是写死的代码常量）。
    pub msvc_version: String,
    /// 挑中版本里 link.exe 的完整路径（有效性校验就看它）。
    pub link_exe: String,
    /// `<版本>\bin\<host>\<target>`（PATH 前置用）。
    pub bin_dir: String,
    /// `<版本>\lib\<target>`（LIB 的 MSVC 部分）。
    pub lib_dir: String,
    /// `<版本>\include`（INCLUDE 的 MSVC 部分）。
    pub include_dir: String,
    /// VsDevCmd/vcvarsall 算出的 LIB（Windows Kits 部分；空 = 当时没算到）。
    pub sdk_lib: String,
    /// VsDevCmd/vcvarsall 算出的 INCLUDE（空 = 当时没算到）。
    pub sdk_include: String,
}

/// 上游约束缓存：package.json 内容指纹 → 读出的约束。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpstreamCache {
    /// package.json 内容的 SHA-256（内容指纹；变了就重读）。
    pub fingerprint: String,
    /// 读出的约束（与 UpstreamRequirements 同构）。
    pub node_min: Option<String>,
    pub pnpm_min: Option<String>,
    pub source: String,
}

/// 单个工具的安装状态（方案 §6.1）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ToolState {
    /// missing / installed / failed（v1 不做 Locked(pin)）。
    pub state: String,
    pub version: String,
    /// 安装当时的约束下限与来源（配合 lock.json 判断「锁定版本是否已不合规」）。
    pub min_version_at_install: String,
    pub constraints_source: String,
    pub path: String,
    pub installed_at: String,
    pub last_error: Option<String>,
}

/// lock.json 的形态：受管安装的落地记录（URL + 哈希 + 时间）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ToolchainLock {
    /// tool id → 落地记录。
    pub entries: BTreeMap<String, LockEntry>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LockEntry {
    pub version: String,
    pub url: String,
    pub sha256: String,
    /// 安装当时生效的约束下限与来源（方案 §1.1b 规则 2）。
    pub min_version_at_install: String,
    pub constraints_source: String,
    pub installed_at: String,
}

/// 受管根目录（`%LOCALAPPDATA%\com.dsh.desktop\toolchain\`）。由 AppHandle 解析，
/// 模块内部只收显式路径（纯函数可测）。
pub fn toolchain_root(app: &tauri::AppHandle) -> PathBuf {
    app.path()
        .app_local_data_dir()
        .unwrap_or_else(|_| PathBuf::from("toolchain-fallback"))
        .join("toolchain")
}

/// `state.json` 的路径。
pub fn state_path(root: &Path) -> PathBuf {
    root.join("state.json")
}

/// `lock.json` 的路径。
pub fn lock_path(root: &Path) -> PathBuf {
    root.join("lock.json")
}

/// 读 state.json；文件缺失 / 损坏 → 默认值（缓存丢了就重算，绝不报错阻断）。
pub fn load_state(root: &Path) -> ToolchainState {
    read_json(&state_path(root)).unwrap_or_default()
}

/// 写 state.json。写失败只影响缓存（下次重算），调用方不因它失败。
pub fn save_state(root: &Path, state: &ToolchainState) {
    let _ = write_json(&state_path(root), state);
}

/// 读 lock.json。
pub fn load_lock(root: &Path) -> ToolchainLock {
    read_json(&lock_path(root)).unwrap_or_default()
}

/// 写 lock.json。
pub fn save_lock(root: &Path, lock: &ToolchainLock) {
    let _ = write_json(&lock_path(root), lock);
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(value)?;
    // 临时文件 + rename：写一半被杀不会留下半个 JSON（解析端有兜底，这里再加一道）
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-toolchain-store-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// state.json 往返：写入再读回，字段不丢。
    #[test]
    fn state_round_trips() {
        let root = temp_root("roundtrip");
        let mut st = ToolchainState::default();
        st.msvc_cache = Some(MsvcCache {
            vs_root: r"D:\VS".into(),
            msvc_version: "14.99.99999".into(),
            link_exe: r"D:\VS\VC\Tools\MSVC\14.99.99999\bin\HostX64\x64\link.exe".into(),
            bin_dir: r"D:\VS\VC\Tools\MSVC\14.99.99999\bin\HostX64\x64".into(),
            lib_dir: r"D:\VS\VC\Tools\MSVC\14.99.99999\lib\x64".into(),
            include_dir: r"D:\VS\VC\Tools\MSVC\14.99.99999\include".into(),
            sdk_lib: r"D:\SDK\Lib\v1".into(),
            sdk_include: r"D:\SDK\Include\v1".into(),
        });
        st.tools.insert(
            "node".into(),
            ToolState {
                state: "installed".into(),
                version: "24.99.0".into(),
                min_version_at_install: "22.19.0".into(),
                constraints_source: "dsh 0.2.x engines".into(),
                path: r"D:\toolchain\tools\node\24.99.0".into(),
                installed_at: "2026-01-01T00:00:00Z".into(),
                last_error: None,
            },
        );
        save_state(&root, &st);
        let back = load_state(&root);
        let cache = back.msvc_cache.expect("msvc 缓存应读回");
        assert_eq!(cache.msvc_version, "14.99.99999");
        let node = back.tools.get("node").expect("node 状态应读回");
        assert_eq!(node.version, "24.99.0");
        assert_eq!(node.min_version_at_install, "22.19.0");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 缺失 / 损坏的 state.json 一律降级为默认值（缓存丢失 = 重算，不是错误）。
    #[test]
    fn missing_or_corrupt_state_falls_back_to_default() {
        let root = temp_root("corrupt");
        assert_eq!(load_state(&root), ToolchainState::default(), "文件缺失应得默认值");
        std::fs::write(state_path(&root), b"{ not json").unwrap();
        assert_eq!(load_state(&root), ToolchainState::default(), "损坏文件应得默认值");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// lock.json 往返 + 缺失降级。
    #[test]
    fn lock_round_trips_and_falls_back() {
        let root = temp_root("lock");
        let mut lock = ToolchainLock::default();
        lock.entries.insert(
            "node".into(),
            LockEntry {
                version: "24.99.0".into(),
                url: "https://nodejs.org/dist/v24.99.0/node-v24.99.0-win-x64.zip".into(),
                sha256: "ab".repeat(32),
                min_version_at_install: "22.19.0".into(),
                constraints_source: "engines.node".into(),
                installed_at: "2026-01-01T00:00:00Z".into(),
            },
        );
        save_lock(&root, &lock);
        let back = load_lock(&root);
        assert_eq!(back.entries.get("node").unwrap().sha256, "ab".repeat(32));
        assert_eq!(load_lock(&temp_root("none")).entries.len(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 路径拼装：state.json / lock.json 都落在受管根目录本身（方案 §2.1）。
    #[test]
    fn paths_live_in_root() {
        let root = Path::new(r"D:\x\toolchain");
        assert_eq!(state_path(root), Path::new(r"D:\x\toolchain\state.json"));
        assert_eq!(lock_path(root), Path::new(r"D:\x\toolchain\lock.json"));
    }
}
