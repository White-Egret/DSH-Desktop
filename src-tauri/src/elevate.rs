// ---------- 提权删除（仅 Windows） ----------
//
// 存在的理由（真机现场 2026-10-07）：pnpm 由 corepack / pnpm 官方安装器装进
// `C:\Program Files\nodejs` 时，**当前用户没有删除权** —— 卸载时四个文件全部
// `拒绝访问 (os error 5)`，界面只能报「失败」。而当初 Node 能装进那个目录，
// 正是因为官方 MSI 走了 UAC 提权。同一个目录、同一类操作，卸载也该有同样的出路。
//
// 怎么做：`ShellExecuteExW` + `runas` 动词弹一次**正规的 UAC 对话框**（系统画的，
// 有 publisher、有取消按钮），提权后运行一个临时的 PowerShell 删除脚本；脚本把结果
// 逐行写进一个结果文件，主进程轮询到文件非空后读走，再删掉临时目录。
//
// 为什么不用「让用户自己去开管理员终端」：那是把这件事原样退回给用户，而用户恰好
// 不知道自己装的东西住在 `C:\Program Files\nodejs` —— 那条路径是我们查出来的，
// 不是他填的。
//
// ⚠ 与 config.rs 那条「本程序刻意不提权」的纪律**不冲突**：那里说的是**后台任务**
// （Node / Python 安装）不该被 UAC 弹窗卡住 —— 弹窗没人点，整个流程就停在那里。
// 这里是一次**用户主动点击的收尾动作**：用户就在界面前面等着，点「以管理员身份重试」
// 就是他要提权。两者的在场条件完全不同。

use std::path::Path;

/// 提权删除的结果。
#[derive(Debug, Default)]
pub struct ElevatedOutcome {
    /// 真的删掉了几个。
    pub removed: Vec<String>,
    /// 没删掉的（路径 + 原因）。
    pub failed: Vec<(String, String)>,
}

impl ElevatedOutcome {
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.failed.is_empty()
    }
}

/// 以管理员权限删除这些路径。
///
/// 返回值三态，**必须分开处理**（混起来就会出现「报成功、其实没删」）：
///   - `None`：**没能提权**（用户点了 UAC 的「否」、或 ShellExecuteEx 失败）；
///   - `Some(out)` 且 `out.is_empty()`：提权了，但脚本没写回结果（超时 / 脚本没跑起来）；
///   - `Some(out)`：提权了，`out` 里逐条列着删掉与没删掉的。
///
/// `paths` 在**进入提权之前**必须过一遍 [`is_safe_target`] 白名单：这段代码以管理员
/// 权限运行，参数又来自检测结果，一旦能构造出 `..`、通配符或任意路径，后果就不止删错文件。
pub fn remove_paths_elevated(paths: &[String]) -> Option<ElevatedOutcome> {
    if paths.is_empty() {
        return Some(ElevatedOutcome::default());
    }
    if !paths.iter().all(|p| is_safe_target(p)) {
        // 白名单没过 = 调用方传错了路径。这属于**代码缺陷**，不是用户能处理的问题，
        // 所以走 None（没能提权）这一支：宁可什么都不删，也不提权删来路不明的东西。
        return None;
    }

    // 私有临时目录装两样东西：删除脚本与结果文件。私有（随机名 + create_dir 排他）
    // 是因为脚本以**管理员**身份运行 —— 中间若被同用户进程抢先放一个同名脚本，
    // 那次提权就等于替它执行了任意代码。
    let dir = match private_temp_dir() {
        Some(d) => d,
        None => return None,
    };
    let script = dir.join("remove.ps1");
    let result = dir.join("result.txt");
    // 结果文件先建一个空文件：主进程以「非空」作为「脚本写完了」的完成信号。
    if std::fs::write(&result, b"").is_err() {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }
    if std::fs::write(&script, script_body(paths)).is_err() {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }

    if !win::launch_runas(&script, &result) {
        // 用户点了「否」—— 这是用户的决定，如实报「没能提权」，不重试、不追问。
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }

    // ShellExecuteExW + runas 是**异步**的（它不等待被启动的进程），
    // 所以这里必须自己轮询结果文件 —— 整个函数里唯一需要「等」的地方。
    let outcome = wait_for_result(&result, std::time::Duration::from_secs(120));
    let _ = std::fs::remove_dir_all(&dir);
    Some(outcome)
}

/// 结果文件里的一行：`OK<TAB>路径` 或 `ERR<TAB>路径<TAB>原因`。
fn parse_result(text: &str) -> ElevatedOutcome {
    let mut out = ElevatedOutcome::default();
    for line in text.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("OK"), Some(p), _) => out.removed.push(p.to_string()),
            (Some("ERR"), Some(p), Some(why)) => out.failed.push((p.to_string(), why.to_string())),
            // 认不出的行一律忽略：脚本在提权上下文里跑，解析失败绝不能让这里 panic
            // —— 那会连带崩掉整个卸载流程，比少认一行糟糕得多。
            _ => {}
        }
    }
    out
}

/// 轮询结果文件直到它被写入（脚本跑完）或超时。
fn wait_for_result(result: &Path, timeout: std::time::Duration) -> ElevatedOutcome {
    let started = std::time::Instant::now();
    loop {
        // 非空 = 脚本写完了（空文件是主进程预置的，所以「非空」就是完成信号）。
        if let Ok(meta) = std::fs::metadata(result) {
            if meta.len() > 0 {
                if let Ok(text) = std::fs::read_to_string(result) {
                    return parse_result(&text);
                }
            }
        }
        if started.elapsed() >= timeout {
            // 超时 = 脚本没写回结果。返回空结果，由调用方报「没能删掉」——
            // 绝不能因为「没拿到失败记录」就当成删干净了。
            return ElevatedOutcome::default();
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

/// 提权目标的安全白名单：**只接受**「文件名是 pnpm 脚本」或「`node_modules\pnpm` 包目录」。
///
/// 为什么这道闸在这里（而不是只靠更外层筛一遍）：这段代码以管理员权限运行，参数又
/// 来自检测结果 —— 一旦能构造出 `C:\Windows\System32\…`、带 `..` 的路径或通配符，
/// 后果就不止「删错文件」。
///
/// 按**文件名**判定，目录不参与判断：目录由 detect 查出来，文件名是我们写死的候选集。
/// 对外公开（`pub`）是为了让调用方**在提权之前**先查一遍、给出可读的错误 ——
/// 提权完才发现路径不对，那次 UAC 就白弹了。
pub fn is_safe_target(raw: &str) -> bool {
    let p = raw.trim();
    // `..` / 通配符一律拒绝（-LiteralPath 本来就不解释通配符，但先挡住更省事）
    if p.is_empty() || p.contains("..") || p.contains('*') || p.contains('?') {
        return false;
    }
    let lower = p.replace('/', "\\").to_lowercase();
    let name = lower.rsplit('\\').next().unwrap_or("");
    const OK_NAMES: [&str; 4] = ["pnpm", "pnpm.cmd", "pnpm.ps1", "pnpm.exe"];
    if OK_NAMES.contains(&name) {
        return true;
    }
    // `node_modules\pnpm` 包目录（整棵删）
    lower.ends_with("\\node_modules\\pnpm")
}

/// 生成删除脚本。路径以 `-LiteralPath` 逐个传入，**不做任何字符串拼接解释**。
///
/// `-LiteralPath` 是这里的关键：`-Path` 会把 `[]`、`*` 当通配符解释；`-LiteralPath` 按
/// 字面处理，同时消掉「路径里的引号或 `$` 被 PowerShell 二次解释」这一类注入。
/// 单引号在 PowerShell 里没有转义机制，只能靠成对加倍（`''`）—— 见下面的 `single`。
fn script_body(paths: &[String]) -> String {
    let mut body = String::new();
    body.push_str("$ErrorActionPreference = 'SilentlyContinue'\n");
    body.push_str("$out = New-Object System.Collections.ArrayList\n");
    for p in paths {
        let single = p.replace('\'', "''");
        body.push_str(&format!(
            "$p = '{single}'\n\
             try {{\n\
             \x20 if (Test-Path -LiteralPath $p) {{\n\
             \x20   $i = Get-Item -LiteralPath $p -Force\n\
             \x20   if ($i.PSIsContainer) {{ Remove-Item -LiteralPath $p -Recurse -Force -ErrorAction Stop }}\n\
             \x20   else {{ Remove-Item -LiteralPath $p -Force -ErrorAction Stop }}\n\
             \x20   if (Test-Path -LiteralPath $p) {{ [void]$out.Add(\"ERR`t\" + $p + \"`tstill exists\") }}\n\
             \x20   else {{ [void]$out.Add(\"OK`t\" + $p) }}\n\
             \x20 }} else {{ [void]$out.Add(\"OK`t\" + $p) }}\n\
             }} catch {{ [void]$out.Add(\"ERR`t\" + $p + \"`t\" + $_.Exception.Message) }}\n"
        ));
    }
    // 结果文件路径经环境变量传入，不硬编码进脚本文本（少一处要转义的拼接）。
    body.push_str("$out -join \"`n\" | Set-Content -LiteralPath $env:DSH_ELEVATE_RESULT -Encoding UTF8\n");
    body
}

/// 随机私有临时目录。
///
/// 与 process.rs 里同名函数的理由完全一致（那边是防 MSI 下载物被抢名做符号链接），
/// 这里防的是「提权脚本被抢名替换」。目录名带随机值 + `create_dir` 排他创建。
fn private_temp_dir() -> Option<std::path::PathBuf> {
    let base = std::env::temp_dir();
    for attempt in 0u32..5 {
        let dir = base.join(format!("dsh-elevate-{}", random_suffix(attempt)));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Some(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// 目录名随机后缀（与 process.rs 的实现同款：splitmix64 变体，不引新依赖）。
fn random_suffix(counter: u32) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    let mut x = nanos ^ ((counter as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    format!("{:016x}", x)
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    // 这几个符号的真实位置（windows-sys 0.59 / CI 实测踩到）：
    //   - `ShellExecuteExW` 与 `SHELLEXECUTEINFOW` 在 `Win32::UI::Shell`，但后者**额外**
    //     需要 `Win32_System_Registry` feature（结构体带 hkeyClass 字段，被条件编译）；
    //   - `SEE_MASK_NOCLOSEPROCESS` 也在 `Win32::UI::Shell`，**不在** Foundation；
    //   - `SWC_NORMAL` **不存在**（SW_SHOWNORMAL 没有对应的 windows-sys 常量），
    //     所以 nShow 直接用整数 1，见下面的 SW_SHOWNORMAL。
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SHELLEXECUTEINFOW};

    /// `SW_SHOWNORMAL`：以「正常窗口」方式显示。
    /// 不能用 `SWC_NORMAL`（不存在），这个值与 Win32 的 SW_SHOWNORMAL 相同。
    const SW_SHOWNORMAL: i32 = 1;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// `ShellExecuteExW(powershell, -File <script>, "runas")` —— 系统弹正规 UAC。
    ///
    /// 为什么是 ShellExecuteEx 而不是「让用户自己去开管理员终端」：后者把这件事原样退回
    /// 给用户，而用户恰好不知道自己装的东西住在 `C:\Program Files\nodejs`。
    /// `runas` 动词是 Windows 官方给「以管理员身份运行」用的那一个 —— 对话框由系统绘制，
    /// 有发布者签名、有「取消」按钮，在任务管理器里也如实显示。
    ///
    /// 结果文件路径经**环境变量**传给脚本：环境块跟着进程启动一起过去，不需要把它
    /// 拼进命令行（少一处引号地狱）。用 `set_var` / 还原只影响本进程，
    /// ShellExecute 派生的进程继承的正是这份环境。
    ///
    /// 返回 false = 用户点了「否」或 UAC 不可用。
    pub(super) fn launch_runas(script: &Path, result: &Path) -> bool {
        // 缓冲区必须在调用期间活着（SHELLEXECUTEINFOW 只读指针）—— 先算好再借用。
        let verb = wide("runas");
        let file = wide("powershell.exe");
        let params = wide(&format!(
            "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"",
            script.to_string_lossy()
        ));
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        // 刻意**不**设 SEE_MASK_NOCLOSEPROCESS：整个流程不等待进程结束 ——
        // 我们轮询结果文件（那才是真正可靠的完成信号），而 runas 派生的是提权进程，
        // 拿到它的句柄也没有可等的东西。
        info.fMask = 0;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        info.hwnd = std::ptr::null_mut();

        let prev = std::env::var_os("DSH_ELEVATE_RESULT");
        std::env::set_var("DSH_ELEVATE_RESULT", result);
        // SAFETY: 结构体零初始化后填齐了 cbSize / fMask / 三个以 NUL 结尾的字符串指针
        // 与 nShow；三个 wide() 缓冲区在整个调用期间都在作用域内活着。
        // hwnd 传 null —— 提权进程与本进程没有窗口属主关系，给错的 hwnd 会让 UAC 对话框
        // 找不到前台窗口（真机上就表现为「UAC 弹在别的窗口后面」）。
        let ok = unsafe { ShellExecuteExW(&mut info) } != 0;
        match prev {
            Some(v) => std::env::set_var("DSH_ELEVATE_RESULT", v),
            None => std::env::remove_var("DSH_ELEVATE_RESULT"),
        }
        ok
    }
}

#[cfg(not(windows))]
mod win {
    use super::*;
    pub(super) fn launch_runas(_script: &Path, _result: &Path) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 提权白名单是**最后一道闸**：它一旦放过 `..`、通配符或任意路径，
    /// 后果就是「以管理员权限删掉了别的东西」。这里逐条钉住边界。
    #[test]
    fn is_safe_target_accepts_only_pnpm_files() {
        // 该放的：pnpm 的五种脚本 + 包目录
        for ok in [
            r"C:\Program Files\nodejs\pnpm",
            r"C:\Program Files\nodejs\pnpm.cmd",
            r"C:\Program Files\nodejs\pnpm.CMD",
            r"C:\Program Files\nodejs\pnpm.ps1",
            r"C:\Program Files\nodejs\pnpm.exe",
            r"C:\Program Files\nodejs\pnpm\pnpm.CMD",
            r"D:\Programs\npm\node_modules\pnpm",
        ] {
            assert!(is_safe_target(ok), "应当放行：{ok}");
        }
        // 不该放的：路径穿越、通配符、系统文件、空串
        for bad in [
            r"C:\Windows\System32\config\SAM",
            r"C:\Program Files\nodejs\..\..\Windows\System32\drivers\etc\hosts",
            r"C:\Program Files\nodejs\pnpm*",
            r"C:\Program Files\nodejs\pnpm?.cmd",
            r"C:\Program Files\nodejs\node.exe",
            r"C:\Program Files\nodejs\npm.cmd",
            r"C:\Program Files\nodejs\node_modules\corepack",
            "",
            "   ",
        ] {
            assert!(!is_safe_target(bad), "必须拒绝：{bad}");
        }
    }

    /// 大小写不敏感（NTFS 语义）：真机现场那个文件就叫 `pnpm.CMD`。
    #[test]
    fn is_safe_target_is_case_insensitive_like_ntfs() {
        assert!(is_safe_target(r"c:\program files\nodejs\PNPM.CMD"));
        assert!(is_safe_target(r"C:\PROGRAM FILES\NODEJS\PNPM.EXE"));
        // 斜杠写法也要认（路径来自不同来源，分隔符未必统一）
        assert!(is_safe_target("C:/Program Files/nodejs/pnpm.cmd"));
        assert!(!is_safe_target("C:/Program Files/nodejs/corepack.cmd"));
    }

    /// 结果文件的解析：认得的行收下，认不出的**忽略**（脚本在提权上下文里跑，
    /// 绝不能因为一行怪内容就 panic —— 那会连带崩掉整个卸载流程）。
    #[test]
    fn parse_result_ignores_unknown_lines_instead_of_panicking() {
        let text = "OK\tC:\\a\\pnpm.cmd\n\
                    这行是乱码\t随便\t什么\n\
                    ERR\tC:\\a\\pnpm.ps1\t拒绝访问\n\
                    \n\
                    OK\tC:\\a\\pnpm.exe\n";
        let out = parse_result(text);
        assert_eq!(
            out.removed,
            vec![r"C:\a\pnpm.cmd".to_string(), r"C:\a\pnpm.exe".to_string()]
        );
        assert_eq!(out.failed.len(), 1);
        assert_eq!(out.failed[0].1, "拒绝访问");
        assert!(!out.is_empty());
        // 空输入 = 什么都没删（上层据此报「没删掉」，而不是当成成功）
        assert!(parse_result("").is_empty());
    }
}
