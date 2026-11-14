//! 静态注册表：七样工具的身份与层级（方案 §1.1）。
//!
//! 这里只有「是什么、归哪层」，没有任何版本号与安装方式 —— 那些会变的东西
//! 分别住在运行时配置（`toolchain.json`）、上游约束解析（`resolve.rs`）与
//! 各自的安装/探测实现里。注册表管的是**能力的边界**：
//!
//! - `Managed`：能装、能卸，装进受管目录（`%LOCALAPPDATA%\com.dsh.desktop\toolchain\`）；
//! - `ProbeOnly`：系统共享组件，只探测不代装（自动卸载系统共享组件是桌面软件的大忌，
//!   方案 §6.2）。
//!
//! 层级写成两个 trait（`ManagedTool` / `SystemProbe`）而不是一个带 `Option` 方法的
//! trait（方案 §3.1）：给 Rust 调 `install()` 应该**编译不过**，而不是运行时返回
//! 「不支持」。

/// 七样工具的身份。Tauri 命令边界上传字符串（`toolchain::ToolId::parse`），
/// 模块内部一律用枚举，避免拼错字符串静默落空。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolId {
    Node,
    Pnpm,
    Dsh,
    Python,
    Rust,
    Msvc,
    Webview2,
}

impl ToolId {
    /// 稳定的字符串身份：前端 / 配置文件 / 日志都用它。
    pub const ALL: [ToolId; 7] = [
        ToolId::Node,
        ToolId::Pnpm,
        ToolId::Dsh,
        ToolId::Python,
        ToolId::Rust,
        ToolId::Msvc,
        ToolId::Webview2,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ToolId::Node => "node",
            ToolId::Pnpm => "pnpm",
            ToolId::Dsh => "dsh",
            ToolId::Python => "python",
            ToolId::Rust => "rust",
            ToolId::Msvc => "msvc",
            ToolId::Webview2 => "webview2",
        }
    }

    /// 解析外部传入的工具 id；不认识的返回 None（调用方报错，不猜）。
    pub fn parse(s: &str) -> Option<ToolId> {
        ToolId::ALL.into_iter().find(|t| t.as_str() == s.trim().to_ascii_lowercase())
    }
}

impl std::fmt::Display for ToolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 工具归哪一层（方案 §1.1 的表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 受管：能装、能卸，装进受管目录。
    Managed,
    /// 只探测：系统共享组件，本模块绝不安装或卸载。
    ProbeOnly,
}

/// 注册表条目。刻意薄：只有身份与层级。
///
/// 为什么不放版本语义 / 安装方式：Node 的下限来自上游约束解析（会随上游变化），
/// MSVC 的判据是「能找到一个可用的 link.exe」而不是版本号（方案 §1.1 的注）——
/// 这些都活着各自的实现里，注册表一旦记了快照就会过期。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpec {
    pub id: ToolId,
    pub scope: Scope,
}

/// 全部七项，顺序即方案 §1.1 的表（受管四项在前，探测三项在后）。
pub const REGISTRY: &[ToolSpec] = &[
    ToolSpec { id: ToolId::Node, scope: Scope::Managed },
    ToolSpec { id: ToolId::Pnpm, scope: Scope::Managed },
    ToolSpec { id: ToolId::Dsh, scope: Scope::Managed },
    ToolSpec { id: ToolId::Python, scope: Scope::Managed },
    ToolSpec { id: ToolId::Rust, scope: Scope::ProbeOnly },
    ToolSpec { id: ToolId::Msvc, scope: Scope::ProbeOnly },
    ToolSpec { id: ToolId::Webview2, scope: Scope::ProbeOnly },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// 注册表必须恰好七项、四受管三探测 —— 状态面板与安装拓扑序都按它展开，
    /// 多一项少一项都意味着范围失控（方案 §1.4 明确把 OnlyOffice 等挡在外面）。
    #[test]
    fn registry_has_exactly_seven_entries_with_four_managed_three_probe() {
        assert_eq!(REGISTRY.len(), 7, "REGISTRY 应恰好七项");
        let managed = REGISTRY.iter().filter(|s| s.scope == Scope::Managed).count();
        let probe = REGISTRY.iter().filter(|s| s.scope == Scope::ProbeOnly).count();
        assert_eq!(managed, 4, "受管应四项（node/pnpm/dsh/python）");
        assert_eq!(probe, 3, "探测应三项（rust/msvc/webview2）");

        // 身份不重复、覆盖全部枚举值
        let ids: Vec<ToolId> = REGISTRY.iter().map(|s| s.id).collect();
        for id in ToolId::ALL {
            assert_eq!(ids.iter().filter(|x| **x == id).count(), 1, "{id} 应恰出现一次");
        }
    }

    /// 字符串身份要能双向对上：前端 / 配置传什么就解析回什么，全部七项可回环。
    #[test]
    fn tool_id_parse_round_trips_all_variants() {
        for id in ToolId::ALL {
            assert_eq!(ToolId::parse(id.as_str()), Some(id), "{id} 回环失败");
            // 大小写不敏感（前端可能传 "Node"）
            let upper = id.as_str().to_ascii_uppercase();
            assert_eq!(ToolId::parse(&upper), Some(id), "{upper} 应大小写不敏感");
            assert_eq!(ToolId::parse(&format!(" {}", id.as_str())), Some(id), "应容忍空白");
        }
        assert_eq!(ToolId::parse("onlyoffice"), None, "范围外的工具必须被拒（方案 §1.4）");
        assert_eq!(ToolId::parse(""), None);
        assert_eq!(ToolId::parse("nodejs"), None, "拼写漂移不算命中");
    }
}
