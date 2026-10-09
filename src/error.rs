//! 统一错误类型。
//!
//! 所有与外部世界的交互（HTTP、文件系统、音频设备）都必须转换成带上下文的
//! [`AppError`] 再向上传播。非测试代码里禁止出现裸 `unwrap()` / `expect()`。

use thiserror::Error;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    /// 传输层失败：DNS 解析、连接被拒、TLS 握手、超时。
    ///
    /// 转换走下面的手写 `From`，不是 `#[from]`：那条路径会先把 URL 抹掉。
    #[error("网络请求失败：{0}")]
    Http(reqwest::Error),

    /// KuGouMusicApi 返回了非 2xx 状态码。
    #[error("接口 {path} 返回状态码 {status}")]
    HttpStatus { path: String, status: u16 },

    /// 业务层错误码，例如 `error_code: 152` 表示缺少认证信息。
    #[error("接口 {path} 返回错误：code={code} {message}")]
    Api {
        path: String,
        code: i64,
        message: String,
    },

    /// 服务端返回 2xx，但响应体不是 JSON。
    ///
    /// 单独成一类，因为**最常见的原因不是「接口坏了」，而是这个端口上跑的根本不是
    /// 接口服务**——比如被别的程序占了。实测撞过：默认的 3000 端口上跑着另一个 Web
    /// 服务，返回一页 HTML；那时 serde 只会说
    /// `expected value at line 1 column 1`，完全看不出该去查什么。
    ///
    /// 顺带把响应体的开头带上：是网页、是空响应、还是一段纯文本，一眼能分辨。
    #[error("接口 {path} 返回的不是 JSON（HTTP {status}）：{preview}")]
    NonJsonBody {
        path: String,
        status: u16,
        /// 响应体开头一小段（已压掉换行与连续空白）
        preview: String,
    },

    /// 未附带路径的 IO 错误（多为 `?` 自动转换产生）。
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),

    /// 带路径的 IO 错误，排查问题时能直接定位到文件。
    #[error("文件 {path} 操作失败：{source}")]
    IoAt {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("配置错误：{0}")]
    Config(String),

    #[error("音频引擎错误：{0}")]
    Audio(String),

    /// 本地接口服务（KuGouMusicApi）的准备或启动失败。
    ///
    /// 与 `Http` 分开：`Http` 的处置动作是「换个时刻重试」，而这类错误重试一百次也
    /// 不会变好——该做的是检查本机环境（有没有 node、依赖装没装、日志里报了什么）。
    /// UI 层的提示也据此不同，见 `bootstrap.rs`。
    #[error("{0}")]
    Service(String),

    #[error("未找到资源：{0}")]
    NotFound(String),
    #[error("{0}")]
    /// 其它内部错误（任务调度失败之类），保留上下文便于定位。
    Other(String),
}

/// `reqwest::Error` 的 `Display` 会附上出错的 URL，而我们的 URL 里带着
/// `token`、`userid`、`dfid`——这些是账号凭据，不该因为一次网络抖动就落进日志。
///
/// 所以这里手写转换，先把 URL 摘掉（`without_url` 正是 reqwest 为
/// 「query 里有 API key」这种场景提供的），再交给 `AppError::Http`。
/// 诊断信息保留错误种类（连接失败/超时/解码失败），只丢 URL——URL 的归属
/// 在调用方已经知道，而 `path` 另有字段承载。
impl From<reqwest::Error> for AppError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error.without_url())
    }
}

impl AppError {
    /// 为 IO 错误补充出错的文件路径。
    pub fn io_at(path: impl Into<String>, source: std::io::Error) -> Self {
        Self::IoAt {
            path: path.into(),
            source,
        }
    }

    /// 是否是「页码越界」。
    ///
    /// 实测搜索接口 `page` 最多到 16 页，再往后返回 `error_code: 149`（Out Page Range）。
    /// 这是正常的"没更多了"，不是故障——翻页时应当据此停止，而不是当错误抛出。
    pub fn is_page_out_of_range(&self) -> bool {
        match self {
            Self::Api { code, .. } => *code == 149,
            _ => false,
        }
    }

    /// 是否属于「需要登录」类错误。UI 层据此弹出登录引导而不是普通报错。
    ///
    /// 已知码（都是实测出来的，上游不公开语义）：
    ///
    /// * `152` —— 搜索接口缺 cookie
    /// * `20005` / `40004` —— 登录态失效
    /// * `20028` —— 取播放直链时的「本次请求需要验证」
    /// * `20010` —— 请求里完全没有可用的认证信息（`/user/playlist` 实测）
    /// * `20017` —— token 本身无效或已过期：`/user/playlist` 只在带了 token 时才
    ///   返回它，不带 token 时返回 `20010`。服务端不附带任何错误描述，所以只认
    ///   这个码，界面才能提示「重新扫码」而不是甩一句 `code=20017`。
    pub fn is_auth_related(&self) -> bool {
        match self {
            Self::Api { code, .. } => matches!(code, 152 | 20005 | 20010 | 20017 | 20028 | 40004),
            _ => false,
        }
    }

    /// 是否是「曾经登录过、但现在失效了」（区别于「从来没登录」）。
    ///
    /// 两者的处置动作都是按 `L`，但说清楚「已失效」能省掉一次自查：
    /// 用户不用先怀疑是不是自己没扫过码。
    pub fn is_login_expired(&self) -> bool {
        match self {
            Self::Api { code, .. } => matches!(code, 20005 | 20017 | 40004),
            _ => false,
        }
    }

    /// 是否是「传输层根本没通」——连接被拒、DNS 失败、超时。
    ///
    /// 用来区分两种失败：服务没起来（连不上），和服务起来了但拒绝了这次请求
    /// （业务错误码 / 非 2xx）。**后者恰恰证明服务是通的**，不该被当成「未连通」。
    pub fn is_connectivity(&self) -> bool {
        match self {
            Self::Http(error) => error.is_connect() || error.is_timeout(),
            _ => false,
        }
    }

    /// 这次失败是不是「换个时刻重发就可能成功」的瞬时故障。
    ///
    /// 判据只有一条：**同样的请求重发一次，结果是否可能不同**。
    ///
    /// 会（→ 可重试）：
    ///
    /// * 传输层断在连接或读体上——连接被拒、连接被重置、响应体读到一半断了。
    ///   这类错误都在毫秒级返回，正是「有网但恰好抖了一下」的样子。
    /// * 服务端 5xx，以及 408 / 429——对端明确表示「现在不行，等会儿再来」。
    ///
    /// 不会（→ 直接放弃）：
    ///
    /// * **超时不算**。那已经等满 15 秒（连接超时 8 秒），说明对端是卡住而不是
    ///   抖了一下；再等两轮是拿用户的时间换一个大概率相同的结果。
    /// * 业务错误码（`AppError::Api`）——服务回了话，只是拒绝了这次请求。
    ///   需要登录、页码越界、没有可用的播放地址都属于这一类，重发只会得到同样的答复。
    /// * 其它 4xx——请求本身有问题（参数、鉴权），与时刻无关。
    /// * `NonJsonBody`——200 却返回非 JSON，是内容问题不是传输问题（真正的截断会走
    ///   `is_body` / `is_decode`，那两条在上面算可重试）。
    /// * 请求构造失败、配置 / IO / 音频错误——问题不在网络上。
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Http(error) => {
                !error.is_timeout() && (error.is_connect() || error.is_body() || error.is_decode())
            }
            Self::HttpStatus { status, .. } => {
                *status == 408 || *status == 429 || (500..600).contains(status)
            }
            _ => false,
        }
    }

    /// 面向用户的一行提示。避免把 reqwest 的长串错误直接糊到状态栏上。
    pub fn user_hint(&self) -> String {
        if self.is_login_expired() {
            return "登录态已失效：按 L 重新扫码（酷狗会定期轮换 token）".to_string();
        }
        if self.is_auth_related() {
            return "需要登录：按 L 扫码，或配置 cookie（--cookie / 配置文件）".to_string();
        }
        match self {
            Self::Http(error) if error.is_timeout() => "请求超时，请检查网络或代理设置".to_string(),
            Self::Http(error) if error.is_connect() => {
                "无法连接 KuGouMusicApi 服务，请确认它已启动（默认 127.0.0.1:3000）".to_string()
            }
            // 端口上跑着别的服务时，响应体多半是网页。这条提示要能直接指向「去改哪个
            // 配置」，否则用户只会看到 serde 的 `expected value at line 1 column 1`。
            Self::NonJsonBody { path, preview, .. } if preview.trim_start().starts_with('<') => {
                format!(
                    "接口 {path} 返回的是网页不是 JSON：该端口上跑的不是 KuGouMusicApi，\
                     检查配置里这个音源的 api_base"
                )
            }
            Self::NonJsonBody { path, .. } => {
                format!("接口 {path} 返回的不是 JSON（HTTP 200）：检查该音源的 api_base")
            }
            other => other.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(code: i64) -> AppError {
        AppError::Api {
            path: "/user/playlist".to_string(),
            code,
            message: "服务端未提供错误描述".to_string(),
        }
    }

    /// `/user/playlist` 实测：不带认证信息返回 20010，带了失效 token 返回 20017。
    /// 两个都必须归入「需要登录」，否则界面只会甩一句 `code=20017`。
    #[test]
    fn auth_codes_are_recognized() {
        for code in [152, 20005, 20010, 20017, 20028, 40004] {
            assert!(api(code).is_auth_related(), "code={code} 应属于登录类");
        }
        assert!(!api(149).is_auth_related(), "页码越界不是登录问题");
        assert!(api(149).is_page_out_of_range());
    }

    fn non_json(preview: &str) -> AppError {
        AppError::NonJsonBody {
            path: "/search".to_string(),
            status: 200,
            preview: preview.to_string(),
        }
    }

    /// 端口上跑着网页服务时，提示必须指向「去改 api_base」。
    ///
    /// 这是实测撞到的场景：默认的 3000 端口上跑着另一个 Web 服务，返回一页 HTML。
    /// 改动前用户看到的是 serde 的原话 `expected value at line 1 column 1`，
    /// 完全看不出该查什么。
    #[test]
    fn html_body_points_at_the_wrong_service() {
        let hint = non_json("<!doctype html><html lang=\"en\">").user_hint();
        assert!(hint.contains("/search"), "要指出是哪个接口：{hint}");
        assert!(hint.contains("网页"), "要说明拿到的是网页：{hint}");
        assert!(hint.contains("api_base"), "要指向该改的配置项：{hint}");
    }

    /// 非网页的非 JSON 响应（空响应体、一段纯文本）也要有可执行的提示。
    #[test]
    fn other_non_json_bodies_still_hint_at_api_base() {
        for preview in ["（空响应体）", "gateway timeout"] {
            let hint = non_json(preview).user_hint();
            assert!(
                hint.contains("api_base"),
                "preview={preview} 时提示：{hint}"
            );
        }
    }

    /// 内容问题不该被当成瞬时故障重试——重试解决不了「端口上是别的服务」。
    #[test]
    fn non_json_body_is_not_retried() {
        assert!(!non_json("<!doctype html>").is_transient());
    }

    #[test]
    fn expired_login_gets_a_clearer_hint() {
        let hint = api(20017).user_hint();
        assert!(hint.contains("失效"), "应说明是失效而不是没登录：{hint}");
        assert!(hint.contains("L"), "应提示重新扫码：{hint}");

        // 从来没登录过的（152 = 搜索缺 cookie）不该说「失效」
        assert!(api(152).user_hint().contains("需要登录"));
    }

    /// 业务错误码不算「连不上」——服务回了话，只是拒绝了这次请求。
    ///
    /// 归错的话，界面会在服务明明活着的时候报「未连通」，把用户引到错误的排查方向。
    #[test]
    fn business_errors_are_not_connectivity_failures() {
        for code in [152, 149, 20005, 20017, 20028] {
            assert!(
                !api(code).is_connectivity(),
                "code={code} 是业务错误，服务是通的"
            );
        }
        assert!(!AppError::Config("配置坏了".to_string()).is_connectivity());
        assert!(!AppError::Io(std::io::Error::other("本地磁盘")).is_connectivity());
    }

    fn status(code: u16) -> AppError {
        AppError::HttpStatus {
            path: "/song/url".to_string(),
            status: code,
        }
    }

    /// 该不该重试，只看一条：**换个时刻重发，结果会不会不同**。
    ///
    /// 这条策略直接决定用户是「等一秒然后正常播」，还是「看到一个和真实原因
    /// 毫无关系的错误」，所以逐类钉住。
    ///
    /// 注：超时那一支（`is_timeout()` → 不重试）没法在单测里构造 `reqwest::Error`，
    /// 只能靠 `is_timeout()` 的语义保证；它由端到端的手测覆盖。
    #[test]
    fn only_transient_failures_are_retried() {
        // 业务错误码：服务回了话，重发还是这个答复
        for code in [152, 149, 20005, 20017, 31863] {
            assert!(
                !api(code).is_transient(),
                "code={code} 是业务错误，不该重试"
            );
        }

        // 服务端明确表示「现在不行，等会儿再来」
        for code in [408, 429, 500, 502, 503, 504] {
            assert!(status(code).is_transient(), "HTTP {code} 应当重试");
        }

        // 客户端错误：请求本身有问题，与时刻无关
        for code in [400, 401, 403, 404, 410, 422] {
            assert!(!status(code).is_transient(), "HTTP {code} 不该重试");
        }

        // 本地问题
        assert!(!AppError::Config("坏了".to_string()).is_transient());
        assert!(!AppError::Io(std::io::Error::other("磁盘")).is_transient());
        assert!(!AppError::NotFound("《X》没有可用的播放地址".to_string()).is_transient());
    }

    /// `reqwest::Error` 的 `Display` 会附上出错的完整 URL（`for url (…)`），
    /// 而我们的 URL 上挂着 `token`、`userid`、`dfid`。一次网络抖动就会把账号凭据
    /// 写进日志——阶段 4 的汇报里真实 token 就是这么漏出去的。
    ///
    /// 这里用一个**真实但必然失败**的请求来验证转换确实把 URL 摘掉了：先绑一个
    /// 端口再关掉它，连过去必定 connection refused。不 mock，因为要验的正是
    /// reqwest 自己那条 Display 路径。
    #[tokio::test]
    async fn http_error_drops_the_url_before_it_can_leak_credentials() {
        // 绑到 0 拿到一个当前空闲的端口，随即释放——之后连过去会被拒。
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
            listener.local_addr().expect("读本地地址").port()
        };

        let secret = "TOKENFIXTURE";
        let url = format!("http://127.0.0.1:{port}/download?token={secret}&userid=10001");
        let error = reqwest::get(&url)
            .await
            .expect_err("端口已关闭，应当连不上");

        // 前提：reqwest 原样保留 URL（否则这条测试就恒真了，证明不了什么）。
        assert!(
            error.to_string().contains(secret),
            "reqwest 本该把 URL 带进 Display，测试前提不成立：{error}"
        );

        let converted = AppError::from(error);
        let text = converted.to_string();
        assert!(!text.contains(secret), "token 不该进错误文案：{text}");
        assert!(!text.contains("10001"), "userid 不该进错误文案：{text}");
        assert!(
            !text.contains(&port.to_string()),
            "URL 整体都该被摘掉：{text}"
        );
        // 错误种类要留着——诊断「是连不上还是超时」全靠它。
        assert!(converted.is_transient(), "连接被拒属于瞬时故障：{text}");
    }
}
