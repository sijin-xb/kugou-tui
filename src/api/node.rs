//! 走本机 KuGouMusicApi 的后端实现。
//!
//! [`NodeApi`] 只是给 [`HttpClient`] 套一层类型，好让酷狗的接口语义
//! （`catalog` / `cloud` / `lyric` 三个文件里的方法体）挂在一个**实现了
//! [`MusicApi`]** 的类型上。
//!
//! # 为什么接口方法体不写在 `impl MusicApi for NodeApi` 里
//!
//! 一个类型只能有一个 `impl Trait for Type` 块，而酷狗的接口按主题分了三个文件。
//! 所以三个文件里写的是**同名 inherent 方法**（`impl NodeApi { ... }`），
//! 本文件末尾的 trait 实现只做一行转发。
//!
//! 方法调用时 inherent 优先于 trait，因此 `NodeApi::search_songs(self, ..)`
//! 解析到 inherent 那个，不会递归回 trait 实现——这条已用最小样例验证过。
//! 反过来说，**别把这里的转发写成 `self.search_songs(..)` 以外的形式**，
//! 也别删掉同名 inherent 方法，否则会变成无限递归。

use serde_json::Value;

use crate::api::catalog::StreamUrl;
use crate::api::client::HttpClient;
use crate::api::cloud::{QrCheck, UserInfo, VipInfo};
use crate::api::model::{Artist, Lyric, Playlist, RankBoard, Song};
use crate::api::traits::MusicApi;
use crate::error::Result;
use crate::source::SourceKind;

/// KuGouMusicApi（Node）后端。
#[derive(Debug, Clone)]
pub struct NodeApi {
    http: HttpClient,
}

impl NodeApi {
    /// 构造。`base` 形如 `http://127.0.0.1:3000`，`proxy` 形如 `http://127.0.0.1:7890`。
    pub fn new(base: &str, cookie: Option<String>, proxy: Option<&str>) -> Result<Self> {
        Ok(Self {
            http: HttpClient::new(base, cookie, proxy)?,
        })
    }

    pub fn base(&self) -> &str {
        self.http.base()
    }

    pub fn cookie(&self) -> Option<&str> {
        self.http.cookie()
    }

    pub fn set_cookie(&mut self, cookie: Option<String>) {
        self.http.set_cookie(cookie);
    }

    /// 透出传输层。网易云音源自己那套接口语义直接架在 `HttpClient` 上。
    pub(crate) fn transport(&self) -> &HttpClient {
        &self.http
    }

    // ------------------------------------------------------------------
    // 传输转发：让 `catalog` / `cloud` / `lyric` 的方法体一行都不用改
    // ------------------------------------------------------------------

    pub(crate) async fn get_json(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.http.get_json(path, query).await
    }

    pub(crate) async fn get_json_uncached(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value> {
        self.http.get_json_uncached(path, query).await
    }

    pub(crate) async fn get_text(&self, path: &str, query: &[(&str, String)]) -> Result<String> {
        self.http.get_text(path, query).await
    }

    pub(crate) async fn get_json_mutating(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value> {
        self.http.get_json_mutating(path, query).await
    }

    pub(crate) async fn get_json_uncached_mutating(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value> {
        self.http.get_json_uncached_mutating(path, query).await
    }
}

// 一行转发。方法体在 catalog.rs / cloud.rs / lyric.rs 的同名 inherent 方法里。
impl MusicApi for NodeApi {
    async fn search_songs(&self, keywords: &str, page: u32, page_size: u32) -> Result<Vec<Song>> {
        NodeApi::search_songs(self, keywords, page, page_size).await
    }

    async fn plaza_playlists(
        &self,
        category_id: i64,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<Playlist>> {
        NodeApi::plaza_playlists(self, category_id, page, page_size).await
    }

    async fn playlist_tracks(
        &self,
        global_id: &str,
        page: u32,
        page_size: u32,
        fresh: bool,
    ) -> Result<Vec<Song>> {
        NodeApi::playlist_tracks(self, global_id, page, page_size, fresh).await
    }

    async fn user_playlists(&self) -> Result<Vec<Playlist>> {
        NodeApi::user_playlists(self).await
    }

    async fn user_playlist_tracks(
        &self,
        list_id: i64,
        page: u32,
        page_size: u32,
        fresh: bool,
    ) -> Result<Vec<Song>> {
        NodeApi::user_playlist_tracks(self, list_id, page, page_size, fresh).await
    }

    async fn artist_list(&self, kind: i64, hot_size: u32) -> Result<Vec<Artist>> {
        NodeApi::artist_list(self, kind, hot_size).await
    }

    async fn rank_boards(&self) -> Result<Vec<RankBoard>> {
        NodeApi::rank_boards(self).await
    }

    async fn playlist_tracks_all(&self, global_id: &str, fresh: bool) -> Result<Vec<Song>> {
        NodeApi::playlist_tracks_all(self, global_id, fresh).await
    }

    async fn user_playlist_tracks_all(&self, list_id: i64, fresh: bool) -> Result<Vec<Song>> {
        NodeApi::user_playlist_tracks_all(self, list_id, fresh).await
    }

    async fn artist_tracks_all(&self, artist_id: i64, sort: &str) -> Result<Vec<Song>> {
        NodeApi::artist_tracks_all(self, artist_id, sort).await
    }

    async fn rank_tracks_all(&self, rank_id: i64) -> Result<Vec<Song>> {
        NodeApi::rank_tracks_all(self, rank_id).await
    }

    async fn song_stream_url(&self, song: &Song, quality: &str) -> Result<StreamUrl> {
        NodeApi::song_stream_url(self, song, quality).await
    }

    async fn fetch_lyric(&self, song: &Song) -> Result<Lyric> {
        crate::api::lyric::fetch_lyric_via(self, song).await
    }

    async fn login_qr_key(&self) -> Result<String> {
        NodeApi::login_qr_key(self).await
    }

    async fn login_qr_create(&self, key: &str) -> Result<String> {
        NodeApi::login_qr_create(self, key).await
    }

    async fn login_qr_check(&self, key: &str) -> Result<QrCheck> {
        NodeApi::login_qr_check(self, key).await
    }

    async fn user_detail(&self) -> Result<UserInfo> {
        NodeApi::user_detail(self).await
    }

    async fn user_vip_detail(&self) -> Result<VipInfo> {
        NodeApi::user_vip_detail(self).await
    }

    async fn claim_day_vip(&self, receive_day: &str) -> Result<Value> {
        NodeApi::claim_day_vip(self, receive_day).await
    }

    async fn upgrade_day_vip(&self) -> Result<Value> {
        NodeApi::upgrade_day_vip(self).await
    }

    async fn claimed_vip_days(&self) -> Result<Vec<String>> {
        NodeApi::claimed_vip_days(self).await
    }

    async fn fetch_device_fingerprint(&self) -> Result<String> {
        NodeApi::fetch_device_fingerprint(self).await
    }

    async fn add_tracks_to_playlist(
        &self,
        source: SourceKind,
        list_id: i64,
        songs: &[Song],
    ) -> Result<usize> {
        NodeApi::add_tracks_to_playlist(self, source, list_id, songs).await
    }

    async fn remove_tracks_from_playlist(
        &self,
        source: SourceKind,
        list_id: i64,
        songs: &[Song],
    ) -> Result<usize> {
        NodeApi::remove_tracks_from_playlist(self, source, list_id, songs).await
    }

    async fn delete_playlist(&self, source: SourceKind, list_id: i64) -> Result<()> {
        NodeApi::delete_playlist(self, source, list_id).await
    }

    async fn create_playlist(&self, source: SourceKind, name: &str) -> Result<Option<i64>> {
        NodeApi::create_playlist(self, source, name).await
    }
}
