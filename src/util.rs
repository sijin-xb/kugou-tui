//! 无第三方依赖的小工具。
//!
//! 这里只放三件事：
//!
//! 1. **伪随机数**。只为了「随机播放」这一个用途，不值得为此引入 `rand` +
//!    `getrandom` 两条依赖链（release 构建下约几十 KB）。用 xorshift64* 足够，
//!    且不涉及任何密码学用途。
//! 2. **时间戳**。避免为了一个 `now()` 引入 `chrono`。
//! 3. **cookie 规范化**（[`normalize_cookie_header`]）。把服务端下发的
//!    `Set-Cookie` 串修成合法的请求头，原因见那个函数的说明。

use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

thread_local! {
    /// xorshift64* 的状态。初值由时钟纳秒与栈地址混合，避免多线程撞种子。
    static RNG_STATE: Cell<u64> = const { Cell::new(0) };
}

/// 当前 UNIX 时间戳（毫秒）。用于给请求加时间戳以绕开服务端缓存。
pub fn now_unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default()
}

/// 生成一个 64 位伪随机数。
pub fn random_u64() -> u64 {
    RNG_STATE.with(|state| {
        let mut value = state.get();
        if value == 0 {
            value = seed();
            // xorshift 的状态不能为 0
            if value == 0 {
                value = 0x9E37_79B9_7F4A_7C15;
            }
        }
        // xorshift64*
        value ^= value >> 12;
        value ^= value << 25;
        value ^= value >> 27;
        state.set(value);
        value.wrapping_mul(0x2545_F491_4F6C_DD1D)
    })
}

/// 生成 `[0, bound)` 内的伪随机数。`bound <= 1` 时返回 0。
///
/// 用 Lemire 的乘法取模法代替整数取模，避免低位偏置。
pub fn random_below(bound: usize) -> usize {
    if bound <= 1 {
        return 0;
    }
    let bound = bound as u64;
    let product = u128::from(random_u64()) * u128::from(bound);
    (product >> 64) as usize
}

fn seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or_default();
    // 与栈上局部变量的地址、进程 id 混合，让不同线程拿到不同种子
    let local = 0u8;
    let address = std::ptr::from_ref(&local) as u64;
    nanos ^ address.rotate_left(17) ^ u64::from(std::process::id())
}

/// 本地时区的「今天」，格式 `2026-09-23`。
///
/// # 为什么必须是**本地**日期
///
/// 酷狗的领取接口要传「要领取的那一天」，传过去的日期就是领到的那天。UTC 日期
/// 在 UTC+8 的凌晨 0 点到 8 点之间还停在昨天，那时按 UTC 算就会去领一天已经过去的
/// VIP——白打一次接口，而且那天的权益也拿不回来。
///
/// 拿不到本地时区时返回 `None`（多线程环境下 `time` 可能拒绝推断偏移）。调用方
/// 应当**放弃领取并如实告知**，而不是退回 UTC 猜一个：猜错是白领，不领只是少一天。
pub fn today_local() -> Option<String> {
    let now = time::OffsetDateTime::now_local().ok()?;
    Some(format!(
        "{:04}-{:02}-{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    ))
}

/// 属性名。`Set-Cookie` 里这些描述的是「这个 cookie 怎么存」，不是 cookie 本身，
/// 回传给服务端只会被当成一串没人认得的 cookie。
const COOKIE_ATTRIBUTES: [&str; 10] = [
    "max-age", "expires", "path", "domain", "httponly", "secure", "samesite", "version", "comment",
    "priority",
];

/// 把服务端下发的 `Set-Cookie` 串规范化成合法的 `Cookie:` 头。全是属性或空串时返回
/// `None`。
///
/// # 为什么需要它
///
/// NeteaseCloudMusicApi（api-enhanced）在扫码成功后，把整段 `Set-Cookie` 响应头
/// 原样塞进 `/login/qr/check` 响应体的 `cookie` 字段——里面既有 `k=v`，也有
/// `Max-Age` / `Expires` / `Path` 这类属性，而且多个 cookie 之间用 **`;;`**
/// （分号后不带空格）连接。
///
/// 而它读回 Cookie 头时用的是 `server.js` 里那条正则 `/;\s+|(?<!\s)\s+$/g`
/// ——**只认「分号 + 空白」这一种分隔**。于是 `;;` 处根本不切分，例如
/// `Path=/openapi/clientlog;;MUSIC_U=00CC…` 会被当成**一个** `k=v`，键是
/// `Path`，`MUSIC_U` 压根没进 `req.cookies`。
///
/// 症状极具误导性：界面按 `MUSIC_U=` 判断登录态，显示「登录成功」；而服务端认为
/// 没人登录，`/login/status` 返回 `profile: null`，云端歌单永远报「尚未登录」。
/// 看起来像客户端状态错乱，实际是这串 cookie 从存下来那一刻起就是坏的。
///
/// # 做法
///
/// 丢掉属性段与空段、按 `"; "` 重新连接。同名 cookie 以**最后一次**出现为准
/// （`Set-Cookie` 后写的覆盖先写的）。
///
/// 对本来就规范的串（酷狗的 `token=x; userid=y`）是幂等的。
pub fn normalize_cookie_header(raw: &str) -> Option<String> {
    let mut pairs: Vec<(&str, &str)> = Vec::new();

    for chunk in raw.split(';') {
        let Some((key, value)) = chunk.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        // 空段（`;;` 的产物）、空键、空值都留不得：服务端会把它们解析成无意义的项
        if key.is_empty() || value.is_empty() {
            continue;
        }
        // 带空白的「键」不是 cookie 名，是属性值被误当成一段
        // （`Expires=Mon, 11 Oct 2094 03:55:23 GMT` 里的 `GMT` 之类）
        if key.contains(char::is_whitespace) {
            continue;
        }
        if COOKIE_ATTRIBUTES
            .iter()
            .any(|attribute| key.eq_ignore_ascii_case(attribute))
        {
            continue;
        }

        // 同名以后者为准，同时保留首次出现的顺序（便于人工核对时对得上原文）
        match pairs.iter_mut().find(|(existing, _)| *existing == key) {
            Some(existing) => existing.1 = value,
            None => pairs.push((key, value)),
        }
    }

    if pairs.is_empty() {
        return None;
    }

    Some(
        pairs
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_below_stays_in_range() {
        for _ in 0..1_000 {
            assert!(random_below(7) < 7);
        }
        assert_eq!(random_below(0), 0);
        assert_eq!(random_below(1), 0);
    }

    #[test]
    fn successive_random_values_differ() {
        assert_ne!(random_u64(), random_u64());
    }

    #[test]
    fn timestamp_is_plausible() {
        // 2020-01-01 之后的毫秒时间戳
        assert!(now_unix_millis() > 1_577_836_800_000);
    }

    /// 真实形状：属性段 + `;;` 无空格分隔。规范化后每个 cookie 都要能独立取出，
    /// 尤其是被 `;;` 粘在 `Path` 后面的 `MUSIC_U`——服务端就是靠它认人的。
    #[test]
    fn normalize_detaches_cookie_glued_to_an_attribute() {
        let raw = "MUSIC_R_T=1708939476575; Max-Age=2147483647; \
                   Expires=Mon, 11 Oct 2094 03:55:23 GMT; \
                   Path=/openapi/clientlog;;MUSIC_U=00CC9B71; \
                   __csrf=8fbfbebc";
        let normalized = normalize_cookie_header(raw).expect("应当能规范化");

        assert_eq!(
            normalized, "MUSIC_R_T=1708939476575; MUSIC_U=00CC9B71; __csrf=8fbfbebc",
            "属性段要丢掉，`;;` 处要切开"
        );
        // 关键断言：服务端按 `;\s+` 切分时，MUSIC_U 必须是独立的一项
        assert!(
            normalized
                .split("; ")
                .any(|pair| pair.starts_with("MUSIC_U=")),
            "MUSIC_U 必须能独立取到，实际：{normalized}"
        );
    }

    /// 酷狗那串本来就规范，规范化必须是幂等的——否则每次发请求都会改写凭据。
    #[test]
    fn normalize_is_idempotent_for_a_clean_cookie() {
        let clean = "token=abc; userid=1";
        assert_eq!(normalize_cookie_header(clean).as_deref(), Some(clean));
    }

    /// 同名 cookie 后写的覆盖先写的，且顺序按首次出现——重排会让 diff 难以比对。
    #[test]
    fn normalize_keeps_last_value_in_first_position() {
        let normalized = normalize_cookie_header("a=1; b=2; a=3").expect("应当能规范化");
        assert_eq!(normalized, "a=3; b=2");
    }

    /// 全是属性或空串时没有可发的凭据，返回 `None` 而不是空串——
    /// 空串会被当成「有 cookie」发出去，多一个空的 `Cookie:` 头。
    #[test]
    fn normalize_returns_none_without_any_cookie() {
        assert_eq!(normalize_cookie_header(""), None);
        assert_eq!(normalize_cookie_header(";;;"), None);
        assert_eq!(
            normalize_cookie_header("Max-Age=0; Path=/; Secure"),
            None,
            "属性段不构成凭据"
        );
        assert_eq!(normalize_cookie_header("MUSIC_U="), None, "空值不算凭据");
    }

    /// 属性名大小写不敏感（`Set-Cookie` 实际下发过 `Path` 与 `path` 两种写法）。
    #[test]
    fn normalize_matches_attributes_case_insensitively() {
        assert_eq!(
            normalize_cookie_header("MUSIC_U=x; PATH=/; expires=Mon, 11 Oct 2094 03:55:23 GMT")
                .as_deref(),
            Some("MUSIC_U=x")
        );
    }

    /// 日期格式必须正好是接口要的 `YYYY-MM-DD`：多一个空格、少一个前导零，
    /// 服务端都只当是「那一天不存在」，报错还看不出原因。
    #[test]
    fn today_local_is_a_plain_iso_date() {
        let Some(today) = today_local() else {
            // 多线程下 time 可能拒绝推断本地偏移，这时调用方会放弃领取。
            // 测试不能因此变成偶发失败。
            return;
        };
        assert_eq!(today.len(), 10, "应当是 YYYY-MM-DD：{today}");
        let parts: Vec<&str> = today.split('-').collect();
        assert_eq!(parts.len(), 3, "应当是三段：{today}");
        assert_eq!(parts[0].len(), 4, "年份四位：{today}");
        assert_eq!(parts[1].len(), 2, "月份两位（要补零）：{today}");
        assert_eq!(parts[2].len(), 2, "日期两位（要补零）：{today}");
        assert!(
            parts
                .iter()
                .all(|part| part.chars().all(|c| c.is_ascii_digit())),
            "只该有数字和短横线：{today}"
        );
    }
}
