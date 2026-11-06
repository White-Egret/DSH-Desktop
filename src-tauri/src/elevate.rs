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

/// 通用提权执行器（OnlyOffice 的安装器/配置合并/卸载器共用，§3.3 ⑤ 的
/// `run_elevated_office_step` 落在这里，因为它只是骨架、不含任何 office 语义）。
///
/// 流程与 [`remove_paths_elevated`] 同款：私有临时目录放脚本与结果文件
/// （结果文件先建空文件作为「脚本写完了」的信号）→ `ShellExecuteExW` + runas
/// → 等结果文件。差异有三处：
///
/// 1. **脚本文本由调用方给**。调用方必须自己保证内容可信（office.rs 里的脚本
///    全部由常量 + 本模块写下的路径拼成）；本函数不再设白名单 —— 白名单挡的
///    是「调用方传错路径」，而这里没有路径参数可传错。
/// 2. **超时由调用方给**。OnlyOffice 安装器要跑几分钟到几十分钟，120 秒那套
///    默认值对它毫无意义。
/// 3. **结果路径以字面量拼进脚本，并拿到提权 powershell 的进程句柄**：
///    - 真机事故 2026-11-06：原来把结果路径放进 `$env:DSH_ELEVATE_RESULT`，
///      但 UAC 提权出的进程**不继承**调用方运行期 `set_var` 的环境变量 ——
///      卸载器跑成功了，脚本里的 `Set-Content -LiteralPath $env:…` 却两次都
///      因为路径为空而失败，结果文件 0 字节，轮询干等 30 分钟超时。
///    - 有了进程句柄，脚本崩掉（什么都没写）也能在 powershell 退出的那一刻
///      立刻失败，不用等满超时。
///
/// 返回值三态与 [`remove_paths_elevated`] 相同：`None` = 没能提权；
/// `Some(text)` = 提权了（text 是结果文件内容，可能为空串 = 脚本没写回）。
/// 结果文件由 PowerShell 以 `-Encoding UTF8` 写出（带 BOM），返回前剥掉 BOM。
pub fn run_elevated(script_body: &str, timeout: std::time::Duration) -> Option<String> {
    let dir = private_temp_dir()?;
    let script = dir.join("run.ps1");
    let result = dir.join("result.txt");
    if std::fs::write(&result, b"").is_err() {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }
    // 结果路径以单引号字面量进脚本文本：脚本文本里的 `$env:DSH_ELEVATE_RESULT`
    // 被原样替换成 `'C:\...\result.txt'`。路径是本函数刚创建的随机私有目录
    // （ASCII、无引号），字面量拼接没有注入面。
    let result_literal = format!("'{}'", result.to_string_lossy().replace('\'', "''"));
    let body = script_body.replace("$env:DSH_ELEVATE_RESULT", &result_literal);
    // UTF-8 **带 BOM** 落盘（P0 实测教训，§10.2：PS 5.1 把无 BOM 的 UTF-8 按 GBK
    // 读，脚本里出现非 ASCII（比如中文安装目录）就直接破坏解析）。纯 ASCII 的
    // 脚本加 BOM 也无害。
    let mut bytes = Vec::with_capacity(body.len() + 3);
    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    bytes.extend_from_slice(body.as_bytes());
    if std::fs::write(&script, bytes).is_err() {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }

    let Some(handle) = win::launch_runas_handle(&script, &result) else {
        // 用户在 UAC 点了「否」：如实报「没能提权」，不重试、不追问。
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    };

    // 等待循环全部收进 win 模块（Win32 符号不外泄）；`check` 闭包查「结果文件
    // 已写回」。`written = false` 的两种可能：超时强杀，或提权 PS 退出但脚本
    // 崩了没写回 —— 两种都必须如实报失败，绝不当成成功。
    let check = || match std::fs::metadata(&result) {
        Ok(meta) => meta.len() > 0,
        Err(_) => false,
    };
    let written = win::wait_process_or_kill(handle, timeout, check);
    let text = if written {
        std::fs::read_to_string(&result).unwrap_or_default()
    } else {
        String::new()
    };
    win::close_handle(handle);
    let _ = std::fs::remove_dir_all(&dir);
    // 剥掉 PowerShell 写结果文件时带的 UTF-8 BOM，调用方拿到的是干净文本
    Some(text.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(text))
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
    //   - `CloseHandle` 在 `Win32::Foundation`；
    //   - `WaitForSingleObject` / `TerminateProcess` 在 `Win32::System::Threading`；
    //   - `WAIT_OBJECT_0` 在 windows-sys 0.59 里**不在** Threading（CI 第 2 轮实测
    //     E0432），干脆用本地常量——Win32 约定它的值就是 0；
    //   - `SWC_NORMAL` **不存在**（SW_SHOWNORMAL 没有对应的 windows-sys 常量），
    //     所以 nShow 直接用整数 1，见下面的 SW_SHOWNORMAL。
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SHELLEXECUTEINFOW, SEE_MASK_NOCLOSEPROCESS};
    use std::time::{Duration, Instant};

    /// `SW_SHOWNORMAL`：以「正常窗口」方式显示。
    /// 不能用 `SWC_NORMAL`（不存在），这个值与 Win32 的 SW_SHOWNORMAL 相同。
    const SW_SHOWNORMAL: i32 = 1;
    /// `WaitForSingleObject` 的返回值：对象已触发（进程退出）。
    const WAIT_OBJECT_0: u32 = 0;

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
    /// ⚠ 结果文件路径**不再经环境变量**传给脚本（`launch_runas_handle` 的注释里有
    /// 真机事故）：UAC 提权出的进程**不继承**调用方运行期 `set_var` 的环境变量，
    /// 脚本里 `$env:DSH_ELEVATE_RESULT` 拿到的是空。现在由 [`run_elevated`] 把结果
    /// 路径作为**字面量**拼进脚本文本（路径是刚建的随机私有目录，ASCII、无引号）。
    ///
    /// 返回值：`Some(句柄)` = 提权成功，句柄指向提权的 powershell 进程（调用方负责
    /// CloseHandle，可 WaitForSingleObject）；`None` = 用户点了「否」或 UAC 不可用。
    pub(super) fn launch_runas_handle(script: &Path, _result: &Path) -> Option<isize> {
        // 缓冲区必须在调用期间活着（SHELLEXECUTEINFOW 只读指针）—— 先算好再借用。
        let verb = wide("runas");
        let file = wide("powershell.exe");
        let params = wide(&format!(
            "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"",
            script.to_string_lossy()
        ));
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        // SEE_MASK_NOCLOSEPROCESS：把提权 powershell 的进程句柄留在 info.hProcess 里。
        // 有了它才能「等脚本进程退出」—— 脚本崩掉（没写结果文件）时立刻失败，
        // 而不是干等结果文件超时（真机事故 2026-11-06：卸载流程干等 30 分钟）。
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        info.hwnd = std::ptr::null_mut();

        // SAFETY: 结构体零初始化后填齐了 cbSize / fMask / 三个以 NUL 结尾的字符串指针
        // 与 nShow；三个 wide() 缓冲区在整个调用期间都在作用域内活着。
        // hwnd 传 null —— 提权进程与本进程没有窗口属主关系，给错的 hwnd 会让 UAC 对话框
        // 找不到前台窗口（真机上就表现为「UAC 弹在别的窗口后面」）。
        let ok = unsafe { ShellExecuteExW(&mut info) } != 0;
        if !ok {
            return None;
        }
        let h = info.hProcess;
        if h.is_null() {
            None
        } else {
            Some(h as isize)
        }
    }

    /// 旧入口：不关心进程句柄的调用方（py_bridge 的删除流程）用这个。
    pub(super) fn launch_runas(script: &Path, result: &Path) -> bool {
        let h = launch_runas_handle(script, result);
        if let Some(h) = h {
            // 调用方不等进程：立刻关掉句柄，别泄漏
            unsafe { CloseHandle(h as _) };
        }
        h.is_some()
    }

    /// 等待提权 powershell 退出，期间每个轮询周期调一次 `check`（调用方用它查
    /// 结果文件是否已写回）。语义：
    ///   - `check()` 命中，或进程退出后缓冲期内 `check()` 命中 → `true`；
    ///   - 超时 → 强杀提权进程并返回 `false`；
    ///   - 进程退出但缓冲期内 `check()` 仍不命中（脚本崩了没写回）→ `false`。
    /// 调用方拿到 `false` 必须**如实报失败**，绝不当成成功。
    pub(super) fn wait_process_or_kill(
        handle: isize,
        timeout: Duration,
        mut check: impl FnMut() -> bool,
    ) -> bool {
        let started = Instant::now();
        loop {
            if check() {
                return true;
            }
            let code = unsafe { WaitForSingleObject(handle as _, 200) };
            if code == WAIT_OBJECT_0 {
                // 给 Set-Content 半秒钟落盘缓冲，再让调用方最后确认一次
                std::thread::sleep(Duration::from_millis(400));
                return check();
            }
            if started.elapsed() >= timeout {
                // 超时：强杀提权脚本。绝不因为「没拿到结果」就当成成功。
                unsafe { TerminateProcess(handle as _, 1) };
                unsafe { WaitForSingleObject(handle as _, 2000) };
                return false;
            }
        }
    }

    pub(super) fn close_handle(handle: isize) {
        unsafe { CloseHandle(handle as _) };
    }
}

#[cfg(not(windows))]
mod win {
    use super::*;
    use std::time::Duration;
    pub(super) fn launch_runas(_script: &Path, _result: &Path) -> bool {
        false
    }
    pub(super) fn launch_runas_handle(_script: &Path, _result: &Path) -> Option<isize> {
        None
    }
    pub(super) fn wait_process_or_kill(
        _handle: isize,
        _timeout: Duration,
        _check: impl FnMut() -> bool,
    ) -> bool {
        false
    }
    pub(super) fn close_handle(_handle: isize) {}
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
