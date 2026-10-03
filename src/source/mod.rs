//! 音源抽象层。
//!
//! # 为什么按「标准版 / 概念版」划分
//!
//! 酷狗的两个平台不是同一个皮肤，而是**两套独立的会员与鉴权体系**：
//!
//! - 平台由 KuGouMusicApi 服务端的 `platform` 环境变量决定（`lite` = 概念版），
//!   服务端据此切换 `appid` / `clientver`，因此**必须各跑一个服务实例**；
//! - 上游文档明确写了「不同版本的平台的 token 是不通用的」——标准版的登录态
//!   拿到概念版去用，会员不会被识别，会退化成试听片段；
//! - 设备标识 `dfid` 同样是平台相关的。
//!
//! 所以这里的「音源」= 「服务地址 + 登录态 + 设备标识」三元组，切换时整体替换。
//!
//! # 界面与业务怎么用它
//!
//! 业务层只认 [`SourceKind`]，不感知具体音源：搜索、取链、歌词、目录浏览、云端歌单
//! 全部通过 `SourceKind` 上的方法调用，由本文件末尾的**分派层**转发到对应实现。
//!
//! - 酷狗的两个平台走同一套接口语义（都是 KuGouMusicApi），差异全部收敛在
//!   `api_base` / `cookie` / `device_id` 上，因此共用 [`crate::api`] 里的实现；
//! - 网易云是**另一套服务**（NeteaseCloudMusicApi）：端点、参数名、以及「登录态
//!   归谁保管」都不一样，实现放在 [`netease`] 子模块，由分派层按 kind 选路；
//! - 汽水**根本不跑本地服务**，直接打公网，而且下发的音频是加密的（取链要把
//!   「下载 + 解密」做完）。它自带一个 HTTP 客户端与一套解密实现，见 [`sodam`]。
//!
//! # 曾经删掉的东西
//!
//! 早先有一个 `Source` 行为包装，转发 search / playlist_tracks / stream_url。
//! 那时只有酷狗两个平台，接口语义完全一致，那层包装只是无意义的转发、且从未被
//! 调用，已按死代码删除。现在确实需要按 kind 分派了，分派直接落在 [`SourceKind`]
//! 的方法上——不必再引入一个与之平行的 trait 对象。
//!

pub mod netease;
pub mod sodam;

use serde::{Deserialize, Serialize};

use crate::api::client::ApiClient;
use crate::api::cloud::UserInfo;
use crate::api::model::{Artist, Lyric, Playlist, RankBoard, Song};
use crate::error::Result;

/// 音源种类。
///
/// # 新增一个音源要动哪里
///
/// 1. 在这里加一个变体；
/// 2. 在下面 `label` / `default_api_base` / `capability` / `platform_env` 等能力方法里补分支；
/// 3. 在 [`SourceSet`] 里加一个配置字段（`profile` / `profile_mut` 也要跟着补）；
/// 4. 写一个实现模块，并在下面**分派层**的每个 `SourceKind` 方法里加分支指向它。
///    新增能力时别忘了一并声明到 [`Capability`]，界面据此决定要不要展示入口。
///
/// 业务层只通过 `SourceKind` 的方法调用，不感知具体音源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// 酷狗音乐标准版。默认 `http://127.0.0.1:3000`（服务端不设 `platform`）。
    #[default]
    Kugou,
    /// 酷狗概念版。默认 `http://127.0.0.1:3001`（服务端 `platform=lite`）。
    KugouConcept,
    /// 网易云音乐（NeteaseCloudMusicApi）。
    Netease,
    /// 汽水音乐（Soda Music）。**直连公网**，不需要本地接口服务。
    ///
    /// 与前三个的区别不只是「服务在哪儿」：它的音频流是 MP4/CENC 加密的，
    /// 取链要把「下载 + 解密 + 落盘」整件事做完才能播，因此不能边下边播。
    /// 详见 [`sodam`] 模块。
    Sodam,
}

/// 一个音源具备哪些能力。
///
/// 不是所有音源都能提供全部功能：例如第三方服务往往没有「云端歌单同步」对应的
/// 登录态，元数据也不完整。UI 据此决定要不要展示某个面板，而不是等调用失败了
/// 再报错——那样用户只会看到一个「不支持」的提示，却不知道为什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    /// 能否取播放直链。
    pub stream: bool,
    /// 能否取歌词。
    pub lyric: bool,
    /// 搜索结果里是否带封面地址。
    pub cover: bool,
    /// 是否支持扫码登录。
    pub login: bool,
    /// 登录态是否由**客户端**持有（需要存进配置）。
    ///
    /// 酷狗是这样：token 由客户端保存，每次请求带上。
    /// 网易云的 NeteaseCloudMusicApi 则是服务端自己管 cookie，客户端拿不到
    /// token——这时登录成功只意味着「服务端那边登上了」，不该去写 config.cookie。
    pub client_token: bool,
    /// 是否支持「歌单广场 / 榜单 / 歌手」这类目录浏览。
    pub catalog: bool,
    /// 是否支持云端歌单的读写（收藏、同步、增删改）。
    pub cloud: bool,
    /// 是否有会员信息接口（`/user/vip/detail` 那套）。
    ///
    /// 只有酷狗有：它的标准版与概念版是两套会员体系。网易云没有对应端点，
    /// 去请求只会拿到 404，所以调用方要先问这里。
    pub vip: bool,
}

impl SourceKind {
    pub const ALL: [SourceKind; 4] = [
        SourceKind::Kugou,
        SourceKind::KugouConcept,
        SourceKind::Netease,
        SourceKind::Sodam,
    ];

    /// 界面显示名。
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Kugou => "酷狗",
            SourceKind::KugouConcept => "酷狗概念版",
            SourceKind::Netease => "网易云",
            SourceKind::Sodam => "汽水音乐",
        }
    }

    /// 该音源具备的能力。
    pub fn capability(self) -> Capability {
        match self {
            // 酷狗两个平台共用 KuGouMusicApi，能力完全一致
            SourceKind::Kugou | SourceKind::KugouConcept => Capability {
                stream: true,
                lyric: true,
                cover: true,
                login: true,
                client_token: true,
                catalog: true,
                cloud: true,
                vip: true,
            },
            // 网易云：NeteaseCloudMusicApi 提供扫码登录与完整的云端歌单接口，
            // 读（列表 + 曲目）与写（加歌/删歌/建/删歌单）都已支持。
            // 目录类（歌单广场、歌手、排行榜）也走它自己那套端点。
            // 会员信息没有对应接口，`vip` 为假。
            SourceKind::Netease => Capability {
                stream: true,
                lyric: true,
                cover: true,
                login: true,
                client_token: false,
                catalog: true,
                cloud: true,
                vip: false,
            },
            // 汽水音乐：直连公网，不经本地接口服务。
            //
            // * `stream` / `lyric` / `cover`：搜索走免签名网关，这三项匿名可用。
            // * `login`：支持——扫码（内置 CDP 签名页）与手填 cookie 两条路都行。
            // * `client_token` 为**假**！汽水登录拿到的是服务端下发的**会话 cookie**
            //   （`sessionid` / `sessionid_ss`），不是 token+userid。界面按这个标志
            //   决定收尾路径：为真时会去找 token，于是登录成功也会被报成
            //   「未拿到 token」——酷狗的形态，硬套到汽水上必然失败。
            // * `catalog`：歌单广场与歌单浏览已接（走 `/luna/pc/`，需要签名服务）。
            //   **但「歌手」与「排行榜」没有对应接口**——libresoda 里只有按 id 查
            //   歌手详情/曲目，没有「热门歌手列表」；榜单也没有等价的端点。
            //   那两处保留明确报错（见 `singer`/`rank` 分支），不是漏接。
            // * `cloud` / `vip`：都已接（我的歌单、用户资料、会员状态）。
            SourceKind::Sodam => Capability {
                stream: true,
                lyric: true,
                cover: true,
                login: true,
                client_token: false,
                catalog: true,
                cloud: true,
                vip: true,
            },
        }
    }

    /// 该音源默认的服务地址。
    ///
    /// 酷狗两个平台各占一个端口：它们需要不同的 `platform` 环境变量，
    /// 而一个 Node 进程只能加载一份 `.env`。
    ///
    /// 汽水是**公网**地址（它不跑本地服务），留在这里是为了让「音源档案」
    /// 的结构对四个音源一致——`switch_source` 与界面展示都按同一套逻辑走，
    /// 不必为汽水开特例。
    pub fn default_api_base(self) -> &'static str {
        match self {
            SourceKind::Kugou => "http://127.0.0.1:3000",
            SourceKind::KugouConcept => "http://127.0.0.1:3001",
            SourceKind::Netease => "http://127.0.0.1:3002",
            SourceKind::Sodam => "https://api.qishui.com",
        }
    }

    /// 该音源是否支持**扫码**登录。
    ///
    /// 与 [`Capability::login`] 区分：后者只说「有没有登录态」。
    /// 汽水两者都为真。它的扫码走 libresoda 内置的 **CDP 签名页**（直控本机
    /// Chromium，不需要 Node），所以前提是机器上有 Chrome/Chromium/Edge；
    /// 没有的话那一步会给出明确报错，不影响搜索与播放。
    pub fn supports_qr_login(self) -> bool {
        match self {
            SourceKind::Kugou | SourceKind::KugouConcept | SourceKind::Netease => true,
            SourceKind::Sodam => true,
        }
    }

    /// 该音源是否直连公网（不经本机接口服务）。
    ///
    /// 影响两件事：`bootstrap` 不为它准备服务；界面不显示「接口地址」那类
    /// 暗示「本机有个服务在跑」的文案。
    pub fn is_remote(self) -> bool {
        matches!(self, SourceKind::Sodam)
    }

    /// 该音源服务端的 `platform` 取值，用于启动脚本与文档提示。
    pub fn platform_env(self) -> Option<&'static str> {
        match self {
            SourceKind::Kugou => None,
            SourceKind::KugouConcept => Some("lite"),
            SourceKind::Netease | SourceKind::Sodam => None,
        }
    }

    /// 该音源是否需要酷狗那套设备指纹（`dfid`）。
    ///
    /// 只有酷狗用得上：它把 dfid 拼进 cookie 一起发给上游做风控校验。
    /// 其它平台没有这个机制，客户端也就不该去请求。
    pub fn uses_device_fingerprint(self) -> bool {
        matches!(self, SourceKind::Kugou | SourceKind::KugouConcept)
    }

    /// 扫码要用哪个 App。登录提示里会念出这个名字。
    ///
    /// 不能写死「酷狗」：登录提示是在**选定音源之后**才显示的，
    /// 对着网易云的用户说「用酷狗 App 扫码」纯属误导。
    pub fn scan_app(self) -> &'static str {
        match self {
            SourceKind::Kugou | SourceKind::KugouConcept => "酷狗",
            SourceKind::Netease => "网易云音乐",
            // 汽水不走扫码（见 `Capability` 的说明），这个值只用于错误提示文案。
            SourceKind::Sodam => "汽水音乐",
        }
    }

    /// 对应的第三方服务项目，用于启动脚本与文档提示。
    pub fn service_name(self) -> &'static str {
        match self {
            SourceKind::Kugou | SourceKind::KugouConcept => "KuGouMusicApi",
            SourceKind::Netease => "NeteaseCloudMusicApi",
            // 没有服务：直连公网。
            SourceKind::Sodam => "汽水公网接口",
        }
    }
}

/// 一个音源的连接与身份信息。
///
/// **三个字段都是平台相关的，不能跨音源复用。**
// 刻意不 derive Default：见下面的手写实现（默认必须是「已启用」）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceProfile {
    /// 该音源 API 服务的地址。
    pub api_base: String,
    /// 登录态，形如 `token=xxx; userid=xxx`。平台间不通用。
    pub cookie: Option<String>,
    /// 设备指纹 `dfid`，同样是平台相关的。
    pub device_id: Option<String>,
    /// 是否启用。禁用的音源不参与轮转，也不在音源管理页被优先展示。
    ///
    /// 默认 `true`：老配置文件里没有这个字段，不能因为升级就把音源关掉。
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 优先级，数字小的排前面。
    ///
    /// 只在音源管理页调整顺序时用到；取值相同则按 [`SourceKind::ALL`] 的声明顺序，
    /// 保证排序稳定、结果可预期。
    #[serde(default)]
    pub priority: u32,
}

fn default_enabled() -> bool {
    true
}

/// 手写 Default 而不是 derive：`enabled` 必须默认是 **true**。
///
/// 配置文件里缺某个音源的段时，serde 会走这里的 Default —— 若用 derive，
/// bool 会拿到 false，新加的音源一上来就是禁用的，用户得先手动启用才能用。
impl Default for SourceProfile {
    fn default() -> Self {
        Self {
            api_base: String::new(),
            cookie: None,
            device_id: None,
            enabled: true,
            priority: 0,
        }
    }
}

/// 组装最终发出去的 cookie 头：先规范化凭据，再按音源决定要不要补 `dfid`。
///
/// # 三件事，各自的理由
///
/// 1. **规范化**（[`crate::util::normalize_cookie_header`]）。网易云服务端下发的是
///    整段 `Set-Cookie`，直接回传会让它认不出里面的 `MUSIC_U`——详见那个函数的
///    说明。对酷狗那串是幂等的。
/// 2. **`dfid` 只补给它认的音源**（[`SourceKind::uses_device_fingerprint`]）。
///    `dfid` 是酷狗的设备指纹，它把 dfid 拼进 cookie 交给上游做风控校验；网易云
///    没有这个机制。以前这里无条件补，等于**把酷狗的 dfid 发给网易云**——既没用，
///    又让人分不清这串凭据到底属于谁。
/// 3. **配置里已经写了 `dfid` 就以它为准**，不再拼第二个：重复的键最终谁生效取决于
///    服务端的解析顺序，不如不给它这个机会。
pub fn cookie_header_for(
    kind: SourceKind,
    cookie: Option<&str>,
    device_id: Option<&str>,
) -> Option<String> {
    let base = crate::util::normalize_cookie_header(cookie.unwrap_or_default());

    if !kind.uses_device_fingerprint() {
        return base;
    }

    // 已经带了 dfid 就别重复拼
    if let Some(base) = base.as_deref()
        && base.split("; ").any(|pair| pair.starts_with("dfid="))
    {
        return Some(base.to_string());
    }

    let dfid = device_id.map(str::trim).filter(|dfid| !dfid.is_empty());

    match (base, dfid) {
        (Some(base), Some(dfid)) => Some(format!("{base}; dfid={dfid}")),
        (Some(base), None) => Some(base),
        // 只有 dfid 也能用：取播放直链靠它过风控，不带登录态照样能听
        (None, Some(dfid)) => Some(format!("dfid={dfid}")),
        (None, None) => None,
    }
}

impl SourceProfile {
    pub fn new(kind: SourceKind) -> Self {
        Self {
            api_base: kind.default_api_base().to_string(),
            cookie: None,
            device_id: None,
            enabled: true,
            priority: Self::default_priority(kind),
        }
    }

    /// 拼出可直接放进请求头的 cookie（必要时补上 dfid）。
    ///
    /// 与 `Config::cookie_header` 走的是同一条规则（见 [`cookie_header_for`]），
    /// 区别是这里读的是**本音源档案**里的凭据。跨音源播放时要用它——队列里的歌
    /// 可能来自另一个音源，拿当前音源的 cookie 去请求是错的。
    pub fn cookie_header(&self, kind: SourceKind) -> Option<String> {
        cookie_header_for(kind, self.cookie.as_deref(), self.device_id.as_deref())
    }

    /// 默认优先级：按声明顺序拉开间距，方便 UI 把某个音源插到中间。
    fn default_priority(kind: SourceKind) -> u32 {
        SourceKind::ALL
            .iter()
            .position(|candidate| *candidate == kind)
            .unwrap_or(0) as u32
            * 10
    }
}

/// 全部音源的配置，以及当前选中的那个。
///
/// 字段是具名的而不是 `Vec`：这样老的配置文件（只有酷狗两个音源）仍能正常解析，
/// 缺的音源走 `Default`，不会因为升级丢掉已有的登录态与地址。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSet {
    #[serde(default)]
    pub kugou: SourceProfile,
    #[serde(default)]
    pub kugou_concept: SourceProfile,
    #[serde(default)]
    pub netease: SourceProfile,
    /// 汽水音乐的连接与身份（公网地址 + 登录态 + 启停与优先级）。
    ///
    /// 结构与另外三个一致，这样 `profile()` / `ordered()` / `enabled()`
    /// 那些遍历全部音源的逻辑不必为它开特例。
    #[serde(default)]
    pub sodam: SourceProfile,
    /// 汽水的应用签名凭证（`x-helios` / `x-medusa` + 设备指纹）。
    ///
    /// 单独放一个具名字段而不是塞进 [`SourceProfile`]：这三个值不是「身份」，
    /// 而是「设备 + 签名」，且**只有汽水用得上**。塞进通用结构会让另外三个
    /// 音源的档案里也出现永远为空的字段。
    #[serde(default)]
    pub sodam_app: sodam::client::AppCredentials,
    /// 当前选中的音源。
    pub active: SourceKind,
}

impl Default for SourceSet {
    fn default() -> Self {
        Self {
            kugou: SourceProfile::new(SourceKind::Kugou),
            kugou_concept: SourceProfile::new(SourceKind::KugouConcept),
            netease: SourceProfile::new(SourceKind::Netease),
            sodam: SourceProfile::new(SourceKind::Sodam),
            sodam_app: sodam::client::AppCredentials::default(),
            active: SourceKind::Kugou,
        }
    }
}

impl SourceSet {
    pub fn profile(&self, kind: SourceKind) -> &SourceProfile {
        match kind {
            SourceKind::Kugou => &self.kugou,
            SourceKind::KugouConcept => &self.kugou_concept,
            SourceKind::Netease => &self.netease,
            SourceKind::Sodam => &self.sodam,
        }
    }

    pub fn profile_mut(&mut self, kind: SourceKind) -> &mut SourceProfile {
        match kind {
            SourceKind::Kugou => &mut self.kugou,
            SourceKind::KugouConcept => &mut self.kugou_concept,
            SourceKind::Netease => &mut self.netease,
            SourceKind::Sodam => &mut self.sodam,
        }
    }

    /// 按优先级排序后的音源列表（禁用的也在，界面自行决定如何展示）。
    pub fn ordered(&self) -> Vec<SourceKind> {
        let mut kinds = SourceKind::ALL.to_vec();
        kinds.sort_by_key(|kind| {
            (
                self.profile(*kind).priority,
                SourceKind::ALL
                    .iter()
                    .position(|candidate| candidate == kind)
                    .unwrap_or(0),
            )
        });
        kinds
    }

    /// 已启用的音源，按优先级排序。切换音源时只在它们之间轮转。
    pub fn enabled(&self) -> Vec<SourceKind> {
        self.ordered()
            .into_iter()
            .filter(|kind| self.profile(*kind).enabled)
            .collect()
    }
}

// ============================================================================
// 分派：业务层只调这里，不感知具体音源
//
// 新增音源 = 在本文件加枚举变体 + 在下面各方法加一个分支 + 写一个实现模块。
// ============================================================================

/// 取歌单曲目时的标识。
///
/// 酷狗把「自己的歌单」与「公开歌单」分成**两套端点**（前者按数字 `listid`，后者按
/// `global_collection_id`），所以调用方必须说清是哪一种。网易云没有这个区分——
/// 两者都是 `/playlist/track/all?id=`。
#[derive(Debug, Clone, Copy)]
pub enum PlaylistRef<'a> {
    /// 自己的歌单：自建、收藏的。酷狗按数字 `listid` 取。
    Own(i64),
    /// 公开歌单：歌单广场、榜单等。酷狗按 `global_collection_id` 取。
    Public(&'a str),
}

/// 给解析出来的歌曲盖上来源章。
///
/// 每个返回 `Vec<Song>` 的分派方法都要调它——队列允许跨音源，
/// 播放时必须知道每首歌该回哪个音源取链接。
fn stamp_songs(songs: &mut [Song], kind: SourceKind) {
    for song in songs {
        song.source = kind;
    }
}

/// 汽水解密中间文件的存放目录。
///
/// 放在**音频缓存目录下的独立子目录**，而不是系统临时目录，理由有二：
///
/// * 同盘：临时目录在某些发行版上是个 tmpfs（内存盘），一首无损几十 MB
///   写进去会直接吃内存；缓存目录是真实磁盘。
/// * 不干扰缓存统计：缓存容量统计会遍历根目录下的音频文件，
///   中间文件混进去会让「已用空间」对不上，也可能在回收时被误删。
///
/// 子目录名与音频缓存里的命名规则（内容哈希）不冲突，两边互不覆盖。
fn scratch_dir() -> std::path::PathBuf {
    crate::config::default_cache_dir().join("sodam")
}

impl SourceKind {
    /// 单曲搜索。
    pub async fn search_songs(
        self,
        client: &ApiClient,
        keyword: &str,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<Song>> {
        let mut songs = match self {
            Self::Kugou | Self::KugouConcept => client.search_songs(keyword, page, page_size).await,
            Self::Netease => netease::search_songs(client, keyword, page, page_size).await,
            Self::Sodam => sodam::search_songs(client, keyword, page, page_size).await,
        }?;
        stamp_songs(&mut songs, self);
        Ok(songs)
    }

    /// 取播放直链。
    pub async fn song_stream_url(
        self,
        client: &ApiClient,
        song: &Song,
        quality: &str,
    ) -> Result<crate::api::catalog::StreamUrl> {
        match self {
            Self::Kugou | Self::KugouConcept => client.song_stream_url(song, quality).await,
            Self::Netease => netease::song_stream_url(client, song, quality).await,
            Self::Sodam => {
                // 解密产物落在音频缓存目录下的专用子目录：它不是「可直接播放的
                // 缓存」（命名也不同），但同属可重建的派生物，放一起便于清理。
                let scratch = scratch_dir();
                sodam::song_stream_url(client, song, quality, &scratch).await
            }
        }
    }

    /// 取封面图片地址。不支持或取不到时返回 \`Ok(None)\`。
    ///
    /// 酷狗与 QQ 音乐的搜索结果里直接带封面 URL；网易云的搜索结果**只有
    /// \`picId\` 没有 URL**，得再查一次 \`/song/detail\` 才能拿到——实测确认。
    pub async fn cover_url(self, client: &ApiClient, song: &Song) -> Result<Option<String>> {
        match self {
            Self::Kugou | Self::KugouConcept => Ok(song.cover.clone()),
            Self::Netease => netease::cover_url(client, song).await,
            Self::Sodam => sodam::cover_url(client, song).await,
        }
    }

    /// 取歌词。
    pub async fn fetch_lyric(self, client: &ApiClient, song: &Song) -> Result<Lyric> {
        match self {
            Self::Kugou | Self::KugouConcept => client.fetch_lyric(song).await,
            Self::Netease => netease::fetch_lyric(client, song).await,
            Self::Sodam => sodam::fetch_lyric(client, song).await,
        }
    }

    /// 扫码登录第一步：创建二维码会话，返回会话键。
    pub async fn login_qr_key(self, client: &ApiClient) -> Result<String> {
        match self {
            Self::Kugou | Self::KugouConcept => client.login_qr_key().await,
            Self::Netease => netease::login_qr_key(client).await,
            // 汽水走 libresoda 内置的 CDP 签名页：直控本机 Chromium，
            // **不需要 Node**。这也解释了为什么它必须借道浏览器——护照接口要
            // `a_bogus`，而确认后的登录态只存在于那个浏览器会话的 cookie jar 里。
            Self::Sodam => sodam::create_qr_session(client).await,
        }
    }

    /// 扫码登录第二步：取二维码内容。
    pub async fn login_qr_create(self, client: &ApiClient, key: &str) -> Result<String> {
        match self {
            Self::Kugou | Self::KugouConcept => client.login_qr_create(key).await,
            Self::Netease => netease::login_qr_create(client, key).await,
            // 汽水的扫码地址在第 1 步就随二维码一起拿到了，这里只是取回它。
            Self::Sodam => sodam::scan_url_for(key),
        }
    }

    /// 扫码登录第三步：轮询结果。
    pub async fn login_qr_check(
        self,
        client: &ApiClient,
        key: &str,
    ) -> Result<crate::api::cloud::QrCheck> {
        match self {
            Self::Kugou | Self::KugouConcept => client.login_qr_check(key).await,
            Self::Netease => netease::login_qr_check(client, key).await,
            Self::Sodam => sodam::check_qr_session(client, key).await,
        }
    }

    // ---- 以下为酷狗专属能力 ----
    //
    // 其它音源直接报「不支持」，而不是返回空列表：第三方服务没有对应的登录态，
    // 返回空会让用户以为是网络问题或自己操作错了，明确说不支持反而省事。

    pub async fn plaza_playlists(
        self,
        client: &ApiClient,
        category_id: i64,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<Playlist>> {
        match self {
            Self::Kugou | Self::KugouConcept => {
                client.plaza_playlists(category_id, page, page_size).await
            }
            Self::Netease => netease::plaza_playlists(client, category_id, page, page_size).await,
            // 汽水没有分类广场（libresoda 的 `get_playlist_categories` 就是
            // Unsupported），只有一份推荐歌单，所以忽略分类与分页参数。
            Self::Sodam => sodam::plaza_playlists(client, category_id, page, page_size).await,
        }
    }

    /// 取歌单曲目的**一页**。
    ///
    /// 首屏用：先给一页让界面立刻有内容，剩下的交给 `*_tracks_all` 在后台补齐。
    ///
    /// ⚠️ 分页也必须走这层分派。首屏曾经图省事直接调 `ApiClient` 的酷狗分页方法
    /// （`user_playlist_tracks` / `playlist_tracks`，参数是 `page` + `pagesize`），
    /// 而那两个端点网易云服务根本没有——于是**网易云下打开歌单必然 404**；又因为
    /// 首屏失败会直接返回、走不到后台补全那段，用户看到的就是「歌单里的歌一直 404」。
    ///
    /// 顺带一提，首屏走分派之后，「解析时 `Song::source` 是默认值、需要调用方补盖
    /// 来源章」这个问题也一并消失了——盖章由本方法统一负责。
    pub async fn playlist_tracks_page(
        self,
        client: &ApiClient,
        playlist: PlaylistRef<'_>,
        page: u32,
        page_size: u32,
        fresh: bool,
    ) -> Result<Vec<Song>> {
        let mut songs = match (self, playlist) {
            (Self::Kugou | Self::KugouConcept, PlaylistRef::Own(list_id)) => {
                client
                    .user_playlist_tracks(list_id, page, page_size, fresh)
                    .await
            }
            (Self::Kugou | Self::KugouConcept, PlaylistRef::Public(global_id)) => {
                client
                    .playlist_tracks(global_id, page, page_size, fresh)
                    .await
            }
            // 网易云两种情况都是同一个端点，按 id 取
            (Self::Netease, PlaylistRef::Own(list_id)) => {
                netease::playlist_tracks_page(client, &list_id.to_string(), page, page_size).await
            }
            (Self::Netease, PlaylistRef::Public(global_id)) => {
                netease::playlist_tracks_page(client, global_id, page, page_size).await
            }
            // 汽水的歌单 id 就是数字串，自有与公开走同一个端点
            (Self::Sodam, PlaylistRef::Own(id)) => {
                sodam::playlist_tracks_page(client, &id.to_string(), "", page_size).await
            }
            (Self::Sodam, PlaylistRef::Public(id)) => {
                sodam::playlist_tracks_page(client, id, "", page_size).await
            }
        }?;
        stamp_songs(&mut songs, self);
        Ok(songs)
    }

    pub async fn playlist_tracks_all(
        self,
        client: &ApiClient,
        global_id: &str,
        fresh: bool,
    ) -> Result<Vec<Song>> {
        let mut songs = match self {
            Self::Kugou | Self::KugouConcept => client.playlist_tracks_all(global_id, fresh).await,
            Self::Netease => netease::playlist_tracks_all(client, global_id).await,
            Self::Sodam => sodam::playlist_tracks(client, global_id).await,
        }?;
        stamp_songs(&mut songs, self);
        Ok(songs)
    }

    pub async fn artist_list(
        self,
        client: &ApiClient,
        kind: i64,
        hot_size: u32,
    ) -> Result<Vec<Artist>> {
        match self {
            Self::Kugou | Self::KugouConcept => client.artist_list(kind, hot_size).await,
            Self::Netease => netease::artist_list(client, kind, hot_size).await,
            Self::Sodam => Err(sodam::unsupported(
                "歌手列表（汽水没有「热门歌手」接口，只能按 id 查）",
            )),
        }
    }

    pub async fn artist_tracks_all(
        self,
        client: &ApiClient,
        artist_id: i64,
        sort: &str,
    ) -> Result<Vec<Song>> {
        let mut songs = match self {
            Self::Kugou | Self::KugouConcept => client.artist_tracks_all(artist_id, sort).await,
            Self::Netease => netease::artist_tracks_all(client, artist_id).await,
            Self::Sodam => Err(sodam::unsupported(
                "歌手列表（汽水没有「热门歌手」接口，只能按 id 查）",
            )),
        }?;
        stamp_songs(&mut songs, self);
        Ok(songs)
    }

    pub async fn rank_boards(self, client: &ApiClient) -> Result<Vec<RankBoard>> {
        match self {
            Self::Kugou | Self::KugouConcept => client.rank_boards().await,
            Self::Netease => netease::rank_boards(client).await,
            Self::Sodam => Err(sodam::unsupported("排行榜（汽水没有等价的榜单端点）")),
        }
    }

    pub async fn rank_tracks_all(self, client: &ApiClient, rank_id: i64) -> Result<Vec<Song>> {
        let mut songs = match self {
            Self::Kugou | Self::KugouConcept => client.rank_tracks_all(rank_id).await,
            Self::Netease => netease::rank_tracks_all(client, rank_id).await,
            Self::Sodam => Err(sodam::unsupported("排行榜（汽水没有等价的榜单端点）")),
        }?;
        stamp_songs(&mut songs, self);
        Ok(songs)
    }

    pub async fn user_playlists(self, client: &ApiClient) -> Result<Vec<Playlist>> {
        match self {
            Self::Kugou | Self::KugouConcept => client.user_playlists().await,
            Self::Netease => netease::user_playlists(client).await,
            Self::Sodam => sodam::user_playlists(client).await,
        }
    }

    /// 取当前登录用户的资料。
    ///
    /// 两个音源的字段完全不同（酷狗在顶层给 `nickname`/`pic`，网易云把它们埋在
    /// `profile` 里，而且必须先有 uid），各自解析后收敛到同一个 [`UserInfo`]。
    pub async fn user_detail(self, client: &ApiClient) -> Result<UserInfo> {
        match self {
            Self::Kugou | Self::KugouConcept => client.user_detail().await,
            Self::Netease => netease::user_detail(client).await,
            Self::Sodam => sodam::user_detail(client).await,
        }
    }

    pub async fn user_playlist_tracks_all(
        self,
        client: &ApiClient,
        list_id: i64,
        fresh: bool,
    ) -> Result<Vec<Song>> {
        let mut songs = match self {
            Self::Kugou | Self::KugouConcept => {
                client.user_playlist_tracks_all(list_id, fresh).await
            }
            // 网易云没有「自己的歌单」专用端点，按 id 取即可
            Self::Netease => netease::playlist_tracks_all(client, &list_id.to_string()).await,
            Self::Sodam => sodam::playlist_tracks(client, &list_id.to_string()).await,
        }?;
        stamp_songs(&mut songs, self);
        Ok(songs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 跨音源队列的核心不变式：分派层必须给每首歌盖上来源章。
    ///
    /// 没有这个章，播放时就只能拿「当前音源」去取链接——而队列是允许跨音源的
    /// （酷狗搜几首入队 → 切到网易云），那些酷狗的歌会因为 hash 在网易云
    /// 的接口里查不到而全部播不了。
    #[test]
    fn stamp_marks_every_song_with_its_source() {
        let mut songs = vec![Song::default(), Song::default()];
        stamp_songs(&mut songs, SourceKind::Netease);
        assert!(
            songs.iter().all(|song| song.source == SourceKind::Netease),
            "每首歌都要带上来源音源"
        );
    }

    /// 盖章要覆盖解析时填的初始值：酷狗标准版与概念版共用同一套解析，
    /// 解析函数里填的是 `Kugou`，概念版必须被改写成 `KugouConcept`，
    /// 否则取链接会打到标准版的端口上。
    #[test]
    fn stamp_overrides_parse_time_default() {
        let mut songs = vec![Song::default()];
        assert_eq!(songs[0].source, SourceKind::Kugou, "默认是酷狗");
        stamp_songs(&mut songs, SourceKind::KugouConcept);
        assert_eq!(songs[0].source, SourceKind::KugouConcept, "应被改写");
    }

    /// 跨音源取链接要用**目标音源档案**里的凭据，不能拿当前音源的。
    #[test]
    fn profile_cookie_header_uses_own_credentials() {
        let mut profile = SourceProfile::new(SourceKind::Kugou);
        assert_eq!(
            profile.cookie_header(SourceKind::Kugou),
            None,
            "没有凭据时不给 cookie"
        );

        profile.cookie = Some("token=abc; userid=1".to_string());
        profile.device_id = Some("df-1".to_string());
        assert_eq!(
            profile.cookie_header(SourceKind::Kugou).as_deref(),
            Some("token=abc; userid=1; dfid=df-1"),
            "应把 dfid 拼进去"
        );

        // 已经带了 dfid 就不要重复拼
        profile.cookie = Some("token=abc; dfid=own".to_string());
        assert_eq!(
            profile.cookie_header(SourceKind::Kugou).as_deref(),
            Some("token=abc; dfid=own")
        );
    }

    /// `dfid` 是酷狗的设备指纹，只该发给酷狗。
    ///
    /// 以前这里无条件拼，于是网易云的请求会带上**酷狗的** dfid——那是另一个平台
    /// 的设备标识，既没用，又让「这串凭据是谁的」变得没法判断。
    #[test]
    fn netease_never_carries_the_kugou_device_fingerprint() {
        let mut profile = SourceProfile::new(SourceKind::Netease);
        profile.cookie = Some("MUSIC_U=abc".to_string());
        // 配置文件里可能残留着早年写进去的 dfid，也得被挡住
        profile.device_id = Some("df-from-kugou".to_string());

        assert_eq!(
            profile.cookie_header(SourceKind::Netease).as_deref(),
            Some("MUSIC_U=abc"),
            "网易云不该带 dfid"
        );

        // 只有 dfid、没有登录态时，网易云就是没有凭据
        profile.cookie = None;
        assert_eq!(profile.cookie_header(SourceKind::Netease), None);
    }

    /// 存下来的凭据可能是服务端下发的整段 `Set-Cookie`，组装时必须先规范化。
    ///
    /// 这条守的是「配置里已经有坏数据」的情况：老配置里存着未规范化的 cookie，
    /// 用户不该为了修它重新扫一次码。
    #[test]
    fn cookie_header_normalizes_stored_credentials() {
        let mut profile = SourceProfile::new(SourceKind::Netease);
        profile.cookie = Some(
            "MUSIC_A_T=1; Max-Age=2147483647; Path=/openapi/clientlog;;MUSIC_U=abc".to_string(),
        );

        assert_eq!(
            profile.cookie_header(SourceKind::Netease).as_deref(),
            Some("MUSIC_A_T=1; MUSIC_U=abc"),
            "属性段与 `;;` 都要被清掉"
        );
    }

    // ==================================================================
    // 汽水音乐
    // ==================================================================

    /// `SourceKind::ALL` 是音源管理页、切换轮转、`normalize` 补默认值的共同依据。
    /// 少登记一个变体，那几处就会静默地漏掉它——用户看不到、也报错不出来。
    #[test]
    fn all_lists_every_source_kind_exactly_once() {
        let all = SourceKind::ALL;
        assert_eq!(all.len(), 4, "四个音源都要登记");
        for kind in SourceKind::ALL {
            let count = all.iter().filter(|candidate| **candidate == kind).count();
            assert_eq!(count, 1, "{kind:?} 重复登记了");
        }
    }

    /// 汽水直连公网：不能被 `bootstrap` 当成本机服务去拉起，
    /// 界面上也不该显示成「本机接口地址」。
    #[test]
    fn sodam_is_remote_and_untouched_by_bootstrap() {
        assert!(SourceKind::Sodam.is_remote());
        for kind in [
            SourceKind::Kugou,
            SourceKind::KugouConcept,
            SourceKind::Netease,
        ] {
            assert!(!kind.is_remote(), "{kind:?} 是本机服务，不该标成 remote");
        }
        // manages_service 为假时 bootstrap 才会跳过（见 bootstrap.rs）
        assert!(!crate::bootstrap::manages_service(SourceKind::Sodam));
        assert!(crate::bootstrap::manages_service(SourceKind::Kugou));
    }

    /// 汽水的凭据与 dfid 不能和酷狗串台。
    #[test]
    fn sodam_never_carries_the_kugou_device_fingerprint() {
        let mut profile = SourceProfile::new(SourceKind::Sodam);
        profile.cookie = Some("sessionid_ss=abc".to_string());
        profile.device_id = Some("2204957404565290".to_string());

        // 汽水的 device_id 是它自己的设备标识，不是酷狗的 dfid，
        // 因此**不能**被拼进 cookie（服务端不认，反而可能被判异常）
        assert_eq!(
            profile.cookie_header(SourceKind::Sodam).as_deref(),
            Some("sessionid_ss=abc")
        );
    }

    /// 汽水的档案要与另外三个一样参与排序与启停——否则它在音源管理页里
    /// 会消失，或者优先级调整对它无效。
    #[test]
    fn sodam_profile_participates_in_ordering() {
        let mut set = SourceSet::default();
        set.sodam.enabled = false;

        let ordered = set.ordered();
        assert!(ordered.contains(&SourceKind::Sodam), "排序结果要含汽水");
        assert!(
            !set.enabled().contains(&SourceKind::Sodam),
            "禁用的汽水不该出现在已启用列表里"
        );

        set.sodam.enabled = true;
        assert!(set.enabled().contains(&SourceKind::Sodam));
    }

    /// 登录选择器按 `supports_qr_login` 过滤：**支持扫码的音源都要在列表里**。
    ///
    /// 回归：汽水早先被排除（当时判断它只能手填 cookie）。后来确认它的扫码
    /// 可以借道签名页服务，于是重新放进来——用户按 L 却看不到汽水，就是这个
    /// 过滤条件写死造成的。
    #[test]
    fn qr_login_candidates_include_every_source_that_supports_it() {
        let candidates: Vec<SourceKind> = SourceKind::ALL
            .iter()
            .copied()
            .filter(|kind| kind.capability().login && kind.supports_qr_login())
            .collect();

        for kind in [SourceKind::Kugou, SourceKind::Netease, SourceKind::Sodam] {
            assert!(candidates.contains(&kind), "{kind:?} 应该在扫码列表里");
        }
    }

    /// `supports_qr_login` 为真必须蕴含 `login` 为真——否则选择器会推出一个
    /// 没有登录态的音源，扫码成功也无处安放。
    #[test]
    fn qr_login_implies_login_support() {
        for kind in SourceKind::ALL {
            if kind.supports_qr_login() {
                assert!(kind.capability().login, "{kind:?} 支持扫码却不支持登录");
            }
        }
    }

    /// 汽水的登录态是**服务端下发的会话 cookie**，不是 token+userid。
    ///
    /// `client_token` 必须为假：界面据此决定收尾路径，为真时会去找 token，
    /// 于是登录成功也被报成「未拿到 token」。
    #[test]
    fn sodam_login_uses_a_server_cookie_not_a_client_token() {
        let capability = SourceKind::Sodam.capability();
        assert!(capability.login, "汽水支持登录");
        assert!(
            !capability.client_token,
            "汽水的凭据是 cookie，不能按「客户端持有 token」处理"
        );
    }

    /// 汽水的目录/云端/会员能力现在都已接上。
    ///
    /// 只**剩「歌手列表」与「排行榜」确实没有对应接口**——libresoda 只提供按
    /// id 查歌手详情与曲目，没有「热门歌手列表」；榜单也没有等价端点。那两处
    /// 保留明确报错（见分派层），不是漏接。
    ///
    /// 这条钉的是「声明与实现一致」：改动能力时要连它一起改。
    #[test]
    fn sodam_declares_the_capabilities_it_actually_has() {
        let capability = SourceKind::Sodam.capability();
        assert!(capability.catalog, "歌单广场与歌单浏览已接");
        assert!(capability.cloud, "云端歌单（我的歌单）已接");
        assert!(capability.vip, "会员状态已接");
        // 这几项匿名可用，必须为真，否则搜索进来也是白搭
        assert!(capability.stream);
        assert!(capability.lyric);
        assert!(capability.cover);
        assert!(capability.login);
        // 汽水的登录态是服务端下发的 cookie，不是客户端持有的 token
        assert!(!capability.client_token);
    }

    /// 老配置文件里没有汽水的段，反序列化后仍要拿到可用的默认值。
    #[test]
    fn sodam_defaults_survive_a_config_without_it() {
        // 模拟老配置：只有酷狗两个音源 + netease，没有 sodam / sodam_app
        let text = r#"
            active = "kugou"

            [sources.kugou]
            api_base = "http://127.0.0.1:3000"

            [sources.kugou_concept]
            api_base = "http://127.0.0.1:3001"

            [sources.netease]
            api_base = "http://127.0.0.1:3002"
        "#;

        let set: SourceSet = toml::from_str(text).expect("老配置应能解析");
        assert_eq!(set.sodam.api_base, "", "缺失的段走 Default");
        assert!(set.sodam.enabled, "默认必须是启用的，否则新音源要手动开");
        assert_eq!(set.sodam_app, sodam::client::AppCredentials::default());

        // normalize 会把空地址补成公网地址（见 config.rs 的 normalize）
        assert_eq!(
            SourceKind::Sodam.default_api_base(),
            "https://api.qishui.com"
        );
    }
}
