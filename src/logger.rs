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

/// 写入一行日志。
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
    let _ = writeln!(file, "{} [{}] {}", timestamp_now(), level, message);
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
    use super::civil_from_unix;

    #[test]
    fn converts_epoch_to_calendar_date() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2026-09-20T03:44:15Z（用 `date -u -d @1789875855` 核对过）
        assert_eq!(civil_from_unix(1_789_875_855), (2026, 9, 20, 3, 44, 15));
        // 闰日
        assert_eq!(civil_from_unix(1_709_164_800), (2024, 2, 29, 0, 0, 0));
    }
}
