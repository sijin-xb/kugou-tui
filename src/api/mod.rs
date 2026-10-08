//! KuGouMusicApi 客户端与接口封装。
//!
//! 分层：
//!
//! ```text
//! client.rs   —— 只管 HTTP：拼 URL、带 cookie、取响应体
//! model.rs    —— 领域模型 + 把酷狗原始 JSON 归一成领域模型
//! catalog.rs  —— 目录类接口：搜索 / 歌单 / 歌手 / 排行榜 / 播放直链
//! lyric.rs    —— 歌词：搜歌词拿 (id, accesskey) → 取正文 → 解析 LRC
//! cloud.rs    —— 登录态相关：设备指纹 / 云端歌单增删
//! ```
//!
//! 接口路径全部对照 <https://github.com/MakcRe/KuGouMusicApi> 的 `docs/README.md` 核对过。

pub(crate) mod catalog;
/// HTTP 层。音源模块（如网易云）需要用它发请求，因此提到 `pub(crate)`。
pub(crate) mod client;
/// 登录与云端歌单写操作。
pub mod cloud;
/// 歌词解析。`pub(crate)` 是因为音源模块（如网易云）要复用 `parse_lrc`。
pub(crate) mod lyric;
/// 领域模型要对 crate 内其它层可见（`app` / `ui` 都要用 `Song` 等类型）。
pub mod model;
/// 内嵌的纯 Rust 后端。
pub(crate) mod native;
/// KuGouMusicApi（Node）后端：酷狗接口的方法体挂在这里。
pub(crate) mod node;
/// 后端抽象。上层只认这个 trait，不认具体是 Node 还是 native。
pub(crate) mod traits;

use crate::error::{AppError, Result};
use client::HttpClient;
use native::NativeApi;
use node::NodeApi;
use serde::{Deserialize, Serialize};
use traits::MusicApi;

use crate::source::SourceKind;

/// 用哪套后端实现。
///
/// 默认 [`ApiBackend::Node`]：内嵌后端在阶段 2 起逐步接入，全部验收通过之前
/// 不改变默认行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ApiBackend {
    /// 走本机 KuGouMusicApi（Node）。
    #[default]
    Node,
    /// 内嵌的纯 Rust 实现，不需要 Node。
    Native,
}

impl ApiBackend {
    /// 该后端**实际**能用在哪类音源上。
    ///
    /// native 只覆盖酷狗两个平台：网易云与汽水的接口语义没有进 [`MusicApi`]，
    /// 它们直接架在 HTTP 传输层上（见 [`crate::source::netease`]），而 native
    /// 没有那一层。所以在这些音源上强行用 native 只会得到「需要 HTTP 传输层」
    /// 的报错，不如在这里就落回 Node，让 `--api native` 的语义是
    /// 「酷狗不用 Node，其它音源照旧」。
    pub fn effective_for(self, kind: SourceKind) -> Self {
        match self {
            Self::Native if kind.uses_device_fingerprint() => Self::Native,
            _ => Self::Node,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Node => "node（本机 KuGouMusicApi）",
            Self::Native => "native（内嵌，不依赖 Node）",
        }
    }
}

impl std::fmt::Display for ApiBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Node => "node",
            Self::Native => "native",
        })
    }
}

/// 客户端是这一层唯一对外暴露的类型。
///
/// 领域模型不在这里 re-export：各层直接从 `crate::api::model` 取，
/// 少一层转发，`use` 语句也更能说明「这个东西从哪来」。
///
/// # 为什么是枚举而不是 `Box<dyn MusicApi>`
///
/// 上游调用点与测试都以「一个具体类型 + 值语义克隆」为前提（`ApiClient` 要
/// `Clone`、能塞进 `JoinSet`、能在切音源时整体替换），而 `async fn in trait`
/// 当前不满足 dyn 兼容。枚举分派保住这些前提，代价是多一层 match——
/// 两个分支都**只调用 trait 实现**，不含任何接口逻辑，将来换成
/// `async-trait` + `dyn` 是机械改动。
#[derive(Debug, Clone)]
pub enum ApiClient {
    /// 走本机 KuGouMusicApi 的 HTTP 后端。
    Node(NodeApi),
    /// 内嵌的纯 Rust 后端。
    Native(NativeApi),
}

impl ApiClient {
    /// 构造 Node 后端。**签名与行为保持与迁移前一致**，测试直接依赖它。
    pub fn new(base: &str, cookie: Option<String>, proxy: Option<&str>) -> Result<Self> {
        Ok(Self::Node(NodeApi::new(base, cookie, proxy)?))
    }

    /// 构造内嵌后端。`kind` 决定平台盐值与 `appid`。
    ///
    /// `proxy` 与 Node 后端同源——native 直连酷狗网关，同样需要走用户配的代理。
    pub fn native(kind: SourceKind, cookie: Option<String>, proxy: Option<&str>) -> Result<Self> {
        Ok(Self::Native(NativeApi::new(kind, cookie, proxy)?))
    }

    /// 按配置选后端。**这是全程序构造客户端的唯一入口**。
    ///
    /// `kind` 参与判定：native 只覆盖酷狗两个平台，其余音源无论配置怎么写都走
    /// Node（理由见 [`ApiBackend::effective_for`]）。
    pub fn for_backend(
        backend: ApiBackend,
        kind: SourceKind,
        base: &str,
        cookie: Option<String>,
        proxy: Option<&str>,
    ) -> Result<Self> {
        match backend.effective_for(kind) {
            ApiBackend::Native => Self::native(kind, cookie, proxy),
            ApiBackend::Node => Self::new(base, cookie, proxy),
        }
    }

    /// 当前后端的传输层。**只有 Node 后端有**；网易云音源架在它上面。
    fn http(&self) -> Result<&HttpClient> {
        match self {
            Self::Node(api) => Ok(api.transport()),
            Self::Native(_) => Err(AppError::Other(
                "该接口需要 HTTP 传输层，native 后端不支持".to_string(),
            )),
        }
    }

    pub fn base(&self) -> &str {
        match self {
            Self::Node(api) => api.base(),
            Self::Native(api) => api.base(),
        }
    }

    pub fn cookie(&self) -> Option<&str> {
        match self {
            Self::Node(api) => api.cookie(),
            Self::Native(api) => api.cookie(),
        }
    }

    pub fn set_cookie(&mut self, cookie: Option<String>) {
        match self {
            Self::Node(api) => api.set_cookie(cookie),
            Self::Native(api) => api.set_cookie(cookie),
        }
    }

    // ------------------------------------------------------------------
    // 传输转发。网易云音源直接把这些当自己的 HTTP 层用，签名不能变。
    //
    // 只暴露网易云**实际用到**的两个：它全部 21 个请求都走 `get_json_uncached`，
    // 四个写接口走 `get_json_uncached_mutating`。酷狗自己的读接口不走这里
    // （在 `NodeApi` 内部直接持有 `HttpClient`），所以不预留另外三个。
    // ------------------------------------------------------------------

    pub(crate) async fn get_json_uncached(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<serde_json::Value> {
        self.http()?.get_json_uncached(path, query).await
    }

    pub(crate) async fn get_json_uncached_mutating(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<serde_json::Value> {
        self.http()?.get_json_uncached_mutating(path, query).await
    }
}

/// 把 [`MusicApi`] 的每个方法转发给内部后端。
///
/// 这里**只做分派**：两个分支各自调对方的 trait 实现，接口逻辑一律不在本文件。
macro_rules! delegate_to_backend {
    ($( async fn $name:ident ( &self $(, $arg:ident : $ty:ty)* ) -> $ret:ty ; )*) => {
        impl MusicApi for ApiClient {
            $(
                async fn $name(&self $(, $arg: $ty)*) -> $ret {
                    match self {
                        Self::Node(api) => MusicApi::$name(api $(, $arg)*).await,
                        Self::Native(api) => MusicApi::$name(api $(, $arg)*).await,
                    }
                }
            )*
        }
    };
}

delegate_to_backend! {
    async fn search_songs(&self, keywords: &str, page: u32, page_size: u32) -> Result<Vec<model::Song>>;
    async fn plaza_playlists(&self, category_id: i64, page: u32, page_size: u32) -> Result<Vec<model::Playlist>>;
    async fn playlist_tracks(&self, global_id: &str, page: u32, page_size: u32, fresh: bool) -> Result<Vec<model::Song>>;
    async fn user_playlists(&self) -> Result<Vec<model::Playlist>>;
    async fn user_playlist_tracks(&self, list_id: i64, page: u32, page_size: u32, fresh: bool) -> Result<Vec<model::Song>>;
    async fn artist_list(&self, kind: i64, hot_size: u32) -> Result<Vec<model::Artist>>;
    async fn rank_boards(&self) -> Result<Vec<model::RankBoard>>;
    async fn playlist_tracks_all(&self, global_id: &str, fresh: bool) -> Result<Vec<model::Song>>;
    async fn user_playlist_tracks_all(&self, list_id: i64, fresh: bool) -> Result<Vec<model::Song>>;
    async fn artist_tracks_all(&self, artist_id: i64, sort: &str) -> Result<Vec<model::Song>>;
    async fn rank_tracks_all(&self, rank_id: i64) -> Result<Vec<model::Song>>;
    async fn song_stream_url(&self, song: &model::Song, quality: &str) -> Result<catalog::StreamUrl>;
    async fn fetch_lyric(&self, song: &model::Song) -> Result<model::Lyric>;
    async fn login_qr_key(&self) -> Result<String>;
    async fn login_qr_create(&self, key: &str) -> Result<String>;
    async fn login_qr_check(&self, key: &str) -> Result<cloud::QrCheck>;
    async fn user_detail(&self) -> Result<cloud::UserInfo>;
    async fn user_vip_detail(&self) -> Result<cloud::VipInfo>;
    async fn claim_day_vip(&self, receive_day: &str) -> Result<Value>;
    async fn upgrade_day_vip(&self) -> Result<Value>;
    async fn claimed_vip_days(&self) -> Result<Vec<String>>;
    async fn fetch_device_fingerprint(&self) -> Result<String>;
    async fn add_tracks_to_playlist(&self, source: SourceKind, list_id: i64, songs: &[model::Song]) -> Result<usize>;
    async fn remove_tracks_from_playlist(&self, source: SourceKind, list_id: i64, songs: &[model::Song]) -> Result<usize>;
    async fn delete_playlist(&self, source: SourceKind, list_id: i64) -> Result<()>;
    async fn create_playlist(&self, source: SourceKind, name: &str) -> Result<Option<i64>>;
}

use serde_json::Value;

/// 取出响应里的 `data` 段。
///
/// 少数接口把结果直接放在顶层，此时回落到 `root` 本身，让后续的候选键扫描
/// 仍然有机会命中，而不是直接判空。
pub(crate) fn data_of(root: &Value) -> &Value {
    root.get("data").unwrap_or(root)
}

/// 在 JSON 树里按 `key` 找第一个数组，深度限制 5 层。
fn find_array_by_key<'a>(value: &'a Value, key: &str, depth: usize) -> Option<&'a Vec<Value>> {
    if depth > 5 {
        return None;
    }
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key).and_then(Value::as_array) {
                return Some(found);
            }
            map.values()
                .find_map(|child| find_array_by_key(child, key, depth + 1))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| find_array_by_key(child, key, depth + 1)),
        _ => None,
    }
}

/// 递归收集所有「元素是对象」的数组。
fn collect_object_arrays<'a>(value: &'a Value, out: &mut Vec<&'a Vec<Value>>, depth: usize) {
    if depth > 5 {
        return;
    }
    match value {
        Value::Array(items) => {
            if items.iter().any(Value::is_object) {
                out.push(items);
            }
            for child in items {
                collect_object_arrays(child, out, depth + 1);
            }
        }
        Value::Object(map) => {
            for child in map.values() {
                collect_object_arrays(child, out, depth + 1);
            }
        }
        _ => {}
    }
}

/// 从响应里提取一组对象。
///
/// 策略分两级：
///
/// 1. 按 `preferred_keys` 依次找数组——覆盖该接口的已知键名；
/// 2. 全部落空时，扫描整棵 JSON 树，取第一个「能解析出非空结果」的对象数组。
///
/// 第二级是兜底：即使酷狗改了字段布局，界面也只是少了某些条目，而不是整页空白。
pub(crate) fn extract_list<T>(
    root: &Value,
    preferred_keys: &[&str],
    parse: impl Fn(&Value) -> Option<T>,
) -> Vec<T> {
    for key in preferred_keys {
        let Some(array) = find_array_by_key(root, key, 0) else {
            continue;
        };
        let parsed: Vec<T> = array.iter().filter_map(&parse).collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }

    // 兜底：扫描整棵树，取第一个能解析出内容的数组。
    //
    // 这一级的代价是**可能选中不该选的数组**（例如歌单条目里嵌套的 `authors[]`），
    // 结果是界面显示了别的实体而没有任何报错——所以命中时必须记一条日志，
    // 让「上游改了字段布局」这件事有据可查，而不是静默降级。
    let mut arrays = Vec::new();
    collect_object_arrays(root, &mut arrays, 0);
    for array in arrays {
        let parsed: Vec<T> = array.iter().filter_map(&parse).collect();
        if !parsed.is_empty() {
            // 带上顶层键名：光说「兜底命中一个数组」没法知道是哪个接口，
            // 而顶层键往往一眼就能认出来（歌单广场是 `special_list`、
            // 用户歌单是 `info`……），下次再出现就不用猜了。
            let top_keys = root.as_object().map(|object| {
                object
                    .keys()
                    .take(8)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            });
            crate::logger::tlog!(
                crate::logger::LEVEL_WARN,
                "响应未命中任何候选键，兜底扫描命中了一个数组（{} 项），请核对字段布局（顶层键：{}）",
                parsed.len(),
                top_keys.as_deref().unwrap_or("(响应不是对象)")
            );
            return parsed;
        }
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_list_prefers_named_key() {
        let root = json!({
            "data": {
                "info": [{"name": "歌单A"}],
                "noise": [{"name": "不该被选中"}]
            }
        });
        let names: Vec<String> = extract_list(&root, &["info"], |value| {
            value.get("name")?.as_str().map(str::to_string)
        });
        assert_eq!(names, vec!["歌单A"]);
    }

    #[test]
    fn extract_list_falls_back_to_tree_scan() {
        // 键名全变了，兜底扫描仍应命中
        let root = json!({
            "data": {
                "unexpected_container": {
                    "deeply": {"nested": [{"name": "歌单B"}]}
                }
            }
        });
        let names: Vec<String> = extract_list(&root, &["info", "list"], |value| {
            value.get("name")?.as_str().map(str::to_string)
        });
        assert_eq!(names, vec!["歌单B"]);
    }

    #[test]
    fn data_of_falls_back_to_root() {
        let root = json!({"lists": []});
        assert!(data_of(&root).get("lists").is_some());
    }

    /// 默认必须是 node：阶段 1 的出口条件就是「默认行为与改动前一致」。
    #[test]
    fn node_is_the_default_backend() {
        assert_eq!(ApiBackend::default(), ApiBackend::Node);
    }

    /// `--api native` 只覆盖酷狗两个平台。
    ///
    /// 网易云与汽水的接口语义没有进 [`MusicApi`]，它们直接架在 HTTP 传输层上，
    /// 而 native 没有那一层。这里锁住「落回 node」这个决定，而不是让它们
    /// 在运行期才报「需要 HTTP 传输层」。
    #[test]
    fn native_backend_only_covers_kugou_sources() {
        for kind in [SourceKind::Kugou, SourceKind::KugouConcept] {
            assert_eq!(
                ApiBackend::Native.effective_for(kind),
                ApiBackend::Native,
                "{} 应当能走 native",
                kind.label()
            );
        }
        for kind in [SourceKind::Netease, SourceKind::Sodam] {
            assert_eq!(
                ApiBackend::Native.effective_for(kind),
                ApiBackend::Node,
                "{} 没有 native 实现，应当落回 node",
                kind.label()
            );
        }
    }

    /// node 后端在任何音源上都是 node，不被 `effective_for` 改写。
    #[test]
    fn node_backend_is_never_rewritten() {
        for kind in SourceKind::ALL {
            assert_eq!(ApiBackend::Node.effective_for(kind), ApiBackend::Node);
        }
    }

    /// 构造 native 客户端不碰网络，也不需要服务地址。
    #[test]
    fn native_client_needs_no_service() {
        let api = ApiClient::for_backend(
            ApiBackend::Native,
            SourceKind::KugouConcept,
            "http://127.0.0.1:3001",
            None,
            None,
        )
        .expect("native 构造不该失败");
        assert!(matches!(api, ApiClient::Native(_)));
        // 界面会把这串字显示在「接口地址」的位置，必须能看出是内嵌的
        assert!(api.base().contains("native"), "实际：{}", api.base());
    }
}
