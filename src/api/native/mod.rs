//! 纯 Rust 后端。底层（签名 / 设备指纹 / KRC 解密）已实现并有 known-answer
//! 测试对照上游；网络接口按阶段逐个接入，尚未接入的仍明确报「尚未实现」。
//!
//! 实现顺序：底层先行，KAT 全绿之后才接网络——签名错一个字节就全部失败，
//! 而且不会有清楚的报错，混在网络调试里查不动。
//!
//! 平台参数由 [`SourceKind`] 携带：标准版与概念版（lite）的盐值、`appid`、
//! `clientver` 都不同，两个都要能跑。**native 知道自己在哪个平台**，这是它相对
//! `NodeApi` 的一个优势：`module/song_url.js` 里那些 `isLite` 分支依赖
//! `process.env.platform`，客户端根本看不到，而这里可以直接按平台取对的那一支。

// 尚未接入网络的底层纯函数。`sign`/`device`/`krc` 都已被用上，不再需要 allow；
// `crypto`（RSA/AES）等阶段 5 的设备注册与云端歌单。
#[allow(dead_code)]
pub mod crypto;
pub mod device;
pub mod krc;
pub mod sign;
pub mod transport;

use crate::api::catalog::{StreamSource, StreamUrl, resolve_stream_url};
use crate::api::cloud::{QrCheck, UserInfo, VipInfo};
use crate::api::data_of;
use crate::api::model::{Artist, Lyric, Playlist, RankBoard, Song, extract_songs};
use crate::api::native::device::random_string;
use crate::api::native::transport::{Endpoint, GATEWAY_BASE, LYRICS_BASE, Transport};
use crate::api::traits::MusicApi;
use crate::error::{AppError, Result};
use crate::source::SourceKind;
use crate::util::random_f64;
use serde_json::Value;

/// 内嵌的纯 Rust 后端。
#[derive(Debug, Clone)]
pub struct NativeApi {
    transport: Transport,
    /// 供界面显示。native 没有服务地址，用平台名代替——标准版与概念版是两套
    /// `appid`/盐值，出问题时「现在到底在用哪一套」是第一个要确认的事。
    label: String,
}

impl NativeApi {
    pub fn new(kind: SourceKind, cookie: Option<String>, proxy: Option<&str>) -> Result<Self> {
        Ok(Self {
            transport: Transport::new(kind, cookie, proxy)?,
            label: format!("native（{}，内嵌）", kind.label()),
        })
    }

    pub fn base(&self) -> &str {
        &self.label
    }

    pub fn cookie(&self) -> Option<&str> {
        self.transport.cookie()
    }

    pub fn set_cookie(&mut self, cookie: Option<String>) {
        self.transport.set_cookie(cookie);
    }

    fn kind(&self) -> SourceKind {
        self.transport.kind()
    }
}

/// 尚未接入的方法统一从这里报错。
///
/// 阶段 1 的出口条件之一就是 `--api native` 必须**明确报错**而不是静默失败，
/// 所以这里不返回空列表、不返回默认值。
fn unimplemented(name: &str) -> AppError {
    AppError::Other(format!("native 后端尚未实现：{name}"))
}

/// 上游 `module/search.js` 的请求规格（路由 `/search`）。
///
/// `type` 白名单里只有 `song` 走 `/v3`；本项目只搜单曲，固定这一支。
pub(crate) fn search_endpoint(
    keywords: &str,
    page: u32,
    page_size: u32,
) -> Endpoint<'static> {
    Endpoint::get(GATEWAY_BASE, "/v3/search/song")
        .header("x-router", "complexsearch.kugou.com")
        .param("albumhide", "0")
        .param("iscorrection", "1")
        .param("keyword", keywords)
        .param("nocollect", "0")
        .param("page", page.to_string())
        .param("pagesize", page_size.to_string())
        .param("platform", "AndroidFilter")
}

/// 上游 `module/privilege_lite.js` 的请求规格（路由 `/privilege/lite`）。
///
/// `NodeApi` 那条路是**把 `hash` 交给 Node 服务、由服务去建 `resource` 数组**；
/// native 直连网关，得自己建。`resource` 数组进的是 POST body，而 android 签名
/// 覆盖 body 字符串，所以它的**字节形态**必须与上游一致。
pub(crate) fn privilege_lite_endpoint(
    kind: SourceKind,
    song: &Song,
) -> Result<Endpoint<'static>> {
    // 逗号分隔可一次问多个 hash：主 hash 在前，audio_info 里那些在后。
    let mut seen = std::collections::HashSet::new();
    let mut hashes = vec![song.hash.clone()];
    seen.insert(song.hash.clone());
    for hash in song.extra_hashes.values() {
        if seen.insert(hash.clone()) {
            hashes.push(hash.clone());
        }
    }

    // `resource[l]['album_id'] = s`，其中 `s` 来自
    // `(params?.album_id || '').split(',')`。我们（和 `NodeApi` 一样）从不传
    // `album_id`，于是 `''.split(',') === ['']`——**只有第 0 个元素**被改写成
    // 空字符串，其余保持初始的数字 0。这个 `""` 与 `0` 的区别会直接改变 body
    // 字节，进而改变签名。
    let resource: Vec<Value> = hashes
        .iter()
        .enumerate()
        .map(|(index, hash)| {
            let album_id = if index == 0 {
                Value::String(String::new())
            } else {
                Value::from(0)
            };
            serde_json::json!({
                "type": "audio",
                "page_id": 0,
                "hash": hash,
                "album_id": album_id,
            })
        })
        .collect();

    // `appid` / `clientver` 在上游取自 `require('../util')`，即平台默认值
    // （标准 1005/20489，概念 3116/11440）。
    let body = serde_json::json!({
        "appid": sign::appid(kind),
        "area_code": 1,
        "behavior": "play",
        "clientver": sign::clientver(kind),
        "need_hash_offset": 1,
        "relate": 1,
        "support_verify": 1,
        "resource": resource,
        "qualities": [
            "128", "320", "flac", "high", "viper_atmos", "viper_tape",
            "viper_clear", "super", "multitrack",
        ],
    });
    let body = serde_json::to_string(&body)
        .map_err(|error| AppError::Other(format!("序列化 /privilege/lite 请求体失败：{error}")))?;

    Ok(Endpoint::post(GATEWAY_BASE, "/v2/get_res_privilege/lite")
        .header("x-router", "media.store.kugou.com")
        .header("Content-Type", "application/json")
        .body(body))
}

/// 上游 `module/song_url.js` 的请求规格（路由 `/song/url`）。
///
/// `dfid` 由调用方给出：上游那一行是
/// `cookie: Object.assign({}, {dfid: randomString(24)}, params?.cookie)`，
/// 即**没登录时是每次调用新生成的 24 字符随机串**，不是 `-` 也不是空。
pub(crate) fn song_url_endpoint(
    kind: SourceKind,
    dfid: String,
    hash: &str,
    quality: &str,
    free_part: bool,
) -> Endpoint<'static> {
    let (page_id, pid, ppage_id) = match kind {
        SourceKind::KugouConcept => ("967177915", "411", "356753938"),
        _ => ("151369488", "2", "463467626,350369493,788954147"),
    };

    Endpoint::get(GATEWAY_BASE, "/v5/url")
        .header("x-router", "trackercdn.kugou.com")
        .dfid(dfid)
        .param("album_id", "0")
        .param("area_code", "1")
        // 上游 `(params?.hash || '').toLowerCase()`。
        .param("hash", hash.to_lowercase())
        .param("ssa_flag", "is_fromtrack")
        .param("version", "11430")
        .param("page_id", page_id)
        // 上游 `quality: quality || 128`。
        .param("quality", if quality.is_empty() { "128" } else { quality })
        .param("album_audio_id", "0")
        .param("behavior", "play")
        .param("pid", pid)
        .param("cmd", "26")
        .param("pidversion", "3001")
        .param("IsFreePart", if free_part { "1" } else { "0" })
        .param("ppage_id", ppage_id)
        .param("cdnBackup", "1")
        .param("module", "")
        // dataMap 里的 `clientver: 11430` 会覆盖平台默认值（见
        // `transport::merge_params`），标准版与概念版都是 11430。
        .param("clientver", "11430")
        .encrypt_key()
}

/// 上游 `module/search_lyric.js` 的请求规格（路由 `/search/lyric`）。
///
/// 这个模块带 `clearDefaultParams: true`，所以 `dfid`/`mid`/`uuid`/`appid`/
/// `clientver`/`clienttime` 全部不进参数（也不进签名）——`appid`/`clientver`
/// 由模块自己按平台写进 `dataMap`。注意 `module/search_lyric.js` 里的
/// `notSign: true` 是**死参数**（`util/request.js:126` 读的是 `notSignature`），
/// 所以照常带 android 签名。
pub(crate) fn search_lyric_endpoint(
    kind: SourceKind,
    song: &Song,
) -> Endpoint<'static> {
    Endpoint::get(LYRICS_BASE, "/v1/search")
        .clear_defaults()
        .param("album_audio_id", "0")
        .param("appid", sign::appid(kind).to_string())
        .param("clientver", sign::clientver(kind).to_string())
        .param("duration", song.duration_ms.to_string())
        .param("hash", song.hash.clone())
        .param("keyword", format!("{} - {}", song.singer_text(), song.name))
        .param("lrctxt", "1")
        .param("man", "yes")
}

/// 上游 `module/lyric.js` 的请求规格（路由 `/lyric`）。
///
/// 与 `/search/lyric` 不同，这个模块**没有** `clearDefaultParams`，所以走的是
/// 标准默认参数（含登录态的 `token`/`userid`），头里也带 `clienttime`。
pub(crate) fn lyric_endpoint(lyric_id: &str, access_key: &str) -> Endpoint<'static> {
    Endpoint::get(LYRICS_BASE, "/download")
        .param("ver", "1")
        .param("client", "android")
        .param("id", lyric_id)
        .param("accesskey", access_key)
        // 必须是 krc：译文与逐字时间戳只在 KRC 里有。
        .param("fmt", "krc")
        .param("charset", "utf8")
}

/// 把 `/lyric` 响应里的 `content` 就地解成 `decodeContent`。
///
/// 等价于上游 `module/lyric.js` 的 `decode` 分支：
/// ```js
/// res.body['decodeContent'] = params?.fmt == 'lrc' || Number(res.body?.contenttype) !== 0
///   ? Buffer.from(content, 'base64').toString()
///   : decodeLyrics(content);
/// ```
/// **`contenttype !== 0` 时是 base64 而不是 KRC**——少了这个分支，那种响应会被
/// 当成 KRC 解，得到一片乱码而不是报错。
///
/// 解密失败时**写空串**而不是返回 `Err`：上游 `decodeLyrics` 解不开就
/// `return ''`，空文本解析出空歌词、候选被跳过，最终是「未找到歌词」而不是
/// 一个错误。这里对齐同一语义，两端的用户可见行为才一致。
///
/// `decode=false`（本项目不用）时上游会把 `content` 原样留下，这里同理什么都不做。
pub(crate) fn inject_decoded_lyric(root: &mut Value) {
    // 与 `lyric::extract_lyric_text` 的 `data_of` 取同一层：有 `data` 对象就写进
    // `data`，否则写进根。
    let target = if root.get("data").is_some_and(Value::is_object) {
        root.get_mut("data")
    } else {
        Some(root)
    };
    let Some(object) = target.and_then(Value::as_object_mut) else {
        return;
    };
    let Some(content) = object.get("content").and_then(Value::as_str) else {
        return;
    };
    let content = content.to_string();

    // `Number(undefined) !== 0` 为真 → 缺 contenttype 时走 base64 分支。
    let contenttype = object
        .get("contenttype")
        .and_then(crate::api::model::value_to_i64)
        .unwrap_or(-1);
    let decoded = if contenttype != 0 {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(content.trim())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_default()
    } else {
        crate::api::native::krc::decode(&content).unwrap_or_default()
    };

    object.insert("decodeContent".to_string(), Value::String(decoded));
}

/// `NativeApi` 的歌词实现。
///
/// 算法在 [`crate::api::lyric::fetch_lyric_via`]，与 `NodeApi` **共用同一份**；
/// 这里只负责把两个请求按上游规则拼出来，并在下载那步本地解密 KRC
/// （`NodeApi` 是把 `decode=true` 交给本机 Node 服务去做）。
impl crate::api::lyric::LyricSource for NativeApi {
    async fn search_lyric(&self, song: &Song) -> Result<Value> {
        let endpoint = search_lyric_endpoint(self.kind(), song);
        // `NodeApi` 这条路走的是 `get_json`（带缓存、不加 timestamp），
        // 所以这里也传 `cached = true`，两端的请求条数才一致。
        self.transport.get_json(&endpoint, true).await
    }

    async fn lyric_body(&self, lyric_id: &str, access_key: &str) -> Result<String> {
        let endpoint = lyric_endpoint(lyric_id, access_key);
        let body = self.transport.get_text(&endpoint, true).await?;

        // `/lyric` 在 `decode=true` 下返回 JSON，偶尔直接吐纯文本；两种都接住，
        // 与 `fetch_lyric_via` 的解释方式保持一致。
        match serde_json::from_str::<Value>(&body) {
            Ok(mut root) => {
                inject_decoded_lyric(&mut root);
                serde_json::to_string(&root)
                    .map_err(|error| AppError::Other(format!("序列化歌词响应失败：{error}")))
            }
            Err(_) => Ok(body),
        }
    }
}

/// `NativeApi` 的取流实现。
///
/// 算法在 [`resolve_stream_url`]，与 `NodeApi` **共用同一份**；这里只负责把
/// 「问权限」「取直链」两个请求按上游 `module/*.js` 的规则拼出来。
impl StreamSource for NativeApi {
    async fn privilege_lite(&self, song: &Song) -> Result<Value> {
        let endpoint = privilege_lite_endpoint(self.kind(), song)?;
        self.transport.get_json(&endpoint, false).await
    }

    async fn song_url(&self, hash: &str, quality: &str, free_part: bool) -> Result<Value> {
        // 登录态带 `dfid` 就用它；否则按上游生成一次性随机串。
        let dfid = self
            .transport
            .cookie_map()
            .get("dfid")
            .filter(|value| !value.is_empty() && *value != "-")
            .cloned()
            .unwrap_or_else(|| random_string(24, &mut random_f64));
        let endpoint = song_url_endpoint(self.kind(), dfid, hash, quality, free_part);
        self.transport.get_json(&endpoint, false).await
    }
}

impl MusicApi for NativeApi {
    async fn search_songs(&self, keywords: &str, page: u32, page_size: u32) -> Result<Vec<Song>> {
        let endpoint = search_endpoint(keywords, page, page_size);
        let root = self.transport.get_json(&endpoint, false).await?;
        Ok(extract_songs(data_of(&root)))
    }

    async fn song_stream_url(&self, song: &Song, quality: &str) -> Result<StreamUrl> {
        resolve_stream_url(self, song, quality).await
    }

    async fn plaza_playlists(
        &self,
        _category_id: i64,
        _page: u32,
        _page_size: u32,
    ) -> Result<Vec<Playlist>> {
        Err(unimplemented("plaza_playlists"))
    }

    async fn playlist_tracks(
        &self,
        _global_id: &str,
        _page: u32,
        _page_size: u32,
        _fresh: bool,
    ) -> Result<Vec<Song>> {
        Err(unimplemented("playlist_tracks"))
    }

    async fn user_playlists(&self) -> Result<Vec<Playlist>> {
        Err(unimplemented("user_playlists"))
    }

    async fn user_playlist_tracks(
        &self,
        _list_id: i64,
        _page: u32,
        _page_size: u32,
        _fresh: bool,
    ) -> Result<Vec<Song>> {
        Err(unimplemented("user_playlist_tracks"))
    }

    async fn artist_list(&self, _kind: i64, _hot_size: u32) -> Result<Vec<Artist>> {
        Err(unimplemented("artist_list"))
    }

    async fn rank_boards(&self) -> Result<Vec<RankBoard>> {
        Err(unimplemented("rank_boards"))
    }

    async fn playlist_tracks_all(&self, _global_id: &str, _fresh: bool) -> Result<Vec<Song>> {
        Err(unimplemented("playlist_tracks_all"))
    }

    async fn user_playlist_tracks_all(&self, _list_id: i64, _fresh: bool) -> Result<Vec<Song>> {
        Err(unimplemented("user_playlist_tracks_all"))
    }

    async fn artist_tracks_all(&self, _artist_id: i64, _sort: &str) -> Result<Vec<Song>> {
        Err(unimplemented("artist_tracks_all"))
    }

    async fn rank_tracks_all(&self, _rank_id: i64) -> Result<Vec<Song>> {
        Err(unimplemented("rank_tracks_all"))
    }

    async fn fetch_lyric(&self, song: &Song) -> Result<Lyric> {
        crate::api::lyric::fetch_lyric_via(self, song).await
    }

    async fn login_qr_key(&self) -> Result<String> {
        Err(unimplemented("login_qr_key"))
    }

    async fn login_qr_create(&self, _key: &str) -> Result<String> {
        Err(unimplemented("login_qr_create"))
    }

    async fn login_qr_check(&self, _key: &str) -> Result<QrCheck> {
        Err(unimplemented("login_qr_check"))
    }

    async fn user_detail(&self) -> Result<UserInfo> {
        Err(unimplemented("user_detail"))
    }

    async fn user_vip_detail(&self) -> Result<VipInfo> {
        Err(unimplemented("user_vip_detail"))
    }

    async fn claim_day_vip(&self, _receive_day: &str) -> Result<Value> {
        Err(unimplemented("claim_day_vip"))
    }

    async fn upgrade_day_vip(&self) -> Result<Value> {
        Err(unimplemented("upgrade_day_vip"))
    }

    async fn claimed_vip_days(&self) -> Result<Vec<String>> {
        Err(unimplemented("claimed_vip_days"))
    }

    /// 设备指纹要打 `/register/dev`（AES-CBC + RSA + `arraybuffer` 响应），属阶段 5。
    ///
    /// 启动路径会调用它，失败只记一条 WARN——搜索与播放都不依赖 `dfid`
    /// （没有它时上游自己退化成随机值），所以这里不实现也不影响阶段 3 的出口。
    async fn fetch_device_fingerprint(&self) -> Result<String> {
        Err(unimplemented("fetch_device_fingerprint"))
    }

    async fn add_tracks_to_playlist(
        &self,
        _source: SourceKind,
        _list_id: i64,
        _songs: &[Song],
    ) -> Result<usize> {
        Err(unimplemented("add_tracks_to_playlist"))
    }

    async fn remove_tracks_from_playlist(
        &self,
        _source: SourceKind,
        _list_id: i64,
        _songs: &[Song],
    ) -> Result<usize> {
        Err(unimplemented("remove_tracks_from_playlist"))
    }

    async fn delete_playlist(&self, _source: SourceKind, _list_id: i64) -> Result<()> {
        Err(unimplemented("delete_playlist"))
    }

    async fn create_playlist(&self, _source: SourceKind, _name: &str) -> Result<Option<i64>> {
        Err(unimplemented("create_playlist"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// 与 `transport` 的 KAT 用同一组固定输入，方便两边互相对照。
    const KAT_CLIENTTIME: &str = "1700000000";
    const KAT_MID: &str = "231699103997194646178265604655475531917";
    const KAT_DFID: &str = "1234567890abcdef12345678";

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

    fn prepared_url(kind: SourceKind, endpoint: &Endpoint<'_>) -> String {
        crate::api::native::transport::build_prepared(
            kind,
            &kat_cookie(KAT_DFID),
            KAT_CLIENTTIME,
            endpoint,
        )
        .url
    }

    /// 阶段 1 的出口条件：尚未接入的方法必须明确报错，不能静默返回空。
    #[tokio::test]
    async fn placeholder_reports_not_implemented() {
        let api = NativeApi::new(SourceKind::KugouConcept, None, None).unwrap();
        let error = api.plaza_playlists(0, 1, 30).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("尚未实现"), "实际：{message}");
        assert!(
            message.contains("plaza_playlists"),
            "要指出是哪个方法：{message}"
        );
    }

    /// 两套平台的盐值/appid 不同，构造时就必须带上平台。
    #[test]
    fn keeps_its_platform() {
        let standard = NativeApi::new(SourceKind::Kugou, None, None).unwrap();
        let lite = NativeApi::new(SourceKind::KugouConcept, None, None).unwrap();
        assert!(standard.base().contains("酷狗"));
        assert!(lite.base().contains("概念版"));
    }

    /// 构造 native 客户端**不碰文件系统、不发网络请求**——启动路径与测试都会构造它。
    #[test]
    fn construction_touches_nothing() {
        let api = NativeApi::new(SourceKind::Kugou, Some("dfid=x".to_string()), None).unwrap();
        assert_eq!(api.cookie(), Some("dfid=x"));
        assert_eq!(api.kind(), SourceKind::Kugou);
    }

    /// `set_cookie` 要能改到传输层，否则登录后仍是匿名身份。
    #[test]
    fn set_cookie_reaches_the_transport() {
        let mut api = NativeApi::new(SourceKind::Kugou, None, None).unwrap();
        assert_eq!(api.cookie(), None);
        api.set_cookie(Some("token=T; userid=1".to_string()));
        assert_eq!(api.cookie(), Some("token=T; userid=1"));
    }

    /// 阶段 3 出口：native 自己拼的搜索 URL 与上游逐字节一致。
    #[test]
    fn search_endpoint_matches_kat() {
        assert_eq!(
            prepared_url(SourceKind::Kugou, &search_endpoint("周杰伦", 1, 30)),
            "https://gateway.kugou.com/v3/search/song?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&albumhide=0&iscorrection=1&keyword=%E5%91%A8%E6%9D%B0%E4%BC%A6&nocollect=0&page=1&pagesize=30&platform=AndroidFilter&signature=9bb2d7192e4ec15c72add9daaf9728a6"
        );
    }

    /// 概念版只有 `appid`/`clientver` 与签名变。
    #[test]
    fn lite_search_endpoint_matches_kat() {
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &search_endpoint("周杰伦", 1, 30)),
            "https://gateway.kugou.com/v3/search/song?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&albumhide=0&iscorrection=1&keyword=%E5%91%A8%E6%9D%B0%E4%BC%A6&nocollect=0&page=1&pagesize=30&platform=AndroidFilter&signature=bca333395e727512b680fba6df0e4b82"
        );
    }

    /// 阶段 3 出口：取链 URL 与上游逐字节一致（含 `encryptKey` 的 `key`）。
    #[test]
    fn song_url_endpoint_matches_kat() {
        assert_eq!(
            prepared_url(
                SourceKind::Kugou,
                &song_url_endpoint(SourceKind::Kugou, KAT_DFID.to_string(), "6af00fbd4d444a82c005843eef9dc2d4", "128", false),
            ),
            "https://gateway.kugou.com/v5/url?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=11430&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&album_id=0&area_code=1&hash=6af00fbd4d444a82c005843eef9dc2d4&ssa_flag=is_fromtrack&version=11430&page_id=151369488&quality=128&album_audio_id=0&behavior=play&pid=2&cmd=26&pidversion=3001&IsFreePart=0&ppage_id=463467626,350369493,788954147&cdnBackup=1&module=&key=1e5533fbcad17c9aa8935349a8b7c1d3&signature=8128fd5afa89324ac65a4c7b77b270e9"
        );
    }

    /// 概念版的 `page_id`/`pid`/`ppage_id` 三个值都不同。
    #[test]
    fn lite_song_url_endpoint_matches_kat() {
        assert_eq!(
            prepared_url(
                SourceKind::KugouConcept,
                &song_url_endpoint(SourceKind::KugouConcept, KAT_DFID.to_string(), "6af00fbd4d444a82c005843eef9dc2d4", "128", false),
            ),
            "https://gateway.kugou.com/v5/url?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11430&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&album_id=0&area_code=1&hash=6af00fbd4d444a82c005843eef9dc2d4&ssa_flag=is_fromtrack&version=11430&page_id=967177915&quality=128&album_audio_id=0&behavior=play&pid=411&cmd=26&pidversion=3001&IsFreePart=0&ppage_id=356753938&cdnBackup=1&module=&key=7d03ca1bba0d0fa1f5c8d55cdd957b8d&signature=64a711097ebb42883b47892ecec00214"
        );
    }

    /// `free_part` 只改 `IsFreePart`（`1`/`0`），签名随之变化。
    #[test]
    fn free_part_flips_is_free_part() {
        let full = prepared_url(
            SourceKind::Kugou,
            &song_url_endpoint(SourceKind::Kugou, KAT_DFID.to_string(), "6af00fbd4d444a82c005843eef9dc2d4", "128", false),
        );
        let trial = prepared_url(
            SourceKind::Kugou,
            &song_url_endpoint(SourceKind::Kugou, KAT_DFID.to_string(), "6af00fbd4d444a82c005843eef9dc2d4", "128", true),
        );
        assert!(full.contains("&IsFreePart=0&"), "{full}");
        assert!(trial.contains("&IsFreePart=1&"), "{trial}");
        assert_ne!(full, trial);
    }

    /// 空 `quality` 落到上游的 `quality || 128`。
    #[test]
    fn empty_quality_falls_back_to_128() {
        let url = prepared_url(
            SourceKind::Kugou,
            &song_url_endpoint(SourceKind::Kugou, KAT_DFID.to_string(), "6AF00FBD4D444A82C005843EEF9DC2D4", "", false),
        );
        assert!(url.contains("&quality=128&"), "{url}");
        // 上游 `(params?.hash || '').toLowerCase()`：大写 hash 必须被压成小写，
        // 否则服务端查不到文件。
        assert!(url.contains("&hash=6af00fbd4d444a82c005843eef9dc2d4&"), "{url}");
    }

    /// 阶段 3 出口：`/privilege/lite` 的 POST body 与上游逐字节一致。
    ///
    /// 这里锁的是 `resource[0].album_id` 为**空字符串**、`resource[1].album_id`
    /// 为**数字 0**——它们进 android 签名的输入，写错不会报错，只会 403。
    #[test]
    fn privilege_lite_body_matches_kat() {
        let song = Song {
            name: "测试".to_string(),
            hash: "6af00fbd4d444a82c005843eef9dc2d4".to_string(),
            extra_hashes: [(
                "hash_320".to_string(),
                "11111111111111111111111111111111".to_string(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let endpoint = privilege_lite_endpoint(SourceKind::Kugou, &song).unwrap();
        assert_eq!(
            endpoint.data.as_deref(),
            Some(r#"{"appid":1005,"area_code":1,"behavior":"play","clientver":20489,"need_hash_offset":1,"relate":1,"support_verify":1,"resource":[{"type":"audio","page_id":0,"hash":"6af00fbd4d444a82c005843eef9dc2d4","album_id":""},{"type":"audio","page_id":0,"hash":"11111111111111111111111111111111","album_id":0}],"qualities":["128","320","flac","high","viper_atmos","viper_tape","viper_clear","super","multitrack"]}"#)
        );
        assert_eq!(endpoint.method, crate::api::native::transport::Method::Post);
        assert_eq!(
            prepared_url(SourceKind::Kugou, &endpoint),
            "https://gateway.kugou.com/v2/get_res_privilege/lite?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=d21eec343b1f4dce059f1127625bbf5e"
        );
    }

    /// 概念版的 `/privilege/lite` body 里 `appid`/`clientver` 换成概念版的值。
    #[test]
    fn lite_privilege_lite_body_matches_kat() {
        let song = Song {
            name: "测试".to_string(),
            hash: "6af00fbd4d444a82c005843eef9dc2d4".to_string(),
            ..Default::default()
        };
        let endpoint = privilege_lite_endpoint(SourceKind::KugouConcept, &song).unwrap();
        let body = endpoint.data.as_deref().unwrap();
        assert!(body.contains(r#""appid":3116"#), "{body}");
        assert!(body.contains(r#""clientver":11440"#), "{body}");
        // 只有一个 hash 时 `resource` 只有一项，且 `album_id` 是空字符串。
        assert_eq!(body.matches("\"type\":\"audio\"").count(), 1, "{body}");
        assert!(body.contains(r#""album_id":"""#), "{body}");
    }

    /// `extra_hashes` 里的重复项要去重：上游 `Object.assign` 与我们的
    /// `HashSet` 都保证同一个 hash 不会进两次。
    #[test]
    fn duplicate_hashes_are_asked_once() {
        let song = Song {
            name: "测试".to_string(),
            hash: "6af00fbd4d444a82c005843eef9dc2d4".to_string(),
            extra_hashes: [
                ("hash_320".to_string(), "6af00fbd4d444a82c005843eef9dc2d4".to_string()),
                ("hash_128".to_string(), "22222222222222222222222222222222".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let endpoint = privilege_lite_endpoint(SourceKind::Kugou, &song).unwrap();
        let body = endpoint.data.as_deref().unwrap();
        assert_eq!(body.matches("\"type\":\"audio\"").count(), 2, "{body}");
        assert!(!body.contains("11111111111111111111111111111111"));
    }

    /// 未登录时 `song_url` 用的 dfid 是 24 字符随机串，不是 `-`。
    #[test]
    fn anonymous_dfid_is_a_fresh_24_char_string() {
        let api = NativeApi::new(SourceKind::Kugou, None, None).unwrap();
        assert!(!api.transport.cookie_map().contains_key("dfid"));
        let first = random_string(24, &mut random_f64);
        let second = random_string(24, &mut random_f64);
        assert_eq!(first.len(), 24);
        assert_eq!(second.len(), 24);
        assert_ne!(first, second, "两次调用不该拿到同一个 dfid");
    }

    /// 阶段 4 出口：歌词搜索 URL 与上游逐字节一致。
    ///
    /// 这个端点带 `clearDefaultParams`，所以 URL 里**没有** `dfid`/`mid`/`uuid`/
    /// `clienttime`/`token`/`userid`——`appid`/`clientver` 是模块自己写进 dataMap
    /// 的。少了这个 `clear_defaults()` 会多出六个参数，签名随之全错。
    #[test]
    fn search_lyric_endpoint_matches_kat() {
        assert_eq!(
            prepared_url(SourceKind::Kugou, &search_lyric_endpoint(SourceKind::Kugou, &kat_song())),
            "https://lyrics.kugou.com/v1/search?album_audio_id=0&appid=1005&clientver=20489&duration=243722&hash=6af00fbd4d444a82c005843eef9dc2d4&keyword=Letter+-+arkady+sevidov&lrctxt=1&man=yes&signature=b90333d489a1aae225eb18ac61718d35"
        );
    }

    /// 概念版歌词搜索：只有 `appid`/`clientver` 与签名变。
    #[test]
    fn lite_search_lyric_endpoint_matches_kat() {
        assert_eq!(
            prepared_url(
                SourceKind::KugouConcept,
                &search_lyric_endpoint(SourceKind::KugouConcept, &kat_song())
            ),
            "https://lyrics.kugou.com/v1/search?album_audio_id=0&appid=3116&clientver=11440&duration=243722&hash=6af00fbd4d444a82c005843eef9dc2d4&keyword=Letter+-+arkady+sevidov&lrctxt=1&man=yes&signature=9f42c6d69256fa6e952cf44c547d0cbf"
        );
    }

    /// 阶段 4 出口：歌词下载 URL 与上游逐字节一致（含登录态的 `token`/`userid`）。
    #[test]
    fn lyric_endpoint_matches_kat() {
        let endpoint = lyric_endpoint("19525574", "0123456789ABCDEF0123456789ABCDEF");
        assert_eq!(
            prepared_url(SourceKind::Kugou, &endpoint),
            "https://lyrics.kugou.com/download?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&ver=1&client=android&id=19525574&accesskey=0123456789ABCDEF0123456789ABCDEF&fmt=krc&charset=utf8&signature=baf87af98752ef7562e9e4756f56cf32"
        );
    }

    /// 概念版歌词下载：`appid`/`clientver`/签名变，参数序不变。
    #[test]
    fn lite_lyric_endpoint_matches_kat() {
        let endpoint = lyric_endpoint("19525574", "0123456789ABCDEF0123456789ABCDEF");
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &endpoint),
            "https://lyrics.kugou.com/download?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&ver=1&client=android&id=19525574&accesskey=0123456789ABCDEF0123456789ABCDEF&fmt=krc&charset=utf8&signature=d3eeb438cbbe61bfd53bcf71999376c5"
        );
    }

    /// `contenttype == 0` 走 KRC 解密，解出的明文与上游 `decodeLyrics` 逐字相同。
    ///
    /// 样本来自真实 `/lyric` 响应（`tools/kat/krc_probe.json`），已脱敏为
    /// 「纯音乐，请欣赏」这一句。
    #[test]
    fn injects_krc_decoded_content_when_contenttype_is_zero() {
        let probe: Value = serde_json::from_str(include_str!("../../../tools/kat/krc_probe.json"))
            .expect("krc_probe.json 应当能解析");
        let mut root = probe.clone();
        inject_decoded_lyric(&mut root);

        let decoded = root["decodeContent"].as_str().unwrap();
        assert_eq!(decoded, probe["decodeContent"].as_str().unwrap());
        assert!(decoded.contains("<0,354,0>纯"), "逐字时间戳要保留：{decoded}");
    }

    /// `contenttype != 0` 走 base64 而不是 KRC——少了这个分支会解出乱码。
    #[test]
    fn injects_base64_content_when_contenttype_is_not_zero() {
        let mut root = serde_json::json!({
            "status": 200,
            "contenttype": 1,
            "content": "aGVsbG8gd29ybGQ=",
        });
        inject_decoded_lyric(&mut root);
        assert_eq!(root["decodeContent"], Value::String("hello world".to_string()));
    }

    /// 解不开的 KRC 折成**空串**而不是报错：上游 `decodeLyrics` 也是
    /// `catch { return '' }`，两端最终都走到「歌词为空」。
    #[test]
    fn undecodable_krc_becomes_an_empty_string() {
        let mut root = serde_json::json!({
            "status": 200,
            "contenttype": 0,
            "content": "bm90LWEta3JjLXBheWxvYWQ=",
        });
        inject_decoded_lyric(&mut root);
        assert_eq!(root["decodeContent"], Value::String(String::new()));
    }

    /// 非 JSON 响应（纯文本歌词）不做任何注入——`lyric_body` 会原样透传。
    #[test]
    fn plain_text_body_is_passed_through() {
        // `inject_decoded_lyric` 只处理对象；字符串根节点不该 panic。
        let mut root = Value::String("[00:01.00]hi".to_string());
        inject_decoded_lyric(&mut root);
        assert_eq!(root, Value::String("[00:01.00]hi".to_string()));
    }

    /// 真实整首歌的 KAT：native 解出的明文必须与 Node 版 `decode=true` 的
    /// `decodeContent` **逐字节相同**，解析出的逐字时间戳也必须一致。
    ///
    /// 样本是真实 `/lyric` 响应（`tools/kat/krc_real_qt.json`，周杰伦《晴天》，
    /// 68 行、其中 63 行带时间标签）。`decodeContent` 那一栏是**上游 Node 版自己
    /// 解出来的**，不是本地推导值——所以这条断言同时锁住了「解密算法一致」和
    /// 「逐字时间戳一致」两件事。
    ///
    /// 解密正确但偏移错、或丢了一个字，都会让下面 `故事的小黄花` 的时间戳对不上。
    #[test]
    fn real_song_decode_matches_node_byte_for_byte() {
        let probe: Value =
            serde_json::from_str(include_str!("../../../tools/kat/krc_real_qt.json"))
                .expect("krc_real_qt.json 应当能解析");
        let expected = probe["decodeContent"].as_str().unwrap();

        let mut root = probe.clone();
        inject_decoded_lyric(&mut root);
        let decoded = root["decodeContent"].as_str().unwrap();
        assert_eq!(decoded, expected, "native 解密结果与 Node 版不一致");

        let lyric = crate::api::lyric::parse_lrc(decoded);
        assert_eq!(lyric.lines.len(), 63, "定时行数");

        let line = lyric
            .lines
            .iter()
            .find(|l| l.text == "故事的小黄花")
            .expect("应当有「故事的小黄花」这一行");
        assert_eq!(line.time_ms, 29264);
        assert_eq!(line.words.len(), 6, "六个字各有一个逐字区间");
        let starts: Vec<u64> = line.words.iter().map(|w| w.start_ms).collect();
        assert_eq!(
            starts,
            vec![29264, 29654, 30046, 30494, 31416, 31790]
        );
        let ends: Vec<u64> = line.words.iter().map(|w| w.end_ms).collect();
        assert_eq!(ends, vec![29654, 30046, 30494, 31416, 31790, 32294]);

        // 首行是 `[0,2250]<0,160,0>晴天…`，19 个字全部带逐字区间。
        let first = &lyric.lines[0];
        assert_eq!(first.time_ms, 0);
        assert_eq!(first.text, "晴天 - 周杰伦 (Jay Chou)");
        assert_eq!(first.words.len(), 19);
        assert_eq!(first.words[0].start_ms, 0);
        assert_eq!(first.words[0].end_ms, 160);
    }

    /// 与上游 KAT 同一首歌：`singer_text() - name` 要拼出 `Letter - arkady sevidov`。
    fn kat_song() -> Song {        Song {
            name: "arkady sevidov".to_string(),
            hash: "6af00fbd4d444a82c005843eef9dc2d4".to_string(),
            duration_ms: 243722,
            singers: vec![crate::api::model::Singer {
                name: "Letter".to_string(),
                id: 0,
            }],
            ..Default::default()
        }
    }
}
