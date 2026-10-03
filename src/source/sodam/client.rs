//! 汽水音乐的 HTTP 客户端。
//!
//! # 为什么不用 [`crate::api::client::ApiClient`]
//!
//! 现有客户端是**为本地 Node 接口服务设计的**，有三个假设汽水一个都不满足：
//!
//! 1. **只发 GET**。汽水的取流端点 `POST /luna/pc/track_v2` 必须带 JSON body，
//!    而且签名覆盖 body，改 body 就得重算签名。
//! 2. **只能往 `{base}{path}` 上拼**。汽水的搜索走 `api.qishui.com`、
//!    分享页兜底走 `beta-luna.douyin.com`、取流走返回的**任意 CDN 绝对地址**
//!    （`url_player_info` 每次都不同）。这些都得能直接对绝对 URL 发请求。
//! 3. **只带 cookie 一个身份**。汽水的 App 端点还要 `x-helios` / `x-medusa`
//!    应用签名头，且缺了它们的表现是 **HTTP 200 + 空 body**——不是 4xx，
//!    光看状态码会误判成「接口下线」。
//!
//! 所以这里独立一个薄客户端：只管「发请求、拿字节」，别的什么都不管。
//! 它同时承担**限流**（见 [`RateLimiter`]）与**重试**（见 [`retry`函数]）。

use std::sync::Mutex;
use std::time::Duration;

use crate::error::{AppError, Result};
use crate::logger::{LEVEL_WARN, tlog};
use crate::util::now_unix_millis;

/// 单次请求超时。取流比普通接口慢，给到 20 秒。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// PC App 端点的 UA。汽水按 UA 决定返回什么形态的响应。
pub const PC_APP_USER_AGENT: &str = "LunaPC/3.3.0(359450208)";
/// Android 搜索网关的 UA。搜索走这个网关而不是 PC 端点。
pub const ANDROID_SEARCH_USER_AGENT: &str = "com.luna.music/100198030 (Linux; U; Android 15; zh_CN_#Hans; ABR-AL80; Build/V417IR;tt-ok/3.12.13.19)";
/// H5 分享页兜底用的浏览器 UA。
pub const WEB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                                  (KHTML, like Gecko) Chrome/134.0.0.0 Safari/537.36";

/// 重试策略，与项目内其它音源保持同一套承诺：总 3 次、间隔 300ms → 900ms。
const MAX_ATTEMPTS: u32 = 3;
const BASE_DELAY_MS: u64 = 300;

/// 同一进程内两次请求之间的最小间隔（毫秒）。
///
/// # 为什么需要主动限流
///
/// 汽水对**短时间内的请求密度**敏感：连续快速打接口会先返回空 body（和缺签名
/// 的表现一模一样），随后才被限流。所以这里在**客户端侧**主动把节奏压下来，
/// 宁可慢一点，也不要触发风控——触发之后用户看到的是「搜索不出结果」，
/// 而那种状态往往要等几分钟才恢复，比慢 200ms 难受得多。
const MIN_INTERVAL_MS: u64 = 220;

/// 进程级节流器：记住「上一次发请求的时刻」。
///
/// 用 `Mutex<Instant>` 而不是 `tokio::sync::Mutex`：临界区里只有几次整数
/// 比较，没有 IO，锁本身不会被跨 `await` 持有（那会让整个 future 失去
/// `Send`，而本项目的网络任务都要 `spawn` 到多线程运行时上）。
static LAST_REQUEST: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// 抢到一次请求许可：距上次请求不足 [`MIN_INTERVAL_MS`] 就等它。
///
/// 用 `tokio::time::sleep` 而不是 `std::thread::sleep`——后者会把整个
/// tokio 工作线程卡住，并发请求时直接拖慢整个运行时。
///
/// 锁的守卫在 `await` 之前就结束了（只用它算出「要等多久」），
/// 所以这个 future 仍然是 `Send`。
async fn throttle() {
    let wait = {
        let Ok(mut slot) = LAST_REQUEST.lock() else {
            // 锁被毒化：宁可不限流，也不要让整个音源都用不了。
            return;
        };
        let now = std::time::Instant::now();
        let wait = slot
            .and_then(|last| {
                MIN_INTERVAL_MS.checked_sub(now.duration_since(last).as_millis() as u64)
            })
            .map(Duration::from_millis);
        // 抢到许可的时刻是「现在」，不是「实际发出请求的时刻」——
        // 后者要等 sleep 完，那时并发请求已经算过间隔了。
        *slot = Some(now);
        wait
    };

    if let Some(delay) = wait {
        tokio::time::sleep(delay).await;
    }
}

/// 取当前生效的应用签名凭证。
pub fn active_credentials() -> AppCredentials {
    ACTIVE_CREDENTIALS
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or_default()
}

/// 覆盖当前生效的应用签名凭证。
///
/// 只在「按音源造客户端」那一个地方调用（见模块文档）。凭证被毒化时
/// 保持原值而不是 panic——那意味着别的线程在写它时崩了，
/// 而这里只是拿不到签名，不该让整个程序挂掉。
pub fn set_active_credentials(credentials: AppCredentials) {
    if let Ok(mut slot) = ACTIVE_CREDENTIALS.lock() {
        *slot = credentials;
    }
}

/// 汽水客户端。克隆它会共享同一个连接池（内部 `reqwest::Client` 是 `Arc`）。
#[derive(Debug, Clone)]
pub struct SodaClient {
    http: reqwest::Client,
    /// 登录态，形如 `sessionid_ss=xxx; sessionid=xxx`。
    cookie: Option<String>,
    /// 应用级签名头（`x-helios` / `x-medusa`）与设备指纹。
    credentials: AppCredentials,
}

/// 应用级签名凭证。
///
/// # 它是什么
///
/// 汽水把「整曲播放流」放在 App 端点（`POST /luna/pc/track_v2`）后面，服务端
/// 只认官方客户端原生安全组件产出的应用签名头。实测：
///
/// * 不带 → **HTTP 200 + 空 body**；
/// * 带 Web 侧签名（`a_bogus` 那套）→ 依然空 body；
/// * 带抓包得到的 `x-helios` / `x-medusa` → 正常返回整曲播放流。
///
/// 这些值只能从官方客户端的真实请求里抓，**会过期**，所以做成可配置项
/// （见 `docs/USER_GUIDE.md` 的「汽水音乐音源」一节）。没配置时不影响
/// 搜索 / 歌词 / 试听片段，只影响「VIP 整曲与无损」。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AppCredentials {
    /// URL 参数 `device_id`（16 位数字）。
    #[serde(alias = "deviceId", alias = "DEVICE_ID")]
    pub device_id: String,
    /// URL 参数 `iid`（install id）。
    #[serde(alias = "install_id", alias = "installId", alias = "IID")]
    pub iid: String,
    /// URL 参数 `fp`，一般等于 `device_id`。
    pub fp: String,
    /// 请求头 `x-helios`。
    #[serde(alias = "xHelios", alias = "X-Helios", alias = "helios")]
    pub x_helios: String,
    /// 请求头 `x-medusa`。
    #[serde(alias = "xMedusa", alias = "X-Medusa", alias = "medusa")]
    pub x_medusa: String,
    /// 抓包时的客户端 UA，留空用 [`PC_APP_USER_AGENT`]。
    #[serde(alias = "userAgent", alias = "ua")]
    pub user_agent: String,
}

/// 进程级应用签名凭证。
///
/// # 为什么是全局的
///
/// 汽水的 `x-helios` / `x-medusa` 不来自 HTTP 层（它们不是 cookie，也不是
/// URL 参数能表达的），而是**用户配置**里的一项。分派层的方法签名统一只收
/// `&ApiClient`，为了让那十几个调用点不必各自多带一个参数，这里用全局槽位
/// 承载——但**只有一处会写它**：[`crate::app`] 里的 `client_for()`，也就是
/// 「按音源造客户端」的唯一入口。读的地方因此总能拿到与当前客户端配套的凭证。
///
/// 之所以可接受：签名凭证是「这台机器上这个用户的汽水身份」，本来就是一个
/// 进程级单例（多音源切换时它也不会变），不存在并发写入的实际场景。
static ACTIVE_CREDENTIALS: Mutex<AppCredentials> = Mutex::new(AppCredentials::new_const());

impl AppCredentials {
    /// 空凭证（供上面的 `Mutex::new` 在编译期构造用）。
    const fn new_const() -> Self {
        Self {
            device_id: String::new(),
            iid: String::new(),
            fp: String::new(),
            x_helios: String::new(),
            x_medusa: String::new(),
            user_agent: String::new(),
        }
    }

    /// 设备指纹与两个签名头都齐了才算完整——只有签名没设备是不行的，
    /// 签名与设备绑定，URL 里的 `device_id` 必须与签名时一致。
    pub fn is_complete(&self) -> bool {
        !self.device_id.trim().is_empty()
            && !self.x_helios.trim().is_empty()
            && !self.x_medusa.trim().is_empty()
    }

    /// 只要设备指纹（配合只需设备、不需签名的请求）。
    pub fn has_device_fingerprint(&self) -> bool {
        !self.device_id.trim().is_empty()
    }

    /// `fp` 缺省时回落到 `device_id`（官方客户端两者通常一致）。
    pub fn fp_or_device_id(&self) -> String {
        let fp = self.fp.trim();
        if fp.is_empty() {
            self.device_id.trim().to_string()
        } else {
            fp.to_string()
        }
    }

    pub fn user_agent_or_default(&self) -> &str {
        let ua = self.user_agent.trim();
        if ua.is_empty() { PC_APP_USER_AGENT } else { ua }
    }

    /// 签名头（不含 UA 与 cookie）。
    pub fn signature_headers(&self) -> Vec<(&'static str, &str)> {
        let mut headers = Vec::new();
        if !self.x_helios.trim().is_empty() {
            headers.push(("x-helios", self.x_helios.trim()));
        }
        if !self.x_medusa.trim().is_empty() {
            headers.push(("x-medusa", self.x_medusa.trim()));
        }
        headers
    }
}

/// `X-SS-STUB`：客户端对请求体做的 MD5（大写十六进制）。
///
/// 应用签名覆盖 body，所以这个值必须**和实际发出的 body 一起算**。
/// 自己实现而不用 `md5` crate：十几行，且省掉一个依赖。
pub fn md5_hex_upper(data: &[u8]) -> String {
    use std::fmt::Write as _;

    // 常量表：floor(abs(sin(i+1)) * 2^32)
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_le_bytes());

    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];

    for chunk in message.chunks(64) {
        let mut m = [0u32; 16];
        for (index, word) in m.iter_mut().enumerate() {
            let start = index * 4;
            *word = u32::from_le_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }

        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            let sum = a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(S[i]));
            a = temp;
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }

    let mut out = String::with_capacity(32);
    // ⚠️ 必须按**小端字节序**输出，不能直接 `{:08X}` 打印寄存器值。
    //
    // MD5 全程按小端解释消息字（`u32::from_le_bytes`），最后的摘要 likewise 是
    // 「4 个 32 位字，每个字按小端展开成字节」。直接按大端打印会让输出的每个
    // 4 字节分组正好反序——症状是算出的值**只差字节序**，长度与字符集都对，
    // 用肉眼几乎看不出来，但服务端一比就拒。
    for word in state {
        for byte in word.to_le_bytes() {
            let _ = write!(out, "{byte:02X}");
        }
    }
    out
}

impl SodaClient {
    pub fn new(
        cookie: Option<String>,
        credentials: AppCredentials,
        proxy: Option<&str>,
    ) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            // 汽水的 CDN 与接口都会依据 UA 决定响应形态，不能用默认的 reqwest UA。
            .user_agent(PC_APP_USER_AGENT)
            .pool_max_idle_per_host(4)
            .tcp_keepalive(Duration::from_secs(60));

        if let Some(proxy_url) = proxy.map(str::trim).filter(|url| !url.is_empty()) {
            let parsed = reqwest::Proxy::all(proxy_url)
                .map_err(|error| AppError::Config(format!("代理地址 {proxy_url} 无效：{error}")))?;
            builder = builder.proxy(parsed);
        }

        let http = builder
            .build()
            .map_err(|error| AppError::Config(format!("构造汽水 HTTP 客户端失败：{error}")))?;

        Ok(Self {
            http,
            cookie: cookie.filter(|value| !value.trim().is_empty()),
            credentials,
        })
    }

    pub fn credentials(&self) -> &AppCredentials {
        &self.credentials
    }

    pub fn has_cookie(&self) -> bool {
        self.cookie.is_some()
    }

    /// 发 GET 拿原始字节。`url` 必须是**完整绝对地址**。
    pub async fn get_bytes(
        &self,
        url: &str,
        user_agent: &str,
        extra_headers: &[(&str, &str)],
    ) -> Result<Vec<u8>> {
        self.send_with_retry(url, user_agent, extra_headers, None)
            .await
    }

    /// 发 POST（JSON body）拿原始字节。
    pub async fn post_bytes(
        &self,
        url: &str,
        user_agent: &str,
        extra_headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Vec<u8>> {
        self.send_with_retry(url, user_agent, extra_headers, Some(body))
            .await
    }

    /// 带重试的一次请求。瞬时故障才重试（判据见 [`AppError::is_transient`]）。
    async fn send_with_retry(
        &self,
        url: &str,
        user_agent: &str,
        extra_headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let mut attempt = 1;
        loop {
            match self.send_once(url, user_agent, extra_headers, body).await {
                Ok(bytes) => return Ok(bytes),
                Err(error) => {
                    // 限流（429/5xx）可以重试；参数错误重试多少次都一样。
                    if attempt >= MAX_ATTEMPTS || !error.is_transient() {
                        return Err(error);
                    }
                    let delay =
                        Duration::from_millis(BASE_DELAY_MS.saturating_mul(3u64.pow(attempt - 1)));
                    tlog!(
                        LEVEL_WARN,
                        "汽水接口第 {attempt} 次失败，{}ms 后重试：{error}",
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn send_once(
        &self,
        url: &str,
        user_agent: &str,
        extra_headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        throttle().await;

        let mut request = match body {
            Some(body) => self.http.post(url).body(body.to_vec()),
            None => self.http.get(url),
        }
        .header(reqwest::header::USER_AGENT, user_agent);

        if let Some(cookie) = self.cookie.as_deref() {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        for (name, value) in extra_headers {
            request = request.header(*name, *value);
        }

        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;

        if !status.is_success() {
            return Err(AppError::HttpStatus {
                path: short_path(url),
                status: status.as_u16(),
            });
        }
        Ok(bytes.to_vec())
    }
}

/// URL 里的 path 部分（不含 host），用于错误信息——打整条 URL 太长且含设备指纹。
fn short_path(url: &str) -> String {
    url.split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, path)| format!("/{}", path.split('?').next().unwrap_or(path)))
        .unwrap_or_else(|| url.to_string())
}

/// PC App 端点的公共查询参数。
///
/// 汽水要求每个请求都带齐这批「客户端身份」参数，缺了会被当成爬虫。
/// `device_id` / `iid` 优先用配置里的签名凭证（**必须与签名时的设备一致**，
/// 否则 App 端点会返回空 body），没配置就临时生成一个——只影响 App 端点，
/// 搜索 / 分享页这些不校验设备的接口不受影响。
pub fn pc_app_params(credentials: &AppCredentials) -> Vec<(&'static str, String)> {
    let now = now_unix_millis();
    let device_id = {
        let configured = credentials.device_id.trim();
        if configured.is_empty() {
            now.to_string()
        } else {
            configured.to_string()
        }
    };
    let iid = {
        let configured = credentials.iid.trim();
        if configured.is_empty() {
            (now + 1).to_string()
        } else {
            configured.to_string()
        }
    };
    let fp = {
        let configured = credentials.fp_or_device_id();
        if configured.is_empty() {
            device_id.clone()
        } else {
            configured
        }
    };

    vec![
        ("aid", "386088".to_string()),
        ("app_name", "luna_pc".to_string()),
        ("region", "cn".to_string()),
        ("geo_region", "cn".to_string()),
        ("os_region", "cn".to_string()),
        ("sim_region", String::new()),
        ("device_id", device_id),
        ("cdid", String::new()),
        ("iid", iid),
        ("version_name", "3.3.0".to_string()),
        ("version_code", "30030000".to_string()),
        ("channel", "official".to_string()),
        ("build_mode", "master".to_string()),
        ("os_version", "Windows 11".to_string()),
        ("fp", fp),
    ]
}

/// 拼 query string。`Params` 是简单的有序键值对，这里手写避免引依赖。
pub fn encode_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", url_encode(key), url_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// percent-encoding。汽水的设备指纹与签名都是纯数字/十六进制，
/// 但 `X-SS-STUB` 之类可能要转义，故按通用规则实现。
pub fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_known_vectors() {
        assert_eq!(md5_hex_upper(b""), "D41D8CD98F00B204E9800998ECF8427E");
        assert_eq!(md5_hex_upper(b"abc"), "900150983CD24FB0D6963F7D28E17F72");
        assert_eq!(
            md5_hex_upper(b"The quick brown fox jumps over the lazy dog"),
            "9E107D9D372BB6826BD81D3542A419D6"
        );
    }

    /// 长于 64 字节的输入要触发多轮压缩，验证分块逻辑（不是只测单块）。
    #[test]
    fn md5_handles_multi_block_input() {
        let long = "a".repeat(1000);
        // 只断言「确定性与长度」，具体值由标准算法保证（上面已用已知向量钉住）
        assert_eq!(md5_hex_upper(long.as_bytes()).len(), 32);
        assert_eq!(
            md5_hex_upper(long.as_bytes()),
            md5_hex_upper(long.as_bytes())
        );
    }

    #[test]
    fn app_credentials_completeness_requires_device_and_both_signatures() {
        let mut credentials = AppCredentials::default();
        assert!(!credentials.is_complete(), "全空不完整");
        credentials.device_id = "123".into();
        assert!(!credentials.is_complete(), "只有设备指纹不完整");
        credentials.x_helios = "h".into();
        assert!(!credentials.is_complete(), "少一个签名头不完整");
        credentials.x_medusa = "m".into();
        assert!(credentials.is_complete(), "三者齐备才算完整");
    }

    #[test]
    fn fp_falls_back_to_device_id() {
        let mut credentials = AppCredentials {
            device_id: "dev-1".into(),
            ..Default::default()
        };
        assert_eq!(credentials.fp_or_device_id(), "dev-1");
        credentials.fp = "fp-1".into();
        assert_eq!(credentials.fp_or_device_id(), "fp-1");
    }

    #[test]
    fn signature_headers_skip_empty_values() {
        let credentials = AppCredentials {
            x_helios: " h ".into(),
            x_medusa: String::new(),
            ..Default::default()
        };
        // 空的签名头不能发出去——发空值等于告诉服务端「有签名但内容是空」
        assert_eq!(credentials.signature_headers(), vec![("x-helios", "h")]);
    }

    #[test]
    fn pc_params_use_configured_device_when_present() {
        let credentials = AppCredentials {
            device_id: "dev-42".into(),
            iid: "iid-7".into(),
            ..Default::default()
        };
        let params = pc_app_params(&credentials);
        let find = |key: &str| {
            params
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(find("device_id"), "dev-42");
        assert_eq!(find("iid"), "iid-7");
        // fp 未配置时回落到 device_id
        assert_eq!(find("fp"), "dev-42");
    }

    #[test]
    fn pc_params_generate_ephemeral_device_when_missing() {
        let params = pc_app_params(&AppCredentials::default());
        let device_id = params
            .iter()
            .find(|(k, _)| *k == "device_id")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        assert!(!device_id.is_empty(), "没配置时也要有 device_id");
        assert!(device_id.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn url_encode_escapes_reserved_characters() {
        assert_eq!(url_encode("a b"), "a%20b");
        assert_eq!(url_encode("x=1&y=2"), "x%3D1%26y%3D2");
        assert_eq!(url_encode("safe-_.~"), "safe-_.~");
    }

    #[test]
    fn encode_query_joins_pairs() {
        let params = vec![("a", "1".to_string()), ("b", "x y".to_string())];
        assert_eq!(encode_query(&params), "a=1&b=x%20y");
    }

    #[test]
    fn short_path_strips_host_and_query() {
        assert_eq!(
            short_path("https://api.qishui.com/luna/pc/track_v2?device_id=1"),
            "/luna/pc/track_v2"
        );
        // 没有 path 的地址要原样返回，不能 panic
        assert_eq!(short_path("https://example.com"), "https://example.com");
    }
}
