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

// 尚未接入网络的底层纯函数。`sign` 与 `device` 已被 `transport` 用上，
// 不再需要 allow；`crypto`（RSA/AES）与 `krc`（歌词解密）分别等阶段 5、阶段 4。
#[allow(dead_code)]
pub mod crypto;
pub mod device;
#[allow(dead_code)]
pub mod krc;
pub mod sign;
pub mod transport;

use crate::api::catalog::{StreamSource, StreamUrl, resolve_stream_url};
use crate::api::cloud::{QrCheck, UserInfo, VipInfo};
use crate::api::data_of;
use crate::api::model::{Artist, Lyric, Playlist, RankBoard, Song, extract_songs};
use crate::api::native::device::random_string;
use crate::api::native::transport::{Endpoint, GATEWAY_BASE, Transport};
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

    async fn fetch_lyric(&self, _song: &Song) -> Result<Lyric> {
        Err(unimplemented("fetch_lyric"))
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
}
