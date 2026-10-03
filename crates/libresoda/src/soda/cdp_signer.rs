//! CDP 签名页：Rust 原生驱动 Chromium，替代 Node 版 `signer-server.mjs`。
//!
//! 做法和 Meting-API / 旧版 Node signer 完全一致，只是把「Playwright 遥控」换成
//! Rust 直接说 CDP：
//!
//! 1. 内置一个 127.0.0.1 静态资产服务，吐 `security_host.html` / `bdms.js`；
//! 2. 每个 `sessionKey` 建一个独立 `BrowserContext`（独立 cookie jar + 设备身份）；
//! 3. 页面里跑官方 `bdms.js`，由 `window.__qishuiRequest` 发真实 XHR（带 a_bogus）；
//! 4. 响应回来后校验 `a_bogus` 必须 44 字符，再把整个上下文的 cookie 交给调用方；
//! 5. 资产服务同时承担二次验证（`error_code=2046`）的桥接路由：官方验证组件
//!    跑在用户浏览器打开的 `security_host.html` 里，它的网络请求经
//!    `/verify/request` 回到签名页上下文代发（带登录 cookie + a_bogus），
//!    对齐 Meting-API 的 `/admin/qr/qishui/verify/*` 桥接。

use std::collections::{BTreeMap, HashMap};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::browser::{BrowserContextId, CloseParams};
use chromiumoxide::cdp::browser_protocol::network::{CookieParam, GetCookiesParams};
use chromiumoxide::cdp::browser_protocol::page::NavigateParams;
use chromiumoxide::cdp::browser_protocol::target::{
    CloseTargetParams, CreateBrowserContextParams, CreateTargetParams,
};
use chromiumoxide::cdp::js_protocol::runtime::{CallArgument, CallFunctionOnParams};
use chromiumoxide::Page;
use futures::StreamExt;
use serde_json::Value;

use crate::error::{Result, SodaError};
use crate::soda::browser::{BrowserCookie, BrowserRequest, BrowserRequester, BrowserResponse};

const SECURITY_HOST_HTML: &str =
    include_str!("../../tools/qishui-signer/security/security_host.html");
const SDK_GLUE_JS: &str = include_str!("../../tools/qishui-signer/security/sdk-glue.js");
const BDMS_JS: &str = include_str!("../../tools/qishui-signer/security/bdms.js");
const REACT_JS: &str = include_str!("../../tools/qishui-signer/security/react.js");
const REACT_DOM_JS: &str = include_str!("../../tools/qishui-signer/security/react-dom.js");

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) SodaMusic/3.2.1 Chrome/136.0.7103.59 Electron/36.4.0 Safari/537.36";
const DEFAULT_SESSION_KEY: &str = "default";
/// 无会话且持续无活动时关闭 Chromium（下次请求重新拉起），避免桌面应用
/// 长驻时白吃几百 MB 内存。对齐 Meting-API signer 的 `closeBrowserWhenIdle`。
const IDLE_SHUTDOWN: Duration = Duration::from_secs(5 * 60);

/// Rust 版签名页。懒启动：第一次请求才拉起 Chromium。
///
/// 实例本身无状态：浏览器、资产服务、二次验证登记全部挂在进程级单例上
/// （见 [`shared`]）。调用方（如 sodam）每次轮询都会重建 `Soda`，若每个
/// `CdpSigner` 各自为政，会每次轮询拉起一个新 Chromium；二次验证窗口也
/// 需要跨轮询存活的浏览器上下文与登记表。
pub struct CdpSigner;

static SHARED: OnceLock<Arc<Inner>> = OnceLock::new();
static INIT_LOCK: Mutex<()> = Mutex::new(());

struct Inner {
    runtime: tokio::runtime::Runtime,
    port: u16,
    state: Mutex<State>,
    /// 二次验证登记表：`check_qrconnect` 2046 时登记，验证窗口凭 token 领取。
    verifies: Mutex<HashMap<String, VerifyEntry>>,
    /// 打开过验证窗口后置位：之后（重）拉起的浏览器都是 headed（用户要交互）。
    headed: AtomicBool,
}

/// 一次二次验证的登记项（`error_code=2046` 时建立）。
#[derive(Debug, Clone)]
struct VerifyEntry {
    /// 二维码会话对应的浏览器上下文键：验证请求经它代发（带登录 cookie + a_bogus）。
    session_key: String,
    /// 2046 响应的 `data`（决策：verify_ways / biz_params / 验证组件 URL 等）。
    decision: Value,
    /// 官方验证组件要求的公共参数（passport JS-SDK 那套）。
    general_params: Value,
    /// 验证窗口已回执「完成」。
    done: bool,
}

struct State {
    browser: Option<Arc<Browser>>,
    sessions: HashMap<String, Session>,
    /// 最近一次活动时间；空闲且无会话时回收 Chromium。
    last_activity: Instant,
}

struct Session {
    context: BrowserContextId,
    page: Page,
    #[allow(dead_code)]
    last_used: Instant,
}

impl CdpSigner {
    pub fn new() -> Self {
        Self
    }

    fn request(&self, request: &BrowserRequest) -> Result<BrowserResponse> {
        let inner = shared()?;
        inner
            .runtime
            .block_on(async_request(inner.clone(), request, true))
    }

    fn close(&self, session_key: &str) -> Result<()> {
        let key = normalize_key(session_key);
        if let Some(inner) = SHARED.get() {
            inner.runtime.block_on(async_close(inner.clone(), &key))
        } else {
            Ok(())
        }
    }
}

impl Default for CdpSigner {
    fn default() -> Self {
        Self::new()
    }
}

/// 进程级共享的签名页实例（懒初始化，互斥锁防并发重建）。
fn shared() -> Result<Arc<Inner>> {
    if let Some(inner) = SHARED.get() {
        return Ok(inner.clone());
    }
    let _guard = INIT_LOCK
        .lock()
        .map_err(|_| SodaError::http("CDP 签名页初始化锁中毒"))?;
    if let Some(inner) = SHARED.get() {
        return Ok(inner.clone());
    }
    let holder: Arc<OnceLock<Arc<Inner>>> = Arc::new(OnceLock::new());
    let port = start_asset_server(holder.clone())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|err| SodaError::http(format!("创建 tokio 运行时失败: {err}")))?;
    let inner = Arc::new(Inner {
        runtime,
        port,
        state: Mutex::new(State {
            browser: None,
            sessions: HashMap::new(),
            last_activity: Instant::now(),
        }),
        verifies: Mutex::new(HashMap::new()),
        headed: AtomicBool::new(false),
    });
    let _ = holder.set(inner.clone());
    spawn_idle_reaper(holder);
    let _ = SHARED.set(inner.clone());
    Ok(inner)
}

/// 空闲回收：无会话且超时无活动时关闭 Chromium，下次请求重新拉起。
fn spawn_idle_reaper(holder: Arc<OnceLock<Arc<Inner>>>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(60));
        let Some(inner) = holder.get() else { continue };
        let retire = {
            let Ok(mut state) = inner.state.lock() else {
                continue;
            };
            if state.browser.is_some()
                && state.sessions.is_empty()
                && state.last_activity.elapsed() > IDLE_SHUTDOWN
            {
                state.browser.take()
            } else {
                None
            }
        };
        if let Some(browser) = retire {
            // 同步线程里没有 async 环境：借 runtime 转一手
            inner.runtime.block_on(close_browser_cmd(&browser));
        }
    });
}

/// 记录一次活动（用于空闲回收判断）。
fn touch_activity(inner: &Inner) {
    if let Ok(mut state) = inner.state.lock() {
        state.last_activity = Instant::now();
    }
}

/// 丢弃僵尸浏览器句柄（不发送关闭命令）：浏览器进程已死时只清状态。
fn evict_browser(inner: &Inner) {
    if let Ok(mut state) = inner.state.lock() {
        state.browser = None;
        state.sessions.clear();
    }
}

/// 清掉一个（可能已死的）会话缓存，下次请求重建。
fn evict_session(inner: &Inner, key: &str) {
    if let Ok(mut state) = inner.state.lock() {
        state.sessions.remove(key);
    }
}

/// 回收整个浏览器：清空会话表，并向 Chromium 发送 `Browser.close` 优雅退出。
async fn retire_browser(inner: &Inner) {
    let stale = {
        let Ok(mut state) = inner.state.lock() else {
            return;
        };
        state.sessions.clear();
        state.browser.take()
    };
    if let Some(browser) = stale {
        close_browser_cmd(&browser).await;
    }
}

/// 发送 `Browser.close`（`Browser::close` 需要 `&mut`，我们只持 `Arc`）。
/// 失败也无妨：连接已断时丢弃句柄即可，下次 `ensure_browser` 会重建。
/// 注意：不能在这里 `runtime.block_on`——调用方可能已经在 runtime 上驱动。
async fn close_browser_cmd(browser: &Browser) {
    let _ = browser.execute(CloseParams::default()).await;
}

impl BrowserRequester for CdpSigner {
    fn request(&self, request: &BrowserRequest) -> Result<BrowserResponse> {
        CdpSigner::request(self, request)
    }

    fn close_session(&self, session_key: &str) -> Result<()> {
        self.close(session_key)
    }

    fn register_second_verify(
        &self,
        token: &str,
        session_key: &str,
        decision: &Value,
        general_params: &Value,
    ) -> Result<()> {
        let inner = shared()?;
        let mut verifies = inner
            .verifies
            .lock()
            .map_err(|_| SodaError::http("验证登记表锁中毒"))?;
        verifies.insert(
            token.trim().to_string(),
            VerifyEntry {
                session_key: normalize_key(session_key),
                decision: decision.clone(),
                general_params: general_params.clone(),
                done: false,
            },
        );
        Ok(())
    }

    fn second_verify_url(&self, token: &str) -> Result<String> {
        let inner = shared()?;
        Ok(format!(
            "http://127.0.0.1:{}/security_host.html?key={}&bridgeRoot=/verify",
            inner.port,
            crate::util::query_escape(token.trim())
        ))
    }

    /// 在签名页浏览器里打开**可见**的二次验证窗口（详见 `async_open_verify`）。
    fn open_second_verify_window(&self, token: &str) -> Result<String> {
        let inner = shared()?;
        inner
            .runtime
            .block_on(async_open_verify(inner.clone(), token))
    }

    fn second_verify_done(&self, token: &str) -> bool {
        let Some(inner) = shared().ok() else {
            return false;
        };
        let Ok(verifies) = inner.verifies.lock() else {
            return false;
        };
        verifies
            .get(token.trim())
            .map(|entry| entry.done)
            .unwrap_or(false)
    }

    fn ack_second_verify(&self, token: &str) -> Result<()> {
        let inner = shared()?;
        let mut verifies = inner
            .verifies
            .lock()
            .map_err(|_| SodaError::http("验证登记表锁中毒"))?;
        if let Some(entry) = verifies.get_mut(token.trim()) {
            entry.done = false;
        }
        Ok(())
    }

    fn clear_second_verify(&self, token: &str) -> Result<()> {
        let inner = shared()?;
        let mut verifies = inner
            .verifies
            .lock()
            .map_err(|_| SodaError::http("验证登记表锁中毒"))?;
        verifies.remove(token.trim());
        Ok(())
    }

    fn name(&self) -> &'static str {
        "cdp-signer"
    }
}

fn normalize_key(session_key: &str) -> String {
    let key = session_key.trim();
    if key.is_empty() {
        DEFAULT_SESSION_KEY.to_string()
    } else {
        key.to_string()
    }
}

#[derive(serde::Deserialize)]
struct JsXhr {
    status: u16,
    body: String,
    #[serde(rename = "responseURL")]
    response_url: String,
    headers: String,
}

async fn async_request(
    inner: Arc<Inner>,
    request: &BrowserRequest,
    strict_sign: bool,
) -> Result<BrowserResponse> {
    touch_activity(&inner);
    let key = normalize_key(&request.session_key);
    let mut session = ensure_session(inner.clone(), &key).await?;
    let spec = serde_json::json!({
        "method": request.method,
        "url": request.url,
        "headers": request.headers,
        "body": request.body,
        "timeout": 180_000,
    });
    // 页面通道可能静默断开（浏览器退出/页面被关，实测踩过：验证窗口打开后
    // Chromium 无声消失，之后所有请求都报 receiver is gone）。失败时清会话
    // 重建一次自愈，仍失败才把错误抛给上层。
    let xhr = match evaluate_xhr(&session.page, &spec).await {
        Ok(xhr) => xhr,
        Err(first) => {
            evict_session(&inner, &key);
            session = ensure_session(inner.clone(), &key).await?;
            evaluate_xhr(&session.page, &spec).await.map_err(|second| {
                SodaError::http(format!(
                    "签名页执行请求失败（重试后）: {second}（首次: {first}）"
                ))
            })?
        }
    };
    // 只有护照 API 才要求 a_bogus；confirmed 后跟随 redirect_url 只是为了
    // 把登录 cookie 落到本上下文，那一跳本来就没有签名。二次验证组件的桥接
    // 请求不严格校验（对齐 Meting 的 requestQishuiSession，组件自身会处理失败）。
    if strict_sign && request.url.contains("/passport/") {
        let a_bogus = a_bogus_of(&xhr.response_url);
        if a_bogus.len() != 44 {
            return Err(SodaError::http(format!(
                "汽水安全签名生成失败：a_bogus 缺失（{} 字符）：{a_bogus}；URL={}",
                a_bogus.len(),
                xhr.response_url
            )));
        }
    }
    // CDP 的 Network.getCookies 默认只返回「当前页面 URL」的 cookie；
    // 签名页本身跑在 127.0.0.1，必须显式按汽水域名查，才能拿到
    // 与 Playwright context.cookies() 等价的整份登录态。
    let cookies = context_cookies(&session.page).await?;
    Ok(BrowserResponse {
        ok: true,
        status: xhr.status,
        body: xhr.body,
        response_url: xhr.response_url,
        headers: xhr.headers,
        cookies,
        error: String::new(),
    })
}

/// 在签名页里执行一次 `window.__qishuiRequest`，返回 XHR 结果。
async fn evaluate_xhr(page: &Page, spec: &Value) -> Result<JsXhr> {
    let call = CallFunctionOnParams::builder()
        .function_declaration("async function (spec) { return await window.__qishuiRequest(spec) }")
        .argument(CallArgument::builder().value(spec.clone()).build())
        .build()
        .map_err(|err| SodaError::http(format!("构造调用失败: {err}")))?;
    let result = page
        .evaluate_function(call)
        .await
        .map_err(|err| SodaError::http(format!("签名页执行请求失败: {err}")))?;
    result
        .into_value()
        .map_err(|err| SodaError::http(format!("签名页返回无法解析: {err}")))
}

/// 读取页面所属上下文里汽水域名的全部 cookie。
///
/// CDP 的 Network.getCookies 默认只返回「当前页面 URL」的 cookie；
/// 签名页本身跑在 127.0.0.1，必须显式按汽水域名查，才能拿到
/// 与 Playwright `context.cookies()` 等价的整份登录态。
async fn context_cookies(page: &Page) -> Result<Vec<BrowserCookie>> {
    let cookie_params = GetCookiesParams::builder()
        .url("https://api.qishui.com")
        .url("https://bff-pc.qishui.com")
        .url("http://api.qishui.com")
        .url("http://bff-pc.qishui.com")
        .build();
    Ok(page
        .execute(cookie_params)
        .await
        .map_err(|err| SodaError::http(format!("读取签名页 cookie 失败: {err}")))?
        .result
        .cookies
        .iter()
        .map(|cookie| BrowserCookie {
            name: cookie.name.clone(),
            value: cookie.value.clone(),
            domain: cookie.domain.clone(),
        })
        .collect())
}

async fn async_close(inner: Arc<Inner>, key: &str) -> Result<()> {
    let (session, browser, now_empty) = {
        let mut state = inner
            .state
            .lock()
            .map_err(|_| SodaError::http("签名页状态锁中毒"))?;
        let session = state.sessions.remove(key);
        (session, state.browser.clone(), state.sessions.is_empty())
    };
    if let Some(session) = session {
        if let Some(browser) = browser {
            browser
                .dispose_browser_context(session.context)
                .await
                .map_err(|err| SodaError::http(format!("关闭签名页上下文失败: {err}")))?;
        }
    }
    // 最后一个会话也关掉时顺手回收整个浏览器：验证窗口（headed）不能残留，
    // headless 情况下也能立即释放内存，下次请求会重新拉起。
    if now_empty {
        retire_browser(&inner).await;
    }
    Ok(())
}

async fn ensure_session(inner: Arc<Inner>, key: &str) -> Result<Session> {
    {
        let state = inner
            .state
            .lock()
            .map_err(|_| SodaError::http("签名页状态锁中毒"))?;
        if let Some(session) = state.sessions.get(key) {
            return Ok(Session {
                context: session.context.clone(),
                page: session.page.clone(),
                last_used: Instant::now(),
            });
        }
    }
    let browser = ensure_browser(inner.clone()).await?;
    match create_session(inner.clone(), &browser, key).await {
        Ok(session) => Ok(session),
        Err(first) => {
            // 上下文/页面创建失败往往意味着浏览器进程已死（CDP 通道静默断开，
            // 实测出现过）：丢弃僵尸句柄重启一次，仍失败才报错。
            evict_browser(&inner);
            let browser = ensure_browser(inner.clone()).await?;
            create_session(inner.clone(), &browser, key)
                .await
                .map_err(|second| {
                    SodaError::http(format!(
                        "创建签名页失败（重启后仍失败）: {second}（首次: {first}）"
                    ))
                })
        }
    }
}

/// 在二维码会话的浏览器上下文里打开**可见**的二次验证窗口。
///
/// 官方验证组件在**默认浏览器**里跑有两道缝：页面只接管了 XHR（组件用
/// `fetch` 的请求会直撞 CORS），bdms 也只能从 CDN 拉且环境与签名页不一致。
/// 所以这里改成在 CDP 浏览器（`--disable-web-security` + 本地 bdms + 同一
//  cookie jar）里开窗口：
///
/// 1. 从现有二维码上下文收割汽水 cookie；
/// 2. 回收当前浏览器，改为 headed 重新拉起（用户要交互，headless 不可见）；
/// 3. 重建二维码上下文与签名页，把 cookie 注回去；
/// 4. 在**同一上下文**里新开一页加载验证窗口 —— 组件的桥接请求回环到
///    同一上下文，cookie/设备身份/a_bogus 全部对齐。
///
/// 返回窗口地址（也可手动粘进任意浏览器作为兑底）。
async fn async_open_verify(inner: Arc<Inner>, token: &str) -> Result<String> {
    let key = {
        let verifies = inner
            .verifies
            .lock()
            .map_err(|_| SodaError::http("验证登记表锁中毒"))?;
        verifies
            .get(token.trim())
            .map(|entry| normalize_key(&entry.session_key))
            .ok_or_else(|| SodaError::not_found("汽水二维码会话已过期，请重新生成二维码"))?
    };
    let url = format!(
        "http://127.0.0.1:{}/security_host.html?key={}&bridgeRoot=/verify",
        inner.port,
        crate::util::query_escape(token.trim())
    );

    // 1. 收割现有二维码上下文的 cookie（浏览器可能已死：尽力而为，失败当空）
    let existing_page = {
        let state = inner
            .state
            .lock()
            .map_err(|_| SodaError::http("签名页状态锁中毒"))?;
        state.sessions.get(&key).map(|session| session.page.clone())
    };
    let mut cookies: Vec<BrowserCookie> = Vec::new();
    if let Some(page) = existing_page {
        if let Ok(harvested) = context_cookies(&page).await {
            cookies = harvested;
        }
    }

    // 2. 先声明 headed（避免并发拉起 headless），再回收旧浏览器
    inner.headed.store(true, Ordering::Relaxed);
    retire_browser(&inner).await;

    // 3. 重建二维码上下文 + 签名页，注入 cookie
    let session = ensure_session(inner.clone(), &key).await?;
    if !cookies.is_empty() {
        let params: Vec<CookieParam> = cookies
            .iter()
            .map(|cookie| {
                let mut builder = CookieParam::builder();
                builder = builder
                    .name(cookie.name.as_str())
                    .value(cookie.value.as_str());
                if !cookie.domain.is_empty() {
                    builder = builder.domain(cookie.domain.as_str());
                }
                builder.path("/").build()
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| SodaError::http(format!("构造 cookie 参数失败: {err}")))?;
        session
            .page
            .set_cookies(params)
            .await
            .map_err(|err| SodaError::http(format!("恢复验证会话 cookie 失败: {err}")))?;
    }

    // 4. 同一上下文里开可见验证窗口
    let browser = ensure_browser(inner.clone()).await?;
    let target = CreateTargetParams::builder()
        .url("about:blank")
        .browser_context_id(session.context.clone())
        .build()
        .map_err(|err| SodaError::http(format!("创建验证窗口参数失败: {err}")))?;
    let page = browser
        .new_page(target)
        .await
        .map_err(|err| SodaError::http(format!("创建验证窗口失败: {err}")))?;
    page.set_user_agent(USER_AGENT)
        .await
        .map_err(|err| SodaError::http(format!("设置验证窗口 UA 失败: {err}")))?;
    page.add_init_script(init_script(inner.port))
        .await
        .map_err(|err| SodaError::http(format!("注入验证窗口脚本失败: {err}")))?;
    let navigation = NavigateParams::builder()
        .url(url.clone())
        .build()
        .map_err(|err| SodaError::http(format!("构造验证窗口导航失败: {err}")))?;
    page.goto(navigation)
        .await
        .map_err(|err| SodaError::http(format!("打开验证窗口失败: {err}")))?;

    // 5. 收拾多余的初始窗口：Chrome 启动会自带一个默认上下文窗口
    //    （新标签页/空白页），用户只应该看到验证窗口（签名页除外——它是
    //    代发请求的引擎）。我们自己的页面都是 127.0.0.1 地址，不会误伤。
    //    注意必须用 browser 级的 Target.closeTarget：Page::close 走的是
    //    page-session 级命令，对从未 attach 过的初始页会静默失败（实测踩过）。
    let pages = browser.pages().await.unwrap_or_default();
    for existing in pages {
        let is_stray = existing
            .url()
            .await
            .map(|url| match url.as_deref() {
                None => true,
                Some(url) => url.is_empty() || url == "about:blank" || url.starts_with("chrome://"),
            })
            .unwrap_or(false);
        if is_stray {
            let close = CloseTargetParams::builder()
                .target_id(existing.target_id().clone())
                .build()
                .map_err(|err| SodaError::http(format!("构造关闭命令失败: {err}")))?;
            let _ = browser.execute(close).await;
        }
    }
    Ok(url)
}

/// 在给定浏览器里新建一个签名页会话（独立上下文 + 页面）。
async fn create_session(inner: Arc<Inner>, browser: &Arc<Browser>, key: &str) -> Result<Session> {
    let context = browser
        .create_browser_context(CreateBrowserContextParams::default())
        .await
        .map_err(|err| SodaError::http(format!("创建签名页上下文失败: {err}")))?;
    let target = CreateTargetParams::builder()
        .url("about:blank")
        .browser_context_id(context.clone())
        .build()
        .map_err(|err| SodaError::http(format!("创建签名页页面参数失败: {err}")))?;
    let page = browser
        .new_page(target)
        .await
        .map_err(|err| SodaError::http(format!("创建签名页失败: {err}")))?;
    page.set_user_agent(USER_AGENT)
        .await
        .map_err(|err| SodaError::http(format!("设置签名页 UA 失败: {err}")))?;
    page.add_init_script(init_script(inner.port))
        .await
        .map_err(|err| SodaError::http(format!("注入签名页脚本失败: {err}")))?;
    let navigation = NavigateParams::builder()
        .url(format!(
            "http://127.0.0.1:{}/security_host.html",
            inner.port
        ))
        .build()
        .map_err(|err| SodaError::http(format!("构造导航失败: {err}")))?;
    page.goto(navigation)
        .await
        .map_err(|err| SodaError::http(format!("打开签名页失败: {err}")))?;
    wait_ready(&page).await?;
    let session = Session {
        context,
        page,
        last_used: Instant::now(),
    };
    inner
        .state
        .lock()
        .map_err(|_| SodaError::http("签名页状态锁中毒"))?
        .sessions
        .insert(key.to_string(), session.clone_like());
    Ok(session)
}

impl Session {
    /// chromiumoxide 的 Page 是 Clone（内部是句柄），这里复制一份放进缓存。
    fn clone_like(&self) -> Session {
        Session {
            context: self.context.clone(),
            page: self.page.clone(),
            last_used: self.last_used,
        }
    }
}

async fn wait_ready(page: &Page) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let state = page
            .evaluate_expression("JSON.stringify({bdms: typeof window.bdms, request: typeof window.__qishuiRequest})")
            .await
            .map_err(|err| SodaError::http(format!("检查签名页状态失败: {err}")))?;
        let text: String = state
            .into_value()
            .map_err(|err| SodaError::http(format!("解析签名页状态失败: {err}")))?;
        if text.contains("\"bdms\":\"object\"") && text.contains("\"request\":\"function\"") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(SodaError::http(
        "汽水安全组件初始化超时（bdms / __qishuiRequest 未就绪）",
    ))
}

async fn ensure_browser(inner: Arc<Inner>) -> Result<Arc<Browser>> {
    {
        let state = inner
            .state
            .lock()
            .map_err(|_| SodaError::http("签名页状态锁中毒"))?;
        if let Some(browser) = &state.browser {
            return Ok(browser.clone());
        }
    }
    let executable = chromium_executable()
        .ok_or_else(|| SodaError::http("未找到 Chrome/Chromium，请设置 QISHUI_CHROMIUM_PATH"))?;
    // Chrome 新版本要求 --disable-web-security 必须搭配非默认 user-data-dir，
    // 否则该开关会被静默忽略（Playwright 之所以能用，是因为它永远开临时 profile）。
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    );
    let user_data_dir = std::env::temp_dir().join(format!("libresoda-cdp-{unique}"));
    std::fs::create_dir_all(&user_data_dir)
        .map_err(|err| SodaError::http(format!("创建 Chromium 临时目录失败: {err}")))?;
    let mut config = BrowserConfig::builder()
        .chrome_executable(&executable)
        .user_data_dir(&user_data_dir)
        // 默认旧 headless：实测 new headless 下 --disable-web-security 会被忽略
        .no_sandbox()
        // 注意：chromiumoxide 的 arg() 会自动补 `--` 前缀，
        // 再传 "--disable-web-security" 会变成 "----disable-web-security" 而被忽略。
        .arg("disable-setuid-sandbox")
        .arg("disable-dev-shm-usage")
        .arg("disable-gpu")
        .arg("disable-extensions")
        .arg("disable-background-networking")
        .arg("disable-sync")
        .arg("disable-default-apps")
        .arg("disable-component-update")
        .arg(("renderer-process-limit", "1"))
        .arg("disable-web-security")
        .arg((
            "disable-features",
            "IsolateOrigins,site-per-process,BlockThirdPartyCookies,ThirdPartyStoragePartitioning",
        ))
        .arg("disable-third-party-cookies");
    // 打开过二次验证窗口后，浏览器要可见（用户需要在窗口里完成验证）
    if inner.headed.load(Ordering::Relaxed) {
        config = config.with_head();
    }
    // 排查浏览器无声退出：SODA_CDP_DUMP=1 时让 Chromium 把日志写进 profile 目录
    if std::env::var("SODA_CDP_DUMP").ok().as_deref() == Some("1") {
        let log_file = user_data_dir.join("chrome.log").display().to_string();
        config = config
            .arg("enable-logging")
            .arg(("v", "1"))
            .arg(("log-file", log_file.as_str()));
    }
    let config = config.build().map_err(|err| {
        SodaError::http(format!(
            "构造 Chromium 启动配置失败（{}）: {err}",
            executable.display()
        ))
    })?;
    let (browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|err| SodaError::http(format!("启动 Chromium 失败: {err}")))?;
    let browser = Arc::new(browser);
    tokio::task::spawn(async move {
        while let Some(event) = handler.next().await {
            if event.is_err() {
                break;
            }
        }
    });
    let mut state = inner
        .state
        .lock()
        .map_err(|_| SodaError::http("签名页状态锁中毒"))?;
    if let Some(existing) = &state.browser {
        return Ok(existing.clone());
    }
    state.browser = Some(browser.clone());
    Ok(browser)
}

/// 找一个可用的 Chromium 系浏览器：显式配置 > 系统安装位置 > PATH。
///
/// 不限 Chrome/Chromium —— Edge（Windows 自带）、Brave、Vivaldi、Opera 都是
/// 同一套 CDP，能用就行；用户机器上大概率至少有一个。
pub fn chromium_executable() -> Option<PathBuf> {
    for key in ["QISHUI_CHROMIUM_PATH", "CHROMIUM_PATH"] {
        if let Ok(value) = std::env::var(key) {
            let path = PathBuf::from(value.trim());
            if path.is_file() {
                return Some(path);
            }
        }
    }
    if let Some(path) = candidate_paths().into_iter().find(|path| path.is_file()) {
        return Some(path);
    }
    let path_var = std::env::var_os("PATH")?;
    find_in_path(&binary_names(), &path_var)
}

/// PATH 查找（单独抽出来便于测试）。
fn find_in_path(names: &[&str], path_var: &std::ffi::OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn binary_names() -> Vec<&'static str> {
    #[cfg(windows)]
    {
        vec![
            "chrome.exe",
            "msedge.exe",
            "brave.exe",
            "vivaldi.exe",
            "opera.exe",
        ]
    }
    #[cfg(not(windows))]
    {
        vec![
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
            "microsoft-edge-stable",
            "brave-browser",
            "vivaldi",
            "opera",
        ]
    }
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        for root in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
            let Some(base) = std::env::var_os(root) else {
                continue;
            };
            let base = PathBuf::from(base);
            paths.push(base.join("Google/Chrome/Application/chrome.exe"));
            paths.push(base.join("Microsoft/Edge/Application/msedge.exe"));
            paths.push(base.join("BraveSoftware/Brave-Browser/Application/brave.exe"));
            paths.push(base.join("Vivaldi/Application/vivaldi.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut roots = vec![PathBuf::from("/Applications")];
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join("Applications"));
        }
        for root in roots {
            paths.push(root.join("Google Chrome.app/Contents/MacOS/Google Chrome"));
            paths.push(root.join("Chromium.app/Contents/MacOS/Chromium"));
            paths.push(root.join("Microsoft Edge.app/Contents/MacOS/Microsoft Edge"));
            paths.push(root.join("Brave Browser.app/Contents/MacOS/Brave Browser"));
            paths.push(root.join("Vivaldi.app/Contents/MacOS/Vivaldi"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for path in [
            "/usr/local/bin/chromium",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/microsoft-edge",
            "/usr/bin/brave-browser",
            "/usr/bin/vivaldi-stable",
            "/snap/bin/chromium",
            "/var/lib/flatpak/exports/bin/org.chromium.Chromium",
        ] {
            paths.push(PathBuf::from(path));
        }
    }
    paths
}

fn a_bogus_of(url: &str) -> String {
    let raw = url
        .split_once("a_bogus=")
        .map(|(_, rest)| rest.split('&').next().unwrap_or_default().to_string())
        .unwrap_or_default();
    percent_decode(&raw)
}

/// 极简 percent-decode：签名里的 `/`、`+` 会被编码成 %2F/%2B，
/// 不解码的话 44 字符会被误判成 46/48。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// sdk-glue 会动态注入官方 CDN 的 bdms.js；把它改写到本地资产服务。
fn init_script(port: u16) -> String {
    format!(
        r#"
(function () {{
  const descriptor = Object.getOwnPropertyDescriptor(HTMLScriptElement.prototype, 'src')
  const original = document.createElement.bind(document)
  document.createElement = function (tag, ...rest) {{
    const element = original(tag, ...rest)
    if (String(tag).toLowerCase() !== 'script' || !descriptor) return element
    let source = ''
    Object.defineProperty(element, 'src', {{
      configurable: true,
      get: () => source,
      set: (value) => {{
        source = /bdms\.js(\?|$)/i.test(String(value))
          ? 'http://127.0.0.1:{port}/bdms.js'
          : String(value)
        descriptor.set.call(element, source)
      }},
    }})
    return element
  }}
}})()
"#
    )
}

/// 启动资产 + 验证桥接服务（仅监听 127.0.0.1，不引 HTTP 依赖）。
///
/// 除 5 个静态签名页文件外，还提供三条二次验证窗口专用的 POST 路由
/// （`security_host.html` 通过 `bridgeRoot=/verify` 参数访问，对齐
/// Meting-API 的 `/admin/qr/qishui/verify/*` 桥接）：
///
/// * `POST /verify/start`    `{key}` → 决策 + 公共参数
/// * `POST /verify/request`  `{key, request}` → 经签名页上下文代发请求
/// * `POST /verify/complete` `{key}` → 标记验证完成
///
/// token 本身由 `get_qrcode` 随机下发、随会话过期失效，作为能力凭证
/// 无法被外部猜测；组件请求的目标域名另有白名单限制。
fn start_asset_server(holder: Arc<OnceLock<Arc<Inner>>>) -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|err| SodaError::http(format!("签名页资产服务启动失败: {err}")))?;
    let port = listener
        .local_addr()
        .map_err(|err| SodaError::http(format!("读取签名页端口失败: {err}")))?
        .port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let holder = holder.clone();
            // 桥接请求可能长达 180s，必须一连接一线程，否则会卡死静态资产下载
            std::thread::spawn(move || {
                let _ = serve_connection(stream, holder);
            });
        }
    });
    Ok(port)
}

/// 解析后的极简 HTTP 请求（够本服务用即可）。
struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn serve_connection(
    mut stream: TcpStream,
    holder: Arc<OnceLock<Arc<Inner>>>,
) -> std::io::Result<()> {
    // 防线程泄漏：只连不发的连接不能永久占用一连接一线程。
    // 读超时只覆盖「等客户端发数据」阶段；桥接代理的长时间耗时发生在处理阶段，
    // 不受影响。
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let Some(request) = read_http_request(&mut stream)? else {
        return write_response(
            &mut stream,
            400,
            "text/plain; charset=utf-8",
            b"bad request",
        );
    };
    let path = request.path.split('?').next().unwrap_or("/").to_string();
    match (request.method.as_str(), path.as_str()) {
        ("GET", "/" | "/security_host.html") => write_response(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            SECURITY_HOST_HTML.as_bytes(),
        ),
        ("GET", "/sdk-glue.js") => write_response(
            &mut stream,
            200,
            "text/javascript; charset=utf-8",
            SDK_GLUE_JS.as_bytes(),
        ),
        ("GET", "/bdms.js") => write_response(
            &mut stream,
            200,
            "text/javascript; charset=utf-8",
            BDMS_JS.as_bytes(),
        ),
        ("GET", "/react.js") => write_response(
            &mut stream,
            200,
            "text/javascript; charset=utf-8",
            REACT_JS.as_bytes(),
        ),
        ("GET", "/react-dom.js") => write_response(
            &mut stream,
            200,
            "text/javascript; charset=utf-8",
            REACT_DOM_JS.as_bytes(),
        ),
        ("POST", "/verify/start") => {
            write_json(&mut stream, &handle_verify_start(&holder, &request.body))
        }
        ("POST", "/verify/request") => {
            write_json(&mut stream, &handle_verify_request(&holder, &request.body))
        }
        ("POST", "/verify/complete") => {
            write_json(&mut stream, &handle_verify_complete(&holder, &request.body))
        }
        _ => write_response(&mut stream, 404, "text/plain; charset=utf-8", b"not found"),
    }
}

/// 读一个完整 HTTP 请求：头部读到 `\r\n\r\n`，body 按 `Content-Length` 定长读。
/// 头超 64KB / body 超 1MB / 连接提前断开都视为坏请求。
fn read_http_request(stream: &mut TcpStream) -> std::io::Result<Option<HttpRequest>> {
    use std::io::Read;

    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(position) = find_subsequence(&buffer, b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > 64 * 1024 {
            return Ok(None);
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let path = parts.next().unwrap_or("/").to_string();
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    if content_length > 1024 * 1024 {
        return Ok(None);
    }
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(Some(HttpRequest { method, path, body }))
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    use std::io::Write;

    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)
}

fn write_json(stream: &mut TcpStream, value: &Value) -> std::io::Result<()> {
    let body = value.to_string();
    write_response(stream, 200, "application/json", body.as_bytes())
}

/// 桥接错误（`security_host.html` 会把 `error` 字段展示在验证页状态条上）。
fn bridge_error(message: &str) -> Value {
    serde_json::json!({ "success": false, "error": message })
}

/// 从 JSON body 里取一个字符串字段（缺失/非字符串时返回空串）。
fn json_str_field(body: &[u8], field: &str) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// `POST /verify/start`：把 2046 决策与公共参数交给验证窗口。
fn handle_verify_start(holder: &Arc<OnceLock<Arc<Inner>>>, body: &[u8]) -> Value {
    let Some(inner) = holder.get() else {
        return bridge_error("签名页尚未就绪");
    };
    let key = json_str_field(body, "key");
    let Ok(verifies) = inner.verifies.lock() else {
        return bridge_error("验证登记表锁中毒");
    };
    match verifies.get(&key) {
        Some(entry) => serde_json::json!({
            "success": true,
            "data": {
                "decision": entry.decision,
                "generalParams": entry.general_params,
            },
        }),
        None => bridge_error("汽水二维码会话已过期，请重新生成二维码"),
    }
}

/// `POST /verify/complete`：验证窗口回执完成，轮询侧随即重发确认。
fn handle_verify_complete(holder: &Arc<OnceLock<Arc<Inner>>>, body: &[u8]) -> Value {
    let Some(inner) = holder.get() else {
        return bridge_error("签名页尚未就绪");
    };
    let key = json_str_field(body, "key");
    let Ok(mut verifies) = inner.verifies.lock() else {
        return bridge_error("验证登记表锁中毒");
    };
    match verifies.get_mut(&key) {
        Some(entry) => {
            entry.done = true;
            serde_json::json!({ "success": true })
        }
        None => bridge_error("汽水二维码会话已过期，请重新生成二维码"),
    }
}

/// `POST /verify/request`：把验证组件的网络请求转发进二维码会话的浏览器上下文
/// （等价 Meting-API 的 `requestQishuiSecondVerify`）。目标域名有白名单。
fn handle_verify_request(holder: &Arc<OnceLock<Arc<Inner>>>, body: &[u8]) -> Value {
    let Some(inner) = holder.get() else {
        return bridge_error("签名页尚未就绪");
    };
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        return bridge_error("验证请求无法解析");
    };
    let key = parsed
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let session_key = {
        let Ok(verifies) = inner.verifies.lock() else {
            return bridge_error("验证登记表锁中毒");
        };
        match verifies.get(&key) {
            Some(entry) => entry.session_key.clone(),
            None => return bridge_error("汽水二维码会话已过期，请重新生成二维码"),
        }
    };
    let Some(spec) = parsed.get("request") else {
        return bridge_error("验证请求缺少 request 字段");
    };
    let method = spec
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let url = spec
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // 白名单（对齐 Meting 的 requestQishuiSession）：token 只能用来代发汽水/字节域的请求
    let allowed = ["api.qishui.com", "auth.zijieapi.com", "bff-pc.qishui.com"]
        .iter()
        .any(|host| url_host(&url).as_deref() == Some(*host));
    if !allowed {
        return bridge_error("验证请求目标不受支持");
    }
    let mut headers = BTreeMap::new();
    if let Some(map) = spec.get("headers").and_then(Value::as_object) {
        for (name, value) in map {
            // cookie 由浏览器上下文自管；host/content-length/origin/referer
            // 交给 Chromium 按目标 URL 生成（对齐 Meting 的过滤）
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "cookie" | "host" | "content-length" | "origin" | "referer"
            ) {
                continue;
            }
            let text = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            headers.insert(name.clone(), text);
        }
    }
    let request_body = match spec.get("body") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) if !other.is_null() => Some(other.to_string()),
        _ => None,
    };
    let request = BrowserRequest {
        session_key,
        method,
        url,
        headers,
        body: request_body,
        ms_token: String::new(),
    };
    let result = inner
        .runtime
        .block_on(async_request(inner.clone(), &request, false));
    match result {
        Ok(response) => serde_json::json!({
            "ok": true,
            "status": response.status,
            "body": response.body,
            "responseURL": response.response_url,
            "headers": response.headers,
        }),
        Err(err) => serde_json::json!({
            "ok": false,
            "status": 0,
            "body": String::new(),
            "responseURL": String::new(),
            "headers": String::new(),
            "error": err.to_string(),
        }),
    }
}

/// 极简取 host：`https://api.qishui.com/x?y` → `api.qishui.com`。
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', ':', '?', '#']).next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bogus_length_survives_percent_encoding() {
        // 真实形态：/ 会被编码成 %2F，不解码会误判成 46 字符
        let url = "https://api.qishui.com/x?a_bogus=mj-DfOgSMsf1SzPeR7kw997pb%2Fy0YW4FgZEz-wPbZtq6&next=1";
        assert_eq!(a_bogus_of(url).len(), 44, "{}", a_bogus_of(url));
    }

    #[test]
    fn percent_decode_handles_plain_text() {
        assert_eq!(percent_decode("abc%2Fd%2B"), "abc/d+");
        assert_eq!(percent_decode("no-encoding"), "no-encoding");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    #[test]
    fn find_in_path_locates_executable() {
        let dir = std::env::temp_dir().join(format!("libresoda-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let name = if cfg!(windows) {
            "msedge.exe"
        } else {
            "microsoft-edge"
        };
        let fake = dir.join(name);
        std::fs::write(&fake, b"#!/bin/sh\n").expect("fake browser");
        let found = find_in_path(&[name], dir.as_os_str()).expect("should find");
        assert_eq!(found, fake);
        assert!(find_in_path(&["definitely-missing-browser"], dir.as_os_str()).is_none());
        let _ = std::fs::remove_file(&fake);
    }

    #[test]
    fn url_host_extracts_only_allowed_domains() {
        assert_eq!(
            url_host("https://api.qishui.com/passport/web/check_qrconnect/?a=1"),
            Some("api.qishui.com".to_string())
        );
        assert_eq!(
            url_host("https://auth.zijieapi.com/x"),
            Some("auth.zijieapi.com".to_string())
        );
        assert_eq!(
            url_host("https://evil.example.com/passport/"),
            Some("evil.example.com".to_string())
        );
        assert_eq!(url_host("not-a-url"), None);
    }

    /// 二次验证桥接路由的全生命周期（不拉 Chromium：代发请求只测白名单拦截）。
    #[test]
    fn verify_bridge_routes_cycle() {
        let signer = CdpSigner::new();
        let token = format!("verify-bridge-test-{}", std::process::id());
        let url = signer.second_verify_url(&token).expect("url");
        assert!(url.contains("bridgeRoot=/verify"), "url={url}");
        assert!(url.contains(&format!("key={token}")), "url={url}");
        let port: u16 = url
            .split_once("://")
            .and_then(|(_, rest)| rest.split('/').next())
            .and_then(|host_port| host_port.split_once(':'))
            .and_then(|(_, port)| port.parse().ok())
            .expect("port");

        // 未登记 → start 拒绝
        let response = post_json(port, "/verify/start", &serde_json::json!({ "key": token }));
        assert!(response.contains("\"success\":false"), "{response}");

        // 登记 → start 返回决策与公共参数
        signer
            .register_second_verify(
                &token,
                "qr-test",
                &serde_json::json!({ "verify_ways": [{ "type": "mobile_sms_verify" }] }),
                &serde_json::json!({ "aid": "386088" }),
            )
            .expect("register");
        let response = post_json(port, "/verify/start", &serde_json::json!({ "key": token }));
        assert!(response.contains("mobile_sms_verify"), "{response}");
        assert!(response.contains("\"success\":true"), "{response}");

        // 白名单外的代发目标 → 拒绝（且不触发浏览器拉起）
        let response = post_json(
            port,
            "/verify/request",
            &serde_json::json!({
                "key": token,
                "request": { "method": "GET", "url": "https://evil.example.com/x", "headers": {} },
            }),
        );
        assert!(response.contains("目标不受支持"), "{response}");

        // complete → done 置位；ack 后回落；clear 后 start 再拒绝
        let response = post_json(
            port,
            "/verify/complete",
            &serde_json::json!({ "key": token }),
        );
        assert!(response.contains("\"success\":true"), "{response}");
        assert!(signer.second_verify_done(&token));
        signer.ack_second_verify(&token).expect("ack");
        assert!(!signer.second_verify_done(&token));
        signer.clear_second_verify(&token).expect("clear");
        let response = post_json(port, "/verify/start", &serde_json::json!({ "key": token }));
        assert!(response.contains("\"success\":false"), "{response}");
    }

    /// 向本地桥接服务发一个 JSON POST，读回完整响应体。
    fn post_json(port: u16, path: &str, payload: &Value) -> String {
        use std::io::{Read, Write};

        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let body = payload.to_string();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        // 去掉头部，只回 body
        response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or(response)
    }
}
