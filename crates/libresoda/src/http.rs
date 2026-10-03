//! 极简 HTTP 封装，语义对齐上游 `music-lib/utils` 的 `Get` / `Post` + `RequestOption`。

use crate::error::{Result, SodaError};
use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

/// 默认 UA（上游 `utils.Get` 内置的 Chrome UA）。
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36";

/// 请求选项；对应 Go 的 `utils.RequestOption` 组合（`WithHeader` / Cookie）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestOption {
    headers: Vec<(String, String)>,
    timeout: Option<Duration>,
}

impl Default for RequestOption {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestOption {
    pub fn new() -> Self {
        Self {
            headers: Vec::new(),
            timeout: None,
        }
    }

    /// 等价 `utils.WithHeader`。
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let key = key.into();
        let value = value.into();
        self.headers
            .retain(|(existing, _)| !existing.eq_ignore_ascii_case(&key));
        self.headers.push((key, value));
        self
    }

    /// 仅在 `cookie` 非空时添加 Cookie 头（对齐上游 `strings.TrimSpace(cookie) != ""` 判断）。
    pub fn cookie(self, cookie: &str) -> Self {
        if cookie.trim().is_empty() {
            self
        } else {
            self.header("Cookie", cookie.trim().to_string())
        }
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    pub fn timeout_value(&self) -> Duration {
        self.timeout.unwrap_or_else(|| Duration::from_secs(30))
    }
}

/// 合并多个选项对象，后者覆盖前者同名头。
pub fn merge_options(options: &[RequestOption]) -> RequestOption {
    let mut merged = RequestOption::new();
    for option in options {
        for (key, value) in option.headers() {
            merged = merged.header(key.clone(), value.clone());
        }
        if option.timeout.is_some() {
            merged.timeout = option.timeout;
        }
    }
    merged
}

/// HTTP 响应：状态码、最终 URL（跟随重定向后）、响应体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub final_url: String,
    pub body: Vec<u8>,
    /// 响应里 `Set-Cookie` 解析出的 key/value（对应上游 `resp.Cookies()`）。
    pub cookies: BTreeMap<String, String>,
}

impl HttpResponse {
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// GET 请求并返回最终 URL + body（对应上游 `fetchSharePage` 需要拿到重定向后的地址）。
pub fn get_full(url: &str, options: &[RequestOption]) -> Result<HttpResponse> {
    let merged = merge_options(options);
    let mut request = ureq::get(url)
        .timeout(merged.timeout_value())
        .set("User-Agent", DEFAULT_USER_AGENT);
    for (key, value) in merged.headers() {
        request = request.set(key, value);
    }

    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let final_url = response.get_url().to_string();
            let mut body = Vec::new();
            let mut reader = response.into_reader();
            let _ = reader.read_to_end(&mut body);
            return Err(SodaError::http(format!(
                "http request failed: status {status} ({final_url})"
            )));
        }
        Err(err) => return Err(SodaError::http(err.to_string())),
    };

    let status = response.status();
    let final_url = response.get_url().to_string();
    let cookies = collect_cookies(&response);
    let mut body = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut body)
        .map_err(|err| SodaError::http(err.to_string()))?;
    Ok(HttpResponse {
        status,
        final_url,
        body,
        cookies,
    })
}

/// GET 请求，返回响应体（对应上游 `utils.Get`）。
pub fn get(url: &str, options: &[RequestOption]) -> Result<Vec<u8>> {
    Ok(get_full(url, options)?.body)
}

/// POST 请求，返回响应体（对应上游 `utils.Post`）。
pub fn post_json(url: &str, body: &[u8], options: &[RequestOption]) -> Result<Vec<u8>> {
    Ok(post_bytes(url, body, options)?.body)
}

/// POST 请求，返回完整响应（含 `Set-Cookie`），供 passport 登录流程使用。
pub fn post_bytes(url: &str, body: &[u8], options: &[RequestOption]) -> Result<HttpResponse> {
    let merged = merge_options(options);
    let mut request = ureq::post(url)
        .timeout(merged.timeout_value())
        .set("User-Agent", DEFAULT_USER_AGENT);
    for (key, value) in merged.headers() {
        request = request.set(key, value);
    }
    let response = match request.send_bytes(body) {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let final_url = response.get_url().to_string();
            return Err(SodaError::http(format!(
                "http request failed: status {status} ({final_url})"
            )));
        }
        Err(err) => return Err(SodaError::http(err.to_string())),
    };
    let status = response.status();
    let final_url = response.get_url().to_string();
    let cookies = collect_cookies(&response);
    let mut data = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut data)
        .map_err(|err| SodaError::http(err.to_string()))?;
    Ok(HttpResponse {
        status,
        final_url,
        body: data,
        cookies,
    })
}

/// 解析响应里的所有 `Set-Cookie`（等价 Go 的 `resp.Cookies()`）。
fn collect_cookies(response: &ureq::Response) -> BTreeMap<String, String> {
    let mut cookies = BTreeMap::new();
    for value in response.all("Set-Cookie") {
        if let Some((name, value)) = parse_set_cookie(value) {
            cookies.insert(name, value);
        }
    }
    cookies
}

/// 从一条 `Set-Cookie` 头里取出 name/value（忽略属性段）。
fn parse_set_cookie(raw: &str) -> Option<(String, String)> {
    let first = raw.split(';').next()?.trim();
    let (name, value) = first.split_once('=')?;
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() || value.is_empty() {
        return None;
    }
    Some((name.to_string(), value.to_string()))
}
