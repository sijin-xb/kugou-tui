//! 浏览器请求器：把请求交给「跑着官方安全组件的 Chromium 页面」执行。
//!
//! 复刻参考实现（`qq01-hub/Meting-API` 的 `providers/qishui/signer.js`）：
//! 页面里的 `window.__qishuiRequest` 会补上 `a_bogus` 等签名参数并附带
//! `X-Helios` / `X-Medusa` 头，因此请求必须由页面发出，而不是我们本地直连。
//!
//! 本 crate 通过一个外部命令（`tools/qishui-signer/sign-cli.mjs`）对接本地
//! 签名服务：stdin 收 JSON、stdout 回 JSON。

use crate::error::{Result, SodaError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// 交给浏览器页面执行的请求。
#[derive(Debug, Clone, Default, Serialize)]
pub struct BrowserRequest {
    /// 会话隔离键：一个二维码对应一个独立的浏览器上下文（cookie jar + 设备身份）。
    ///
    /// 留空时签名服务会退化成共用一个上下文（老版本行为），容易被按设备限流。
    #[serde(rename = "sessionKey", skip_serializing_if = "String::is_empty")]
    pub session_key: String,
    pub method: String,
    pub url: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub ms_token: String,
}

/// 页面返回的结果。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BrowserResponse {
    #[serde(default)]
    pub ok: bool,
    pub status: u16,
    pub body: String,
    #[serde(rename = "responseURL", alias = "response_url")]
    pub response_url: String,
    pub headers: String,
    pub cookies: Vec<BrowserCookie>,
    #[serde(default)]
    pub error: String,
}

impl BrowserResponse {
    /// 把浏览器上下文里的 Cookie 整理成 `name=value` 列表。
    pub fn cookie_pairs(&self) -> Vec<String> {
        self.cookies
            .iter()
            .filter(|cookie| !cookie.name.is_empty() && !cookie.value.is_empty())
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BrowserCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
}

/// 请求执行器。
pub trait BrowserRequester: Send + Sync {
    fn request(&self, request: &BrowserRequest) -> Result<BrowserResponse>;
    /// 关闭该会话的浏览器上下文。默认空实现，签名服务不支持时静默跳过。
    fn close_session(&self, _session_key: &str) -> Result<()> {
        Ok(())
    }
    /// 登记一次二次验证（`check_qrconnect` 返回 `error_code=2046` 时由
    /// `qr_login` 调用）：决策 JSON 会原样回给验证窗口，网络请求经
    /// `session_key` 对应的浏览器上下文代发。默认不支持。
    fn register_second_verify(
        &self,
        _token: &str,
        _session_key: &str,
        _decision: &Value,
        _general_params: &Value,
    ) -> Result<()> {
        Err(SodaError::http(
            "当前签名服务不支持二次验证窗口，请改用官方客户端完成验证后导出 Cookie",
        ))
    }
    /// 二次验证窗口的地址（用系统浏览器打开；token 作为能力凭证）。
    fn second_verify_url(&self, _token: &str) -> Result<String> {
        Err(SodaError::http(
            "当前签名服务不支持二次验证窗口，请改用官方客户端完成验证后导出 Cookie",
        ))
    }
    /// 在签名页浏览器里打开**可见**的二次验证窗口（同一浏览器上下文，
    /// cookie/设备身份自动对齐，无 CORS 缝隙）。默认不支持。
    fn open_second_verify_window(&self, _token: &str) -> Result<String> {
        Err(SodaError::http(
            "当前签名服务不支持在浏览器窗口中打开二次验证",
        ))
    }
    /// 用户是否已在验证窗口里完成验证（由验证页回执置位）。
    fn second_verify_done(&self, _token: &str) -> bool {
        false
    }
    /// 消费「已完成」标志（重发确认前调用，避免同一完成回执触发多次重发）。
    fn ack_second_verify(&self, _token: &str) -> Result<()> {
        Ok(())
    }
    /// 清理二次验证登记（登录结束/过期时调用）。
    fn clear_second_verify(&self, _token: &str) -> Result<()> {
        Ok(())
    }
    fn name(&self) -> &'static str;
}

/// 通过外部命令调用签名/浏览器服务（stdin JSON → stdout JSON）。
#[derive(Debug, Clone)]
pub struct CommandRequester {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandRequester {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// 跑一次外部 CLI：stdin 喂 JSON，stdout 原样返回。
    fn exec(&self, payload: &[u8]) -> Result<String> {
        let mut child = Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| SodaError::http(format!("requester spawn {}: {err}", self.program)))?;
        {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| SodaError::http("requester stdin unavailable"))?;
            stdin
                .write_all(payload)
                .map_err(|err| SodaError::http(format!("requester stdin write: {err}")))?;
        }
        drop(child.stdin.take());
        let output = child
            .wait_with_output()
            .map_err(|err| SodaError::http(format!("requester wait: {err}")))?;
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

impl BrowserRequester for CommandRequester {
    fn request(&self, request: &BrowserRequest) -> Result<BrowserResponse> {
        let payload = serde_json::to_vec(request)
            .map_err(|err| SodaError::json(format!("browser request encode: {err}")))?;
        let text = self.exec(&payload)?;
        let parsed: BrowserResponse = serde_json::from_str(text.trim()).map_err(|err| {
            SodaError::json(format!(
                "requester 返回无法解析: {err}（输出: {}）",
                text.trim()
            ))
        })?;
        if !parsed.ok || parsed.error.is_empty() && parsed.status == 0 {
            return Err(SodaError::http(if parsed.error.is_empty() {
                "签名服务未返回结果".to_string()
            } else {
                parsed.error.clone()
            }));
        }
        Ok(parsed)
    }

    fn close_session(&self, session_key: &str) -> Result<()> {
        if session_key.trim().is_empty() {
            return Ok(());
        }
        let payload = serde_json::json!({ "op": "close", "sessionKey": session_key });
        let text = self.exec(payload.to_string().as_bytes())?;
        let parsed: BrowserResponse = serde_json::from_str(text.trim())
            .map_err(|err| SodaError::json(format!("close session 返回无法解析: {err}")))?;
        if !parsed.ok {
            return Err(SodaError::http("签名服务关闭会话失败"));
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "command-requester"
    }
}
