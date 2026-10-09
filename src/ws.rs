//! WebSocket 服务：把播放状态与歌词推给第三方客户端，并接受播放控制。
//!
//! 协议对齐 MoeKoeMusic（<https://music.moekoe.cn/websocket-api.html>），这样面向
//! 它的桌面歌词 / 状态栏客户端不用改一行就能连上来。**只在 `127.0.0.1` 上监听**，
//! 并且在握手阶段校验 `Origin`：浏览器里的任意网页都能连本机端口，不校验等于把
//! 播放控制开放给当时打开的每一个标签页。
//!
//! # 消息
//!
//! 统一信封 `{"type": ..., "data": ...}`，见 [`ServerMessage`] 与 [`ClientMessage`]。
//! 连接建立后依次发 welcome、当前歌词（有才发）、当前播放状态；此后每拍把变化广播
//! 出去。字段名与结构照文档，文档与上游源码不一致处**以源码实际行为为准**，差异在
//! 测试里逐条标注（例如 welcome 的 `data` 是纯字符串，不是文档写的嵌套对象）。
//!
//! # 控制
//!
//! 入站的 `control` 只映射到已有的播放 [`Action`]，不新增任何能读写文件、改配置、
//! 执行命令的入口。未知命令回一条 `error` 而不是像上游那样静默忽略——遥控端收不到
//! 反馈时只会怀疑自己没按对。
//!
//! # 与状态推送的关系
//!
//! 与 `mpris.rs` 同一套路：主循环每拍调 [`crate::app::desktop`] 里的 `sync_ws`
//! 写一份快照，这里负责比对与广播。`ws.rs` 自己**不碰播放、不碰网络业务**。

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::api::model::Song;
use crate::event::{Event, EventBus};
use crate::keymap::Action;

/// 单条入站消息的大小上限。
///
/// 客户端只会发几十字节的 `control` 命令，64 KiB 已经宽松到不可能误伤正常客户端，
/// 同时挡住「往内存里灌消息」这类把戏。出站不受此限：KRC 歌词本身可能几十 KB。
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// 广播通道容量。
///
/// 每个客户端一个接收端；消费不过来的客户端会收到 `Lagged`，我们直接跳过丢失的
/// 中间帧继续发最新的——播放状态本来就是「只关心最新」，积压旧帧没有意义。
const BROADCAST_CAPACITY: usize = 64;

/// 控制命令 → 已有动作。
///
/// 只认文档定义的三个。刻意不支持 `volume` / `seek` 之类：协议里没有，凭空扩展等于
/// 发明一套只有本程序认得的方言；要加也得先和上游对齐。
fn control_action(command: &str) -> Option<Action> {
    match command {
        "toggle" => Some(Action::PlayPause),
        "next" => Some(Action::Next),
        "prev" => Some(Action::Prev),
        _ => None,
    }
}

/// 只接受本机回环地址的连接。
///
/// 正常情况这个检查必然成立（监听套接字本身就只绑了 `127.0.0.1`）。显式写出来是
/// 为了将来有人把绑定地址改成 `0.0.0.0` 时不会**静默地**把播放控制面暴露出去——
/// 那时这行会拦住，而不是等到有人扫到端口才发现。
fn peer_allowed(peer: std::net::SocketAddr) -> bool {
    peer.ip().is_loopback()
}

/// `Origin` 头是否可接受。
///
/// 没有 `Origin` 说明不是浏览器发起的（原生客户端、`websocat` 都不带），放行；
/// 带了就必须是本机来源。`file://` 页面会发 `Origin: null`，按拒绝处理。
fn origin_allowed(request: &Request) -> bool {
    let Some(origin) = request.headers().get("origin") else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some((host, port)) = split_origin(origin) else {
        return false;
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host == "[::1]";
    if !loopback {
        return false;
    }
    // 端口是可选部分，给了就必须是合法端口号，免得 `localhost:80.evil` 这类糊弄过去。
    match port {
        None => true,
        Some(port) => !port.is_empty() && port.parse::<u16>().is_ok(),
    }
}

/// 从 `scheme://host[:port]` 里拆出主机与端口。
///
/// 不引入 URL 解析库：`Origin` 的语法被规范限定得很死，这里只需要够用且严格。
fn split_origin(origin: &str) -> Option<(&str, Option<&str>)> {
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        return Some((host, tail.strip_prefix(':')));
    }
    match authority.split_once(':') {
        Some((host, port)) => Some((host, Some(port))),
        None => Some((authority, None)),
    }
}

/// 一次状态快照。字段都是 `AppState` 里的原样值，不含任何业务判断。
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// 当前曲目。`None` 表示没在放歌。
    pub song: Option<Song>,
    /// 当前歌词原文（KRC / LRC）。`None` 表示这首歌没拿到歌词。
    pub lyric_text: Option<String>,
    pub is_playing: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
}

impl Snapshot {
    fn song_hash(&self) -> Option<&str> {
        self.song.as_ref().map(|song| song.hash.as_str())
    }

    /// 播放状态消息的数据体。时间统一换成**秒**（上游是 HTML audio 的 `currentTime`）。
    fn player_state(&self) -> PlayerStateData {
        PlayerStateData {
            is_playing: self.is_playing,
            current_time: self.position_ms as f64 / 1000.0,
        }
    }

    /// 歌词消息的数据体。缺歌或缺歌词时为 `None`（上游同样只在有歌词时推送）。
    fn lyrics_data(&self) -> Option<LyricsData> {
        Some(LyricsData {
            current_time: self.position_ms as f64 / 1000.0,
            lyrics_data: self.lyric_text.clone()?,
            current_song: self.song.clone(),
            duration: self.duration_ms as f64 / 1000.0,
        })
    }
}

/// 只比较「会改变推送内容」的字段。
///
/// 刻意不用 `Song` 的完整比较：曲目换了但 hash 相同时推送内容不会变，而逐字段比较
/// 每次都要把 `privilege` 这类嵌套 JSON 走一遍。[`WsHandle::update_from`] 在构造
/// 快照前就按这套字段比对，没变化时连克隆都不做。
impl PartialEq for Snapshot {
    fn eq(&self, other: &Self) -> bool {
        self.song_hash() == other.song_hash()
            && self.lyric_text == other.lyric_text
            && self.is_playing == other.is_playing
            && self.position_ms == other.position_ms
            && self.duration_ms == other.duration_ms
    }
}

/// `{"type":"lyrics","data":{...}}` 的数据体。
///
/// 不派生 `PartialEq`：`Song` 没有实现它（内含 `Value` 型字段），而这里也没有比较
/// 需求——要不要推送由 [`Snapshot`] 的比较决定，逐字段比一份 `Song` 反而是浪费。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsData {
    /// 秒。
    pub current_time: f64,
    /// 歌词原文。文档写 `string`，上游发的也是原始 KRC 文本。
    pub lyrics_data: String,
    /// 当前歌曲。没有时为 `null`（上游发 `null`）。
    pub current_song: Option<Song>,
    /// 秒。
    pub duration: f64,
}

/// `{"type":"playerState","data":{...}}` 的数据体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerStateData {
    pub is_playing: bool,
    /// 秒。
    pub current_time: f64,
}

/// `{"type":"error","data":{...}}` 的数据体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorData {
    pub code: i32,
    pub message: String,
}

/// 服务端发出的消息。
///
/// `tag` + `content` 直接产出 `{"type": ..., "data": ...}` 的信封；`rename_all`
/// 让 `PlayerState` 变成 `playerState`。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
enum ServerMessage {
    /// `data` 是**纯字符串**。文档样例写的是嵌套对象，但上游源码发的是字符串，
    /// 按惜别的要求以源码为准，差异见本文件测试。
    Welcome(String),
    /// 装箱：`LyricsData` 里装着一整份 `Song`，不装箱会让枚举的每个实例都有
    /// 264 字节（其余三个变体只有几十字节），而它是按值在 `Vec` 里传递的。
    Lyrics(Box<LyricsData>),
    PlayerState(PlayerStateData),
    Error(ErrorData),
}

/// 客户端发来的消息。只认 `control`，其余类型直接反序列化失败 → 回一条 `error`。
#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
enum ClientMessage {
    Control(ControlData),
}

#[derive(Debug, Deserialize)]
struct ControlData {
    command: String,
}

/// 服务端句柄。与 `MprisHandle` 同形：主循环只往里写快照。
#[derive(Debug)]
pub struct WsHandle {
    changes: watch::Sender<Arc<Snapshot>>,
    running: Arc<AtomicBool>,
}

impl WsHandle {
    /// 用**引用**写入一份新快照：内容没变时不克隆任何东西。
    ///
    /// 直接构造 [`Snapshot`] 再判等，会先把整份 `Song` 与可能几十 KB 的歌词文本
    /// 克隆一遍，然后才发现根本没变——而主循环每拍（默认 200ms）都会调一次。
    /// 这里先用引用与当前快照比一遍，只有真的变了才克隆构造。
    ///
    /// 曲目比的是 [`Song::hash`] 这个**原始 id 字符串**（不是它的摘要），逐字节相等才
    /// 算没变，因此不存在哈希碰撞导致漏推的余地。
    pub fn update_from(
        &self,
        song: Option<&Song>,
        lyric_text: Option<&str>,
        is_playing: bool,
        position_ms: u64,
        duration_ms: u64,
    ) {
        let _ = self.changes.send_if_modified(|current| {
            if current.song_hash() == song.map(|song| song.hash.as_str())
                && current.lyric_text.as_deref() == lyric_text
                && current.is_playing == is_playing
                && current.position_ms == position_ms
                && current.duration_ms == duration_ms
            {
                return false;
            }
            *current = Arc::new(Snapshot {
                song: song.cloned(),
                lyric_text: lyric_text.map(str::to_string),
                is_playing,
                position_ms,
                duration_ms,
            });
            true
        });
    }

    /// 是否已在监听。
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

/// 启动 WebSocket 服务。绑定失败（端口被占等）时返回 `None`，不影响播放。
pub fn spawn(bus: EventBus, port: u16) -> Option<WsHandle> {
    // 同步绑定：端口被占这种事要在启动时就暴露出来，而不是扔进线程里悄悄失败。
    let listener = match std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        Ok(listener) => listener,
        Err(error) => {
            crate::logger::tlog!(
                crate::logger::LEVEL_WARN,
                "WebSocket 服务无法监听 127.0.0.1:{port}（不影响播放）：{error}"
            );
            return None;
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        crate::logger::tlog!(
            crate::logger::LEVEL_WARN,
            "WebSocket 服务设置非阻塞失败（不影响播放）：{error}"
        );
        return None;
    }
    let actual_port = listener
        .local_addr()
        .map(|address| address.port())
        .unwrap_or(port);

    let (changes, changes_rx) = watch::channel(Arc::new(Snapshot::default()));
    let (out, _) = broadcast::channel::<Arc<str>>(BROADCAST_CAPACITY);
    let running = Arc::new(AtomicBool::new(true));
    let running_thread = Arc::clone(&running);

    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            running_thread.store(false, Ordering::Relaxed);
            return;
        };
        runtime.block_on(async move {
            let listener = match TcpListener::from_std(listener) {
                Ok(listener) => listener,
                Err(error) => {
                    crate::logger::tlog!(
                        crate::logger::LEVEL_WARN,
                        "WebSocket 服务接管监听失败（不影响播放）：{error}"
                    );
                    running_thread.store(false, Ordering::Relaxed);
                    return;
                }
            };
            serve(listener, changes_rx, out, bus).await;
        });
    });

    crate::logger::tlog!(
        crate::logger::LEVEL_INFO,
        "WebSocket 服务已启动：ws://127.0.0.1:{actual_port}/"
    );
    Some(WsHandle { changes, running })
}

/// 接受连接。永不返回（除非进程退出）。
async fn serve(
    listener: TcpListener,
    changes: watch::Receiver<Arc<Snapshot>>,
    out: broadcast::Sender<Arc<str>>,
    bus: EventBus,
) {
    tokio::spawn(broadcast_loop(changes.clone(), out.clone()));

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                if !peer_allowed(peer) {
                    crate::logger::tlog!(
                        crate::logger::LEVEL_WARN,
                        "拒绝来自 {peer} 的 WebSocket 连接：只接受本机"
                    );
                    continue;
                }
                let changes = changes.clone();
                let out = out.clone();
                let bus = bus.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_client(stream, changes, out, bus).await {
                        crate::logger::tlog!(
                            crate::logger::LEVEL_DEBUG,
                            "WebSocket 客户端断开：{error}"
                        );
                    }
                });
            }
            Err(error) => {
                // 接受失败通常是 fd 耗尽之类的暂时性问题，退一步再试，别把线程打死。
                crate::logger::tlog!(crate::logger::LEVEL_WARN, "WebSocket 接受连接失败：{error}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// 比对快照并把变化广播出去。
async fn broadcast_loop(
    mut changes: watch::Receiver<Arc<Snapshot>>,
    out: broadcast::Sender<Arc<str>>,
) {
    let mut last = changes.borrow_and_update().clone();
    while changes.changed().await.is_ok() {
        let current = changes.borrow_and_update().clone();
        for message in diff_messages(&last, &current) {
            // 没有客户端时发送必然失败，不是错误。
            let _ = out.send(message);
        }
        last = current;
    }
}

/// 两份快照之间要推哪些消息。
///
/// 歌词带时间轴，所以播放中每拍都推（上游同样是 200ms 一拍）。播放状态只在
/// 「播放 / 暂停」真正翻转时推——上游 `updatePlayerState` 的唯一调用点是
/// `electron/main.js` 的 `play-pause-action` 处理函数，连接建立时的首次状态由
/// `handle_client` 单独补发，不在这个差分里。
fn diff_messages(previous: &Snapshot, current: &Snapshot) -> Vec<Arc<str>> {
    let mut messages = Vec::new();
    let lyrics_changed =
        previous.song_hash() != current.song_hash() || previous.lyric_text != current.lyric_text;
    let time_changed = previous.position_ms != current.position_ms;

    if let Some(data) = current.lyrics_data()
        && (lyrics_changed || time_changed)
    {
        messages.push(encode(&ServerMessage::Lyrics(Box::new(data))));
    }
    if previous.is_playing != current.is_playing {
        messages.push(encode(&ServerMessage::PlayerState(current.player_state())));
    }
    messages
}

/// 序列化一条消息。
///
/// 正常路径不可能失败（消息里没有 `NaN` 之类），真失败了也不能把连接整死：回一条
/// 通用的 `error`，让客户端知道是服务端出了问题。
fn encode(message: &ServerMessage) -> Arc<str> {
    match serde_json::to_string(message) {
        Ok(text) => Arc::from(text),
        Err(error) => {
            crate::logger::tlog!(
                crate::logger::LEVEL_WARN,
                "WebSocket 消息序列化失败：{error}"
            );
            Arc::from(r#"{"type":"error","data":{"code":500,"message":"消息序列化失败"}}"#)
        }
    }
}

/// 处理一个连接：先补发当前状态，然后读命令、收广播。
async fn handle_client(
    stream: TcpStream,
    mut changes: watch::Receiver<Arc<Snapshot>>,
    out: broadcast::Sender<Arc<str>>,
    bus: EventBus,
) -> Result<(), WsError> {
    // 入站消息大小上限。默认值是 64 MiB / 16 MiB，对「只发 control」的场景太宽了。
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));

    // `Err` 那一侧是 tungstenite 的 `ErrorResponse`（约 136 字节），它的形状由
    // `Callback` trait 定死，改不了。`allow` 只针对这一处闭包。
    #[allow(clippy::result_large_err)]
    let callback = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        if origin_allowed(request) {
            return Ok(response);
        }
        crate::logger::tlog!(
            crate::logger::LEVEL_WARN,
            "拒绝 WebSocket 握手：Origin 不是本机来源"
        );
        let mut deny = ErrorResponse::new(Some("Origin 不被允许".to_string()));
        *deny.status_mut() = StatusCode::FORBIDDEN;
        Err(deny)
    };

    let socket =
        tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(config)).await?;
    let (mut write, mut read) = socket.split();

    // 补发当前状态：welcome → 歌词（有才发）→ 播放状态，顺序与上游一致。
    let snapshot = changes.borrow_and_update().clone();
    let welcome = encode(&ServerMessage::Welcome(
        "感谢接入 kugou-tui，协议与 MoeKoeMusic 兼容：https://music.moekoe.cn/websocket-api.html"
            .to_string(),
    ));
    write.send(Message::text(welcome.to_string())).await?;
    if let Some(data) = snapshot.lyrics_data() {
        let message = encode(&ServerMessage::Lyrics(Box::new(data)));
        write.send(Message::text(message.to_string())).await?;
    }
    let state = encode(&ServerMessage::PlayerState(snapshot.player_state()));
    write.send(Message::text(state.to_string())).await?;

    let mut subscription = out.subscribe();

    loop {
        tokio::select! {
            incoming = read.next() => {
                let Some(incoming) = incoming else { break };
                match incoming {
                    Ok(Message::Text(text)) => {
                        if let Some(reply) = handle_command(text.as_str(), &bus) {
                            write.send(Message::text(reply.to_string())).await?;
                        }
                    }
                    Ok(Message::Binary(_)) => {
                        let reply = encode(&ServerMessage::Error(ErrorData {
                            code: 400,
                            message: "只接受文本消息".to_string(),
                        }));
                        write.send(Message::text(reply.to_string())).await?;
                    }
                    // 协议层的 ping 要回 pong，否则对端会按超时断开。
                    Ok(Message::Ping(payload)) => {
                        write.send(Message::Pong(payload)).await?;
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(error) => return Err(error),
                }
            }
            received = subscription.recv() => {
                match received {
                    Ok(message) => write.send(Message::text(message.to_string())).await?,
                    // 客户端读得太慢，中间帧被丢了。播放状态只关心最新，跳过即可。
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    Ok(())
}

/// 处理一条入站文本。返回 `Some` 表示要回一条 `error`，`None` 表示命令已被接受。
fn handle_command(text: &str, bus: &EventBus) -> Option<Arc<str>> {
    let error = |message: String| {
        Some(encode(&ServerMessage::Error(ErrorData {
            code: 400,
            message,
        })))
    };

    let message = match serde_json::from_str::<ClientMessage>(text) {
        Ok(message) => message,
        Err(cause) => return error(format!("无法解析的消息：{cause}")),
    };

    match message {
        ClientMessage::Control(data) => match control_action(&data.command) {
            Some(action) => {
                bus.send(Event::Action(action));
                None
            }
            // 未知命令**不**静默忽略（上游是忽略的）：遥控端需要知道命令没生效。
            None => error(format!(
                "未知的 control 命令：{}（只支持 toggle / next / prev）",
                data.command
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// 文档 <https://music.moekoe.cn/websocket-api.html> 里每种消息的 JSON 样例，
    /// **原样抄录**（含行尾注释）。夹具与上游文档一一对应，改动前先回去看文档。
    ///
    /// 统一信封：
    /// ```text
    /// {
    ///   "type": "string",    // 消息类型
    ///   "data": object,      // 消息数据
    /// }
    /// ```
    const DOC_WELCOME: &str = r#"{
  "type": "welcome",
  "data": {
        "type": "welcome",
        "data": "感谢接入MoeKoe Music，文档地址：https://music.moekoe.cn/"
    }
}"#;
    const DOC_LYRICS: &str = r#"{
  "type": "lyrics",
  "data": { 
        "currentTime": number, // 当前时间
        "lyricsData": string, // 歌词数据
        "currentSong": object, // 当前歌曲信息
        "duration": number // 总时长
    }
}"#;
    const DOC_PLAYER_STATE: &str = r#"{
  "type": "playerState",
  "data": {
        "isPlaying": boolean, // 是否正在播放
        "currentTime": number // 当前时间
    }
}"#;
    const DOC_CONTROL: &str = r#"{
  "type": "control",
  "data": {
    "command": "toggle|next|prev", // 控制命令，toggle 切换播放状态，next 下一首，prev 上一首
  }
}"#;
    const DOC_ERROR: &str = r#"{
  "type": "error",
  "data": {
    "code": number,     // 错误代码
    "message": string   // 错误描述
  }
}"#;

    /// 去掉 `//` 行尾注释，但不碰字符串字面量里的 `//`（欢迎语里的 URL 就有）。
    fn without_comments(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        let mut in_string = false;
        while let Some(ch) = chars.next() {
            if in_string {
                out.push(ch);
                match ch {
                    '\\' => {
                        if let Some(next) = chars.next() {
                            out.push(next);
                        }
                    }
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            if ch == '"' {
                in_string = true;
                out.push(ch);
                continue;
            }
            if ch == '/' && chars.peek() == Some(&'/') {
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    fn keys_of(value: &Value) -> Vec<String> {
        value
            .as_object()
            .expect("应当是对象")
            .keys()
            .cloned()
            .collect()
    }

    fn encode_to_value(message: &ServerMessage) -> Value {
        serde_json::to_value(message).expect("消息应当可以序列化")
    }

    /// 文档样例本身是可解析的 JSON（去注释后），用来锁住夹具没抄错。
    #[test]
    fn documented_samples_are_intact() {
        let welcome: Value =
            serde_json::from_str(&without_comments(DOC_WELCOME)).expect("欢迎样例应当可解析");
        assert_eq!(welcome["type"], "welcome");
        assert_eq!(
            welcome["data"]["data"],
            "感谢接入MoeKoe Music，文档地址：https://music.moekoe.cn/"
        );

        // 其余样例用类型名占位（`number` / `string` / `object`），本身不是合法 JSON；
        // 这里只确认「字段名 + 注释」没有被改动。
        assert!(DOC_LYRICS.contains("\"currentTime\": number"));
        assert!(DOC_LYRICS.contains("\"lyricsData\": string"));
        assert!(DOC_LYRICS.contains("\"currentSong\": object"));
        assert!(DOC_LYRICS.contains("\"duration\": number"));
        assert!(DOC_PLAYER_STATE.contains("\"isPlaying\": boolean"));
        assert!(DOC_PLAYER_STATE.contains("\"currentTime\": number"));
        assert!(DOC_CONTROL.contains("\"command\": \"toggle|next|prev\""));
        assert!(DOC_ERROR.contains("\"code\": number"));
        assert!(DOC_ERROR.contains("\"message\": string"));
    }

    /// 欢迎消息：文档样例的 `data` 是嵌套对象，上游源码发的是**纯字符串**。
    /// 按惜别的要求以源码为准——这条测试就是那处差异的书面记录。
    #[test]
    fn welcome_data_is_a_plain_string_like_upstream() {
        let value = encode_to_value(&ServerMessage::Welcome("hi".to_string()));
        assert_eq!(value["type"], "welcome");
        assert_eq!(value["data"], "hi");
        assert!(value["data"].is_string());
        // 文档样例里的嵌套结构我们没有采用，这里显式记下来。
        let documented: Value =
            serde_json::from_str(&without_comments(DOC_WELCOME)).expect("样例应当可解析");
        assert!(documented["data"].is_object());
    }

    #[test]
    fn lyrics_message_matches_documented_shape() {
        let data = LyricsData {
            current_time: 12.5,
            lyrics_data: "[ti:晴天]".to_string(),
            current_song: Some(Song {
                name: "晴天".to_string(),
                hash: "6af00fbd4d444a82c005843eef9dc2d4".to_string(),
                ..Song::default()
            }),
            duration: 269.0,
        };
        let value = encode_to_value(&ServerMessage::Lyrics(Box::new(data)));
        assert_eq!(value["type"], "lyrics");
        let body = &value["data"];
        assert_eq!(
            keys_of(body),
            ["currentTime", "lyricsData", "currentSong", "duration"]
        );
        assert!(body["currentTime"].is_f64());
        assert!(body["lyricsData"].is_string());
        assert!(body["currentSong"].is_object());
        assert!(body["duration"].is_f64());
    }

    #[test]
    fn lyrics_current_song_is_null_without_a_song() {
        let data = LyricsData {
            current_time: 0.0,
            lyrics_data: "x".to_string(),
            current_song: None,
            duration: 0.0,
        };
        let value = encode_to_value(&ServerMessage::Lyrics(Box::new(data)));
        assert!(value["data"]["currentSong"].is_null());
    }

    #[test]
    fn player_state_message_matches_documented_shape() {
        let value = encode_to_value(&ServerMessage::PlayerState(PlayerStateData {
            is_playing: true,
            current_time: 3.5,
        }));
        assert_eq!(value["type"], "playerState");
        let body = &value["data"];
        assert_eq!(keys_of(body), ["isPlaying", "currentTime"]);
        assert_eq!(body["isPlaying"], true);
        assert!(body["currentTime"].is_f64());
    }

    #[test]
    fn error_message_matches_documented_shape() {
        let value = encode_to_value(&ServerMessage::Error(ErrorData {
            code: 400,
            message: "未知的 control 命令".to_string(),
        }));
        assert_eq!(value["type"], "error");
        let body = &value["data"];
        assert_eq!(keys_of(body), ["code", "message"]);
        assert_eq!(body["code"], 400);
        assert!(body["message"].is_string());
    }

    /// 控制命令按文档样例反序列化，并映射到已有的播放动作。
    #[test]
    fn control_sample_maps_to_existing_actions() {
        // 这条样例里 `"command": "toggle|next|prev",` 后有个尾随逗号，本身不是合法
        // JSON（文档里是示意写法），所以只核对字段名没有被抄错，不做反序列化。
        assert!(DOC_CONTROL.contains("\"type\": \"control\""));
        assert!(DOC_CONTROL.contains("\"command\": \"toggle|next|prev\""));

        for (command, expected) in [
            ("toggle", Action::PlayPause),
            ("next", Action::Next),
            ("prev", Action::Prev),
        ] {
            let text = format!(r#"{{"type":"control","data":{{"command":"{command}"}}}}"#);
            let message: ClientMessage = serde_json::from_str(&text).expect("应当可以解析");
            let ClientMessage::Control(data) = message;
            assert_eq!(data.command, command);
            assert_eq!(control_action(&data.command), Some(expected));
        }
    }

    #[test]
    fn unknown_control_command_is_rejected_not_ignored() {
        assert_eq!(control_action("volume"), None);
        assert_eq!(control_action("seek"), None);
        assert_eq!(control_action("rm -rf /"), None);
        assert_eq!(control_action(""), None);

        // 走完整路径：未知命令要回一条 error，而不是静默丢弃。
        let (bus, receiver) = EventBus::new();
        let reply = handle_command(r#"{"type":"control","data":{"command":"exec"}}"#, &bus)
            .expect("未知命令应当回一条 error");
        let value: Value = serde_json::from_str(&reply).expect("回包应当是 JSON");
        assert_eq!(value["type"], "error");
        assert_eq!(value["data"]["code"], 400);
        assert!(value["data"]["message"].as_str().unwrap().contains("exec"));
        assert!(receiver.try_recv().is_err(), "未知命令不该派发任何动作");
    }

    #[test]
    fn known_control_command_dispatches_action() {
        let (bus, receiver) = EventBus::new();
        assert!(handle_command(r#"{"type":"control","data":{"command":"next"}}"#, &bus).is_none());
        match receiver.try_recv() {
            Ok(Event::Action(action)) => assert_eq!(action, Action::Next),
            other => panic!("应当是 Action::Next，实际是 {other:?}"),
        }
    }

    /// 未知消息类型 / 坏 JSON 都要回 error，而不是被当成「无事发生」。
    #[test]
    fn unknown_message_type_is_rejected() {
        let (bus, _receiver) = EventBus::new();
        let reply = handle_command(r#"{"type":"exec","data":{"command":"ls"}}"#, &bus)
            .expect("未知类型应当回一条 error");
        let value: Value = serde_json::from_str(&reply).expect("回包应当是 JSON");
        assert_eq!(value["type"], "error");
    }

    #[test]
    fn malformed_json_is_rejected() {
        let (bus, _receiver) = EventBus::new();
        for text in ["", "{", "not json", r#"{"type":"control"}"#] {
            let reply = handle_command(text, &bus).expect("坏消息应当回一条 error");
            let value: Value = serde_json::from_str(&reply).expect("回包应当是 JSON");
            assert_eq!(value["type"], "error", "输入 {text:?} 应当被拒绝");
        }
    }

    /// 浏览器跨站连接会被拒；原生客户端不带 `Origin`，放行。
    #[test]
    fn origin_check_rejects_foreign_sites() {
        fn request_with_origin(origin: Option<&str>) -> Request {
            let mut builder = Request::builder().uri("/");
            if let Some(origin) = origin {
                builder = builder.header("origin", origin);
            }
            builder.body(()).expect("构造请求")
        }

        for allowed in [
            None,
            Some("http://localhost"),
            Some("http://localhost:8080"),
            Some("https://127.0.0.1:1234"),
            Some("http://[::1]:6520"),
        ] {
            assert!(
                origin_allowed(&request_with_origin(allowed)),
                "{allowed:?} 应当被允许"
            );
        }
        for denied in [
            Some("null"),
            Some("https://evil.example"),
            Some("http://localhost.evil.example"),
            Some("http://127.0.0.1.evil.example:80"),
            Some("ftp://localhost"),
        ] {
            assert!(
                !origin_allowed(&request_with_origin(denied)),
                "{denied:?} 应当被拒绝"
            );
        }
    }

    /// 只接受本机回环地址的连接；`0.0.0.0` 上的对端一律拒绝。
    #[test]
    fn only_loopback_peers_are_accepted() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

        for allowed in [
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 40000),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 40000),
        ] {
            assert!(peer_allowed(allowed), "{allowed} 应当被允许");
        }
        for denied in [
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), 40000),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 40000),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 40000),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 40000),
        ] {
            assert!(!peer_allowed(denied), "{denied} 应当被拒绝");
        }
    }

    /// 快照比较只看会改变推送内容的字段，避免暂停期间空转。
    #[test]
    fn snapshot_equality_tracks_pushed_fields() {
        let base = Snapshot {
            song: Some(Song {
                name: "晴天".to_string(),
                hash: "abc".to_string(),
                ..Song::default()
            }),
            lyric_text: Some("[ti:晴天]".to_string()),
            is_playing: true,
            position_ms: 1_000,
            duration_ms: 269_000,
        };
        let same = base.clone();
        assert_eq!(base, same);

        let mut moved = base.clone();
        moved.position_ms += 200;
        assert_ne!(base, moved, "时间推进要触发推送");

        // 同一首歌换个名字（hash 不变）不影响推送内容。
        let mut renamed = base.clone();
        renamed.song = Some(Song {
            name: "晴天（重命名）".to_string(),
            hash: "abc".to_string(),
            ..Song::default()
        });
        assert_eq!(base, renamed);

        let mut paused = base.clone();
        paused.is_playing = false;
        assert_ne!(base, paused);
    }

    #[test]
    fn diff_emits_lyrics_on_progress_and_state_only_on_toggle() {
        let previous = Snapshot {
            song: Some(Song {
                hash: "abc".to_string(),
                ..Song::default()
            }),
            lyric_text: Some("[ti:x]".to_string()),
            is_playing: true,
            position_ms: 1_000,
            duration_ms: 10_000,
        };
        let mut current = previous.clone();
        current.position_ms = 1_200;

        // 时间推进但播放状态没变：只推歌词。上游 `playerState` 只在播放 / 暂停
        // 翻转时发（`electron/main.js` 的 `play-pause-action`），不能每拍都推。
        let messages = diff_messages(&previous, &current);
        assert_eq!(messages.len(), 1, "时间推进只推歌词");
        let first: Value = serde_json::from_str(&messages[0]).expect("歌词消息");
        assert_eq!(first["type"], "lyrics");
        // 毫秒换算成秒。
        assert_eq!(first["data"]["currentTime"], 1.2);

        // 播放 → 暂停：状态翻转，推一条 playerState；时间没动，不推歌词。
        let paused = Snapshot {
            is_playing: false,
            position_ms: 1_200,
            ..current.clone()
        };
        let messages = diff_messages(&current, &paused);
        assert_eq!(messages.len(), 1, "暂停要推播放状态");
        let value: Value = serde_json::from_str(&messages[0]).expect("状态消息");
        assert_eq!(value["type"], "playerState");
        assert_eq!(value["data"]["isPlaying"], false);
        assert_eq!(value["data"]["currentTime"], 1.2);

        // 暂停且位置没变：什么都不推（这是「暂停时不空转」的那条保证）。
        let still = paused.clone();
        assert!(diff_messages(&paused, &still).is_empty());

        // 没有歌词时，时间推进什么都不推。
        let mut without_lyric = current.clone();
        without_lyric.lyric_text = None;
        without_lyric.position_ms = 1_400;
        assert!(diff_messages(&current, &without_lyric).is_empty());
    }

    /// 入站上限比 tungstenite 默认的 64 MiB 收紧得多。
    #[test]
    fn inbound_message_size_is_capped() {
        assert_eq!(MAX_MESSAGE_BYTES, 64 * 1024);
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        assert_eq!(config.max_message_size, Some(MAX_MESSAGE_BYTES));
        assert_eq!(config.max_frame_size, Some(MAX_MESSAGE_BYTES));
    }

    /// 起一个真的服务端，返回它的端口与一个「等它开始监听」的握手地址。
    async fn spawn_test_server() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("绑一个临时端口");
        let port = listener.local_addr().expect("取端口").port();
        let (changes, changes_rx) = watch::channel(Arc::new(Snapshot::default()));
        let (out, _) = broadcast::channel::<Arc<str>>(BROADCAST_CAPACITY);
        let (bus, _receiver) = EventBus::new();
        // 快照丢在测试作用域里无所谓，`serve` 只读它。
        drop(changes);
        tokio::spawn(async move {
            serve(listener, changes_rx, out, bus).await;
        });
        port
    }

    /// 用裸 TCP 发一次握手请求，返回服务端回的状态行。
    async fn raw_handshake(port: u16, origin: Option<&str>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .expect("连上测试服务端");
        let mut request = String::from(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n",
        );
        if let Some(origin) = origin {
            request.push_str(&format!("Origin: {origin}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).await.expect("发请求");
        let mut buffer = vec![0u8; 1024];
        let read = stream.read(&mut buffer).await.expect("读响应");
        String::from_utf8_lossy(&buffer[..read]).to_string()
    }

    /// 跨站 `Origin` 的握手必须被真的拒掉（不是只让判定函数返回 false）。
    #[tokio::test]
    async fn handshake_from_a_foreign_origin_is_rejected() {
        let port = spawn_test_server().await;

        let denied = raw_handshake(port, Some("https://evil.example")).await;
        assert!(
            denied.starts_with("HTTP/1.1 403"),
            "跨站 Origin 应当被拒，实际：{denied}"
        );

        let allowed = raw_handshake(port, None).await;
        assert!(
            allowed.starts_with("HTTP/1.1 101"),
            "无 Origin 的本地客户端应当被接受，实际：{allowed}"
        );
    }

    /// 超长入站消息会被断开，而不是被读进内存。
    #[tokio::test]
    async fn oversized_inbound_message_is_refused() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let port = spawn_test_server().await;
        let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .expect("连上测试服务端");
        let request = format!("ws://127.0.0.1:{port}/")
            .as_str()
            .into_client_request()
            .expect("构造握手请求");
        // 用 `client_async` 而不是 `connect_async`：后者要开 `connect` feature，
        // 而那个 feature 会把 TLS 客户端栈拖进所有平台（见 Cargo.toml 注释）。
        let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .expect("本机客户端应当连得上");

        // 连接建立后服务端会先发 welcome（与当前状态），先读掉再灌超长消息，
        // 否则会把 greeting 当成「超长消息被正常处理」。
        let mut greeting = 0;
        while greeting < 2 {
            match socket.next().await {
                Some(Ok(Message::Text(_))) => greeting += 1,
                Some(Ok(_)) => continue,
                other => panic!("握手后应当先收到 greeting，实际：{other:?}"),
            }
        }

        // 超过 64 KiB 的一帧：服务端应当以错误关闭连接。
        let huge = "a".repeat(MAX_MESSAGE_BYTES + 1024);
        let sent = socket.send(Message::text(huge)).await;
        if sent.is_ok() {
            // 发送可能因为服务端已关闭而失败，两者都算「被拒」；
            // 真正的判据是后续读不到任何正常回包。
            match socket.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {}
                Some(Ok(other)) => panic!("超长消息不该被正常处理，收到：{other:?}"),
            }
        }
    }

    /// 内容没变时 `update_from` 不该唤醒广播（主循环每拍都调，唤醒会白推一帧）。
    #[test]
    fn update_from_wakes_only_when_the_snapshot_changes() {
        let (changes, mut rx) = watch::channel(Arc::new(Snapshot::default()));
        let handle = WsHandle {
            changes,
            running: Arc::new(AtomicBool::new(false)),
        };
        let song = Song {
            hash: "abc".to_string(),
            ..Song::default()
        };

        // 与默认快照一致：不产生变化。
        handle.update_from(None, None, false, 0, 0);
        assert!(!rx.has_changed().expect("通道未关闭"), "没变化不该唤醒广播");

        // 有变化：写入并唤醒。
        handle.update_from(Some(&song), Some("[ti:x]"), true, 1_000, 10_000);
        assert!(rx.has_changed().expect("通道未关闭"), "变化必须唤醒");
        let snapshot = rx.borrow_and_update().clone();
        assert_eq!(snapshot.song_hash(), Some("abc"));
        assert_eq!(snapshot.lyric_text.as_deref(), Some("[ti:x]"));
        assert!(snapshot.is_playing);
        assert_eq!(snapshot.position_ms, 1_000);
        assert_eq!(snapshot.duration_ms, 10_000);

        // 再写一次相同内容：不该重复唤醒。
        handle.update_from(Some(&song), Some("[ti:x]"), true, 1_000, 10_000);
        assert!(
            !rx.has_changed().expect("通道未关闭"),
            "内容相同不该重复唤醒"
        );
    }
}
