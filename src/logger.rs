//! 极简文件日志。
//!
//! 刻意不引入 `tracing` + `tracing-subscriber`：一个 TUI 播放器只需要把异常落到
//! 文件里，完整的订阅者体系会额外带来数百 KB 二进制体积和启动期开销，与「低资源
//! 占用」的目标相冲突。日志写入失败一律静默吞掉——日志本身不该拖垮主流程。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

pub const LEVEL_ERROR: &str = "ERROR";
pub const LEVEL_WARN: &str = "WARN";
pub const LEVEL_INFO: &str = "INFO";
pub const LEVEL_DEBUG: &str = "DEBUG";

static SINK: OnceLock<Mutex<File>> = OnceLock::new();
static DEBUG_ENABLED: OnceLock<bool> = OnceLock::new();

/// DEBUG 级别默认不输出——按键、位置同步这类日志每帧都会产生，默认打开会把
/// 日志淹掉。排查输入问题时用 `KUGOU_TUI_DEBUG=1 kugou-tui` 临时开启。
fn debug_enabled() -> bool {
    *DEBUG_ENABLED.get_or_init(|| {
        std::env::var("KUGOU_TUI_DEBUG")
            .map(|value| !value.is_empty() && value != "0")
            .unwrap_or(false)
    })
}

/// 打开（不存在则创建）日志文件。
///
/// 未调用本函数时所有 `tlog!` 调用都是空操作，因此日志是可选的。
pub fn init(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    // 与配置文件（0600）保持一致：日志里会记响应体片段（见 client.rs 的非 JSON
    // 分支），不该让同机其他用户可读。失败不影响日志本身，忽略。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    let _ = SINK.set(Mutex::new(file));
    Ok(())
}

/// 进程当前的驻留集（KiB）。取不到时返回 0。
///
/// 只在内存追踪打开时被调用（见 [`mem_trace_enabled`] 与 `App::tick`），用途是把
/// 「听歌听久了内存涨」落到日志时间线上：日志里能直接看到「第几首歌之后涨了多少」，
/// 而不是盯着 htop 手工对齐时间。
///
/// 页大小按 4 KiB 算：Linux 上 x86_64 与 arm64 都是这个值（16K/64K 页的架构下会
/// 有偏差，但这个数字是给趋势看的，不是给计量用的）。
pub fn rss_kib() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let Ok(text) = std::fs::read_to_string("/proc/self/statm") else {
            return 0;
        };
        text.split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u64>().ok())
            .map(|pages| pages * 4)
            .unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// 是否打开内存追踪：`KUGOU_TUI_MEM_TRACE=1`。
///
/// 与 `KUGOU_TUI_DEBUG` 分开：那个会连按键日志一起打开（每帧一条，日志瞬间变大），
/// 而定位内存问题只需要每几秒一行的 RSS，不该被按键日志淹掉。
pub fn mem_trace_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("KUGOU_TUI_MEM_TRACE")
            .map(|value| !value.is_empty() && value != "0")
            .unwrap_or(false)
    })
}

/// 写入一行日志。
///
/// 所有日志都从这里出去，所以**凭据脱敏也放在这里**：调用点会打印出站 URL、
/// 请求头、响应体片段，这些地方天然带着 `token`、`userid`、`dfid`。逐个调用点
/// 去记得脱敏，早晚会漏一个（阶段 4 就漏了，真实 token 进了日志）；收口到一处
/// 才守得住。见 [`redact`]。
pub fn write(level: &str, message: &str) {
    if level == LEVEL_DEBUG && !debug_enabled() {
        return;
    }
    let Some(sink) = SINK.get() else {
        return;
    };
    let Ok(mut file) = sink.lock() else {
        return;
    };
    let _ = writeln!(file, "{} [{}] {}", timestamp_now(), level, redact(message));
}

/// 日志里必须遮掉值的键名（比较时忽略大小写）。
///
/// 只列「值的泄露会造成账号或设备被冒用」的键：
///
/// * `token` / `userid` —— 账号凭据本身。
/// * `dfid` / `mid` —— 设备指纹。不是账号密码，但足以让服务端把请求认成
///   同一台设备；上游按 24 小时轮换，属于不该长期留在磁盘上的东西。
/// * `KUGOU_API_*` —— 设备指纹在 cookie 里的另几个名字。
///
/// **不遮** `accesskey`、`signature`、`hash`：前两者是单次请求的凭证（用过即废，
/// 且签名值本身是「请求参数算出来的」，留着才能对账），后者是公开的歌曲标识。
const SENSITIVE_KEYS: &[&str] = &[
    "token",
    "userid",
    "dfid",
    "mid",
    "KUGOU_API_MID",
    "KUGOU_API_GUID",
    "KUGOU_API_DEV",
];

const REDACTED: &str = "<redacted>";

/// 把消息里敏感键的值换成 `<redacted>`，**保留键名与参数顺序**。
///
/// 保留键名和顺序是有意的：阶段 3/4 的「与 Node 版出站逐项对比」正是靠日志里
/// 的参数名与顺序做的，值被遮掉不影响那种对比，而 URL 形态仍然可读。
///
/// 要认的形状有三种，都是本仓库真实打出来的：
///
/// * query 串：`...&token=abc&userid=123`
/// * JSON：`{"token":"abc"}`
/// * Rust 的 `{:?}` 元组列表：`[("dfid", "abc")]`（出站日志就是这么打请求头的）
pub fn redact(message: &str) -> String {
    let mut out = message.to_string();
    for key in SENSITIVE_KEYS {
        out = redact_key(&out, key);
    }
    out
}

/// 遮掉 `key` 的所有出现处。逐处重扫：改短之后下标会变，不能缓存。
fn redact_key(message: &str, key: &str) -> String {
    let mut out = message.to_string();
    let mut from = 0;
    while let Some(offset) = find_key(&out[from..], key) {
        let after = from + offset + key.len();
        let Some(value) = locate_value(&out[after..]) else {
            // 认不出值在哪（例如键名出现在散文里），跳过这一处继续找。
            from = after;
            continue;
        };
        let (start, end) = value;
        // 引号本身留着——日志读起来仍是完整的 JSON。
        let range = after + start..after + end;
        out.replace_range(range.clone(), REDACTED);
        from = range.start + REDACTED.len();
    }
    out
}

/// 在 `haystack` 里找 `key`（忽略 ASCII 大小写），要求两侧都不是标识符字符。
///
/// 忽略大小写是因为同一个键在不同形状里大小写不同：URL 里是 `mid=`，cookie 里是
/// `KUGOU_API_MID`，JSON 里可能写成 `"MID"`。
///
/// 两侧的边界检查是为了不误伤 `access_token` 里的 `token`、`userid_list` 里的
/// `userid`——遮掉这些会让人看不懂日志，而它们本来就不是凭据。
fn find_key(haystack: &str, key: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let key = key.as_bytes();
    if key.is_empty() || bytes.len() < key.len() {
        return None;
    }
    for start in 0..=bytes.len() - key.len() {
        if !bytes[start..start + key.len()].eq_ignore_ascii_case(key) {
            continue;
        }
        let end = start + key.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return Some(start);
        }
    }
    None
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// 从键名之后定位**值的可替换区间**，返回 `(起点, 终点)`，偏移都相对 `rest`。
///
/// 要认的形状有四种，都是本仓库真实打出来的：
///
/// * query 串：`token=abc&userid=123` —— 分隔符是 `=`
/// * JSON：`"token":"abc"`、`"userid":10001` —— 分隔符是 `:`
/// * Rust 的 `{:?}` 元组列表：`[("dfid", "abc")]` —— 分隔符是 `,`
/// * 被 `{:?}` 转义过的 JSON：`body=Some("{\"userid\":10001}")` ——
///   键与值的引号前面都多一个反斜杠
///
/// 带引号的值只返回引号**里面**那一段（转义的反斜杠留在外面），所以替换后
/// 日志仍是结构完整的 JSON；裸值返回整段。键名出现在散文里、后面见不到任何
/// 分隔符时返回 `None`，不遮。
fn locate_value(rest: &str) -> Option<(usize, usize)> {
    let bytes = rest.as_bytes();
    let mut index = 0;

    // 键自己的收尾引号，可能是 `"`，也可能是 Debug 转义后的 `\"`。
    if is_escaped_quote(bytes, index) {
        index += 2;
    } else if is_quote(bytes.get(index)) {
        index += 1;
    }

    let mut saw_separator = false;
    while index < bytes.len() {
        match bytes[index] {
            b'=' | b':' | b',' => {
                saw_separator = true;
                index += 1;
            }
            b' ' | b'\t' => index += 1,
            _ => break,
        }
    }
    if !saw_separator {
        return None;
    }

    // 值的起始引号，同样可能是转义后的 `\"`。
    let escaped = is_escaped_quote(bytes, index);
    if escaped {
        index += 1;
    }
    if !is_quote(bytes.get(index)) {
        let start = index;
        let mut end = start;
        while end < bytes.len() && !is_value_terminator(bytes[end]) {
            end += 1;
        }
        return Some((start, end));
    }

    let quote = bytes[index];
    let start = index + 1;
    let mut end = start;
    while end < bytes.len() {
        if escaped {
            // 转义形态下，收尾引号前面必定有一个反斜杠，它不属于内容。
            if bytes[end] == b'\\' && bytes.get(end + 1) == Some(&quote) {
                return Some((start, end));
            }
            end += 1;
            continue;
        }
        if bytes[end] == b'\\' {
            end += 2;
            continue;
        }
        if bytes[end] == quote {
            return Some((start, end));
        }
        end += 1;
    }
    // 引号没闭合：把剩下的都当值，宁可多遮。
    Some((start, bytes.len()))
}

fn is_quote(byte: Option<&u8>) -> bool {
    matches!(byte, Some(b'"') | Some(b'\''))
}

/// `\` 紧跟一个引号——Debug 格式化把 JSON 的引号写成了 `\"`。
fn is_escaped_quote(bytes: &[u8], index: usize) -> bool {
    bytes.get(index) == Some(&b'\\') && is_quote(bytes.get(index + 1))
}

/// 裸值的结束字符，与旧实现的集合一致。
fn is_value_terminator(byte: u8) -> bool {
    matches!(
        byte,
        b'&' | b' '
            | b'\t'
            | b'"'
            | b'\''
            | b')'
            | b']'
            | b'}'
            | b','
            | b';'
            | b'\n'
            | b'/'
            | b'?'
    )
}

/// 把进程的 stderr 接到日志文件上。
///
/// # 为什么必须做
///
/// 音频后端（libjack / libasound）会**直接往 fd 2 写报错**，例如
/// `jack server is not running or cannot be started`、
/// `JackShmReadWritePtr::~JackShmReadWritePtr - Init not done for -1, skipping unlock`。
/// 进了 TUI 之后终端在 alternate screen 上，这些字符会直接打在 ratatui 画好的
/// 界面里；而 ratatui 的增量重绘只写「内容变了的单元格」——屏幕上的第三方字符
/// 不在任何 buffer 里，于是**永远不会被覆盖**，残留成一片乱码。窗口越窄、
/// 报错行折行越多，看着越糟。
///
/// 接到日志而不是 `/dev/null`：这些报错正是排查「没声音」时的关键线索。
/// 必须在 `ratatui::init()` 之前调用。
///
/// # 两个平台的做法不一样
///
/// * **Unix** 有 fd 表，`dup2` 把 fd 2 换成日志文件的副本即可，连 C 库
///   （上面那两个正是 C 库）都会跟着走。
/// * **Windows** 没有 fd 表，标准流是三个 Win32 句柄，只能 `SetStdHandle`
///   把 `STD_ERROR_HANDLE` 指过去。这**只覆盖 Rust 侧的输出**（`eprintln!`、
///   panic 消息），不覆盖 CRT 的 fd 2。够用是因为 Windows 上的音频后端是
///   WASAPI（cpal 直接用 `windows` crate 调 COM），不存在上面那种绕过 Rust
///   直接写 fd 的 C 库——真要写，也是写进 Windows 自己的调试输出通道。
pub fn redirect_stderr_to_log() {
    // 日志没初始化成功（SINK 没设上）时无从重定向，保持原样。
    let Some(sink) = SINK.get() else {
        return;
    };
    let Ok(file) = sink.lock() else {
        return;
    };

    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY：`dup2` 只改本进程的 fd 表，失败返回 -1 不破坏其它状态。
        // 锁守卫随后释放，但 fd 2 已经是独立副本，仍然有效。
        let result = unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) };
        if result < 0 {
            // 这里**不能**用 eprintln!——那正是要拦下来的东西。
            report_redirect_failure();
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};

        // SAFETY：句柄来自仍然活着的 `File`（它被 `SINK` 持有到进程结束），
        // `SetStdHandle` 只是把标准错误指向它，不转移所有权、不关闭任何东西。
        let ok = unsafe { SetStdHandle(STD_ERROR_HANDLE, file.as_raw_handle() as _) };
        if ok == 0 {
            report_redirect_failure();
        }
    }

    /// 失败只记一行日志——写不出去也不该把启动带崩。
    #[cfg(any(unix, windows))]
    fn report_redirect_failure() {
        write(
            LEVEL_WARN,
            &format!(
                "重定向 stderr 到日志失败：{}",
                std::io::Error::last_os_error()
            ),
        );
    }
}

/// 形如 `2026-09-20 03:44:15Z` 的 UTC 时间戳。
fn timestamp_now() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default();
    let (year, month, day, hour, minute, second) = civil_from_unix(seconds);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}Z")
}

/// 把 UNIX 时间戳换算成 UTC 日历时间。
///
/// 使用 Howard Hinnant 的 `civil_from_days` 算法，从而不依赖 `chrono` / `time`。
fn civil_from_unix(seconds: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = seconds.div_euclid(86_400);
    let remainder = seconds.rem_euclid(86_400);

    // 以 0000-03-01 为原点，让闰年落在周期末尾，简化后续运算
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097); // 400 年 = 146097 天
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;

    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_index = (5 * day_of_year + 2) / 153;

    let day = day_of_year - (153 * march_index + 2) / 5 + 1;
    let month = if march_index < 10 {
        march_index + 3
    } else {
        march_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    (
        year,
        month as u32,
        day as u32,
        (remainder / 3_600) as u32,
        ((remainder % 3_600) / 60) as u32,
        (remainder % 60) as u32,
    )
}

/// 统一的日志宏，避免到处写 `crate::logger::write(...)`。
///
/// 用 `pub(crate) use` 而不是 `#[macro_export]`：后者会把宏暴露到 crate 根、
/// 污染公开 API，而这里只需要内部使用。代价是每个用到的模块要显式
/// `use crate::logger::tlog;`——这反而让依赖关系一目了然。
macro_rules! tlog {
    ($level:expr, $($arg:tt)*) => {
        $crate::logger::write($level, &format!($($arg)*))
    };
}

pub(crate) use tlog;

#[cfg(test)]
mod tests {
    use super::{civil_from_unix, redact};

    #[test]
    fn converts_epoch_to_calendar_date() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2026-09-20T03:44:15Z（用 `date -u -d @1789875855` 核对过）
        assert_eq!(civil_from_unix(1_789_875_855), (2026, 9, 20, 3, 44, 15));
        // 闰日
        assert_eq!(civil_from_unix(1_709_164_800), (2024, 2, 29, 0, 0, 0));
    }

    /// 出站日志的真实形状：URL 里 token 与 userid 都在 query 上。
    /// 键名与顺序必须留着——阶段 3/4 的逐项对比就是靠它们做的。
    #[test]
    fn redacts_credentials_in_a_query_string() {
        let line = "native 出站 GET https://lyrics.kugou.com/download?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1791453130&token=TOKENFIXTURE&userid=10001&ver=1&client=android&id=274944371&accesskey=0123456789ABCDEF0123456789ABCDEF&fmt=krc&charset=utf8&signature=d54747fed21dbff8fcc2f6acae4698c7";
        let out = redact(line);

        assert!(!out.contains("TOKENFIXTURE"));
        assert!(!out.contains("10001"));
        assert!(!out.contains("1234567890abcdef12345678"));
        assert!(!out.contains("231699103997194646178265604655475531917"));

        // 参数名、顺序、以及非敏感值都原样保留。
        assert!(out.contains("&token=<redacted>&"));
        assert!(out.contains("&userid=<redacted>&"));
        assert!(out.contains("?dfid=<redacted>&"));
        assert!(out.contains("&mid=<redacted>&"));
        assert!(out.contains("appid=3116"));
        assert!(out.contains("accesskey=0123456789ABCDEF0123456789ABCDEF"));
        assert!(out.contains("signature=d54747fed21dbff8fcc2f6acae4698c7"));
        assert!(out.contains("uuid=-"));
    }

    /// 出站日志打请求头用的是 Rust 的 `{:?}`：`[("dfid", "abc")]`。
    #[test]
    fn redacts_credentials_in_a_debug_header_list() {
        let line = r#"headers=[("User-Agent", "Android15-1070"), ("dfid", "1234567890abcdef12345678"), ("clienttime", "1791453130"), ("mid", "231699103997194646178265604655475531917"), ("kg-rc", "1")]"#;
        let out = redact(line);

        assert!(!out.contains("1234567890abcdef12345678"));
        assert!(!out.contains("231699103997194646178265604655475531917"));
        assert!(out.contains(r#"("dfid", "<redacted>")"#));
        assert!(out.contains(r#"("mid", "<redacted>")"#));
        assert!(out.contains(r#"("User-Agent", "Android15-1070")"#));
        assert!(out.contains(r#"("clienttime", "1791453130")"#));
    }

    #[test]
    fn redacts_credentials_in_json() {
        let line = r#"{"token":"abc123","userid":10001,"dfid":"1234567890abcdef12345678","hash":"0a6916"}"#;
        let out = redact(line);

        assert!(!out.contains("abc123"));
        assert!(!out.contains("10001"));
        assert!(!out.contains("1234567890abcdef12345678"));
        assert!(out.contains(r#""token":"<redacted>""#));
        // 值原本没带引号（是数字），替换后也不带引号——日志是给人看的，不是给解析器。
        assert!(out.contains(r#""userid":<redacted>"#));
        assert!(out.contains(r#""hash":"0a6916""#));
    }

    /// 出站日志的 `body` 是 `{:?}` 打出来的 `Option<String>`，里面那层 JSON 的
    /// 引号全被转义成 `\"`。这正是 5b 阶段真实漏过一次的形状：`userid` 的
    /// 数字值就这么留在了日志里，而普通 JSON 的用例测不出来。
    #[test]
    fn redacts_credentials_in_a_debug_escaped_json_body() {
        let line = r#"body=Some("{\"visit_time\":1791457093,\"usertype\":1,\"p\":\"C2D2\",\"userid\":10001}")"#;
        let out = redact(line);

        assert!(!out.contains("10001"), "userid 仍在日志里：{out}");
        // 转义引号留在原位，只有值被换掉。
        assert!(out.contains(r#"\"userid\":<redacted>"#), "{out}");
        // 不敏感的字段一个都不能动。
        assert!(out.contains(r#"\"visit_time\":1791457093"#));
        assert!(out.contains(r#"\"p\":\"C2D2\""#));
    }

    /// 转义形态下的字符串值（带引号）也要遮，且不能把外层引号一起吃掉。
    #[test]
    fn redacts_escaped_json_string_values() {
        let line = r#"body=Some("{\"token\":\"abc123def\",\"mid\":\"23169910399719464617\"}")"#;
        let out = redact(line);

        assert!(!out.contains("abc123def"));
        assert!(!out.contains("23169910399719464617"));
        assert!(out.contains(r#"\"token\":\"<redacted>\""#), "{out}");
        assert!(out.contains(r#"\"mid\":\"<redacted>\""#), "{out}");
        // 结尾的 `}")` 没被吞掉。
        assert!(out.ends_with(r#"}")"#), "{out}");
    }

    /// cookie 串里设备指纹是另几个名字，大小写也不一样。
    #[test]
    fn redacts_device_fingerprint_cookie_names() {
        let line = "cookie: token=abc; userid=123; KUGOU_API_MID=231699103997194646178265604655475531917; KUGOU_API_GUID=abc-def; KUGOU_API_DEV=ABCDEFGHIJ";
        let out = redact(line);

        assert!(!out.contains("231699103997194646178265604655475531917"));
        assert!(!out.contains("abc-def"));
        assert!(!out.contains("ABCDEFGHIJ"));
        assert!(out.contains("KUGOU_API_MID=<redacted>"));
    }

    /// 键名出现在散文或别的标识符里时不能误伤——否则日志会变得看不懂，
    /// 而 `access_token`、`userid_list` 本来就不是凭据。
    #[test]
    fn leaves_lookalike_keys_alone() {
        let line = "access_token 不该被遮，userid_list 同理；token 只作为单词出现时也没有值可遮";
        assert_eq!(redact(line), line);
    }

    /// 空值也要遮成 `<redacted>`，不能因为「没值」就留下 `token=`。
    #[test]
    fn redacts_empty_values() {
        assert_eq!(redact("token=&userid=5"), "token=<redacted>&userid=<redacted>");
    }

    /// 一次日志里同一个键出现多次（URL 一份、请求头一份）要全部遮掉。
    #[test]
    fn redacts_every_occurrence() {
        let line = "url?token=aaa&mid=bbb headers=[(\"token\", \"aaa\"), (\"mid\", \"bbb\")]";
        let out = redact(line);
        assert!(!out.contains("aaa"));
        assert!(!out.contains("bbb"));
        assert_eq!(out.matches("<redacted>").count(), 4);
    }
}
