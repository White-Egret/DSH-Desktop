//! `EnvOverlay`：一次 spawn 需要注入的环境增量（方案 §3.1 / §4.2）。
//!
//! 两类内容：
//! - `path_dirs`：要**前置**到 PATH 的目录（顺序即优先级，受管工具在前）；
//! - `vars`：其它环境变量（MSVC 的 `LIB` / `INCLUDE`，将来 Linux 版的 shims 语义等）。
//!
//! 为什么是「增量」而不是完整环境：子进程的 PATH 基线是**当前进程**的 PATH
//! （它已经承载了用户系统 PATH + 注册表 PATH 的旧快照问题，见 detect.rs 的长注释），
//! 本模块只决定「谁排在最前面」与「补哪些变量」。接管整个环境既没必要也更危险。
//!
//! MSVC 的 overlay 也要合进来（方案 §4.4）：`cargo build` / build script 需要
//! `link.exe` 与 `LIB`，这正是 `SystemProbe::env_overlay` 存在的理由。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::toolchain::resolve::{ResolvedTool, ToolSource};

/// 一次 spawn 的环境增量。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvOverlay {
    /// 要前置到 PATH 的目录（顺序即优先级）。
    pub path_dirs: Vec<PathBuf>,
    /// 其它环境变量。键的大小写原样保留；合并时同键**先到先得**
    /// （Windows 环境变量名不区分大小写，比较用 ASCII 小写）。
    pub vars: Vec<(String, String)>,
}

impl EnvOverlay {
    /// 是否什么都不注入。调用方（spawn 门面）用它走「零改动」快捷路径。
    #[allow(dead_code)] // 模块契约的一部分；当前 spawn 路径直接拼 PATH，不单独判空
    pub fn is_empty(&self) -> bool {
        self.path_dirs.is_empty() && self.vars.is_empty()
    }

    /// 合并 `other` 进来：`other` 排在**后面**（先合并的优先级更高）。
    ///
    /// 顺序约定：门面按「受管工具 → MSVC → …」的次序合并，受管 node 的目录
    /// 必须排在 MSVC bin 之前 —— npm 的 shim 脚本解析 node 时就该命中受管那份。
    pub fn merge(&mut self, other: EnvOverlay) {
        for d in other.path_dirs {
            if !self.path_dirs.iter().any(|x| dirs_equal(x, &d)) {
                self.path_dirs.push(d);
            }
        }
        for (k, v) in other.vars {
            if !self.vars.iter().any(|(ek, _)| ek.eq_ignore_ascii_case(&k)) {
                self.vars.push((k, v));
            }
        }
    }

    /// 前置一个目录（去重）。spawn_env 逐工具贡献时用它。
    pub fn push_dir(&mut self, d: PathBuf) {
        if !self.path_dirs.iter().any(|x| dirs_equal(x, &d)) {
            self.path_dirs.push(d);
        }
    }

    /// 读一个变量（大小写不敏感）。
    #[allow(dead_code)] // 模块契约的一部分（单测与将来的 spawn 诊断用）
    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    /// 生成完整的 PATH 值：本 overlay 的目录在最前，接现有 PATH（去重、保序）。
    ///
    /// `existing = None`（进程没有 PATH）时只留 overlay 自己的目录 —— 子进程至少
    /// 能解析到受管工具；基线 PATH 由调用方用 `std::env::var_os("PATH")` 传入。
    pub fn path_with_prefix(&self, existing: Option<&std::ffi::OsStr>) -> OsString {
        let mut dirs: Vec<PathBuf> = self.path_dirs.clone();
        if let Some(e) = existing {
            dirs.extend(std::env::split_paths(e));
        }
        let mut seen: Vec<String> = Vec::with_capacity(dirs.len());
        let mut out: Vec<PathBuf> = Vec::with_capacity(dirs.len());
        for d in dirs {
            let key = dir_key(&d);
            if key.is_empty() || seen.contains(&key) {
                continue;
            }
            seen.push(key);
            out.push(d);
        }
        std::env::join_paths(&out).unwrap_or_default()
    }
}

/// 目录比较键：去首尾空白与尾部分隔符 + 小写（Windows 路径大小写不敏感）。
/// 与 detect.rs 的 `dir_key` 同一套语义（那边是私有函数，这里按同一规则实现，
/// 两处注释互为镜像）。
fn dir_key(d: &Path) -> String {
    d.to_string_lossy()
        .trim()
        .trim_end_matches(['\\', '/'])
        .to_lowercase()
}

fn dirs_equal(a: &Path, b: &Path) -> bool {
    dir_key(a) == dir_key(b)
}

/// 把七项工具的解析结论 + MSVC 的 overlay 合成一次 spawn 的环境增量。
///
/// 判据 (a)：受管工具的 PATH 前缀在前，MSVC 的 bin 在后，现有 PATH 最后 ——
/// npm 的 shim 解析 node 时应先命中受管那份，link.exe / LIB 不与它们竞争。
/// 判据 (b)：只有系统工具（全部 `path_prefix = None`）时，结果只含 MSVC 的贡献。
/// 判据 (c)：某工具缺失/被拒不影响其他 —— 各工具独立贡献，`usable` 为假直接跳过。
///
/// MSVC 的 overlay 必须合进来（方案 §4.4）：`cargo build` / build script 需要
/// `link.exe` 与 `LIB`，这正是 `SystemProbe::env_overlay` 存在的理由。
pub fn build_env(tools: &[ResolvedTool], msvc_overlay: EnvOverlay) -> EnvOverlay {
    let mut ov = EnvOverlay::default();
    for t in tools {
        if !t.usable {
            continue;
        }
        if matches!(t.source, ToolSource::Managed(_)) {
            if let Some(dir) = t.path_prefix.as_ref() {
                ov.push_dir(dir.clone());
            }
        }
    }
    ov.merge(msvc_overlay);
    ov
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 判据 (a)：overlay 的目录必须排在现有 PATH **前面**（顺序即优先级），
    /// 且重复目录（大小写 / 尾分隔符差异）不产生第二份。
    #[test]
    fn path_with_prefix_puts_overlay_dirs_first_and_dedupes() {
        let mut ov = EnvOverlay::default();
        ov.path_dirs = vec![
            PathBuf::from(r"D:\toolchain\tools\node\24.22.0"),
            PathBuf::from(r"D:\toolchain\tools\msvc\bin"),
        ];
        let existing = r"C:\Windows\system32;d:\TOOLCHAIN\tools\node\24.22.0\;C:\Windows";
        let joined = ov
            .path_with_prefix(Some(std::ffi::OsStr::new(existing)))
            .to_string_lossy()
            .to_string();
        let parts: Vec<&str> = joined.split(';').collect();
        // overlay 两个目录在最前，且与 existing 里重复的那个（大小写+尾分隔符差异）只出现一次
        assert_eq!(parts[0], r"D:\toolchain\tools\node\24.22.0");
        assert_eq!(parts[1], r"D:\toolchain\tools\msvc\bin");
        // existing 3 项里被去重 1 项 → 余 2 项接在后面
        assert_eq!(parts.len(), 4, "重复目录应被去重：{joined}");
        assert_eq!(parts[2], r"C:\Windows\system32");
        assert_eq!(parts[3], r"C:\Windows");
    }

    /// 现有 PATH 为空时：只留 overlay 自己的目录（不能返回空串，否则子进程连受管工具都找不到）。
    #[test]
    fn path_with_prefix_without_existing_keeps_overlay_dirs() {
        let mut ov = EnvOverlay::default();
        ov.path_dirs = vec![PathBuf::from(r"D:\toolchain\tools\node\24.22.0")];
        let joined = ov.path_with_prefix(None).to_string_lossy().to_string();
        assert_eq!(joined, r"D:\toolchain\tools\node\24.22.0");
        // 空 overlay + 空 existing = 空 PATH（调用方不该走到这里，但行为要确定）
        assert_eq!(EnvOverlay::default().path_with_prefix(None).to_string_lossy(), "");
    }

    /// merge：先合并的优先级更高（受管在前），路径去重、变量同键先到先得。
    #[test]
    fn merge_keeps_first_writer_for_dirs_and_vars() {
        let mut base = EnvOverlay::default();
        base.path_dirs = vec![PathBuf::from(r"D:\managed\node")];
        base.vars = vec![("LIB".to_string(), r"D:\msvc\lib".to_string())];

        let mut other = EnvOverlay::default();
        other.path_dirs = vec![
            PathBuf::from(r"D:\managed\node\"),   // 同目录（尾分隔符差异）：不该重复
            PathBuf::from(r"D:\managed\msvc\bin"),
        ];
        other.vars = vec![
            ("lib".into(), r"D:\elsewhere\lib".into()), // 同键不同大小写：先到先得
            ("INCLUDE".into(), r"D:\msvc\include".into()),
        ];
        base.merge(other);

        assert_eq!(base.path_dirs.len(), 2, "重复目录不应合并进来");
        assert_eq!(base.path_dirs[0], PathBuf::from(r"D:\managed\node"));
        assert_eq!(base.path_dirs[1], PathBuf::from(r"D:\managed\msvc\bin"));
        assert_eq!(base.var("LIB"), Some(r"D:\msvc\lib"), "LIB 应先到先得");
        assert_eq!(base.var("INCLUDE"), Some(r"D:\msvc\include"));
        assert_eq!(base.vars.len(), 2);
    }

    /// 判据 (b) 的直接前置：空 overlay 必须是「零注入」，调用方据此走原行为快捷路径。
    #[test]
    fn empty_overlay_is_noop() {
        let ov = EnvOverlay::default();
        assert!(ov.is_empty());
        assert_eq!(ov.var("LIB"), None);
        // merge 空 overlay 也不改变原状
        let mut base = EnvOverlay::default();
        base.path_dirs = vec![PathBuf::from(r"D:\x")];
        let before = base.clone();
        base.merge(EnvOverlay::default());
        assert_eq!(base, before);
    }

    // ---------- build_env（resolve 的环境合成判据 a/b/c） ----------

    use crate::toolchain::resolve::ToolSource;
    use crate::toolchain::{Scope, ToolId as RId};

    fn tool(id: RId, usable: bool, prefix: Option<&str>, managed: bool) -> ResolvedTool {
        ResolvedTool {
            id,
            scope: if managed { Scope::Managed } else { Scope::ProbeOnly },
            source: match (managed, prefix) {
                (true, Some(p)) => ToolSource::Managed(PathBuf::from(p)),
                (false, _) => ToolSource::Probe,
                (true, None) => ToolSource::Missing,
            },
            version: None,
            usable,
            detail: None,
            path_prefix: prefix.map(PathBuf::from),
        }
    }

    /// 判据 (a)：受管工具的目录在前、MSVC 在后、顺序即优先级。
    #[test]
    fn build_env_orders_managed_before_msvc() {
        let tools = vec![
            tool(RId::Node, true, Some(r"D:\tc\tools\node\24.22.0"), true),
            tool(RId::Pnpm, true, Some(r"D:\tc\tools\pnpm\11.0.0"), true),
        ];
        let mut msvc = EnvOverlay::default();
        msvc.path_dirs = vec![PathBuf::from(r"D:\MSVC\14.x\bin\HostX64\x64")];
        msvc.vars = vec![("LIB".into(), r"D:\MSVC\14.x\lib\x64".into())];
        let ov = build_env(&tools, msvc);
        assert_eq!(
            ov.path_dirs,
            vec![
                PathBuf::from(r"D:\tc\tools\node\24.22.0"),
                PathBuf::from(r"D:\tc\tools\pnpm\11.0.0"),
                PathBuf::from(r"D:\MSVC\14.x\bin\HostX64\x64"),
            ],
            "受管在前、MSVC 在后"
        );
        assert_eq!(ov.var("LIB").unwrap(), r"D:\MSVC\14.x\lib\x64");
    }

    /// 判据 (b)：只有系统工具（无受管安装）时，overlay 只含 MSVC 的贡献。
    #[test]
    fn build_env_with_only_system_tools_is_msvc_only() {
        let tools = vec![
            tool(RId::Node, true, None, false),      // 系统复用：path_prefix = None
            tool(RId::Pnpm, true, None, false),
            tool(RId::Dsh, true, None, false),
            tool(RId::Python, true, None, false),
        ];
        let mut msvc = EnvOverlay::default();
        msvc.path_dirs = vec![PathBuf::from(r"D:\MSVC\bin")];
        let ov = build_env(&tools, msvc);
        assert_eq!(ov.path_dirs, vec![PathBuf::from(r"D:\MSVC\bin")], "只应含 MSVC");
        assert!(ov.vars.is_empty());
    }

    /// 判据 (c)：某工具缺失/被拒不影响其他工具的贡献。
    #[test]
    fn build_env_skips_unusable_tools_without_touching_others() {
        let tools = vec![
            tool(RId::Node, false, Some(r"D:\tc\node"), true), // 被拒（低于下限）
            tool(RId::Python, true, Some(r"D:\tc\python"), true),
            tool(RId::Msvc, false, None, false),
        ];
        let ov = build_env(&tools, EnvOverlay::default());
        assert_eq!(ov.path_dirs, vec![PathBuf::from(r"D:\tc\python")], "被拒的不贡献，其余照常");
    }
}
