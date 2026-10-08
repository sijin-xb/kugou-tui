//! 内嵌后端的传输层：把上游 `util/request.js` 的 `createRequest` 搬到 Rust。
//!
//! 与 [`crate::api::client::HttpClient`] 的分工完全不同：
//!
//! * `HttpClient` 面向**本机 KuGouMusicApi 的 REST 路由**——只拼 URL、带 cookie，
//!   不签名、不带 `kg-*` 头，因为那些都是那个 Node 服务替我们做的。
//! * 这里面向**酷狗网关**——必须自己签名、自己带设备头，参数顺序与上游逐字一致。
//!
//! # 与上游的两处刻意不同
//!
//! 1. **不加 `timestamp`**。`HttpClient::get_json_uncached` 往本机服务的 URL 上
//!    塞时间戳，是为了绕开 *那个服务* 的 apicache；时间戳只存在于客户端与
//!    本机服务之间，`module/*.js` 自己拼参数时并不包含它。native 直连网关，
//!    所谓「不缓存」就是**不走本地缓存**，往上游 URL 里塞一个上游不认识的
//!    参数只会让签名与请求都对不上。
//!
//! 2. **失败判据照抄 `util/request.js:210`**：`status === 0` 或 `error_code`
//!    非 0 都算失败。只认 `error_code` 会把 `status: 0` 那类静默失败当成功。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::api::model::check_error_code;
use crate::api::native::device::Device;
use crate::api::native::sign::{self, ParamValue};
use crate::error::{AppError, Result};
use crate::logger::tlog;
use crate::source::SourceKind;
use crate::util::now_unix_millis;

/// 酷狗默认网关。对应上游 `util/request.js:142` 的 `baseURL` 兜底值。
pub const GATEWAY_BASE: &str = "https://gateway.kugou.com";

/// 歌词接口的独立域名（`module/lyric.js`、`module/search_lyric.js` 自带 `baseURL`）。
///
/// 阶段 4 接歌词时用。
#[allow(dead_code)]
pub const LYRICS_BASE: &str = "https://lyrics.kugou.com";

/// 设备注册接口的独立域名（`module/register_dev.js` 自带 `baseURL`）。
pub const USER_SERVICE_BASE: &str = "https://userservice.kugou.com";

/// 上游默认 UA（`util/request.js:143`）。**不是** `kugou-tui/…`——
/// 网关按 UA 判客户端类型，换掉它取流会失败。
pub const USER_AGENT: &str = "Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi";

/// 上游 `kg-thash` / `kg-rf`，写死在 `util/request.js:83`。
const KG_THASH: &str = "5d816a0";
const KG_RF: &str = "B9EDA08A64250DEFFBCADDEE00F8F25F";

/// 本地响应缓存的有效期，对齐上游 `server.js:318` 的 `cache('2 minutes', …)`。
const CACHE_TTL: Duration = Duration::from_secs(120);

/// 缓存条目上限。上游 apicache 不设上限，但它是个长驻服务；native 要长期占着
/// 用户的内存，缓存一个歌单几百 KB 的 JSON 时必须有天花板。
const CACHE_MAX_ENTRIES: usize = 64;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const POOL_MAX_IDLE: usize = 4;

/// 上游 `options.encryptType`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Web/Register 留给阶段 5 的登录与设备注册
pub enum EncryptType {
    /// 默认。`signatureAndroidParams`，区分标准版 / 概念版。
    Android,
    /// `signatureWebParams`。登录接口用。
    Web,
    /// `signatureRegisterParams`。设备注册用。
    Register,
}

/// 上游 `options.method`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// 一次上游请求的规格，逐字段对应 `module/*.js` 传给 `useAxios` 的 options。
#[derive(Debug, Clone)]
pub struct Endpoint<'a> {
    pub base: &'a str,
    pub path: &'a str,
    pub method: Method,
    /// 模块自带的头（`options.headers`），如 `x-router`、`Content-Type`。
    pub headers: Vec<(&'a str, &'a str)>,
    /// 模块自带的参数（`options.params`）。**顺序即插入序**，直接决定 URL。
    pub params: Vec<(String, String)>,
    /// `clearDefaultParams`：为真时**不**注入 `defaultParams`。
    ///
    /// 迁移范围内只有 `module/search_lyric.js` 用它，属阶段 4。
    #[allow(dead_code)]
    pub clear_default_params: bool,
    /// 请求体（已序列化）。参与 android 签名。
    pub data: Option<String>,
    /// `encryptKey`：为真时补一个 `key` 参数。
    pub encrypt_key: bool,
    /// 见 [`Endpoint::clear_default_params`]。
    pub encrypt_type: EncryptType,
    /// 覆盖 cookie 里的 `dfid`。
    ///
    /// `module/song_url.js` 传的是
    /// `cookie: Object.assign({}, {dfid: randomString(24)}, params?.cookie)`——
    /// **没登录时 dfid 是每次调用新生成的 24 字符随机串**，不是 `-`，也不是空。
    /// 这个值同时进参数与请求头，所以只能在组装前替换。
    pub dfid_override: Option<String>,
}

impl<'a> Endpoint<'a> {
    pub fn get(base: &'a str, path: &'a str) -> Self {
        Self {
            base,
            path,
            method: Method::Get,
            headers: Vec::new(),
            params: Vec::new(),
            clear_default_params: false,
            data: None,
            encrypt_key: false,
            encrypt_type: EncryptType::Android,
            dfid_override: None,
        }
    }

    pub fn post(base: &'a str, path: &'a str) -> Self {
        Self {
            method: Method::Post,
            ..Self::get(base, path)
        }
    }

    pub fn header(mut self, key: &'a str, value: &'a str) -> Self {
        self.headers.push((key, value));
        self
    }

    pub fn param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.push((key.into(), value.into()));
        self
    }

    pub fn clear_defaults(mut self) -> Self {
        self.clear_default_params = true;
        self
    }

    pub fn body(mut self, data: impl Into<String>) -> Self {
        self.data = Some(data.into());
        self
    }

    pub fn encrypt_key(mut self) -> Self {
        self.encrypt_key = true;
        self
    }

    #[allow(dead_code)] // 阶段 5 的登录与设备注册用
    pub fn encrypt_type(mut self, encrypt_type: EncryptType) -> Self {
        self.encrypt_type = encrypt_type;
        self
    }

    /// 用 `dfid` 覆盖 cookie 里的值。见 [`Endpoint::dfid_override`]。
    pub fn dfid(mut self, dfid: impl Into<String>) -> Self {
        self.dfid_override = Some(dfid.into());
        self
    }
}

/// 一次已经组装好的请求。
///
/// 只把 `url` 放出来给 KAT 测试逐字节断言组装结果，其余字段是本模块内部形态。
#[derive(Debug)]
pub(crate) struct Prepared {
    pub(crate) url: String,
    method: Method,
    /// 本地缓存键，**不是** `url`。
    ///
    /// 上游的 apicache 架在 Node 服务前面，键是 `util/apicache.js:892` 的
    /// `req.hostname + req.originalUrl`——即**客户端发过来的那条 URL**，
    /// 只含 `module/*.js` 自己拼的参数。`dfid`/`clienttime`/`signature` 那些
    /// 是 Node 服务拿到请求之后才注入的，从来不在键里。
    ///
    /// 用组装完成的网关 URL 当键会永远命中不了：`clienttime` 精确到秒，
    /// 两次相同调用必然是不同的 URL。所以这里只取模块自带参数。
    cache_key: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

/// 按 URL 缓存响应体，对齐上游 `server.js:318` 的 apicache。
#[derive(Debug, Default)]
struct Cache {
    entries: BTreeMap<String, (Instant, Value)>,
}

impl Cache {
    fn get(&mut self, key: &str) -> Option<Value> {
        let (stored_at, value) = self.entries.get(key)?;
        if stored_at.elapsed() > CACHE_TTL {
            self.entries.remove(key);
            return None;
        }
        Some(value.clone())
    }

    fn put(&mut self, key: String, value: Value) {
        // 先清过期项；仍然满员就丢掉最早插入的那个（BTreeMap 的 key 序与
        // 插入序无关，所以这里退而求其次——缓存只是省流量，不需要 LRU 那么讲究）。
        let now = Instant::now();
        self.entries
            .retain(|_, (stored_at, _)| now.duration_since(*stored_at) <= CACHE_TTL);
        while self.entries.len() >= CACHE_MAX_ENTRIES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (stored_at, _))| *stored_at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
        self.entries.insert(key, (now, value));
    }
}

/// 内嵌后端的传输层。
#[derive(Debug, Clone)]
pub struct Transport {
    http: reqwest::Client,
    kind: SourceKind,
    cookie: Option<String>,
    /// 设备标识**按需**加载：`NativeApi::new` 不能碰文件系统。
    ///
    /// 构造客户端发生在启动路径上，而单元测试会构造它——那时 `KUGOU_TUI_CONFIG_DIR`
    /// 通常没设，懒加载是「测试不去写用户真实配置目录」的唯一保证。
    device: Arc<OnceLock<Device>>,
    cache: Arc<Mutex<Cache>>,
}

impl Transport {
    pub fn new(kind: SourceKind, cookie: Option<String>, proxy: Option<&str>) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(USER_AGENT)
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
            kind,
            cookie: cookie.filter(|value| !value.trim().is_empty()),
            device: Arc::new(OnceLock::new()),
            cache: Arc::new(Mutex::new(Cache::default())),
        })
    }

    pub fn kind(&self) -> SourceKind {
        self.kind
    }

    pub fn cookie(&self) -> Option<&str> {
        self.cookie.as_deref()
    }

    pub fn set_cookie(&mut self, cookie: Option<String>) {
        self.cookie = cookie.filter(|value| !value.trim().is_empty());
    }

    /// 设备标识。首次调用时从配置目录读，读不到就生成并落盘。
    pub fn device(&self) -> &Device {
        self.device.get_or_init(|| {
            let path = Device::path();
            Device::load_or_create(&path).unwrap_or_else(|error| {
                tlog!(
                    crate::logger::LEVEL_WARN,
                    "设备标识读写失败（{}），本次运行改用临时标识：{error}",
                    path.display()
                );
                Device::generate_random()
            })
        })
    }

    /// 解析 cookie，并补上设备标识——对应上游 `server.js:230-266` 的
    /// `ensureCookie`（客户端给了就不覆盖）。
    ///
    /// 只有 `dfid` / `KUGOU_API_MID` / `KUGOU_API_GUID` / `KUGOU_API_DEV` /
    /// `token` / `userid` 会被真正用到：上游 `createRequest` **从不发送 Cookie 头**，
    /// 它只用这些值派生请求头与参数。
    pub fn cookie_map(&self) -> BTreeMap<String, String> {
        let mut map = parse_cookie(self.cookie.as_deref().unwrap_or_default());
        let device = self.device();
        map.entry("KUGOU_API_MID".to_string())
            .or_insert_with(|| device.mid.clone());
        map.entry("KUGOU_API_GUID".to_string())
            .or_insert_with(|| device.guid.clone());
        map.entry("KUGOU_API_DEV".to_string())
            .or_insert_with(|| device.dev.clone());
        map
    }

    /// 取一次 JSON。`cached` 为假时绕开本地缓存。
    pub async fn get_json(&self, endpoint: &Endpoint<'_>, cached: bool) -> Result<Value> {
        self.with_retry(|| self.get_json_once(endpoint, cached)).await
    }

    /// 取一次原始文本（歌词接口在 `decode=true` 下偶尔直接吐 LRC 纯文本）。
    pub async fn get_text(&self, endpoint: &Endpoint<'_>, cached: bool) -> Result<String> {
        self.with_retry(|| self.get_text_once(endpoint, cached)).await
    }

    /// **写接口**：与 [`Self::get_json`] 相同但不重试，理由见
    /// [`crate::api::client::HttpClient::get_json_mutating`]。
    #[allow(dead_code)] // 阶段 5 的云端写接口用
    pub async fn get_json_mutating(&self, endpoint: &Endpoint<'_>, cached: bool) -> Result<Value> {
        self.get_json_once(endpoint, cached).await
    }

    /// 取一次**原始字节**。
    ///
    /// `/register/dev` 的 `responseType` 是 `arraybuffer`：响应体是一段 AES 密文，
    /// 不是文本。用 `response.text()` 会按 UTF-8 做有损替换，密文就毁了——而且
    /// 不会报错，只是解密出一堆垃圾。所以这条路径必须拿到字节。
    ///
    /// 与上游一致，不做重试判定之外的任何处理：上游 `createRequest` 在
    /// `responseType === 'arraybuffer'` 下把 `Buffer` 原样塞进 `answer.body`。
    pub async fn post_bytes(&self, endpoint: &Endpoint<'_>) -> Result<(u16, Vec<u8>)> {
        let prepared = self.prepare(endpoint);
        self.send_bytes(&prepared).await
    }

    /// 跑一次 `once`，失败且属于瞬时故障时按 [`crate::api::client::RetryPolicy`] 重试。
    async fn with_retry<F, Fut, T>(&self, once: F) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut attempt = 1;
        loop {
            match once().await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if attempt >= crate::api::client::RetryPolicy::MAX_ATTEMPTS
                        || !error.is_transient()
                    {
                        return Err(error);
                    }
                    let delay = crate::api::client::RetryPolicy::delay_after(attempt);
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

    async fn get_json_once(&self, endpoint: &Endpoint<'_>, cached: bool) -> Result<Value> {
        let prepared = self.prepare(endpoint);

        if cached
            && let Some(value) = self
                .cache
                .lock()
                .ok()
                .and_then(|mut cache| cache.get(&prepared.cache_key))
        {
            return Ok(value);
        }

        let (status, body) = self.send(&prepared).await?;
        let value = interpret_json(endpoint.path, status, &body)?;

        if cached && status == 200 && let Ok(mut cache) = self.cache.lock() {
            cache.put(prepared.cache_key.clone(), value.clone());
        }
        Ok(value)
    }

    async fn get_text_once(&self, endpoint: &Endpoint<'_>, cached: bool) -> Result<String> {
        let prepared = self.prepare(endpoint);
        if cached
            && let Ok(mut cache) = self.cache.lock()
            && let Some(value) = cache.get(&prepared.cache_key)
        {
            return Ok(value.as_str().unwrap_or_default().to_string());
        }

        let (status, body) = self.send(&prepared).await?;
        if !(200..300).contains(&status) {
            return Err(AppError::HttpStatus {
                path: endpoint.path.to_string(),
                status,
            });
        }
        if cached && let Ok(mut cache) = self.cache.lock() {
            cache.put(prepared.cache_key.clone(), Value::String(body.clone()));
        }
        Ok(body)
    }

    /// 组装一次请求：注入默认参数、算签名、拼 URL。
    fn prepare(&self, endpoint: &Endpoint<'_>) -> Prepared {
        let clienttime = (now_unix_millis() / 1000).to_string();
        build_prepared(self.kind, &self.cookie_map(), &clienttime, endpoint)
    }

    async fn send(&self, prepared: &Prepared) -> Result<(u16, String)> {
        // 出站请求原样落日志，供与 Node 版逐项对照（阶段 3 出口要求）。
        // 用 DEBUG 级别：默认关，`KUGOU_TUI_DEBUG=1` 打开。
        tlog!(
            crate::logger::LEVEL_DEBUG,
            "native 出站 {} {} headers={:?} body={:?}",
            match prepared.method {
                Method::Post => "POST",
                Method::Get => "GET",
            },
            prepared.url,
            prepared.headers,
            prepared.body
        );

        // 按 `endpoint.method` 发，而不是「有 body 就是 POST」——上游有 POST
        // 但无 body 的用法，靠 body 猜会把它发成 GET。
        let mut request = match prepared.method {
            Method::Post => self.http.post(&prepared.url),
            Method::Get => self.http.get(&prepared.url),
        };
        for (key, value) in &prepared.headers {
            request = request.header(key.as_str(), value.as_str());
        }
        if let Some(body) = &prepared.body {
            request = request.body(body.clone());
        }
        // 上游 `createRequest` 不发 Cookie 头；身份全在头与参数里。

        let response = request.send().await?;
        let status = response.status().as_u16();
        let body = response.text().await?;
        Ok((status, body))
    }

    /// [`Self::send`] 的字节版本，给 `arraybuffer` 响应（`/register/dev`）用。
    async fn send_bytes(&self, prepared: &Prepared) -> Result<(u16, Vec<u8>)> {
        tlog!(
            crate::logger::LEVEL_DEBUG,
            "native 出站 {} {} headers={:?} body={:?}",
            match prepared.method {
                Method::Post => "POST",
                Method::Get => "GET",
            },
            prepared.url,
            prepared.headers,
            prepared.body
        );

        let mut request = match prepared.method {
            Method::Post => self.http.post(&prepared.url),
            Method::Get => self.http.get(&prepared.url),
        };
        for (key, value) in &prepared.headers {
            request = request.header(key.as_str(), value.as_str());
        }
        if let Some(body) = &prepared.body {
            request = request.body(body.clone());
        }

        let response = request.send().await?;
        let status = response.status().as_u16();
        let body = response.bytes().await?.to_vec();
        Ok((status, body))
    }
}

/// 组装一次请求。**纯函数**：`clienttime` 由调用方注入，所以能用固定的
/// 时间戳对着上游的 known-answer 基准做断言（见本模块测试）。
///
/// 顺序严格照上游 `util/request.js`：
/// `defaultParams` → 模块自带 params → `encryptKey` 补 `key` → 签名。
pub(crate) fn build_prepared(
    kind: SourceKind,
    cookie: &BTreeMap<String, String>,
    clienttime: &str,
    endpoint: &Endpoint<'_>,
) -> Prepared {
    let dfid = endpoint
        .dfid_override
        .clone()
        .or_else(|| cookie.get("dfid").cloned())
        .unwrap_or_else(|| "-".to_string());
    let mid = cookie.get("KUGOU_API_MID").cloned().unwrap_or_default();
    let token = cookie.get("token").cloned().unwrap_or_default();
    // 上游 `options?.cookie?.userid || 0`：缺省是数字 0，只有真值才进参数。
    let userid = cookie
        .get("userid")
        .map(String::as_str)
        .filter(|value| !value.is_empty() && *value != "0")
        .map(str::to_string);

    let mut params: Vec<(String, String)> = Vec::new();
    if !endpoint.clear_default_params {
        params.push(("dfid".to_string(), dfid.clone()));
        params.push(("mid".to_string(), mid.clone()));
        params.push(("uuid".to_string(), "-".to_string()));
        params.push(("appid".to_string(), sign::appid(kind).to_string()));
        params.push(("clientver".to_string(), sign::clientver(kind).to_string()));
        params.push(("clienttime".to_string(), clienttime.to_string()));
        if !token.is_empty() {
            params.push(("token".to_string(), token));
        }
        if let Some(userid) = &userid {
            params.push(("userid".to_string(), userid.clone()));
        }
    }
    // `Object.assign({}, defaultParams, options.params)` 的语义：**同名参数
    // 覆盖值但保留原有的插入位置**。`song_url.js` 的 dataMap 里就有
    // `clientver: 11430`，它必须改写默认的 `clientver`，而不是在末尾再出现一次
    // ——重复键会让 URL 与签名都错。
    merge_params(&mut params, &endpoint.params);

    // `encryptKey` 在签名之前补 `key`——所以 `key` 本身也参与签名。
    if endpoint.encrypt_key {
        let lookup = |key: &str| {
            params
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        let hash = lookup("hash").unwrap_or_default();
        let mid = lookup("mid").unwrap_or_default();
        let userid = lookup("userid").and_then(|value| value.parse::<i64>().ok());
        let appid = lookup("appid").and_then(|value| value.parse::<u32>().ok());
        let key = sign::sign_key(kind, &hash, &mid, userid, appid);
        params.push(("key".to_string(), key));
    }

    if !params.iter().any(|(name, _)| name == "signature") {
        let map: BTreeMap<String, ParamValue> = params
            .iter()
            .map(|(key, value)| (key.clone(), ParamValue::from(value.clone())))
            .collect();
        let data = endpoint.data.as_deref().unwrap_or_default().as_bytes();
        let signature = match endpoint.encrypt_type {
            EncryptType::Android => sign::signature_android_params(kind, &map, data),
            EncryptType::Web => sign::signature_web_params(&map),
            EncryptType::Register => sign::signature_register_params(&map),
        };
        params.push(("signature".to_string(), signature));
    }

    // 头：上游 `Object.assign({UA}, options.headers, {dfid, clienttime, mid})`
    // 再并上 `{kg-rc, kg-thash, kg-rec, kg-rf}`。
    let mut headers: Vec<(String, String)> =
        vec![("User-Agent".to_string(), USER_AGENT.to_string())];
    for (key, value) in &endpoint.headers {
        headers.push((key.to_string(), value.to_string()));
    }
    headers.push(("dfid".to_string(), dfid));
    // `headers.clienttime = params.clienttime`：`clearDefaultParams` 时
    // 参数里没有 clienttime，这个头也就跟着消失（不是空串，是**不发**）。
    if let Some((_, value)) = params
        .iter()
        .find(|(name, _)| name == "clienttime")
        .cloned()
    {
        headers.push(("clienttime".to_string(), value));
    }
    headers.push(("mid".to_string(), mid));
    headers.push(("kg-rc".to_string(), "1".to_string()));
    headers.push(("kg-thash".to_string(), KG_THASH.to_string()));
    headers.push(("kg-rec".to_string(), "1".to_string()));
    headers.push(("kg-rf".to_string(), KG_RF.to_string()));

    let query = build_query(&params);
    let url = if query.is_empty() {
        format!("{}{}", endpoint.base, endpoint.path)
    } else {
        format!("{}{}?{query}", endpoint.base, endpoint.path)
    };

    Prepared {
        url,
        method: endpoint.method,
        cache_key: format!(
            "{}{}?{}",
            endpoint.base,
            endpoint.path,
            build_query(&endpoint.params)
        ),
        headers,
        body: endpoint.data.clone(),
    }
}

/// 把 `extra` 并进 `base`，语义等价于 JS 的
/// `Object.assign({}, defaultParams, options.params)`。
///
/// 关键在**同名键的位置**：JS 对象的赋值是「改值不改位」，所以被覆盖的键仍
/// 留在原插入位置，而不是跑到末尾。`song_url.js` 的 `clientver: 11430` 正是
/// 这种覆盖——如果按「先删后加」处理，URL 里 `clientver` 会挪到最后，
/// 签名与 URL 双双出错。
fn merge_params(base: &mut Vec<(String, String)>, extra: &[(String, String)]) {
    for (key, value) in extra {
        match base.iter_mut().find(|(name, _)| name == key) {
            Some(slot) => slot.1 = value.clone(),
            None => base.push((key.clone(), value.clone())),
        }
    }
}

/// 解析 cookie 串。上游 `server.js:211-219`：按第一个 `=` 切，两边 trim。
pub fn parse_cookie(raw: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for chunk in raw.split(';') {
        let Some((key, value)) = chunk.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty() || value.is_empty() {
            continue;
        }
        map.insert(key.to_string(), value.to_string());
    }
    map
}

/// 按 axios 的规则拼 query 串。
///
/// **不能用 `encodeURIComponent`**：axios 1.20 的 `buildURL.encode` 是
/// `encodeURIComponent` 之后再放行 `:` `$` `,`，并把空格写成 `+`。实测
/// `1,2,3` → `1,2,3`（逗号原样）、`a b` → `a+b`、`a+b` → `a%2Bb`。
/// 用 `reqwest` 的 `.query()` 会走 `serde_urlencoded`，把 `,` `:` `$` `!` `'`
/// `(` `)` `~` 全部编码——URL 与 Node 版就不再逐字节相同。
pub fn build_query(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", encode_axios(key), encode_axios(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `encodeURIComponent` + axios 放行集。
fn encode_axios(value: &str) -> String {
    /// `encodeURIComponent` 不转义的字符。
    const KEEP: &[u8] = b"-_.!~*'()";

    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        let kept = byte.is_ascii_alphanumeric()
            || KEEP.contains(&byte)
            // axios 额外放行这三个
            || byte == b':'
            || byte == b'$'
            || byte == b',';
        if kept {
            out.push(byte as char);
        } else if byte == b' ' {
            out.push('+');
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// 把 HTTP 响应解释成 `Result<Value>`。
///
/// 判据照抄上游 `util/request.js:210`：`status === 0` 或 `error_code` 非 0
/// 都算失败（那个服务会把它转成 HTTP 502 再吐给客户端）。这里的顺序是
/// 「先给业务错误码、再退回状态码」，与
/// [`crate::api::client::HttpClient`] 完全一致——`error_code` 才是权威。
fn interpret_json(path: &str, status: u16, body: &str) -> Result<Value> {
    match serde_json::from_str::<Value>(body) {
        Ok(value) => {
            let failed = value.get("status").and_then(Value::as_i64) == Some(0)
                || value
                    .get("error_code")
                    .and_then(Value::as_i64)
                    .is_some_and(|code| code != 0);
            // 有业务错误码就先报它：`error_code: 152` 比「HTTP 502」有用得多。
            check_error_code(path, &value)?;
            if failed {
                return Err(AppError::HttpStatus {
                    path: path.to_string(),
                    status: 502,
                });
            }
            if !(200..300).contains(&status) {
                return Err(AppError::HttpStatus {
                    path: path.to_string(),
                    status,
                });
            }
            Ok(value)
        }
        Err(error) => {
            let preview = crate::api::client::body_preview(body);
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
            Err(AppError::NonJsonBody {
                path: path.to_string(),
                status,
                preview,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 基准来自 `tools/kat/kat_request.js`（上游 `module/*.js` 真实 options）。
    /// 固定输入与 `kat_request.js` 一致。
    const KAT_CLIENTTIME: &str = "1700000000";
    const KAT_MID: &str = "231699103997194646178265604655475531917";
    const KAT_HASH: &str = "6af00fbd4d444a82c005843eef9dc2d4";

    fn kat_cookie(dfid: &str) -> BTreeMap<String, String> {
        [
            ("dfid", dfid),
            ("KUGOU_API_MID", KAT_MID),
            ("token", "TOKENFIXTURE"),
            ("userid", "10001"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
    }

    /// `module/search.js`：`/v3/search/song`，`x-router: complexsearch.kugou.com`。
    fn search_endpoint<'a>() -> Endpoint<'a> {
        Endpoint::get(GATEWAY_BASE, "/v3/search/song")
            .header("x-router", "complexsearch.kugou.com")
            .param("albumhide", "0")
            .param("iscorrection", "1")
            .param("keyword", "周杰伦")
            .param("nocollect", "0")
            .param("page", "1")
            .param("pagesize", "30")
            .param("platform", "AndroidFilter")
    }

    /// 上游 `module/song_url.js`：`/v5/url`，`encryptKey`，参数插入序即 URL 顺序。
    fn song_url_endpoint<'a>(kind: SourceKind, free_part: bool) -> Endpoint<'a> {
        let (page_id, pid, ppage_id) = match kind {
            SourceKind::KugouConcept => ("967177915", "411", "356753938"),
            _ => ("151369488", "2", "463467626,350369493,788954147"),
        };
        Endpoint::get(GATEWAY_BASE, "/v5/url")
            .header("x-router", "trackercdn.kugou.com")
            .param("album_id", "0")
            .param("area_code", "1")
            .param("hash", KAT_HASH)
            .param("ssa_flag", "is_fromtrack")
            .param("version", "11430")
            .param("page_id", page_id)
            .param("quality", "128")
            .param("album_audio_id", "0")
            .param("behavior", "play")
            .param("pid", pid)
            .param("cmd", "26")
            .param("pidversion", "3001")
            .param("IsFreePart", if free_part { "1" } else { "0" })
            .param("ppage_id", ppage_id)
            .param("cdnBackup", "1")
            .param("module", "")
            // dataMap 里的 `clientver: 11430` 会**覆盖**默认参数里的
            // `clientver`（`Object.assign({}, defaultParams, options.params)`），
            // 所以最终 `clientver` 是 11430 而不是平台默认值。
            .param("clientver", "11430")
            .encrypt_key()
    }

    /// 标准版搜索：URL 逐字节等于 `kat_request.js` 抓到的 `getUri`。
    #[test]
    fn standard_search_url_matches_kat() {
        let prepared = build_prepared(
            SourceKind::Kugou,
            &kat_cookie("1234567890abcdef12345678"),
            KAT_CLIENTTIME,
            &search_endpoint(),
        );
        assert_eq!(
            prepared.url,
            "https://gateway.kugou.com/v3/search/song?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&albumhide=0&iscorrection=1&keyword=%E5%91%A8%E6%9D%B0%E4%BC%A6&nocollect=0&page=1&pagesize=30&platform=AndroidFilter&signature=9bb2d7192e4ec15c72add9daaf9728a6"
        );
    }

    /// 概念版搜索：只有 `appid`/`clientver` 与签名变。
    #[test]
    fn lite_search_url_matches_kat() {
        let prepared = build_prepared(
            SourceKind::KugouConcept,
            &kat_cookie("1234567890abcdef12345678"),
            KAT_CLIENTTIME,
            &search_endpoint(),
        );
        assert_eq!(
            prepared.url,
            "https://gateway.kugou.com/v3/search/song?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&albumhide=0&iscorrection=1&keyword=%E5%91%A8%E6%9D%B0%E4%BC%A6&nocollect=0&page=1&pagesize=30&platform=AndroidFilter&signature=bca333395e727512b680fba6df0e4b82"
        );
    }

    /// 标准版取链：`encryptKey` 的 `key` 在 `signature` 之前、在模块参数之后，
    /// 且 `ppage_id` 里的逗号不编码。
    #[test]
    fn standard_song_url_matches_kat() {
        let prepared = build_prepared(
            SourceKind::Kugou,
            &kat_cookie("1234567890abcdef12345678"),
            KAT_CLIENTTIME,
            &song_url_endpoint(SourceKind::Kugou, false),
        );
        assert_eq!(
            prepared.url,
            "https://gateway.kugou.com/v5/url?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=11430&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&album_id=0&area_code=1&hash=6af00fbd4d444a82c005843eef9dc2d4&ssa_flag=is_fromtrack&version=11430&page_id=151369488&quality=128&album_audio_id=0&behavior=play&pid=2&cmd=26&pidversion=3001&IsFreePart=0&ppage_id=463467626,350369493,788954147&cdnBackup=1&module=&key=1e5533fbcad17c9aa8935349a8b7c1d3&signature=8128fd5afa89324ac65a4c7b77b270e9"
        );
    }

    /// 概念版取链：`page_id`/`pid`/`ppage_id` 三个值都是概念版专有的。
    #[test]
    fn lite_song_url_matches_kat() {
        let prepared = build_prepared(
            SourceKind::KugouConcept,
            &kat_cookie("1234567890abcdef12345678"),
            KAT_CLIENTTIME,
            &song_url_endpoint(SourceKind::KugouConcept, false),
        );
        assert_eq!(
            prepared.url,
            "https://gateway.kugou.com/v5/url?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11430&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&album_id=0&area_code=1&hash=6af00fbd4d444a82c005843eef9dc2d4&ssa_flag=is_fromtrack&version=11430&page_id=967177915&quality=128&album_audio_id=0&behavior=play&pid=411&cmd=26&pidversion=3001&IsFreePart=0&ppage_id=356753938&cdnBackup=1&module=&key=7d03ca1bba0d0fa1f5c8d55cdd957b8d&signature=64a711097ebb42883b47892ecec00214"
        );
    }

    /// `song_url.js` 自带 `cookie: Object.assign({}, {dfid: randomString(24)}, params.cookie)`，
    /// 即**没登录时 dfid 是随机 24 字符**而不是空。这条路径的签名与有 dfid 时不同。
    #[test]
    fn song_url_with_random_dfid_matches_kat() {
        let prepared = build_prepared(
            SourceKind::Kugou,
            &kat_cookie("158BEILPSW158BEILPSW158B"),
            KAT_CLIENTTIME,
            &song_url_endpoint(SourceKind::Kugou, false),
        );
        assert!(
            prepared.url.ends_with("&signature=332535f1dff6b07ffa64a75960e33ebc"),
            "实际：{}",
            prepared.url
        );
        assert!(prepared.url.contains("dfid=158BEILPSW158BEILPSW158B&"));
    }

    /// 歌词搜索：`clearDefaultParams: true`——**没有** `dfid`/`mid`/`uuid`/
    /// `clienttime`，因此 `clienttime` 头也**不发**（不是空串）。
    ///
    /// 上游 `module/search_lyric.js` 还写了 `notSign: true`，但 `util/request.js:126`
    /// 读的是 `options.notSignature`，全仓库无人读 `notSign`——签名照常。
    #[test]
    fn search_lyric_clears_defaults_but_keeps_signature() {
        let endpoint = Endpoint::get(LYRICS_BASE, "/v1/search")
            .clear_defaults()
            .param("album_audio_id", "0")
            .param("appid", "1005")
            .param("clientver", "20489")
            .param("duration", "243722")
            .param("hash", KAT_HASH)
            .param("keyword", "Letter - arkady sevidov")
            .param("lrctxt", "1")
            .param("man", "yes");
        let prepared = build_prepared(
            SourceKind::Kugou,
            &kat_cookie("1234567890abcdef12345678"),
            KAT_CLIENTTIME,
            &endpoint,
        );
        assert_eq!(
            prepared.url,
            "https://lyrics.kugou.com/v1/search?album_audio_id=0&appid=1005&clientver=20489&duration=243722&hash=6af00fbd4d444a82c005843eef9dc2d4&keyword=Letter+-+arkady+sevidov&lrctxt=1&man=yes&signature=b90333d489a1aae225eb18ac61718d35"
        );
        assert!(
            !prepared.headers.iter().any(|(key, _)| key == "clienttime"),
            "clearDefaultParams 下 clienttime 头不该发：{:?}",
            prepared.headers
        );
    }

    /// 请求头逐项等于 `kat_request.js` 抓到的 `headers`。
    #[test]
    fn search_headers_match_kat() {
        let prepared = build_prepared(
            SourceKind::Kugou,
            &kat_cookie("1234567890abcdef12345678"),
            KAT_CLIENTTIME,
            &search_endpoint(),
        );
        assert_eq!(
            prepared.headers,
            vec![
                ("User-Agent".to_string(), "Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi".to_string()),
                ("x-router".to_string(), "complexsearch.kugou.com".to_string()),
                ("dfid".to_string(), "1234567890abcdef12345678".to_string()),
                ("clienttime".to_string(), KAT_CLIENTTIME.to_string()),
                ("mid".to_string(), KAT_MID.to_string()),
                ("kg-rc".to_string(), "1".to_string()),
                ("kg-thash".to_string(), "5d816a0".to_string()),
                ("kg-rec".to_string(), "1".to_string()),
                ("kg-rf".to_string(), "B9EDA08A64250DEFFBCADDEE00F8F25F".to_string()),
            ]
        );
    }

    /// 基准来自 `/tmp/axios_wire.js`：**真实抓到的请求行**，不是 `getUri` 的推测。
    ///
    /// 那个脚本同时打印 `getUri` 与本机服务收到的 `req.url`，两者逐字符相同。
    #[test]
    fn query_encoding_matches_the_wire() {
        let params: Vec<(String, String)> = [
            ("plain", "abc"),
            ("space", "a b"),
            ("plus", "a+b"),
            ("comma", "1,2,3"),
            ("colon", "a:b"),
            ("dollar", "a$b"),
            ("bang", "a!b"),
            ("quote", "a'b"),
            ("paren", "a(b)c"),
            ("tilde", "a~b"),
            ("star", "a*b"),
            ("dash", "a-b"),
            ("under", "a_b"),
            ("dot", "a.b"),
            ("slash", "a/b"),
            ("qmark", "a?b"),
            ("hash", "a#b"),
            ("amp", "a&b"),
            ("eq", "a=b"),
            ("pct", "a%b"),
            ("at", "a@b"),
            ("bracket", "a[b]"),
            ("cjk", "周杰伦"),
            ("empty", ""),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();

        assert_eq!(
            build_query(&params),
            "plain=abc&space=a+b&plus=a%2Bb&comma=1,2,3&colon=a:b&dollar=a$b&bang=a!b&quote=a'b&paren=a(b)c&tilde=a~b&star=a*b&dash=a-b&under=a_b&dot=a.b&slash=a%2Fb&qmark=a%3Fb&hash=a%23b&amp=a%26b&eq=a%3Db&pct=a%25b&at=a%40b&bracket=a%5Bb%5D&cjk=%E5%91%A8%E6%9D%B0%E4%BC%A6&empty="
        );
    }

    /// 空格是 `+`，而字面加号是 `%2B`——这两个混了 URL 就错，且不会报错。
    #[test]
    fn space_is_plus_and_plus_is_escaped() {
        assert_eq!(encode_axios("a b"), "a+b");
        assert_eq!(encode_axios("a+b"), "a%2Bb");
        assert_eq!(encode_axios("Letter - arkady sevidov"), "Letter+-+arkady+sevidov");
    }

    /// 逗号不编码，`ppage_id` 才能与上游一致。
    #[test]
    fn comma_is_kept_literal() {
        assert_eq!(
            encode_axios("463467626,350369493,788954147"),
            "463467626,350369493,788954147"
        );
    }

    /// 上游 `server.js:211-219` 的解析规则：第一个 `=` 切分、两边 trim、
    /// 空键空值丢弃。
    #[test]
    fn parses_cookie_header() {
        let map = parse_cookie("token=abc; userid=10001; dfid=xyz; ; empty=;  spaced = v ");
        assert_eq!(map.get("token").map(String::as_str), Some("abc"));
        assert_eq!(map.get("userid").map(String::as_str), Some("10001"));
        assert_eq!(map.get("dfid").map(String::as_str), Some("xyz"));
        assert_eq!(map.get("empty"), None);
        assert_eq!(map.get("spaced").map(String::as_str), Some("v"));
    }

    /// `status: 0` 与 `error_code` 非 0 都算失败——只认后者会漏掉一类静默失败。
    #[test]
    fn status_zero_is_a_failure() {
        let body = r#"{"status":0,"data":{}}"#;
        assert!(interpret_json("/x", 200, body).is_err());

        let body = r#"{"error_code":152,"error_msg":"need login"}"#;
        let error = interpret_json("/x", 200, body).unwrap_err();
        assert!(matches!(error, AppError::Api { code: 152, .. }), "实际：{error:?}");
    }

    /// 业务错误码优先于 HTTP 状态码——与 `HttpClient` 的判据一致。
    #[test]
    fn business_code_wins_over_http_status() {
        let body = r#"{"error_code":152,"error_msg":"need login"}"#;
        let error = interpret_json("/x", 502, body).unwrap_err();
        assert!(matches!(error, AppError::Api { code: 152, .. }), "实际：{error:?}");
    }

    /// `status: 2`（需要验证）是**成功路径**：`song_stream_url` 要靠它给用户
    /// 解释「为什么没有直链」，在这里当成失败就再也读不到了。
    #[test]
    fn status_two_passes_through() {
        let value = interpret_json("/song/url", 200, r#"{"status":2,"data":{}}"#).unwrap();
        assert_eq!(value.get("status").and_then(Value::as_i64), Some(2));
    }

    /// 缓存键是**模块自带参数**拼出来的 URL，不含 `dfid`/`clienttime`/`signature`。
    /// 否则同一首歌的两次调用会因秒级时间戳不同而永不命中，出站请求条数
    /// 就会比 Node 版多——这正是阶段 3 出口要比对的东西。
    #[test]
    fn cache_key_ignores_volatile_params() {
        let first = build_prepared(
            SourceKind::Kugou,
            &kat_cookie("1234567890abcdef12345678"),
            "1700000000",
            &search_endpoint(),
        );
        let second = build_prepared(
            SourceKind::Kugou,
            &kat_cookie("1234567890abcdef12345678"),
            "1700009999",
            &search_endpoint(),
        );

        assert_ne!(first.url, second.url, "clienttime 不同，出站 URL 必然不同");
        assert_eq!(first.cache_key, second.cache_key, "缓存键不该带 clienttime");
        assert!(!first.cache_key.contains("signature="));
        assert!(!first.cache_key.contains("clienttime="));
        assert!(!first.cache_key.contains("dfid="));
    }

    /// 命中后不再发第二次请求；过期项在读时就被清掉。
    #[test]
    fn cache_serves_a_hit_and_drops_expired_entries() {
        let mut cache = Cache::default();
        assert!(cache.get("k").is_none());

        cache.put("k".to_string(), serde_json::json!({"a": 1}));
        assert_eq!(cache.get("k"), Some(serde_json::json!({"a": 1})));

        // 直接把存入时间往前拨，模拟 2 分钟过期——不 sleep，测试要快。
        if let Some((stored_at, _)) = cache.entries.get_mut("k") {
            *stored_at = Instant::now() - CACHE_TTL - Duration::from_secs(1);
        }
        assert!(cache.get("k").is_none(), "过期项应当读不到");
        assert!(cache.entries.is_empty(), "过期项应当被顺手清掉");
    }

    /// 上游 apicache 不设条目上限；native 要长期占用户内存，所以有天花板，
    /// 满了先丢最早插入的。
    #[test]
    fn cache_evicts_the_oldest_entry_at_capacity() {
        let mut cache = Cache::default();
        for index in 0..CACHE_MAX_ENTRIES {
            cache.put(format!("k{index}"), serde_json::json!(index));
        }
        assert_eq!(cache.entries.len(), CACHE_MAX_ENTRIES);

        cache.put("fresh".to_string(), serde_json::json!("v"));
        assert_eq!(cache.entries.len(), CACHE_MAX_ENTRIES, "不该超过上限");
        assert!(cache.entries.contains_key("fresh"));
        assert!(!cache.entries.contains_key("k0"), "k0 最早插入，应被淘汰");
    }
}
