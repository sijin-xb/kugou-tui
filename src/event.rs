//! 全进程唯一的事件总线。
//!
//! # 为什么只留一条通道
//!
//! 参与方有三个：键盘输入线程、音频线程、Tokio 网络任务。如果各自维护一条通道，
//! 主循环就要做多路复用（`select!`），而 Tokio 的 `mpsc` 与 crossbeam 的
//! `select!` 无法直接混用，代码会迅速变脏。
//!
//! 所以统一成一条 `crossbeam_channel::unbounded::<Event>()`：
//!
//! * `unbounded` —— 发送端永不阻塞，因此可以从异步任务里同步调用，不需要 `await`；
//! * 主循环 `recv_timeout(tick)` —— 有事件立刻醒（按键零延迟），无事件就按 tick 刷新。
//!
//! 队列长度由「用户按键速率 + 音频 4Hz 位置上报 + 网络任务完成」决定，天然有界。

use std::path::PathBuf;

use crossbeam_channel::{Receiver, Sender, unbounded};
use ratatui::crossterm::event::{KeyEvent, MouseEvent};

use crate::api::model::{Artist, Lyric, Playlist, RankBoard, Song};
use crate::audio::engine::AudioEvent;
use crate::audio::streaming::StreamingBuffer;
use crate::error::AppError;

/// 歌单歌曲请求的发起方。
///
/// 歌单广场与云端歌单共用同一条请求路径，但结果要落到各自的歌曲面板。
/// 用枚举记录发起方、而不是在结果回来时读「当前标签页」，是因为请求是异步的——
/// 用户完全可能在结果回来之前切走标签页，那样结果就会写错面板。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistSource {
    /// 歌单广场。
    Plaza,
    /// 云端（个人）歌单。
    Cloud,
}

/// 一次异步载入失败时，该把哪一处「载入中」标记收掉。
///
/// 存在的理由：`loading` 原本只在成功路径（`replace()`）里清零，于是任何一次
/// 请求失败都会让面板**永远停在「载入中…」**——状态栏报着错，面板里还在转圈，
/// 用户既不知道是失败了、也不知道该按什么。失败路径必须能指名道姓地关掉它。
///
/// 用枚举而不是「失败就清掉全部」：同时有两个请求在飞时，清全部会把另一个
/// 仍在进行中的列表也标成已结束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadingTarget {
    /// 搜索页的结果列表。
    SearchResults,
    /// 歌单广场的条目列表。
    Playlists,
    /// 歌手条目列表。
    Artists,
    /// 排行榜条目列表。
    Ranks,
    /// 云端歌单的条目列表。
    CloudPlaylists,
    /// 某一侧（广场 / 云端）的歌曲列表。
    PlaylistSongs(PlaylistSource),
    /// 歌手页的歌曲列表。
    ArtistSongs,
    /// 排行榜的歌曲列表。
    RankSongs,
    /// 当前登录用户的资料（首页「我的资料」）。
    UserInfo,
}

/// 同步当日「概念版」VIP 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VipClaimOutcome {
    /// 这次领到了（领一天 + 升级两步都过了）。
    Claimed,
    /// 服务端说今天已经领过了。
    ///
    /// **单独分一档是必要的**：领取接口对这种情况只回一个 `error_code`、不给描述，
    /// 混进 `Failed` 就会显示成「领取失败 · 请到手机端领取」——而事实恰恰相反，
    /// 手机上领过了才是原因。
    AlreadyClaimed,
    /// 真的失败了，附带可读原因。
    Failed(String),
}

/// 一次异步任务的产出。
///
/// 每个变体都自带「这次请求是针对什么」的上下文（关键词、歌单、歌手、歌曲 hash），
/// 这样即使用户在结果返回前已经切歌或切换视图，主循环也能判断该不该消费这批数据。
#[derive(Debug)]
pub enum Loaded {
    Search {
        keyword: String,
        songs: Vec<Song>,
        /// true 表示追加到现有结果后面（「加载更多」），false 表示替换。
        append: bool,
    },
    /// 歌单广场 / 搜索结果里的歌单列表。
    Playlists {
        title: String,
        items: Vec<Playlist>,
    },
    PlaylistTracks {
        playlist: Playlist,
        songs: Vec<Song>,
        source: PlaylistSource,
    },
    Artists(Vec<Artist>),
    ArtistSongs {
        artist: Artist,
        songs: Vec<Song>,
    },
    RankBoards(Vec<RankBoard>),
    RankTracks {
        board: RankBoard,
        songs: Vec<Song>,
    },
    /// 当前登录用户的云端歌单。
    CloudPlaylists(Vec<Playlist>),
    Lyric {
        hash: String,
        lyric: Lyric,
    },
    /// 歌词取失败。
    ///
    /// 单独一个变体，而不是复用 [`Self::Lyric`] 塞一份空歌词：那样面板会显示
    /// 「暂无歌词」——**把「没取到」说成「这首歌本来就没有」**，正是这个项目里
    /// 反复出现的那类谎（「已连接」「已暂停」「载入中…」都是同一族）。
    ///
    /// 也不走 [`Self::Failed`]：那条路会往状态栏写一条错误，而歌词失败不影响
    /// 播放，每切一首歌闪一条错误太吵。面板里说清楚就够了。
    LyricFailed {
        hash: String,
        reason: String,
    },
    /// 已经拿到播放直链。
    StreamReady {
        song: Box<Song>,
        url: String,
        start_at_ms: u64,
        /// 是否为试听片段。为真时播完不应被当作「正常结束」而自动切歌。
        is_trial: bool,
        /// 完整版拿不到的原因（如「需要开通会员或单独购买该专辑」）。
        reason: Option<String>,
    },
    /// 下载进度。已按 256 KiB 节流，不会淹没事件通道。
    DownloadProgress {
        received: u64,
        total: Option<u64>,
    },
    /// 音频已落盘，可以交给音频线程播放。
    ///
    /// 只有**续播**那条路会发它：要跳到中间时不能边下边播（缓冲里只有开头那点
    /// 数据），所以先整首下完，再从这个位置开始放。
    StreamCached {
        song: Box<Song>,
        path: PathBuf,
        start_at_ms: u64,
    },
    /// 边下边播的那首**已经下完**了（内存缓冲用不上了，数据都在盘上）。
    ///
    /// 它和 [`Self::StreamCached`] 必须分开：那条路要 `audio.load()` 才会出声，
    /// 而这条路上歌**已经在放了**——再 load 一次等于把播放位置冲回 0，
    /// 用户听到的就是「放着放着突然从头开始」。所以它只做收尾（进度、预取、
    /// 缓存回收），绝不碰播放器。
    StreamCompleted {
        song: Box<Song>,
        path: PathBuf,
    },
    /// 自动探测到的设备指纹，需要回写配置。
    DeviceFingerprint(String),
    /// 音频缓存已占用字节数。
    CacheUsage(u64),
    /// 登录二维码已就绪：`content` 是二维码内容（一段 URL）。
    LoginQr {
        key: String,
        content: String,
    },
    /// 扫码状态提示（等待扫码 / 待确认 / 已过期）。
    LoginStatus {
        message: String,
    },
    /// 扫码成功，带回登录令牌。
    /// 扫码登录成功。`token` 为 `None` 表示登录态由服务端持有（如网易云），
    /// 客户端不需要也不应该保存凭据。
    LoginSucceeded {
        token: Option<String>,
        userid: Option<String>,
        /// 服务端下发的登录 cookie（网易云走这条路，见 `QrCheck::cookie`）。
        /// 有它就写进配置并在本次会话热更新，之后的请求才带得上身份。
        cookie: Option<String>,
    },
    /// 登录失败。
    LoginFailed {
        message: String,
    },
    /// 封面已解码。`hash` 用于丢弃过期结果（用户已切歌）。
    CoverReady {
        hash: String,
        /// 解码后的原图。图片协议要按显示区域重新编码，所以传解码结果而不是
        /// 原始字节——省掉主线程再解一次。
        image: image::DynamicImage,
    },
    /// 当前账号的会员信息。
    ///
    /// 传结构体而不是拼好的字符串：侧边栏窄、首页宽，两处要的形态不同
    /// （见 `VipInfo::label` / `VipInfo::short_label`），在这里就定型的话
    /// 渲染层没法按自己的宽度挑。
    VipStatus(Box<crate::api::cloud::VipInfo>),
    /// 同步「概念版」当天 VIP 的结果。
    VipClaimed {
        day: String,
        outcome: VipClaimOutcome,
        /// 是不是用户手动触发的（按 `V` 或点那一行）。
        ///
        /// 自动触发时「今天已经领过」不该弹提示——那会每次启动都刷一条没用的
        /// 状态栏消息，而面板上本来就写着「今日 VIP 已领取」。
        manual: bool,
    },
    /// 当前登录用户的资料（昵称 / 头像 / 等级 / 听歌时长）。
    ///
    /// 装箱：`UserInfo` 里几个 `String` 让它比别的变体大一圈，而 `Loaded`
    /// 是每帧都要搬运的枚举。
    UserInfo(Box<crate::api::cloud::UserInfo>),
    /// 头像已下载并解码。协议要回到主线程才能建（\`Picker\` 不是 Send）。
    AvatarReady {
        image: image::DynamicImage,
    },
    /// 流式缓冲已经攒够开头，可以开播了（边下边播）。
    StreamPrerolled {
        song: Box<Song>,
        buffer: StreamingBuffer,
        start_at_ms: u64,
    },
    /// 云端写操作（加歌/删歌）的提示信息。
    CloudNotice(String),
    /// 云端歌单的内容变了（加歌 / 删歌成功），需要重新拉取。
    ///
    /// 少了这一步的表现是：提示「已收藏」，但歌单里看不到这首歌、歌曲数也不变
    /// ——用户只能手动按 `R` 刷新。收到它时重新载入该歌单的歌曲，并刷新歌单列表。
    CloudPlaylistChanged {
        playlist: Box<crate::api::model::Playlist>,
    },
    /// 异步任务失败。
    Failed {
        context: String,
        error: AppError,
        /// 这次失败该收掉哪一处「载入中」。不是载入类请求（下载、写云端…）
        /// 就是 `None`。
        target: Option<LoadingTarget>,
    },
}

impl Loaded {
    /// 这个事件是不是「KuGouMusicApi 服务回了一次话」。
    ///
    /// 只用来判断连通性。**必须把本地产生的事件排除在外**：缓存占用、下载进度、
    /// 封面解码这些都是本机算出来的，接口挂着也照样会到；把它们算成「服务回应」
    /// 的话，侧边栏会在服务已经死掉时显示「已连通」——那就又变成了一个没验证过
    /// 的断言，正是要修掉的那个毛病。
    ///
    /// 反过来，业务错误码（需要登录、页码越界…）算**是**回应：服务回了话，
    /// 只是拒绝了这次请求。
    ///
    /// 新增变体时要想一下它从哪来：`bus.emit(...)` 在请求成功后发的是，
    /// 在主线程本地算完发的不算。
    pub fn is_api_response(&self) -> bool {
        matches!(
            self,
            Self::Search { .. }
                | Self::Playlists { .. }
                | Self::PlaylistTracks { .. }
                | Self::Artists(_)
                | Self::ArtistSongs { .. }
                | Self::RankBoards(_)
                | Self::RankTracks { .. }
                | Self::CloudPlaylists(_)
                | Self::Lyric { .. }
                | Self::StreamReady { .. }
                | Self::LoginQr { .. }
                | Self::LoginStatus { .. }
                | Self::LoginSucceeded { .. }
                | Self::LoginFailed { .. }
                | Self::VipStatus(_)
                | Self::VipClaimed { .. }
                | Self::UserInfo(_)
                | Self::DeviceFingerprint(_)
        )
    }
}

#[derive(Debug)]
pub enum Event {
    /// 原始按键。
    ///
    /// 刻意不在输入线程里翻译成语义动作：按键的含义取决于当前是否在输入框里，
    /// 只有主循环知道这个状态。让输入线程保持「哑」的，也避免了共享可变状态。
    Key(KeyEvent),
    /// 终端尺寸变化。
    ///
    /// 不携带尺寸：布局每帧都从 `Frame::area()` 重算，这个事件的作用只是把主循环
    /// 从 `recv_timeout` 里立刻唤醒，让用户拖动窗口时画面马上跟上。
    Resize,
    /// 鼠标事件（点击 / 滚轮 / 拖动）。
    Mouse(MouseEvent),
    /// 音频线程上报。
    Audio(AudioEvent),
    /// 网络任务完成。
    Loaded(Box<Loaded>),
    /// 定时心跳：推进进度条与歌词，没有事件时也会到达。
    Tick,
    /// 语义动作（来自 MPRIS 等外部控制源）。
    ///
    /// 与 [`Event::Key`] 分开：按键要经主循环按当前焦点翻译，而这里传进来的
    /// 已经是明确的语义动作（播放 / 下一首 …），直接执行即可。
    Action(crate::keymap::Action),
}

/// 事件总线的发送端。可以自由克隆，跨线程移动。
///
/// 接收端不放在这里——它只属于主循环。crossbeam 的通道在接收端全部丢弃后才
/// 断开，把接收端也塞进可克隆的总线里会让「断开」永远不发生。
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: Sender<Event>,
}

impl EventBus {
    /// 创建总线，返回发送端与唯一的接收端。
    pub fn new() -> (Self, Receiver<Event>) {
        let (sender, receiver) = unbounded();
        (Self { sender }, receiver)
    }

    /// 发送一个事件。接收端已关闭（进程正在退出）时静默忽略。
    pub fn send(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    /// 发送一次异步产出。
    pub fn emit(&self, loaded: Loaded) {
        self.send(Event::Loaded(Box::new(loaded)));
    }

    /// 上报一次异步失败，`context` 说明是哪个操作失败了。
    ///
    /// 用于不涉及「载入中」状态的失败（下载、云端写操作、登录…）。
    pub fn fail(&self, context: impl Into<String>, error: AppError) {
        self.fail_with_target(None, context, error);
    }

    /// 上报一次**载入类**请求的失败，顺带收掉对应的「载入中」标记。
    pub fn fail_loading(
        &self,
        target: LoadingTarget,
        context: impl Into<String>,
        error: AppError,
    ) {
        self.fail_with_target(Some(target), context, error);
    }

    fn fail_with_target(
        &self,
        target: Option<LoadingTarget>,
        context: impl Into<String>,
        error: AppError,
    ) {
        self.emit(Loaded::Failed {
            context: context.into(),
            error,
            target,
        });
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本地产生的事件不能被当成「服务回了话」。
    ///
    /// 这条断言就是为「侧边栏在接口全挂时显示『已连通』」那个 bug 立的：缓存占用、
    /// 下载进度都是本机算出来的，把它们算进去，接口挂着也照样显示已连通。
    #[test]
    fn locally_produced_events_are_not_api_responses() {
        let local = [
            Loaded::CacheUsage(1024),
            Loaded::DownloadProgress {
                received: 1,
                total: Some(2),
            },
            Loaded::CloudNotice("已收藏".to_string()),
            Loaded::CloudPlaylistChanged {
                playlist: Box::new(crate::api::model::Playlist::default()),
            },
        ];
        for loaded in &local {
            assert!(
                !loaded.is_api_response(),
                "{loaded:?} 是本机产生的，不该被算成服务回应"
            );
        }
    }

    /// 从服务拿到回应的都算——包括「业务上失败」的那种。
    #[test]
    fn responses_from_the_service_count_as_reachable() {
        let remote = [
            Loaded::Search {
                keyword: "海阔天空".to_string(),
                songs: Vec::new(),
                append: false,
            },
            Loaded::VipStatus(Box::default()),
            Loaded::DeviceFingerprint("dfid".to_string()),
            Loaded::LoginFailed {
                message: "二维码已过期".to_string(),
            },
        ];
        for loaded in &remote {
            assert!(
                loaded.is_api_response(),
                "{loaded:?} 是服务的回应，应算作已连通"
            );
        }
    }
}
