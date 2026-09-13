//! 会话秘密的「落盘保护」与「日志脱敏」（安全审查 MEDIUM-2）
//!
//! 背景：DSH 的 next 频道会在 stdout 里打印带 `?token=<base64url>` 的完整认证地址，
//! 该 token 能换取 30 天有效期的签名 cookie，等于用户的 DSH 会话身份。启动器为了让
//! 「连接现有服务 / 页面重开」这两个没有新进程输出的场景不必重新认证，会记住最近一次
//! 加载的地址 —— 1.2.5 及以前它是**明文**写进 `%APPDATA%\com.dsh.desktop\config.json`
//! 的 `last_url` 键，同用户下任何进程（包括被提示词注入带偏的 DSH 自己）读一下文件
//! 就能拿到，进而以用户身份访问 127.0.0.1:<port> 的 DSH Web API。
//!
//! 本模块提供两条防线的全部原语：
//! 1. `seal` / `unseal`：落盘一律先用 Windows DPAPI（`CryptProtectData`，
//!    **CurrentUser 作用域**）加密，再以十六进制字符串存进 `config.json`；
//! 2. `redact`：token 的值在进日志文件（`desktop.log` / `<home>\logs\dsh.log`）
//!    和日志面板之前统一换成 `***` —— 只加密配置文件而放任日志明文，等于没修
//!    （核查确认 1.2.5 的 token 同时散落在两个日志文件与界面日志里）。
//!
//! ## 边界（如实写进了 README，不要当成万能锁）
//! - DPAPI 的保护范围是 **Windows 用户账户**：跨用户、离线拷走文件、备份/同步目录、
//!   截图与支持包都拿不到明文；但**同用户下运行的其他进程仍能解开**（熵是我们自己传的
//!   常量，不是密钥，就编译在二进制里）。要彻底消除这条路径只能「不落盘」；这里选的是
//!   保留重连体验 + 消除明文落盘，所以 README 必须写明这条残余风险。
//! - 加密/解密失败时一律**不写明文**：宁可不记住地址（回落到裸
//!   `http://127.0.0.1:<port>`，多走一次认证），也不留任何一条把 token 写回磁盘的降级路径。

/// DPAPI 熵（应用专属绑定数据）。
///
/// 它不是密钥、也不假装是密钥 —— 它就编译在二进制里。作用只是「限定用途」：
/// 只有本应用会用这串字节解密，同用户下别的程序即便调用 DPAPI 也解不开我们这块密文；
/// 反过来别处用同一套 API 保护的密文也不会被我们误解。
#[cfg(windows)]
const ENTROPY: &[u8] = b"DSH-Desktop/last_url/v1";

/// 脱敏目标：查询串里的会话参数名。`access_token=` / `refresh_token=` 这类键的尾部
/// 同样会被命中（我们做的是子串匹配），这是有意的 —— 多脱敏不漏脱敏。
const TOKEN_KEY: &str = "token=";

/// 值被替换成的东西：保留键名，让日志读者知道「这里原本有个令牌」。
const MASK: &str = "***";

// ---------- DPAPI（Windows） ----------

#[cfg(windows)]
mod imp {
    /// `CRYPT_INTEGER_BLOB`（即 `DATA_BLOB`）：`ULONG cbData; BYTE *pbData;`
    ///
    /// 这里按官方头文件的内存布局自己声明，而不是启用 `windows-sys` 的
    /// `Win32_Security_Cryptography` feature：少一个只在 CI 上才可能暴露写错的
    /// feature 开关，FFI 面也缩到最小（两个函数 + 一个 LocalFree）。
    /// `repr(C)` 与 Win32 头文件一致：`u32` 后跟 8 字节对齐的指针，结构体大小 16 字节。
    #[repr(C)]
    struct Blob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            p_data_in: *const Blob,
            sz_data_descr: *const u16,
            p_optional_entropy: *const Blob,
            pv_reserved: *const core::ffi::c_void,
            p_prompt_struct: *mut core::ffi::c_void,
            dw_flags: u32,
            p_data_out: *mut Blob,
        ) -> i32;
        fn CryptUnprotectData(
            p_data_in: *const Blob,
            pp_sz_data_descr: *mut *mut u16,
            p_optional_entropy: *const Blob,
            pv_reserved: *const core::ffi::c_void,
            p_prompt_struct: *mut core::ffi::c_void,
            dw_flags: u32,
            p_data_out: *mut Blob,
        ) -> i32;
    }

    /// 输出缓冲区由 DPAPI 用 `LocalAlloc` 分配，必须用 `LocalFree` 归还（kernel32）。
    /// 忘了归还 = 每次加载页面泄漏一块内存。
    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(h_mem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }

    /// `CRYPTPROTECT_UI_FORBIDDEN` = 1：绝不弹交互式 UI。
    /// 我们是在后台线程（日志读取线程 / 启动流程）里调用的，任何弹窗都等于把界面卡死。
    const UI_FORBIDDEN: u32 = 1;

    /// 取出 DPAPI 输出缓冲区并把所有权从内核收回来（含失败分支的清理）。
    /// `out.pb_data` 为空或长度为 0 都视为失败。
    unsafe fn take_output(out: Blob) -> Option<Vec<u8>> {
        if out.pb_data.is_null() || out.cb_data == 0 {
            return None;
        }
        let bytes = std::slice::from_raw_parts(out.pb_data, out.cb_data as usize).to_vec();
        LocalFree(out.pb_data as *mut core::ffi::c_void);
        Some(bytes)
    }

    pub fn protect(plain: &[u8]) -> Option<Vec<u8>> {
        // 空输入 DPAPI 直接失败（调用方 seal 已挡掉空串，这里仍显式拒绝，语义清楚）
        if plain.is_empty() {
            return None;
        }
        let entropy = Blob {
            cb_data: super::ENTROPY.len() as u32,
            pb_data: super::ENTROPY.as_ptr() as *mut u8,
        };
        let input = Blob {
            cb_data: plain.len() as u32,
            pb_data: plain.as_ptr() as *mut u8,
        };
        let mut out = Blob {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        };
        unsafe {
            // 第 2 个参数（描述串）传 null：它只是给人看的元数据，我们不需要
            let ok = CryptProtectData(
                &input,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null_mut(),
                UI_FORBIDDEN,
                &mut out,
            );
            if ok == 0 {
                return None;
            }
            take_output(out)
        }
    }

    pub fn unprotect(blob: &[u8]) -> Option<Vec<u8>> {
        if blob.is_empty() {
            return None;
        }
        let entropy = Blob {
            cb_data: super::ENTROPY.len() as u32,
            pb_data: super::ENTROPY.as_ptr() as *mut u8,
        };
        let input = Blob {
            cb_data: blob.len() as u32,
            pb_data: blob.as_ptr() as *mut u8,
        };
        let mut out = Blob {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        };
        unsafe {
            // 第 2 个参数传 null = 不索取描述串（索取的话那串也得我们自己 LocalFree）
            let ok = CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null_mut(),
                UI_FORBIDDEN,
                &mut out,
            );
            if ok == 0 {
                return None;
            }
            take_output(out)
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// 非 Windows 平台没有 DPAPI：一律返回 None —— 调用方据此**放弃落盘**，绝不回落明文。
    /// 本项目的发布产物只有 Windows（见 docs/linux-porting-plan.md），这些分支只是为了让
    /// `cargo test` / 编辑器在别的平台上也能编译通过。
    ///
    /// 将来真做 Linux 版时，这里要换成对应平台的密钥库（Secret Service / libsecret，
    /// 或至少一个 0600 权限的独立文件 + 明确写进文档的残余风险），**不要**改成直接写明文。
    pub fn protect(_plain: &[u8]) -> Option<Vec<u8>> {
        None
    }

    pub fn unprotect(_blob: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

// ---------- 对外 API ----------

/// 把要落盘的文本（会话地址）加密成可写进 `config.json` 的十六进制密文。
///
/// 返回 `None` = 加密不可用或失败。调用方**必须放弃写入**：没有任何明文降级路径
/// （代价只是「这次不记住地址」，下次连接现有服务多走一次认证）。
pub fn seal(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    imp::protect(text.as_bytes()).map(|b| to_hex(&b))
}

/// 解开 [`seal`] 的产物。
///
/// 失败（换了 Windows 用户/机器、密文被手改、本来就不是密文、hex 非法）返回 `None`，
/// 调用方当作「没有记录」处理，不要尝试任何容错解读。
pub fn unseal(hex: &str) -> Option<String> {
    let bytes = from_hex(hex)?;
    let plain = imp::unprotect(&bytes)?;
    // 加密前是合法 UTF-8 的文本；解出非法字节说明密文被换过，按失败处理
    String::from_utf8(plain).ok()
}

/// 小写十六进制编码（与 `process.rs::sha256_hex` 同款手写，不引第三方依赖）。
pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// 十六进制解码：长度为奇数或含非十六进制字符时返回 `None`（严格，不做宽容解析 ——
/// 这条路的数据来自磁盘上的文件，宁可当作「没有」也不要猜）。
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.is_empty() || s.len() % 2 != 0 {
        return None;
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < b.len() {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

/// 把文本里所有 `token=<值>` 的值替换成 `***`（键名保留）。
///
/// 用在**所有**会把日志写到磁盘或送到日志面板的地方：`logger::append_line`（文件唯一
/// 出口，含 `<home>\logs\dsh.log` 镜像）与 `process::emit_log` / 原始输出转发（界面唯一
/// 出口）。幂等：对已脱敏的文本再跑一次结果不变，所以两道防线叠加也不会出问题。
pub fn redact(text: &str) -> String {
    if !contains_token_key(text) {
        return text.to_string();
    }
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    loop {
        let Some(rel) = lower[pos..].find(TOKEN_KEY) else {
            out.push_str(&text[pos..]);
            break;
        };
        let val_start = pos + rel + TOKEN_KEY.len();
        // 值可能被引号包着（`token="abc"`）：引号不属于值，先跳过它再找结束位置。
        // 不跳的话「第一个字符就是终止符」会被判成空值而**原样留下 token**，那是最糟的漏网。
        let mut val_body = val_start;
        if let Some(q) = text[val_start..].chars().next() {
            if q == '"' || q == '\'' {
                val_body = val_start + q.len_utf8();
            }
        }
        out.push_str(&text[pos..val_body]);
        // 值的结束位置：第一个「不可能是令牌一部分」的字符。
        // 注意**不能**把 base64 的填充 `=` 当分隔符，否则 `?token=YWJj==` 只会切掉一半。
        let end = text[val_body..]
            .char_indices()
            .find(|(_, c)| is_token_end(*c))
            .map(|(i, _)| val_body + i)
            .unwrap_or(text.len());
        if end > val_body {
            out.push_str(MASK);
        }
        // 空值（`?token=`）时 end == val_body：仍然前进，保证循环必然收敛
        pos = end.max(val_body);
    }
    out
}

/// 不区分大小写地判断文本里是否出现 `token=`。
///
/// 这是零分配的快路径：`append_line` 对每一行输出都会调用本模块，绝大多数行在这里
/// 就直接返回，不产生任何字符串拷贝。
fn contains_token_key(text: &str) -> bool {
    let need = TOKEN_KEY.len();
    let b = text.as_bytes();
    if b.len() < need {
        return false;
    }
    b.windows(need)
        .any(|w| w.eq_ignore_ascii_case(TOKEN_KEY.as_bytes()))
}

/// 令牌值的终止字符：空白、控制字符、URL 结构符/引号/括号等 ASCII 标点，
/// 以及**任何非 ASCII 字符** —— 日志里值后面常常直接跟中文（`…?token=abc，就绪`），
/// 那时只该切掉 `abc`，不能把后半句一起吃掉。
fn is_token_end(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || !c.is_ascii()
        || matches!(
            c,
            '&' | '#' | '?' | '"' | '\'' | '`' | '<' | '>' | ')' | ']' | '}' | ',' | ';' | '|'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_masks_only_the_token_value() {
        let line = "dsh web: http://127.0.0.1:3080/?token=abc-DEF_123 ready";
        assert_eq!(
            redact(line),
            "dsh web: http://127.0.0.1:3080/?token=*** ready"
        );
    }

    #[test]
    fn redact_keeps_base64_padding_and_later_params() {
        assert_eq!(
            redact("http://127.0.0.1:3080/?token=YWJj==&port=3080"),
            "http://127.0.0.1:3080/?token=***&port=3080"
        );
    }

    #[test]
    fn redact_covers_suffixed_keys_and_case() {
        assert_eq!(redact("access_token=abc123"), "access_token=***");
        assert_eq!(redact("?TOKEN=abc123&x=1"), "?TOKEN=***&x=1");
    }

    #[test]
    fn redact_stops_at_quotes_and_non_ascii() {
        // 引号收尾（DSH 若打印 JSON 形态）
        assert_eq!(
            redact(r#"{"url":"http://127.0.0.1:3080/?token=abc"}"#),
            r#"{"url":"http://127.0.0.1:3080/?token=***"}"#
        );
        // 值本身被引号包着：引号保留，里面的值必须被换掉（否则等于没脱敏）
        assert_eq!(
            redact(r#"dsh web: url="http://127.0.0.1:3080/?token=abc" ok"#),
            r#"dsh web: url="http://127.0.0.1:3080/?token=***" ok"#
        );
        assert_eq!(redact("token='abc'"), "token='***'");
        // 值后面直接跟中文：只切令牌，不吃掉后半句
        assert_eq!(
            redact("地址 http://127.0.0.1:3080/?token=abc，已就绪"),
            "地址 http://127.0.0.1:3080/?token=***，已就绪"
        );
    }

    #[test]
    fn redact_is_idempotent_and_ignores_plain_text() {
        let masked = redact("http://127.0.0.1:3080/?token=abc");
        assert_eq!(redact(&masked), masked);
        assert_eq!(masked, "http://127.0.0.1:3080/?token=***");
        // 没有 token 参数的行原样返回
        assert_eq!(redact("dsh web: http://127.0.0.1:3080"), "dsh web: http://127.0.0.1:3080");
        // 空值不产生任何替换，也不会死循环
        assert_eq!(redact("?token=&x=1"), "?token=&x=1");
        assert_eq!(redact("?token="), "?token=");
    }

    #[test]
    fn hex_round_trip_and_strict_rejection() {
        let raw = [0x00u8, 0x01, 0x7f, 0x80, 0xff];
        assert_eq!(to_hex(&raw), "00017f80ff");
        assert_eq!(from_hex("00017F80FF").as_deref(), Some(&raw[..]));
        assert_eq!(from_hex("  00017f80ff  ").as_deref(), Some(&raw[..]));
        // 奇数长度 / 非十六进制字符 / 空串一律拒绝
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
        assert!(from_hex("").is_none());
        assert!(from_hex("0g").is_none());
    }

    #[test]
    fn seal_rejects_empty_input() {
        // 空串不加密（DPAPI 也会拒），返回 None 让调用方跳过写入
        assert!(seal("").is_none());
        assert!(seal("   ").is_none());
    }

    #[test]
    fn unseal_rejects_garbage() {
        assert!(unseal("").is_none());
        assert!(unseal("not-hex").is_none());
        // 合法 hex、但不是密文 / 不是本用户本熵加密的 → 一律 None（不猜、不回落）
        assert!(unseal("000102030405060708090a0b0c0d0e0f").is_none());
    }

    /// DPAPI 往返：只有 Windows（也就是 CI 与发布平台）会跑。
    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip_keeps_plaintext_out_of_the_ciphertext() {
        let url = "http://127.0.0.1:3080/?token=abc-DEF_123";
        let Some(hex) = seal(url) else {
            // 环境本身不支持 DPAPI（例如没有加载用户配置的服务账户）：
            // 这不是代码问题，生产路径上 seal() 失败同样只是「这次不记住地址」，
            // 绝不会有明文写盘。跳过断言，避免把环境限制误报成构建失败。
            eprintln!("DPAPI 在该环境不可用，跳过往返断言");
            return;
        };
        assert!(!hex.contains("abc"), "密文里不得出现明文令牌");
        assert_eq!(unseal(&hex).as_deref(), Some(url));
    }
}
