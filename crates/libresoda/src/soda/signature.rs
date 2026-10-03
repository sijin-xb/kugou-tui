//! 签名提供者：`msToken` / `a_bogus` / `bd-ticket-guard-*` 的注入点。
//!
//! ## 背景
//!
//! 汽水/抖音的风控参数 **不由 JS 生成**：客户端把它们交给原生层
//! （`ttnet.node` + 客户端目录里的 `mssdk/`、`bdticket.node`）计算。
//! 纯 Rust / 纯 HTTP 实现无法自行算出，因此本 crate 提供三种接法：
//!
//! | 实现 | 适用场景 |
//! | --- | --- |
//! | [`CapturedSignature`] | 抓包回填（已有抓包文件时的零依赖方案） |
//! | [`CommandSignature`] | **直接调用 mssdk**：把签名交给外部程序（Windows 上的 mssdk 桥接器 / wine / SSH 到 Windows 机器） |
//! | [`NoopSignature`] | 默认：不签名（只用公开接口） |
//!
//! 详见 `docs/MSSDK-PORTING.md`。

use crate::error::{Result, SodaError};
use crate::util::now_millis;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// 应用级签名凭证：官方客户端每次请求都会带上的 `x-helios` / `x-medusa`
/// 以及与之绑定的设备指纹。
///
/// ## 为什么需要它
///
/// 汽水把「整曲播放流」放在 App 端点（`POST /luna/pc/track_v2`）后面，服务端
/// 只认原生安全组件（`mssdk/metasecml.dll`）产出的应用签名头。实测结论：
///
/// * 不带这几个头 → `HTTP 200` + **空 body**（不是 4xx，容易被误判成"接口下线"）；
/// * 带 Web 侧 `a_bogus`（本 crate 的扫码登录签名链路）→ 依然空 body；
/// * 带抓包得到的 `x-helios` / `x-medusa` → 正常返回整曲的播放流。
///
/// 这几个值只能从官方客户端的真实请求里抓（社区项目 music-lib / Meting-API /
/// qishui-api / qishuiMusicAnalysis 全都是同一套做法），因此本 crate 用一个
/// 可持久化的凭证结构接住它们。抓取步骤见 `docs/FULL-QUALITY-STREAM.md`。
///
/// ## 有效期
///
/// 凭证与设备/会话绑定，会过期。过期后 App 端点重新返回空 body，此时用
/// [`crate::Soda::check_stream_access`] 会看到 `pc_error` 提示，重新抓一次即可。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppCredentials {
    /// URL 查询参数 `device_id`（16 位数字，与官方客户端一致）。
    #[serde(alias = "deviceId", alias = "DEVICE_ID")]
    pub device_id: String,
    /// URL 查询参数 `iid`（install id）。
    #[serde(alias = "install_id", alias = "installId", alias = "IID")]
    pub iid: String,
    /// URL 查询参数 `fp`（一般等于 `device_id`）。
    pub fp: String,
    /// 请求头 `x-helios`。
    #[serde(alias = "xHelios", alias = "X-Helios", alias = "helios")]
    pub x_helios: String,
    /// 请求头 `x-medusa`。
    #[serde(alias = "xMedusa", alias = "X-Medusa", alias = "medusa")]
    pub x_medusa: String,
    /// 可选：抓包时的客户端 UA（默认用 `LunaPC/3.0.0(290101097)`）。
    #[serde(alias = "userAgent", alias = "ua")]
    pub user_agent: String,
}

impl AppCredentials {
    /// 只要设备指纹与两个签名头齐了就算可用。
    pub fn is_complete(&self) -> bool {
        !self.device_id.trim().is_empty()
            && !self.x_helios.trim().is_empty()
            && !self.x_medusa.trim().is_empty()
    }

    /// 只有设备指纹（`device_id`）——配合**实时签名器**使用时的形态：
    /// 签名每次都新算，这里只需要保证 URL 里的 `device_id` / `iid` / `fp`
    /// 与签名器里的设备一致。
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

    pub fn user_agent_or_default(&self) -> String {
        let ua = self.user_agent.trim();
        if ua.is_empty() {
            super::types::PC_APP_USER_AGENT.to_string()
        } else {
            ua.to_string()
        }
    }

    /// 应用签名请求头（空值跳过）。
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = Vec::new();
        if !self.x_helios.trim().is_empty() {
            headers.push(("x-helios", self.x_helios.trim().to_string()));
        }
        if !self.x_medusa.trim().is_empty() {
            headers.push(("x-medusa", self.x_medusa.trim().to_string()));
        }
        headers
    }

    /// 从 JSON 文本解析（字段名同结构体，支持 `xHelios` / `xMedusa` 驼峰别名）。
    pub fn from_json(raw: &str) -> Result<Self> {
        let parsed: AppCredentials = serde_json::from_str(raw)
            .map_err(|err| SodaError::json(format!("app credentials json parse error: {err}")))?;
        Ok(parsed)
    }

    /// 从 JSON 文件解析。
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|err| {
            SodaError::invalid_input(format!(
                "app credentials file {} read error: {err}",
                path.display()
            ))
        })?;
        Self::from_json(&raw)
    }

    /// 抓包结果里常带 `X-Helios` / `xMedusa` 等大小写变体，这里统一归一化。
    pub fn normalized(mut self) -> Self {
        self.device_id = self.device_id.trim().to_string();
        self.iid = self.iid.trim().to_string();
        self.fp = self.fp.trim().to_string();
        self.x_helios = self.x_helios.trim().to_string();
        self.x_medusa = self.x_medusa.trim().to_string();
        self.user_agent = self.user_agent.trim().to_string();
        self
    }
}

/// 一次签名请求的输入。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SignRequest {
    pub url: String,
    pub method: String,
    pub body: String,
    pub ts_ms: i64,
    /// 这次请求**将要发送**的完整请求头。
    ///
    /// 汽水的应用签名覆盖「URL + 请求头」（客户端在 `src/app.ts` 里把 headers
    /// 展平成 `k\r\nv` 交给 `bdms.generateHttpSignatureHeaders`），因此签名器必须
    /// 看到与实际发送一致的头，尤其是 `cookie` 和 `x-ss-stub`（body 的 MD5 大写）。
    pub headers: BTreeMap<String, String>,
}

/// 签名结果：空的字段会被忽略。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SignResponse {
    #[serde(alias = "msToken")]
    pub ms_token: String,
    #[serde(alias = "aBogus", alias = "a_bogus")]
    pub a_bogus: String,
    /// 额外请求头（例如 `bd-ticket-guard-*`、`x-tt-passport-trace-id`）。
    pub headers: BTreeMap<String, String>,
}

impl SignResponse {
    pub fn is_empty(&self) -> bool {
        self.ms_token.is_empty() && self.a_bogus.is_empty() && self.headers.is_empty()
    }
}

/// 签名提供者。
pub trait SignatureProvider: Send + Sync {
    /// 为一次请求生成签名参数/头。出错时应返回 `Err`，调用方会退化为不签名。
    fn sign(&self, request: &SignRequest) -> Result<SignResponse>;
    /// 名称，便于日志与诊断。
    fn name(&self) -> &'static str;
}

/// 默认实现：什么都不加。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSignature;

impl SignatureProvider for NoopSignature {
    fn sign(&self, _request: &SignRequest) -> Result<SignResponse> {
        Ok(SignResponse::default())
    }
    fn name(&self) -> &'static str {
        "noop"
    }
}

/// 抓包回填：把抓包中看到的 `msToken` / `a_bogus` / 头原样带上。
#[derive(Debug, Clone, Default)]
pub struct CapturedSignature {
    pub ms_token: String,
    pub a_bogus: String,
    pub headers: BTreeMap<String, String>,
}

impl SignatureProvider for CapturedSignature {
    fn sign(&self, _request: &SignRequest) -> Result<SignResponse> {
        Ok(SignResponse {
            ms_token: self.ms_token.clone(),
            a_bogus: self.a_bogus.clone(),
            headers: self.headers.clone(),
        })
    }
    fn name(&self) -> &'static str {
        "captured"
    }
}

/// 远程 HTTP 签名服务：**分离架构的落点**。
///
/// `x-helios` / `x-medusa` 只能由原生组件（`mssdk/metasecml.dll`）生成，而它只有
/// Windows/macOS 版；于是把"签名"抽成一个独立进程/服务，libresoda 通过 HTTP 调用：
///
/// ```text
///   [libresoda / 第三方客户端 (Linux)] --HTTP--> [signer service (Windows)]
///          POST /sign {"url","method","body","ts_ms"}      │
///          <-- {"headers":{"x-helios":"..","x-medusa":".."}}│
///                                                    └─ 官方客户端 / mssdk 桥接器
/// ```
///
/// 请求/响应体与 [`CommandSignature`] 完全一致（同一份 JSON 契约），因此本地命令、
/// SSH 调用、局域网/容器里的 HTTP 服务可以互换。也兼容 Meting-API 那种
/// `{"ok":true,"X-Helios":"…","X-Medusa":"…"}` 的扁平回包。
///
/// ```no_run
/// use std::sync::Arc;
/// use libresoda::soda::signature::HttpSignature;
/// use libresoda::Soda;
///
/// let soda = Soda::new("sessionid_ss=...");
/// soda.set_signature_provider(Arc::new(HttpSignature::new("http://win-box:8899/sign")));
/// # Ok::<(), libresoda::SodaError>(())
/// ```
#[derive(Debug, Clone)]
pub struct HttpSignature {
    pub url: String,
    pub timeout_ms: u64,
    /// 可选：调用签名服务时附加的请求头（例如鉴权用的 `Authorization`）。
    pub headers: BTreeMap<String, String>,
}

impl HttpSignature {
    /// 默认超时 6 秒：签名服务通常在同一局域网，超过这个时间说明它挂了。
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            timeout_ms: 6_000,
            headers: BTreeMap::new(),
        }
    }

    pub fn timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// 设置 `Authorization: Bearer <token>`（签名服务开了鉴权时用）。
    ///
    /// ```no_run
    /// use libresoda::soda::signature::HttpSignature;
    ///
    /// let provider = HttpSignature::new("http://signer.example.com:8899/sign")
    ///     .with_token("换成一串随机值");
    /// # Ok::<(), libresoda::SodaError>(())
    /// ```
    pub fn with_token(self, token: impl AsRef<str>) -> Self {
        let token = token.as_ref().trim();
        if token.is_empty() {
            return self;
        }
        self.header("Authorization", format!("Bearer {token}"))
    }

    /// 从环境变量创建（与 Meting-API 的约定同名）：
    ///
    /// * `QISHUI_SIGNER_URL` —— 签名服务地址（必填，空值视为未配置）
    /// * `QISHUI_SIGNER_TOKEN` —— 可选；非空时带上 `Authorization: Bearer <token>`
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("QISHUI_SIGNER_URL").ok()?;
        let url = url.trim();
        if url.is_empty() {
            return None;
        }
        let token = std::env::var("QISHUI_SIGNER_TOKEN").unwrap_or_default();
        Some(Self::new(url).with_token(token))
    }

    /// 解析签名服务回包；兼容结构体形式与 Meting 的扁平形式。
    pub fn parse_response(raw: &[u8]) -> Result<SignResponse> {
        let value: serde_json::Value = serde_json::from_slice(raw)
            .map_err(|err| SodaError::json(format!("signer response decode: {err}")))?;
        if value.get("ok").and_then(|ok| ok.as_bool()) == Some(false) {
            let message = value
                .get("error")
                .and_then(|error| error.as_str())
                .unwrap_or("签名服务返回失败");
            let code = value
                .get("code")
                .and_then(|code| code.as_str())
                .unwrap_or_default();
            // 服务端要求鉴权时给出可操作的提示（libmssdk 会回 ok:false + code=unauthorized）
            let unauthorized = code.eq_ignore_ascii_case("unauthorized")
                || message.to_ascii_lowercase().contains("unauthorized")
                || message.contains("401");
            return Err(SodaError::http(if unauthorized {
                format!(
                    "signer error: {message}（签名服务要求鉴权：设置 QISHUI_SIGNER_TOKEN，\
                     或用 HttpSignature::new(url).with_token(..) / .header(\"Authorization\", ..)）"
                )
            } else {
                format!("signer error: {message}")
            }));
        }
        let mut response: SignResponse = serde_json::from_value(value.clone())
            .map_err(|err| SodaError::json(format!("signer response decode: {err}")))?;
        // 兼容 {"ok":true,"X-Helios":"…","X-Medusa":"…"}
        for (alias, canonical) in [
            ("X-Helios", "x-helios"),
            ("X-Medusa", "x-medusa"),
            ("x-helios", "x-helios"),
            ("x-medusa", "x-medusa"),
        ] {
            if let Some(text) = value.get(alias).and_then(|item| item.as_str()) {
                if !text.trim().is_empty() {
                    response
                        .headers
                        .insert(canonical.to_string(), text.trim().to_string());
                }
            }
        }
        Ok(response)
    }
}

impl SignatureProvider for HttpSignature {
    fn sign(&self, request: &SignRequest) -> Result<SignResponse> {
        let payload = serde_json::to_vec(request)
            .map_err(|err| SodaError::json(format!("sign request encode: {err}")))?;
        let mut option = crate::http::RequestOption::new()
            .header("Content-Type", "application/json; charset=utf-8")
            .header("Accept", "application/json")
            .timeout(std::time::Duration::from_millis(self.timeout_ms));
        for (name, value) in &self.headers {
            option = option.header(name.clone(), value.clone());
        }
        let raw = crate::http::post_json(&self.url, &payload, &[option])?;
        Self::parse_response(&raw)
    }

    fn name(&self) -> &'static str {
        "http"
    }
}

/// 外部命令签名：把 [`SignRequest`] 的 JSON 写到子进程 stdin，
/// 从 stdout 读回 [`SignResponse`] 的 JSON。
///
/// 这是"直接调用 mssdk"的落点 —— 命令可以是任意东西：
///
/// * Windows 机器上的 mssdk 桥接器：`mssdk-bridge.exe`
/// * 通过 SSH 调用 Windows：`ssh win-box mssdk-bridge.exe`
/// * Wine 下运行的小工具，或自写的 Frida 脚本
///
/// 契约（两端都可只用标准库实现）：
///
/// ```text
/// stdin : {"url":"...","method":"POST","body":"...","ts_ms":1789394000000}
/// stdout: {"ms_token":"...","a_bogus":"...","headers":{"bd-ticket-guard-version":"2"}}
/// ```
#[derive(Debug, Clone)]
pub struct CommandSignature {
    pub program: String,
    pub args: Vec<String>,
    pub timeout_ms: u64,
}

impl CommandSignature {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            timeout_ms: 5_000,
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

    pub fn timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }
}

impl SignatureProvider for CommandSignature {
    fn sign(&self, request: &SignRequest) -> Result<SignResponse> {
        let payload = serde_json::to_vec(request)
            .map_err(|err| SodaError::json(format!("sign request encode: {err}")))?;

        let mut child = Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| SodaError::http(format!("signer spawn {}: {err}", self.program)))?;

        {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| SodaError::http("signer stdin unavailable"))?;
            stdin
                .write_all(&payload)
                .map_err(|err| SodaError::http(format!("signer stdin write: {err}")))?;
        }
        drop(child.stdin.take());

        let output = child
            .wait_with_output()
            .map_err(|err| SodaError::http(format!("signer wait: {err}")))?;
        if !output.status.success() {
            return Err(SodaError::http(format!(
                "signer exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|err| SodaError::json(format!("sign response decode: {err}")))
    }

    fn name(&self) -> &'static str {
        "command"
    }
}

/// 便捷助手：构造带签名的请求 URL（把 `msToken` / `a_bogus` 追加到 query）。
pub fn apply_signature_to_url(url: &str, response: &SignResponse) -> String {
    if response.ms_token.is_empty() && response.a_bogus.is_empty() {
        return url.to_string();
    }
    let mut params = crate::util::Params::from_pairs(parse_query_pairs(url));
    if !response.ms_token.is_empty() {
        params.set("msToken", response.ms_token.clone());
    }
    if !response.a_bogus.is_empty() {
        params.set("a_bogus", response.a_bogus.clone());
    }
    let base = url.split('?').next().unwrap_or(url);
    let base = format!("{base}?{}", params.encode());
    base
}

fn parse_query_pairs(url: &str) -> Vec<(String, String)> {
    let Some(query) = url.split_once('?').map(|(_, query)| query) else {
        return Vec::new();
    };
    let query = query.split('#').next().unwrap_or(query);
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (
                crate::util::query_unescape(key).unwrap_or_else(|| key.to_string()),
                crate::util::query_unescape(value).unwrap_or_else(|| value.to_string()),
            )
        })
        .collect()
}

/// 便于外部构造请求上下文。
pub fn sign_request(url: &str, method: &str, body: &str) -> SignRequest {
    SignRequest {
        url: url.to_string(),
        method: method.to_string(),
        body: body.to_string(),
        ts_ms: now_millis(),
        headers: std::collections::BTreeMap::new(),
    }
}

/// 同 [`sign_request`]，但把"将要发送的请求头"一并交给签名器（取流场景必须这样）。
pub fn sign_request_with_headers(
    url: &str,
    method: &str,
    body: &str,
    headers: &[(String, String)],
) -> SignRequest {
    SignRequest {
        url: url.to_string(),
        method: method.to_string(),
        body: body.to_string(),
        ts_ms: now_millis(),
        headers: headers.iter().cloned().collect(),
    }
}

/// 为「整曲取流」请求取一次应用级签名，并把结果写回请求。
///
/// 为什么必须逐请求签：实测（见 `docs/FULL-QUALITY-STREAM.md`）同一对
/// `x-helios` / `x-medusa` 只要换了 body（哪怕只是 JSON 键顺序变化）或换了
/// `track_id`，`POST /luna/pc/track_v2` 就返回 `HTTP 200` + 0 字节；原样重放
/// 才能拿到整曲。所以签名只能来自**能算签名的进程**：
///
/// * 官方客户端本身（Windows / macOS 虚拟机）；
/// * 调 `mssdk/metasecml.dll` 的桥接器（见 `docs/MSSDK-PORTING.md`）。
///
/// 契约：`SignRequest{url, method:"POST", body}` → `SignResponse.headers` 里返回
/// `x-helios` / `x-medusa`（也可以顺手带 `x-tt-trace-id` 等），
/// `ms_token` / `a_bogus` 非空时会追加到 URL 查询参数上。
///
/// 返回 `Some(url)` 表示 URL 可能被签名参数改写；签名不可用时返回 `None`，
/// 调用方按"未签名"继续（服务器会回空 body，由上层给出可读错误）。
pub(crate) fn apply_stream_signature(
    soda: &super::Soda,
    url: &str,
    body: &str,
    options: &mut Vec<crate::http::RequestOption>,
) -> Option<String> {
    let provider = soda.signature_provider()?;
    // 先合并出"这次真的要发出去的头"，再交给签名器：汽水签名覆盖 URL + 头，
    // 少一个（比如 cookie / x-ss-stub）都会被判空响应。
    let mut merged = crate::http::merge_options(options);
    let request = sign_request_with_headers(url, "POST", body, merged.headers());
    let response = provider.sign(&request).ok()?;
    if response.is_empty() {
        return None;
    }

    // 先整体覆盖成合并后的头，避免调用方再关心顺序。
    for (name, value) in &response.headers {
        merged = merged.header(name.clone(), value.clone());
    }
    *options = vec![merged];

    let signed_url = apply_signature_to_url(url, &response);
    if signed_url == url {
        None
    } else {
        Some(signed_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn stream_signature_injects_helios_and_medusa_headers() {
        let soda = super::super::Soda::new("sessionid_ss=test");
        let mut captured = CapturedSignature {
            ms_token: "token-value".to_string(),
            ..Default::default()
        };
        captured
            .headers
            .insert("x-helios".to_string(), "helios-from-bridge".to_string());
        captured
            .headers
            .insert("x-medusa".to_string(), "medusa-from-bridge".to_string());
        soda.set_signature_provider(Arc::new(captured));

        let mut options = vec![crate::http::RequestOption::new()
            .header("User-Agent", "LunaPC/3.8.0(467160162)")
            .header("Content-Type", "application/json; charset=utf-8")];
        let signed = apply_stream_signature(
            &soda,
            "https://api.qishui.com/luna/pc/track_v2?aid=386088",
            r#"{"track_id":"1","media_type":"track"}"#,
            &mut options,
        )
        .expect("签名器返回了参数，URL 应被改写");

        assert!(signed.contains("msToken=token-value"));
        assert!(signed.contains("aid=386088"), "原有查询参数不能被丢掉");
        let merged = crate::http::merge_options(&options);
        let names: Vec<String> = merged
            .headers()
            .iter()
            .map(|(name, _)| name.to_ascii_lowercase())
            .collect();
        assert!(names.contains(&"x-helios".to_string()));
        assert!(names.contains(&"x-medusa".to_string()));
        assert!(
            names.contains(&"user-agent".to_string()),
            "原有头不应被覆盖掉"
        );
    }

    #[test]
    fn stream_signature_without_provider_keeps_request_untouched() {
        let soda = super::super::Soda::new("sessionid_ss=test");
        let mut options =
            vec![crate::http::RequestOption::new().header("User-Agent", "LunaPC/3.8.0(467160162)")];
        let signed = apply_stream_signature(
            &soda,
            "https://api.qishui.com/luna/pc/track_v2?aid=386088",
            "{}",
            &mut options,
        );
        assert!(signed.is_none());
        assert_eq!(options.len(), 1);
    }
}
