//! HTTP 客户端。
//!
//! 只做三件事：拼 URL、带 cookie、把响应体取回来。所有接口语义都在
//! [`crate::api::catalog`] / [`crate::api::lyric`] / [`crate::api::cloud`] 里。
//!
//! # 关于 KuGouMusicApi 的缓存
//!
//! 该服务内置了 2 分钟响应缓存（相同 URL 只回源一次）。对搜索、榜单这类希望拿到
//! 最新数据的接口，需要在 query 里塞一个时间戳让 URL 唯一。这类请求走
//! [`HttpClient::get_json_uncached`]。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::COOKIE;
use serde_json::Value;

use crate::api::model::check_error_code;
use crate::error::{AppError, Result};
use crate::logger::tlog;
use crate::util::now_unix_millis;

/// 单次请求的超时。取播放直链偶尔会慢，给到 15 秒。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// 连接池里每个 host 保留的空闲连接数。4 足够覆盖「搜索 + 歌词 + 取链」的并发。
const POOL_MAX_IDLE: usize = 4;

const USER_AGENT_VALUE: &str = concat!("kugou-tui/", env!("CARGO_PKG_VERSION"));

/// 瞬时故障的重试策略。
///
/// # 为什么需要
///
/// 本机走 fake-IP 代理，网络快慢波动大：一次连接被拒、一次响应体读到一半断掉，
/// 都会让请求直接失败。这类失败**换个时刻重发就成功**。
///
/// 代价最实在的是取播放地址那条路：`song_stream_url` 会逐个试多个候选
/// (hash, 音质)，某个候选因为一次网络抖动失败就被跳过；全都抖过去之后，
/// 用户看到的是「**没有可用的播放地址（可能需要 VIP 或已下架）**」——
/// 一个和真实原因（网络抖了）完全无关的结论。同理，列表加载失败也常常只是抖了一下。
///
/// # 判据与次数
///
/// 该不该重试由 [`AppError::is_transient`] 决定（见那里的说明：**超时、业务错误码、
/// 其它 4xx 都不重试**）。次数与间隔：
///
/// * **总尝试 3 次**（首次 + 2 次重试）。瞬时抖动基本在第一次重试内就恢复了，
///   再多只是让用户对着加载指示干等。
/// * **间隔 300ms → 900ms**（×3 递增）。指数退避，但不加抖动：这是单用户的本地
///   客户端，不存在「一群客户端同时重试」的问题。
struct RetryPolicy;

impl RetryPolicy {
    /// 总尝试次数，含首次。
    const MAX_ATTEMPTS: u32 = 3;
    /// 第 1 次重试前等 300ms，第 2 次前等 900ms。
    const BASE_DELAY_MS: u64 = 300;

    /// 第 `attempt` 次尝试失败之后该等多久（`attempt` 从 1 开始）。
    fn delay_after(attempt: u32) -> Duration {
        let factor = 3u64.saturating_pow(attempt.saturating_sub(1));
        Duration::from_millis(Self::BASE_DELAY_MS.saturating_mul(factor))
    }
}

/// 纯 HTTP 传输层：拼 URL、带 cookie、取响应体。
///
/// 接口语义不在这里——酷狗的接口在 [`crate::api::node::NodeApi`]，
/// 网易云 / 汽水各自在 `source/` 下。
///
/// 内部 `reqwest::Client` 自带连接池，克隆它不会复制连接池，因此可以放心地
/// 在每个异步任务里 clone 一份。
#[derive(Debug, Clone)]
pub struct HttpClient {
    http: reqwest::Client,
    base: Arc<str>,
    cookie: Option<Arc<str>>,
}

impl HttpClient {
    /// 构造客户端。`proxy` 形如 `http://127.0.0.1:7890`。
    pub fn new(base: &str, cookie: Option<String>, proxy: Option<&str>) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(USER_AGENT_VALUE)
            .pool_max_idle_per_host(POOL_MAX_IDLE)
            .tcp_keepalive(Duration::from_secs(60));

        if let Some(proxy_url) = proxy.map(str::trim).filter(|url| !url.is_empty()) {
            let parsed = reqwest::Proxy::all(proxy_url)
                .map_err(|error| AppError::Config(format!("代理地址 {proxy_url} 无效：{error}")))?;
            builder = builder.proxy(parsed);
        }
        let http = builder
            .build()
            .map_err(|error| AppError::Config(format!("构造 HTTP 客户端失败：{error}")))?;

        Ok(Self {
            http,
            base: Arc::from(base.trim_end_matches('/')),
            cookie: cookie
                .filter(|value| !value.trim().is_empty())
                .map(Arc::from),
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// 当前带的 cookie（未经请求头拼装，原始值）。
    ///
    /// 音源模块（如汽水）用它重建自己的客户端：`HttpClient` 的请求形态
    /// （只发 GET、只能往 `{base}{path}` 拼、只带 cookie 一个身份）不满足它们，
    /// 但「有哪些身份凭据」这个信息是通用的，不该被这里藏起来。
    pub fn cookie(&self) -> Option<&str> {
        self.cookie.as_deref()
    }

    /// 更新 cookie（例如自动探测到 dfid 之后）。
    pub fn set_cookie(&mut self, cookie: Option<String>) {
        self.cookie = cookie
            .filter(|value| !value.trim().is_empty())
            .map(Arc::from);
    }

    /// 发送 GET 并返回 `(状态码, 响应体文本)`。
    async fn send(&self, path: &str, query: &[(&str, String)]) -> Result<(u16, String)> {
        let url = format!("{}{}", self.base, path);
        let mut request = self.http.get(&url).query(query);

        if let Some(cookie) = self.cookie.as_ref() {
            request = request.header(COOKIE, cookie.as_ref());
        }

        let response = request.send().await?;
        let status = response.status();
        let body = response.text().await?;
        Ok((status.as_u16(), body))
    }

    /// 发送 GET 并解析 JSON，同时校验业务错误码。
    ///
    /// 这是**读接口**：瞬时故障会按 [`RetryPolicy`] 自动重试。写接口请用
    /// [`Self::get_json_mutating`]。
    ///
    /// # 为什么先解析、后判状态码
    ///
    /// KuGouMusicApi 在业务失败时会把 `error_code` 放进响应体，而 HTTP 状态码可能同时
    /// 是 `502` 这类网关码。实测 `/search` 未登录时就是「HTTP 502 + body 里
    /// `error_code: 152`」。如果因为状态码非 2xx 就提前返回，体里那个真正能解释原因
    /// 的业务码就被丢掉了，用户只会看到一句没用的「返回状态码 502」。
    ///
    /// 所以顺序是：能解析出 JSON 就先看 `error_code`，它才是权威；只有体不可解析时
    /// 才退回用 HTTP 状态码报错。
    pub async fn get_json(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.with_retry(|| self.get_json_once(path, query)).await
    }

    /// 同 [`Self::get_json`]，但在 query 里加时间戳绕开服务端 2 分钟缓存。
    pub async fn get_json_uncached(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        // 时间戳在**每次重试时重算**：服务端有 2 分钟响应缓存，沿用同一个 URL
        // 重试可能直接命中上一轮那份失败的响应，重试就白做了。
        self.with_retry(|| async {
            let mut query = query.to_vec();
            query.push(("timestamp", now_unix_millis().to_string()));
            self.get_json_once(path, &query).await
        })
        .await
    }

    /// 取原始文本响应。歌词接口在 `decode=true` 下偶尔直接返回 LRC 纯文本。
    ///
    /// 读接口，瞬时故障会重试。
    pub async fn get_text(&self, path: &str, query: &[(&str, String)]) -> Result<String> {
        self.with_retry(|| self.get_text_once(path, query)).await
    }

    /// **写接口**：与 [`Self::get_json`] 相同，但**不重试**。
    ///
    /// 重试的前提是「同一请求重发不会改变结果」，写操作不满足这一点。以
    /// `/playlist/del` 为例：第一次其实已经删成功了、只是响应在路上丢了，重发会得到
    /// 「歌单不存在」——用户看到一句失败提示，而歌单其实已经没了。相比之下直接报
    /// 网络错误至少是诚实的：用户会自己重试，然后得到同样的「不存在」。
    ///
    /// 这类接口不多（收藏 / 移出 / 删歌单 / 新建歌单 / 领 VIP），逐个显式选不重试，
    /// 比让它们默默继承重试要安全。
    pub async fn get_json_mutating(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.get_json_once(path, query).await
    }

    /// **写接口**：同 [`Self::get_json_mutating`]，但加时间戳绕开服务端缓存。
    pub async fn get_json_uncached_mutating(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value> {
        let mut query = query.to_vec();
        query.push(("timestamp", now_unix_millis().to_string()));
        self.get_json_once(path, &query).await
    }

    /// 跑一次 `once`，失败且属于瞬时故障时按 [`RetryPolicy`] 重试。
    async fn with_retry<F, Fut, T>(&self, once: F) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut attempt = 1;
        loop {
            match once().await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if attempt >= RetryPolicy::MAX_ATTEMPTS || !error.is_transient() {
                        return Err(error);
                    }
                    let delay = RetryPolicy::delay_after(attempt);
                    // 用 `{error}`（Display）而不是 `user_hint()`：可重试的失败都是
                    // 带上下文的（HttpStatus 与 reqwest 的 Http 里都有路径），
                    // 而 user_hint 会把路径再写一遍，日志里就成了「接口 /x … （接口 /x …）」
                    tlog!(
                        crate::logger::LEVEL_WARN,
                        "第 {attempt} 次失败，{}ms 后重试：{error}",
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    /// 单次尝试：发送、解析、校验业务错误码。重试逻辑在 [`Self::with_retry`]。
    async fn get_json_once(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let (status, body) = self.send(path, query).await?;

        match serde_json::from_str::<Value>(&body) {
            Ok(value) => {
                check_error_code(path, &value)?;
                if !(200..300).contains(&status) {
                    return Err(AppError::HttpStatus {
                        path: path.to_string(),
                        status,
                    });
                }
                Ok(value)
            }
            Err(error) => {
                let preview = body_preview(&body);
                tlog!(
                    crate::logger::LEVEL_WARN,
                    "接口 {path} 返回了非 JSON 内容（HTTP {status}）：serde 说「{error}」，前 200 字节：{}",
                    body.chars().take(200).collect::<String>()
                );
                if !(200..300).contains(&status) {
                    return Err(AppError::HttpStatus {
                        path: path.to_string(),
                        status,
                    });
                }
                // 2xx 却不是 JSON：多半是**这个端口上跑着别的服务**，而不是接口坏了。
                // 单独报这一类，提示才能说清该去查什么——serde 的原话
                // （`expected value at line 1 column 1`）看不出这个。
                Err(AppError::NonJsonBody {
                    path: path.to_string(),
                    status,
                    preview,
                })
            }
        }
    }

    /// 单次尝试：取原始文本响应。
    async fn get_text_once(&self, path: &str, query: &[(&str, String)]) -> Result<String> {
        let (status, body) = self.send(path, query).await?;
        if !(200..300).contains(&status) {
            return Err(AppError::HttpStatus {
                path: path.to_string(),
                status,
            });
        }
        Ok(body)
    }
}

/// 取响应体的开头一小段，压掉换行与连续空白，用于错误信息里认出「这是什么」。
///
/// 只用来给人看：`<!doctype html>` 一眼就知道端口上跑的是网页服务，空字符串说明
/// 服务端什么都没返回。**按字符截而不是按字节**——响应体可能是中文。
fn body_preview(body: &str) -> String {
    const MAX_CHARS: usize = 120;

    let mut out = String::new();
    let mut count = 0;
    let mut last_was_space = false;
    for ch in body.chars() {
        let ch = if ch.is_whitespace() { ' ' } else { ch };
        if ch == ' ' {
            if last_was_space {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }
        if count >= MAX_CHARS {
            out.push('…');
            return out.trim().to_string();
        }
        out.push(ch);
        count += 1;
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        "（空响应体）".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 重试次数与间隔是给用户看的承诺，钉住它。
    ///
    /// 总尝试 3 次（首次 + 2 次重试）、间隔 300ms → 900ms。所以一次抖动最多让用户
    /// 多等 1.2 秒；再多等就不是「扛抖动」而是「拖着不报错」了。
    #[test]
    fn retry_backoff_is_exponential_and_bounded() {
        assert_eq!(RetryPolicy::MAX_ATTEMPTS, 3);
        assert_eq!(RetryPolicy::delay_after(1), Duration::from_millis(300));
        assert_eq!(RetryPolicy::delay_after(2), Duration::from_millis(900));
        // attempt 只会取到 MAX_ATTEMPTS - 1，但函数本身不能因此溢出
        assert_eq!(
            RetryPolicy::delay_after(20),
            Duration::from_millis(300 * 3u64.pow(19))
        );
    }

    /// 端口上跑着别的服务时，响应体是网页——摘要要能让人一眼认出来。
    #[test]
    fn body_preview_keeps_the_recognizable_start() {
        let html = "<!doctype html>\n<html lang=\"en\">\n\t<head>…";
        let preview = body_preview(html);
        assert!(preview.starts_with("<!doctype html>"), "实际：{preview}");
        assert!(!preview.contains('\n'), "换行应当被压掉：{preview}");
    }

    /// 空响应体要明说，不能给一个空串让人以为「没报错」。
    #[test]
    fn body_preview_names_an_empty_body() {
        assert_eq!(body_preview(""), "（空响应体）");
        assert_eq!(body_preview("   \n\t "), "（空响应体）");
    }

    /// 连续空白要压成一个空格，否则状态栏里全是空白。
    #[test]
    fn body_preview_collapses_whitespace_runs() {
        assert_eq!(body_preview("a \n\t  b"), "a b");
    }

    /// 截断按**字符**算。按字节截会把中文切成半个字，那是乱码。
    #[test]
    fn body_preview_truncates_by_chars_not_bytes() {
        let long = "中文".repeat(200);
        let preview = body_preview(&long);
        assert!(preview.ends_with('…'), "超长应当带省略号：{preview}");
        // 120 个字符 + 省略号；按字节截的话这里会短很多
        assert_eq!(preview.chars().count(), 121);
    }
}
