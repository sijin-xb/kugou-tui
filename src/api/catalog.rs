//! 目录类接口：搜索、歌单、歌手、排行榜、播放直链。
//!
//! 所有路径都对照 KuGouMusicApi 的接口文档核对过：
//!
//! | 功能 | 路径 |
//! |---|---|
//! | 单曲搜索 | `GET /search?type=song` |
//! | 歌单广场 | `GET /top/playlist?category_id=` |
//! | 歌单详情 | `GET /playlist/detail?ids=` |
//! | 歌单歌曲 | `GET /playlist/track/all?id=` |
//! | 用户歌单 | `GET /user/playlist` |
//! | 用户歌单歌曲 | `GET /playlist/track/all/new?listid=` |
//! | 歌手列表 | `GET /artist/lists` |
//! | 歌手单曲 | `GET /artist/audios?id=` |
//! | 排行榜列表 | `GET /rank/list` |
//! | 排行榜歌曲 | `GET /rank/audio?rankid=` |
//! | 播放直链 | `GET /song/url?hash=` |
//!
//! # 关于搜索
//!
//! 上游 `/search` 支持 `type=song|special|author`，但本客户端**只用了 `type=song`**：
//! 歌单与歌手改为在各自的标签页里浏览完整目录，一次搜索只发一个请求。
//!
//! # 关于分页
//!
//! `pagesize` 在歌单类接口上是**硬上限 30**（传更大值也只回 30 条），
//! 且响应里的 `count` 只是回显当页条数、不是总数。因此"取全"一律靠翻页，
//! 见本文件末尾的 `*_all` 系列函数。

use serde_json::Value;

use crate::api::client::ApiClient;
use crate::api::model::{
    Artist, Playlist, RankBoard, Song, artist_from_json, extract_songs, pick_array, pick_i64,
    pick_string, playlist_from_json, rank_board_from_json,
};
use crate::api::{data_of, extract_list};
use crate::error::{AppError, Result};
use crate::logger::tlog;

/// 歌单 / 榜单类接口每页的硬上限。首屏加载也用它，保证与翻页一致。
///
/// 实测 `/playlist/track/all` 传 `pagesize=100|300|500` 都只回 30 条，所以取全只能翻页。
/// 榜单接口其实能吃下更大的值，但统一用这个上限更省心，也免得榜单扩容后要回头改。
pub(crate) const PAGE_LIMIT: u32 = 30;

/// 翻页上限，防止接口异常（比如始终回满页）时无限循环。60 页 = 1800 首，足够极端歌单。
const MAX_PAGES: u32 = 60;

/// 并发翻页时一批发多少页。太大容易触发上游限流，太小提速不明显；6 是个折中。
const CONCURRENT_PAGES: u32 = 6;

impl ApiClient {
    // ------------------------------------------------------------------
    // 搜索
    // ------------------------------------------------------------------

    /// 单曲搜索。
    ///
    /// 需要认证信息：cookie 缺失时服务端返回 `error_code: 152`，
    /// [`AppError::is_auth_related`] 会把它翻译成「需要登录」提示。
    pub async fn search_songs(
        &self,
        keywords: &str,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<Song>> {
        let root = self
            .get_json_uncached(
                "/search",
                &[
                    ("keywords", keywords.to_string()),
                    ("type", "song".to_string()),
                    ("page", page.to_string()),
                    ("pagesize", page_size.to_string()),
                ],
            )
            .await?;
        Ok(extract_songs(data_of(&root)))
    }

    // ------------------------------------------------------------------
    // 歌单
    // ------------------------------------------------------------------

    /// 歌单广场。`category_id = 0` 表示推荐，`11292` 是 HI-RES。
    pub async fn plaza_playlists(
        &self,
        category_id: i64,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<Playlist>> {
        let root = self
            .get_json(
                "/top/playlist",
                &[
                    ("category_id", category_id.to_string()),
                    ("withsong", "0".to_string()),
                    ("withtag", "1".to_string()),
                    ("page", page.to_string()),
                    ("pagesize", page_size.to_string()),
                ],
            )
            .await?;
        Ok(collect_playlists(&root, false))
    }

    /// 歌单内一页歌曲（公开歌单，用 `global_collection_id`）。
    ///
    /// # `pagesize` 是硬上限 30
    ///
    /// 实测传 `100` / `300` / `500`，服务端一律只回 30 条；而 `data.count` 只是
    /// **回显当页条数**，不是总数（歌单元数据里的 `percount` 恒为 0，也拿不到总数）。
    /// 所以要取全必须自己翻页，见 [`Self::playlist_tracks_all`]。
    pub async fn playlist_tracks(
        &self,
        global_id: &str,
        page: u32,
        page_size: u32,
        fresh: bool,
    ) -> Result<Vec<Song>> {
        let query = [
            ("id", global_id.to_string()),
            ("page", page.to_string()),
            ("pagesize", page_size.to_string()),
        ];
        let root = if fresh {
            self.get_json_uncached("/playlist/track/all", &query)
                .await?
        } else {
            self.get_json("/playlist/track/all", &query).await?
        };
        Ok(extract_songs(data_of(&root)))
    }

    /// 当前用户歌单（含自建与收藏）。
    pub async fn user_playlists(&self) -> Result<Vec<Playlist>> {
        let root = self
            .get_json_uncached(
                "/user/playlist",
                &[("page", "1".to_string()), ("pagesize", "100".to_string())],
            )
            .await?;
        Ok(collect_playlists(&root, true))
    }

    /// 用户歌单内的歌曲（新版接口，按数字 `listid`）。
    ///
    /// `fresh` 为 `true` 时走 `get_json_uncached`（带一次性时间戳），
    /// 绕开服务端那 2 分钟缓存——
    /// 刷新、以及加歌/删歌之后重新拉取时必须为真，否则看到的是旧列表。
    pub async fn user_playlist_tracks(
        &self,
        list_id: i64,
        page: u32,
        page_size: u32,
        fresh: bool,
    ) -> Result<Vec<Song>> {
        let query = [
            ("listid", list_id.to_string()),
            ("page", page.to_string()),
            ("pagesize", page_size.to_string()),
        ];
        let root = if fresh {
            self.get_json_uncached("/playlist/track/all/new", &query)
                .await?
        } else {
            self.get_json("/playlist/track/all/new", &query).await?
        };
        Ok(extract_songs(data_of(&root)))
    }

    // ------------------------------------------------------------------
    // 歌手
    // ------------------------------------------------------------------

    /// 歌手列表。`kind`: 0 全部 / 1 华语 / 2 欧美 / 3 日韩。
    pub async fn artist_list(&self, kind: i64, hot_size: u32) -> Result<Vec<Artist>> {
        let root = self
            .get_json(
                "/artist/lists",
                &[
                    ("sextypes", "0".to_string()),
                    ("type", kind.to_string()),
                    ("musician", "0".to_string()),
                    ("hotsize", hot_size.to_string()),
                ],
            )
            .await?;
        Ok(collect_artists(&root))
    }

    /// 歌手单曲。`sort`: `hot` 热门 / `new` 最新。
    pub async fn artist_tracks(
        &self,
        artist_id: i64,
        sort: &str,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<Song>> {
        let root = self
            .get_json(
                "/artist/audios",
                &[
                    ("id", artist_id.to_string()),
                    ("sort", sort.to_string()),
                    ("page", page.to_string()),
                    ("pagesize", page_size.to_string()),
                ],
            )
            .await?;
        Ok(extract_songs(data_of(&root)))
    }

    // ------------------------------------------------------------------
    // 排行榜
    // ------------------------------------------------------------------

    /// 排行榜列表。
    pub async fn rank_boards(&self) -> Result<Vec<RankBoard>> {
        let root = self
            .get_json("/rank/list", &[("withsong", "0".to_string())])
            .await?;
        Ok(extract_list(
            &root,
            &["info", "list", "rank_list"],
            rank_board_from_json,
        ))
    }

    /// 排行榜歌曲。
    pub async fn rank_tracks(&self, rank_id: i64, page: u32, page_size: u32) -> Result<Vec<Song>> {
        let root = self
            .get_json(
                "/rank/audio",
                &[
                    ("rankid", rank_id.to_string()),
                    ("page", page.to_string()),
                    ("pagesize", page_size.to_string()),
                ],
            )
            .await?;
        Ok(extract_songs(data_of(&root)))
    }

    // ------------------------------------------------------------------
    // 取全（翻页）
    // ------------------------------------------------------------------

    /// 逐页取全量歌曲的通用实现（**并发**翻页）。
    ///
    /// # 为什么要并发
    ///
    /// 歌单接口硬限每页 30 条，400 首歌就是 14 次往返。串行做的话 RTT 直接累加，
    /// 大歌单要等好几秒——而这几秒里界面只有一个"加载中"。改成一批页同时发，
    /// 耗时就接近单页的延迟。
    ///
    /// # 批次策略
    ///
    /// 不知道总数，所以**先取第 1 页**：它满页说明后面可能还有，就按
    /// [`CONCURRENT_PAGES`] 一批继续取；某一页不满就停（同串行版的判定）。
    /// 一批里只要有一页失败就整体报错，避免静默漏歌。
    async fn collect_all_pages<F, Fut>(&self, make: F) -> Result<Vec<Song>>
    where
        F: Fn(ApiClient, u32) -> Fut,
        Fut: std::future::Future<Output = Result<Vec<Song>>> + Send + 'static,
    {
        let mut all: Vec<Song> = Vec::new();

        // 第 1 页单独取：确定后面还有没有内容，避免一上来就并发一堆空请求
        let first = make(self.clone(), 1).await?;
        // 只有**空**才说明真没了。
        //
        // 不能用「不足 PAGE_LIMIT」判断：解析时会有条目被过滤掉
        // （缺 hash、字段类型不对等），一页 30 条剩 29 条是常事，
        // 那样会误判成"没有下一页"，歌单直接被截断在 29 首。
        all.extend(first);
        if all.is_empty() {
            return Ok(all);
        }

        let mut page: u32 = 2;
        while page <= MAX_PAGES {
            let batch_end = (page + CONCURRENT_PAGES - 1).min(MAX_PAGES);

            let mut set = tokio::task::JoinSet::new();
            for batch_page in page..=batch_end {
                // 每页一份克隆：ApiClient 内部是 Arc，克隆很便宜。
                // 把页码跟着结果一起搬回来——下面要按页码重排，见 `collected`。
                let future = make(self.clone(), batch_page);
                set.spawn(async move { future.await.map(|songs| (batch_page, songs)) });
            }

            // `join_next()` 返回的是**任务完成顺序**，不是发起顺序。并发请求谁先回来
            // 是不确定的，直接按返回顺序 `extend` 会让整表的页序被打乱：
            // 榜单尤其致命——它的「顺序」就是榜单本身的内容。
            //
            // 所以先把一批的结果收进 `collected`，再按页码排好序拼上去。
            let mut collected: Vec<(u32, Vec<Song>)> = Vec::with_capacity(
                (batch_end.saturating_sub(page) + 1)
                    .try_into()
                    .unwrap_or(usize::MAX),
            );

            let mut stop = false;
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(Ok((_batch_page, songs))) => {
                        // 同理，只有空页才停；短页后面可能还有内容
                        if songs.is_empty() {
                            stop = true;
                        }
                        collected.push((_batch_page, songs));
                    }
                    Ok(Err(error)) => {
                        // 页码越界 = 后面没有了，属正常终止，保留已取到的内容。
                        if error.is_page_out_of_range() {
                            stop = true;
                            continue;
                        }

                        // 后续某页失败（限流、超时、上游偶发错误）：
                        // **不再让整次失败**，而是记日志并停在已取到的内容上。
                        //
                        // 之前整次失败时，界面会退回首屏那一页——用户刚做完一次
                        // 搜索、紧接着打开几百首的歌单时很容易碰到，表现为
                        // "明明有几百首却只显示 30 首"。部分结果比没有强，
                        // 而且首屏已经显示过了，中断只是少后面几页。
                        if all.is_empty() {
                            // 一首都还没取到，那确实是失败
                            return Err(error);
                        }
                        crate::logger::tlog!(
                            crate::logger::LEVEL_WARN,
                            "翻到第 {page} 页起失败，保留已取到的 {} 首：{error}",
                            all.len()
                        );
                        stop = true;
                    }
                    Err(error) => {
                        // 任务本身panic/取消。不该发生，报出来而不是静默丢页
                        crate::logger::tlog!(crate::logger::LEVEL_WARN, "翻页任务失败：{error}");
                        return Err(AppError::Other(format!("翻页任务失败：{error}")));
                    }
                }
            }

            // 按页码重排再拼——顺序错了整张表就是乱的（见上面那条注释）
            collected.sort_by_key(|(batch_page, _)| *batch_page);
            for (_batch_page, songs) in collected {
                all.extend(songs);
            }

            if stop {
                break;
            }
            page = batch_end + 1;
        }

        Ok(all)
    }

    /// 歌单内**全部**歌曲（公开歌单）。
    pub async fn playlist_tracks_all(&self, global_id: &str, fresh: bool) -> Result<Vec<Song>> {
        self.collect_all_pages(|client, page| {
            let global_id = global_id.to_string();
            async move {
                client
                    .playlist_tracks(&global_id, page, PAGE_LIMIT, fresh)
                    .await
            }
        })
        .await
    }

    /// 用户歌单内**全部**歌曲（自建/收藏，按数字 `listid`）。
    pub async fn user_playlist_tracks_all(&self, list_id: i64, fresh: bool) -> Result<Vec<Song>> {
        self.collect_all_pages(move |client, page| async move {
            client
                .user_playlist_tracks(list_id, page, PAGE_LIMIT, fresh)
                .await
        })
        .await
    }

    /// 歌手**全部**歌曲。`sort` 同 [`Self::artist_tracks`]。
    pub async fn artist_tracks_all(&self, artist_id: i64, sort: &str) -> Result<Vec<Song>> {
        self.collect_all_pages(|client, page| {
            let sort = sort.to_string();
            async move {
                client
                    .artist_tracks(artist_id, &sort, page, PAGE_LIMIT)
                    .await
            }
        })
        .await
    }

    /// 榜单**全部**歌曲。
    ///
    /// `/rank/audio` 不像歌单那样硬限 30（传 100 能回 100），但统一按页取更省心，
    /// 也避免榜单扩容后要回头改。
    pub async fn rank_tracks_all(&self, rank_id: i64) -> Result<Vec<Song>> {
        self.collect_all_pages(move |client, page| async move {
            client.rank_tracks(rank_id, page, PAGE_LIMIT).await
        })
        .await
    }

    // ------------------------------------------------------------------
    // 播放直链
    // ------------------------------------------------------------------

    /// 取播放直链。
    ///
    /// 注意：该接口依赖 `dfid`，缺失时酷狗会返回「本次请求需要验证」并给出空 url。
    /// 调用方应保证 [`crate::api::cloud::ApiClient::fetch_device_fingerprint`] 已成功执行。
    ///
    /// # 为什么分两步
    ///
    /// `free_part=true` 的含义是「返回试听部分」。实测只要带上它，服务端就直接给
    /// 60 秒试听片段（937 KiB ≈ 60 秒），**即使账号有会员也被降级**——这正是
    /// 「VIP 歌曲只能听几十秒」的直接原因。
    ///
    /// 所以先按完整版请求；只有完整版确实拿不到（未登录 / 会员类型不匹配 /
    /// 需单独购买）时，才退而求其次要试听片段，并明确标记 [`StreamUrl::is_trial`]，
    /// 由界面告诉用户这是片段、不要误当成播完了。
    pub async fn song_stream_url(&self, song: &Song, quality: &str) -> Result<StreamUrl> {
        // 登录用户先调 `/privilege/lite` 问「这账号能听哪几档音质」——不同音质的
        // hash 不一样（VIP 用户有 flac 的 hash，普通用户没有），用同一个 hash
        // 试所有音质会一直碰壁。**这是「设了 flac 但没 VIP 就只能听试听片段」的
        // 真凶**：之前直接拿原 hash 调 `/song/url`，服务端一看这个 hash 没 flac
        // 权限就给空 url。
        //
        // 未登录或 `/privilege/lite` 失败时回退到「原 hash + 用户选的音质」——
        // 不能因为这个查询挂了就完全走不通。
        let mut candidates = self.privilege_candidates(song, quality).await;

        // 蝰蛇系列（viper_clear / viper_atmos / viper_tape）是酷狗的**付费加项**，
        // 需要独立的「蝰蛇 VIP」——普通 VIP / TVIP 账号设了它，上游会直接拒绝
        // （实测 error_code 31863），表现为「所有歌都播不了」。
        //
        // 兜底思路：**降到这个账号实际能用的最高音质**，而不是无条件降到 128。
        // 用户要的是「能听」，不是「能听但音质最差」——他是 VIP 就该拿到 flac
        // 而不是 128。所以先从 `/privilege/lite` 给的可用音质里挑一个非蝰蛇的
        // （那份列表已经是该账号有权限的），实在没有才退到 128。
        if VIPER_QUALITIES.contains(&quality) {
            let fallback = candidates
                .iter()
                .map(|(_, candidate_quality)| candidate_quality)
                .find(|candidate_quality| !VIPER_QUALITIES.contains(&candidate_quality.as_str()))
                .cloned()
                .unwrap_or_else(|| VIPER_FALLBACK_QUALITY.to_string());
            candidates.push((song.hash.clone(), fallback));
        }

        let mut last_full = None;
        for (hash, q) in &candidates {
            // 单个候选失败（比如服务端对某个组合直接 502）**不能**中断整个流程
            // ——我们要的是「试出第一个能用的组合」，不是「第一个组合必须成功」。
            let response = match self.request_song_url_with_hash(song, hash, q, false).await {
                Ok(response) => response,
                Err(error) => {
                    tlog!(
                        crate::logger::LEVEL_DEBUG,
                        "/song/url 候选失败（hash={hash} quality={q}）：{}，继续下一个",
                        error.user_hint()
                    );
                    continue;
                }
            };
            if let Some(url) = extract_stream_url(&response) {
                let degraded = q != quality && VIPER_QUALITIES.contains(&quality);
                return Ok(StreamUrl {
                    url,
                    is_trial: false,
                    reason: degraded
                        .then(|| format!("{quality} 不可用（需要蝰蛇 VIP），已降级到标准 {q}")),
                });
            }
            last_full = Some(response);
        }

        // 完整版拿不到，先记下原因再退到试听片段
        let reason = last_full.as_ref().and_then(|root| self.fail_reason(root));

        // 试听兜底：这一路失败也别抛出去了——上面完整版已经试过一轮，能到这里说明
        // 全都没成，应该给用户一个**完整的原因**而不是最后这次的网络错误。
        let trial = self
            .request_song_url_with_hash(song, &song.hash, quality, true)
            .await
            .unwrap_or_else(|error| {
                tlog!(
                    crate::logger::LEVEL_DEBUG,
                    "试听兜底请求失败：{}",
                    error.user_hint()
                );
                Value::Null
            });
        if let Some(url) = extract_stream_url(&trial) {
            return Ok(StreamUrl {
                url,
                is_trial: true,
                reason,
            });
        }

        // 两次都没有。优先把服务端给的原因透出去，比笼统的「可能需要 VIP」有用得多。
        for root in last_full.iter().chain(std::iter::once(&trial)) {
            // KuGouMusicApi 的 `/song/url` 把真正的失败信息放在 `data` 嵌套层里，
            // 顶层只有元数据——所以读两层。
            let data = root.get("data").unwrap_or(root);
            if let Some(message) = pick_string(data, &["error", "error_msg", "msg", "errmsg"]) {
                let status = pick_i64(data, &["status"]).unwrap_or(0);
                let reason = self.status_reason(status).unwrap_or(message);
                return Err(AppError::Api {
                    path: "/song/url".to_string(),
                    code: pick_i64(data, &["errcode", "error_code"]).unwrap_or(status),
                    message: reason,
                });
            }

            // `fail_process` 会说明卡在哪一步，实测见过 ["pkg","buy"]（需购买/开通）
            if let Some(process) = data.get("fail_process").and_then(Value::as_array)
                && !process.is_empty()
            {
                let steps = process
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("/");
                return Err(AppError::NotFound(format!(
                    "《{}》需要开通或购买（{}）",
                    song.name, steps
                )));
            }
        }

        Err(AppError::NotFound(format!(
            "《{}》没有可用的播放地址（可能需要 VIP、已下架或版权受限）",
            song.name
        )))
    }

    /// 把这一首歌的**所有** hash 配上用户选的音质，作为兜底候选。
    ///
    /// 顺序：主 hash 在前，`audio_info` 里的其它 hash 在后。
    ///
    /// # 为什么不能只用 `song.hash`
    ///
    /// 搜索接口给的 `FileHash` 常指向**已下架**的版本（酷狗搜索保留历史 hash），
    /// 而 `audio_info.hash_320` / `hash_128` 等往往还是能播的——这就是用户说的
    /// 「下架歌曲收藏到歌单里就能听」：歌单接口给的正是 `audio_info` 那一组 hash。
    ///
    /// 所以 `/privilege/lite` 查不到、或者服务端根本没有这个接口（旧版
    /// KuGouMusicApi）时，把整组 hash 挨个试一遍，而不是只抱着失效的那个不放。
    fn fallback_candidates(song: &Song, quality: &str) -> Vec<(String, String)> {
        let mut candidates = vec![(song.hash.clone(), quality.to_string())];
        for hash in song.extra_hashes.values() {
            if *hash != song.hash {
                candidates.push((hash.clone(), quality.to_string()));
            }
        }
        candidates
    }

    /// 把 `/privilege/lite` 的响应转换成「按用户选的音质降级排序」的候选列表。
    ///
    /// 没拿到响应或解析不出候选时，回退到 `fallback_candidates`——这一首歌的所有
    /// hash 挨个试。走老路不一定能拿到，但至少不会因为这个查询失败就整条堵死。
    async fn privilege_candidates(&self, song: &Song, quality: &str) -> Vec<(String, String)> {
        let response = match self.request_privilege_lite(song).await {
            Ok(value) => value,
            Err(error) => {
                tlog!(
                    crate::logger::LEVEL_DEBUG,
                    "/privilege/lite 失败：{}，按整组 hash 兜底",
                    error.user_hint()
                );
                return Self::fallback_candidates(song, quality);
            }
        };
        let candidates = parse_quality_candidates(&response, quality);
        if candidates.is_empty() {
            // 服务端认这个 hash 但没给任何可用档位——多半是下架歌曲。
            // 换这一首歌的其它 hash 再试，别只咬着失效的那个。
            return Self::fallback_candidates(song, quality);
        }
        candidates
            .into_iter()
            .map(|candidate| (candidate.hash, candidate.quality))
            .collect()
    }

    /// 问服务端「这个 hash 在登录账号下能听哪几档音质」。
    ///
    /// 酷狗为每档音质维护**独立的文件指纹（hash）**——VIP 用户拿到的 flac hash
    /// 和 128 hash 是完全不同的两个串。直接拿歌单里查到的 hash 去试 320 / flac
    /// 全是空，所以这里必须先查一次。
    ///
    /// # 必须是 GET + `hash` 参数（踩过的坑）
    ///
    /// 服务端 `privilege_lite.js` 是这样读参数的：
    /// ```js
    /// const resource = (params?.hash || '').split(',').map((s) => ({...}));
    /// ```
    /// 它读的是**顶层 `hash` 参数**（逗号分隔可传多个），不是 `resource` 数组。
    /// 早先我用 POST + `resource` 数组的写法，服务端拿不到 hash，返回
    /// `error_code 20010 "param error, hash and AlbumAudioID is empty"`——
    /// 而那个错误码被我们的客户端当成「需要登录」，于是整条 privilege 路径
    /// 一直静默失效，用户只能拿到试听片段。
    ///
    /// MoeKoeMusic 就是 `get('/privilege/lite', { hash })`，照抄它的用法。
    async fn request_privilege_lite(&self, song: &Song) -> Result<Value> {
        // 逗号分隔可一次问多个 hash：主 hash 在前，audio_info 里那些在后。
        // 搜索给的顶层 hash 可能指向已下架版本，后者往往还能用。
        let mut seen = std::collections::HashSet::new();
        let mut hashes = vec![song.hash.clone()];
        seen.insert(song.hash.clone());
        for hash in song.extra_hashes.values() {
            if seen.insert(hash.clone()) {
                hashes.push(hash.clone());
            }
        }

        let query = vec![("hash", hashes.join(","))];
        self.get_json_uncached("/privilege/lite", &query).await
    }

    /// 从响应里读出「为什么给不了完整版」。
    ///
    /// `fail_process` 是服务端给的步骤数组，实测见过 `["pkg","buy"]`（需开通或购买）。
    /// 读不到就返回 None，交给调用方用通用文案。
    fn fail_reason(&self, root: &Value) -> Option<String> {
        let data = root.get("data").unwrap_or(root);
        let array = data.get("fail_process").and_then(Value::as_array)?;
        let steps: Vec<&str> = array.iter().filter_map(Value::as_str).collect();
        if steps.is_empty() {
            return None;
        }
        Some(match steps.join("/").as_str() {
            "pkg" => "需要开通会员".to_string(),
            "pkg/buy" => "需要开通会员或单独购买该专辑".to_string(),
            other => format!("服务端返回 {other}"),
        })
    }

    /// 把服务端 `status` 字段翻译成人话。
    ///
    /// KuGouMusicApi 把上游酷狗的状态码转成自己的 `status`，下面是实测过的几个：
    ///
    /// * `1` 成功（这一支不会是这条路径返回的——`status == 1` 时已经拿到 url 了）
    /// * `2` 需要验证（缺 dfid 或 token），界面提示去登录 / 检查设备指纹
    /// * `3` 该歌曲暂无版权，下架或地区限制
    /// * 其它  透出服务端原文
    fn status_reason(&self, status: i64) -> Option<String> {
        match status {
            0 => None,
            2 => Some("需要登录或重新登录后再试".to_string()),
            // 实测 `3` 不等于「无版权」：同一首歌带 `audio_id` 是 status=1 有直链，
            // 带 `/privilege/lite` 返回的那个（mixsongid）就是 status=3、空 url。
            // 所以它是「请求里的文件标识与账号权限对不上」，别再误导成下架。
            3 => Some("服务端拒绝了这个请求（文件标识与账号权限不匹配）".to_string()),
            other => Some(format!("服务端返回 {other}")),
        }
    }

    /// 发一次 `/song/url`。`free_part` 决定是否只要试听片段。
    ///
    /// # 别自己编 `ppage_id`
    ///
    /// 服务端 `song_url.js` 里是这样取客户端指纹的：
    /// ```js
    /// const ppage_id = isLite
    ///   ? (params.ppage_id || '356753938,823673182,967485191')  // 概念版：用客户端传的
    ///   : '463467626,350369493,788954147';                       // 标准版：硬编码，忽略客户端
    /// ```
    /// **概念版会用客户端传的值**，标准版则完全忽略。我们既不知道用户跑的是哪个
    /// 平台（`platform` 是服务端的环境变量，客户端看不到），也就无从给对——
    /// 一旦传了错的（标准版的指纹喂给概念版服务端），指纹与平台不匹配，酷狗直接
    /// 拒绝，错误码 31863，表现是「所有歌都拿不到地址」。
    ///
    /// 所以**不传**，让服务端各用各的默认值：标准版拿它的硬编码、概念版拿它的
    /// 默认串，两种都对。
    ///
    /// # 关于 `album_id` / `album_audio_id`
    ///
    /// **现在刻意不传它们**，尽管 `Song` 上有这两个字段（[`Song::album_audio_id`]）。
    /// 2026-09-28 在概念版服务上实测过四种组合：只带 `hash` + `quality`、
    /// 再加 `ppage_id`、再加 `album_id` + `album_audio_id`（搜索给的 `MixSongID`
    /// 与 `/privilege/lite` 给的值都试了）——**四种都拿到了直链**，而且实测那两首歌
    /// 的两个 id 完全相等。也就是说「传了命中率更高」在当前服务上没有体现。
    ///
    /// 反过来，[`Self::status_reason`] 里记着另一条实测：带 `/privilege/lite`
    /// 返回的那个 `album_audio_id` 会让 `/song/url` 回 `status=3`、空 url。
    /// 既然传没有可证明的收益、传错又有明确代价，**就一个都不传**。
    ///
    /// 哪天出现「hash 没问题但服务端死活不给直链」的歌，优先怀疑这里：把
    /// `Song` 自己的 `album_id` / `album_audio_id` 加上去试一次（注意用歌曲自带
    /// 的那组，不是 `/privilege/lite` 那组），并把结论记回这里。
    async fn request_song_url_with_hash(
        &self,
        _song: &Song,
        hash: &str,
        quality: &str,
        free_part: bool,
    ) -> Result<Value> {
        let mut query = vec![
            ("hash", hash.to_string()),
            ("quality", quality.to_string()),
            ("ppage_id", PPAGE_ID_LITE.to_string()),
        ];
        if free_part {
            query.push(("free_part", "true".to_string()));
        }

        self.get_json_uncached("/song/url", &query).await
    }
}

/// 「蝰蛇音效」系列的音质名——这些是酷狗的**付费加项**，需要独立的蝰蛇 VIP，
/// 不是普通 TVIP 能拿到的。账号没蝰蛇权限时上游直接给 \`error_code 31863\`
/// 不在这一层加兜底的话用户会「所有歌都听不了」还以为是播放器坏了。
///
/// \`viper_clear\` 是蝰蛇母带（最高音质），\`viper_atmos\` 全景声，\`viper_tape\`
/// 母带修复。普通用户 / 标准 VIP 别选这几个——优先选 128 / 320 / flac。
const VIPER_QUALITIES: &[&str] = &["viper_clear", "viper_atmos", "viper_tape"];

/// 蝰蛇音质没权限时的兜底音质。128 是酷狗默认音质，**不需要任何 VIP**。
const VIPER_FALLBACK_QUALITY: &str = "128";

/// 音质降级链，从低到高。用户选的那档在最前（代码里会 `rev()`），逐级往下退。
///
/// **顺序与 MoeKoeMusic 的 `QUALITY_LEVELS` 一致**（见其
/// `src/components/player/songQueue/OnlineMusicQueue.js`），七个档次全保留：
///
/// ```js
/// const QUALITY_LEVELS = ['128', '320', 'flac', 'high', 'viper_atmos', 'viper_clear', 'viper_tape'];
/// ```
///
/// 之前我砍掉了三个蝰蛇档，结果 `relate_goods` 里的相关变体全被过滤掉
/// ——而「下架歌曲在歌单里能听」走的就是 `relate_goods` 这条路。
/// 没权限的档位服务端会给 `level == 0`，由 `parse_quality_candidates` 过滤，
/// 所以这里放宽是安全的。
const PRIVILEGE_FALLBACK_CHAIN: &[&str] = &[
    "128",
    "320",
    "flac",
    "high",
    "viper_atmos",
    "viper_clear",
    "viper_tape",
];

/// 一个候选音质：账号在该音质下有权限时，服务端给的 hash + 品质名。
///
/// `album_audio_id` 是**服务端在这一档里返回的真实值**。它和搜索接口给的
/// `MixSongID` 未必相同，而 `/song/url` 用它来定位"用户买的是哪个版本"——
/// 实测同一首歌带上真实值就能拿到直链，带搜索给的那个只能拿试听。
#[derive(Debug, Clone, PartialEq)]
struct QualityCandidate {
    hash: String,
    quality: String,
    album_audio_id: String,
}

/// 把 `/privilege/lite` 的响应整理成「按用户选的音质降级排序」的候选列表。
///
/// 服务端响应（参见 MoeKoeMusic 的 `getQualityOptions`）：
/// ```text
/// data: [
///   {hash, quality, level, relate_goods: [{...}, ...]},
///   ...
/// ]
/// ```
/// `level == 0` 表示没权限，跳过。每首歌可能有多个 variant（自己 + relate_goods），
/// 每个 variant 是不同的 hash（同一首歌的 128 和 flac 完全是两个文件指纹）。
fn parse_quality_candidates(response: &Value, requested: &str) -> Vec<QualityCandidate> {
    // 先收集每个 quality 任意一个有权限的 variant（一首歌同 quality 的不同 variant
    // 都给同一个 hash，取第一个就行）。
    //
    // 顺带把服务端给的 `album_audio_id` 也捎上——它是"用户买的究竟是哪个版本"
    // 的凭据，`/song/url` 认这个。实测同一首歌带上真实值就能拿到直链，
    // 带搜索结果里那个 `MixSongID` 只能拿试听。
    let mut available: std::collections::HashMap<&str, (&str, String)> =
        std::collections::HashMap::new();
    if let Some(items) = response.get("data").and_then(Value::as_array) {
        for item in items {
            // 自己 + relate_goods 都是同一首歌的不同 variant
            let mut variants: Vec<&Value> = vec![item];
            if let Some(related) = item.get("relate_goods").and_then(Value::as_array) {
                variants.extend(related.iter());
            }
            for variant in variants {
                // level == 0 = 没权限（VIP 限制）。缺失也按有权限处理——
                // 服务端有时会省略该字段，默认开放是合理的猜测。
                let has_level = !matches!(variant.get("level").and_then(Value::as_i64), Some(0));
                if !has_level {
                    continue;
                }
                let Some(quality) = variant.get("quality").and_then(Value::as_str) else {
                    continue;
                };
                let Some(hash) = variant.get("hash").and_then(Value::as_str) else {
                    continue;
                };
                if !PRIVILEGE_FALLBACK_CHAIN.contains(&quality) {
                    continue;
                }
                let album_audio_id = pick_string(variant, &["album_audio_id", "audio_id"])
                    .or_else(|| {
                        pick_i64(variant, &["album_audio_id", "audio_id"]).map(|id| id.to_string())
                    })
                    .unwrap_or_default();
                available.entry(quality).or_insert((hash, album_audio_id));
            }
        }
    }

    // 按降级链顺序取每个 quality 对应的 variant
    let chain = fallback_chain(requested);
    chain
        .into_iter()
        .filter_map(|quality| {
            available
                .get(quality)
                .map(|(hash, album_audio_id)| QualityCandidate {
                    hash: (*hash).to_string(),
                    quality: (*quality).to_string(),
                    album_audio_id: album_audio_id.clone(),
                })
        })
        .collect()
}

/// 用户选的音质 + 其下的所有档位，按优先级降序。
///
/// 例：requested=flac → [flac, 320, 128]（先试 flac，再 320，最后 128）。
/// requested 不在链里时按 128 处理——和 KoeKoeMusic 的 `normalizeQuality` 一致。
fn fallback_chain(requested: &str) -> Vec<&'static str> {
    let normalized = if PRIVILEGE_FALLBACK_CHAIN.contains(&requested) {
        requested
    } else {
        "128"
    };
    let index = PRIVILEGE_FALLBACK_CHAIN
        .iter()
        .position(|quality| *quality == normalized)
        .unwrap_or(PRIVILEGE_FALLBACK_CHAIN.len() - 1);
    PRIVILEGE_FALLBACK_CHAIN[..=index]
        .iter()
        .rev()
        .copied()
        .collect()
}

/// 概念版（lite）的「官方客户端指纹」，照抄 MoeKoeMusic 的写法：
/// ```js
/// get('/song/url', { hash, quality, ppage_id: '356753938' })
/// ```
///
/// # 为什么非要传它
///
/// 实测同一首歌（歌单里的下架歌，hash=0C9E876A…）：
///
/// * 不传 `ppage_id` → 服务端用默认的三段值 `356753938,823673182,967485191`
///   → `/song/url` 返回 `status=3`、空 url
/// * 传 `356753938`（单个）→ `status=1`，128/320/flac 全都能拿到直链
///
/// 也就是说概念版服务端认的是这**一个**数，多段反而不对。
///
/// 标准版不受影响：`song_url.js` 里标准版分支是硬编码、完全忽略客户端传的值，
/// 所以统一传这个数对两种平台都安全。
const PPAGE_ID_LITE: &str = "356753938";

/// `/song/url` 的结果。
pub struct StreamUrl {
    pub url: String,
    /// 是否为试听片段（完整版拿不到时的降级结果，通常只有 60 秒）。
    pub is_trial: bool,
    /// 完整版为什么拿不到。取自服务端 `fail_process`（如 `["pkg","buy"]`）。
    ///
    /// 有了它，用户看到「只有试听」时就知道是被什么挡住了，而不是只能瞎猜
    /// 「是不是会员没生效」。
    pub reason: Option<String>,
}

/// 从响应里收集歌单列表。
///
/// `assume_own` 为真时，对服务端未返回 `is_self` 但带数字 `listid` 的条目按
/// 「当前用户可写」处理。**只有 `/user/playlist` 能这么假设**：歌单广场返回的是
/// 别人创建的歌单，同样带 `specialid` 却没有 `is_self`，误判会把陌生人的歌单
/// 标成「可写」，还能被设成同步目标。
fn collect_playlists(root: &Value, assume_own: bool) -> Vec<Playlist> {
    extract_list(
        root,
        &["info", "list", "lists", "special_list", "data"],
        |value| {
            let mut playlist = playlist_from_json(value)?;
            if assume_own && playlist.list_id.is_some() && value.get("is_self").is_none() {
                playlist.is_own = true;
            }
            Some(playlist)
        },
    )
}

/// 从 `/artist/lists` 的响应里收集歌手。
///
/// 这个接口**按首字母分组**返回：`data.info` 是 28 个 `{title, singer: [...]}`
/// 的组（热门 + A–Z + `#`），歌手在组内的 `singer` 数组里，实测四类筛选
/// （全部 / 华语 / 欧美 / 日韩）都是这个形状，合计约 1050 人。
///
/// **组包装本身不是歌手**——直接交给 `artist_from_json` 会一组都解析不出来，
/// 然后掉进 `extract_list` 的兜底扫描，只命中第一组（热门 60 人），
/// A–Z 全表被静默丢掉，同时每加载一次就刷一条「请核对字段布局」的警告。
/// 所以先把各组的 `singer` 展平再解析。
fn collect_artists(root: &Value) -> Vec<Artist> {
    let data = data_of(root);
    let mut artists = Vec::new();
    for group in pick_array(data, &["info", "list", "singer_list"]) {
        // 空组是常态（实测 U / V 组就是空的），跳过即可，不影响其它组
        for entry in pick_array(group, &["singer", "singers", "list"]) {
            if let Some(artist) = artist_from_json(entry) {
                artists.push(artist);
            }
        }
    }
    if !artists.is_empty() {
        return artists;
    }

    // 扁平布局：条目本身就是歌手。保留原来的候选键扫描（含兜底扫描），
    // 上游若改回扁平结构或换了容器键名，这里仍然能兜住。
    extract_list(
        root,
        &["info", "list", "singer_list", "authors"],
        artist_from_json,
    )
}

/// 从 `/song/url` 的响应里挖出直链。
///
/// 这个接口的布局尤其不统一，实测见过四种：
///
/// * **顶层** `url` / `backupUrl`（数组）—— 目前最常见，`data` 根本不存在
/// * `data[0].url`
/// * `data.url`
/// * `data.play_url`
///
/// 且 `url` 有时是字符串、有时是数组。所以按「data 数组 → data 对象 → 顶层」
/// 三级找，每级内部再试多个键名。
///
/// **关键**：必须看 `status` 字段。`status == 1` 才是真正的成功；
/// `status == 3` 给的 url 是空的，我们之前会把它当成"没 url"然后给
/// 用户一个「可能需要 VIP」的笼统提示——但其实是版权问题。提前一步
/// 把这种响应挡在 URL 解析外面，让上层看到明确的 status 翻译。
fn extract_stream_url(root: &Value) -> Option<String> {
    if let Some(status) = root.get("status").and_then(Value::as_i64)
        && status != 1
    {
        return None;
    }

    const URL_KEYS: &[&str] = &["url", "play_url", "backup_url", "backupUrl"];

    if let Some(items) = root.get("data").and_then(Value::as_array)
        && let Some(found) = items.iter().find_map(|item| pick_url(item, URL_KEYS))
    {
        return Some(found);
    }

    if let Some(data) = root.get("data")
        && let Some(found) = pick_url(data, URL_KEYS)
    {
        return Some(found);
    }

    pick_url(root, URL_KEYS)
}

fn pick_url(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        match value.get(*key) {
            Some(Value::String(url)) if is_http_url(url) => return Some(url.trim().to_string()),
            Some(Value::Array(items)) => {
                let found = items.iter().find_map(|item| match item {
                    Value::String(text) if is_http_url(text) => Some(text.trim().to_string()),
                    _ => None,
                });
                if found.is_some() {
                    return found;
                }
            }
            _ => {}
        }
    }
    None
}

fn is_http_url(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with("http://") || trimmed.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ------------------------------------------------------------------
    // 并发翻页的顺序
    //
    // `collect_all_pages` 是异步的，一批页同时发出去。测它需要一个真的服务端，
    // 而且**必须能控制每页的返回顺序**——否则「页序对不对」只能碰运气。
    // 和 `audio::download` 那组测试一样，用 std 的 TcpListener 手写一个，
    // 不引 wiremock / httpmock。
    // ------------------------------------------------------------------

    /// 起一个「第 N 页等 `SLEW_MS * (7 - N)` 才回」的服务端。
    ///
    /// 于是第一批（页码 2..7）**必然**按 7、6、5、4、3、2 的顺序回来——
    /// 正是把「按完成顺序拼接」打回原形的那种顺序。
    fn spawn_paged_server() -> String {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        const LAST_PAGE: u32 = 7;
        const SLEW_MS: u64 = 60;

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取本地地址");

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().expect("克隆流"));
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                // 只解析测试用得到的那一个参数
                let page = line
                    .split("page=")
                    .nth(1)
                    .and_then(|rest| rest.split(['&', ' ']).next())
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or(0);

                // 把请求头读完，否则 reqwest 会认为连接异常
                loop {
                    let mut header = String::new();
                    match reader.read_line(&mut header) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if header.trim().is_empty() => break,
                        Ok(_) => {}
                    }
                }

                // 高页码先回来：这是让测试**确定性**地暴露顺序问题的关键
                if page < LAST_PAGE {
                    std::thread::sleep(std::time::Duration::from_millis(
                        SLEW_MS * (LAST_PAGE - page) as u64,
                    ));
                }

                let body = if page == 0 || page > LAST_PAGE {
                    json!({"data": []})
                } else {
                    json!({"data": [{"hash": format!("h{page}"), "name": format!("第{page}页")}]})
                };
                let payload = body.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = stream.flush();
            }
        });

        format!("http://{addr}")
    }

    /// **回归测试**：并发翻页的结果必须按页码拼，不能按完成顺序拼。
    ///
    /// 修复前 `collect_all_pages` 在 `join_next()` 里直接 `all.extend(songs)`，
    /// 而 `join_next()` 给的是**任务完成顺序**。一批 6 页同时发出去，谁先回来
    /// 谁先 append，于是整表的页序是随机的——榜单尤其致命，它的「顺序」就是
    /// 榜单内容本身。
    #[tokio::test]
    async fn concurrent_pages_are_assembled_in_page_order() {
        let client = ApiClient::new(&spawn_paged_server(), None, None).expect("构造客户端");

        let songs = client
            .playlist_tracks_all("any", false)
            .await
            .expect("翻页应当成功");

        let hashes: Vec<&str> = songs.iter().map(|song| song.hash.as_str()).collect();
        assert_eq!(
            hashes,
            vec!["h1", "h2", "h3", "h4", "h5", "h6", "h7"],
            "整表必须按页码顺序，实际：{hashes:?}"
        );
    }

    #[test]
    fn finds_url_in_array_payload() {
        let root = json!({"data": [{"url": ["http://a/1.mp3"], "play_url": "http://a/2.mp3"}]});
        assert_eq!(extract_stream_url(&root).as_deref(), Some("http://a/1.mp3"));
    }

    #[test]
    fn finds_url_at_top_level() {
        // 实测最常见的新形状：没有 `data`，url / backupUrl 直接挂在顶层
        let root = json!({
            "status": 1,
            "url": ["http://fsandroid.tx.kugou.com/a.mp3", "http://fsmobile.kugou.com/a.mp3"],
            "backupUrl": ["http://fsmobile.kugou.com/b.mp3"]
        });
        assert_eq!(
            extract_stream_url(&root).as_deref(),
            Some("http://fsandroid.tx.kugou.com/a.mp3")
        );
    }

    #[test]
    fn falls_back_to_backup_url() {
        let root = json!({
            "status": 1,
            "url": [],
            "backupUrl": ["http://fsmobile.kugou.com/b.mp3"]
        });
        assert_eq!(
            extract_stream_url(&root).as_deref(),
            Some("http://fsmobile.kugou.com/b.mp3")
        );
    }

    #[test]
    fn finds_url_in_object_payload() {
        let root = json!({"data": {"play_url": "https://b/1.flac"}});
        assert_eq!(
            extract_stream_url(&root).as_deref(),
            Some("https://b/1.flac")
        );
    }

    #[test]
    fn ignores_non_http_placeholder() {
        let root = json!({"data": {"url": ""}});
        assert_eq!(extract_stream_url(&root), None);
    }

    /// `status == 3` 是版权问题——之前我们拿到 url 是空的、却当"找不到 url"
    /// 一路兜底，给用户「可能需要 VIP」的笼统提示。这一步必须拦在前面，
    /// 让上层走 status_reason 给出「该歌曲暂无版权」的具体文案。
    #[test]
    fn rejects_when_status_is_not_one() {
        let root = json!({
            "status": 3,
            "url": ["https://fsmobile.kugou.com/a.mp3"]
        });
        assert_eq!(extract_stream_url(&root), None);
    }

    /// 没有 status 字段就当老接口看待（旧版服务端不返回 status）。
    #[test]
    fn works_without_status_field() {
        let root = json!({
            "url": ["https://fsmobile.kugou.com/a.mp3"]
        });
        assert_eq!(
            extract_stream_url(&root).as_deref(),
            Some("https://fsmobile.kugou.com/a.mp3")
        );
    }

    /// 用户选了 flac 但账号只有 320 / 128 权限——这种情况之前会拿不到 URL
    /// （原 hash 没有 flac 权限），新逻辑应该从 `/privilege/lite` 拿到
    /// 320 和 128 的真实 hash，按降级链返回。
    #[test]
    fn privilege_candidates_follow_fallback_chain() {
        let response = json!({
            "data": [
                {
                    "quality": "128",
                    "level": 1,
                    "hash": "h_128",
                    "relate_goods": [
                        {"quality": "320", "level": 1, "hash": "h_320"},
                    ],
                },
                // 没权限的 flac —— 必须被过滤掉
                {
                    "quality": "flac",
                    "level": 0,
                    "hash": "h_flac_no_perm",
                    "relate_goods": [],
                },
            ],
        });
        let candidates = parse_quality_candidates(&response, "flac");
        assert_eq!(
            candidates,
            vec![
                QualityCandidate {
                    quality: "320".to_string(),
                    hash: "h_320".to_string(),
                    album_audio_id: String::new(),
                },
                QualityCandidate {
                    quality: "128".to_string(),
                    hash: "h_128".to_string(),
                    album_audio_id: String::new(),
                },
            ],
            "没权限的 flac 必须跳过；降级链先 320 再 128"
        );
    }

    /// 用户选了 128 → 只返 128，不会无端把 320 也加进来（用户没选）。
    #[test]
    fn privilege_candidates_strict_to_requested_quality_or_below() {
        let response = json!({
            "data": [{
                "quality": "128",
                "level": 1,
                "hash": "h_128",
                "relate_goods": [
                    {"quality": "320", "level": 1, "hash": "h_320"},
                ],
            }],
        });
        let candidates = parse_quality_candidates(&response, "128");
        assert_eq!(
            candidates,
            vec![QualityCandidate {
                quality: "128".to_string(),
                hash: "h_128".to_string(),
                album_audio_id: String::new(),
            }],
            "选了 128 就只返 128，不会自动升级到 320"
        );
    }

    /// 不认识的 quality 当 128 处理——和 KoeKoeMusic 的 `normalizeQuality` 一致。
    #[test]
    fn privilege_candidates_normalize_unknown_quality_to_128() {
        let response = json!({
            "data": [{
                "quality": "128",
                "level": 1,
                "hash": "h_128",
                "relate_goods": [],
            }],
        });
        let candidates = parse_quality_candidates(&response, "high_res_thing");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].quality, "128");
    }

    /// 响应里压根没 data，返回空列表——上层会回退到「原 hash + 用户选的音质」。
    #[test]
    fn privilege_candidates_returns_empty_for_unrecognized_response() {
        let candidates = parse_quality_candidates(&json!({}), "flac");
        assert!(candidates.is_empty());

        let candidates = parse_quality_candidates(&json!({"data": "garbage"}), "flac");
        assert!(candidates.is_empty());
    }

    /// 下架歌曲的修复点：/privilege/lite 查不到时，不能只抱着失效的主 hash，
    /// 要把这一首歌的**整组** hash（主 hash + audio_info 里那些）挨个试。
    /// 用户说的「下架歌曲收藏到歌单里能听」，歌单给的正是后者那一组。
    #[test]
    fn fallback_candidates_include_every_hash_of_the_song() {
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("320".to_string(), "HASH_320".to_string());
        extra.insert("flac".to_string(), "HASH_FLAC".to_string());
        let song = Song {
            name: "下架的歌".to_string(),
            hash: "PRIMARY_HASH".to_string(),
            album_id: "1".to_string(),
            album_audio_id: 0,
            album_name: String::new(),
            singers: vec![],
            duration_ms: 0,
            cover: None,
            privilege: None,
            file_id: None,
            extra_hashes: extra,
            source: crate::source::SourceKind::Kugou,
        };

        let candidates = ApiClient::fallback_candidates(&song, "128");
        // 主 hash 在前，其余两个在后；每个都用用户选的音质
        assert_eq!(candidates.len(), 3);
        assert_eq!(
            candidates[0],
            ("PRIMARY_HASH".to_string(), "128".to_string())
        );
        assert!(candidates.iter().any(|(h, _)| h == "HASH_320"));
        assert!(candidates.iter().any(|(h, _)| h == "HASH_FLAC"));
    }

    /// 没有 extra_hashes 时兜底就只有主 hash 一项，别凭空造数据。
    #[test]
    fn fallback_candidates_is_just_primary_hash_when_no_extras() {
        let song = Song {
            name: "普通歌".to_string(),
            hash: "ONLY_HASH".to_string(),
            album_id: String::new(),
            album_audio_id: 0,
            album_name: String::new(),
            singers: vec![],
            duration_ms: 0,
            cover: None,
            privilege: None,
            file_id: None,
            extra_hashes: Default::default(),
            source: crate::source::SourceKind::Kugou,
        };
        let candidates = ApiClient::fallback_candidates(&song, "320");
        assert_eq!(
            candidates,
            vec![("ONLY_HASH".to_string(), "320".to_string())]
        );
    }

    /// `/privilege/lite` 那一档里的 `album_audio_id` 要能被解析出来。
    ///
    /// 这个字段**目前只解析、不发送**——见 [`ApiClient::request_song_url_with_hash`]
    /// 里的说明：实测四种参数组合都能拿到直链，而带错 id 反而会让服务端回
    /// `status=3`。留着它是因为那是「用户买的究竟是哪个版本」的唯一凭据，
    /// 将来要在 `/song/url` 上补字段时，值就在这里，不用重新解析一遍。
    ///
    /// 注意别把它和搜索接口给的 `MixSongID` 混为一谈：实测同一个值在两处都出现过，
    /// 但 [`ApiClient::status_reason`] 记着「带 privilege 那个会 status=3」的
    /// 反面案例，所以两者在 `Song` 上是**分开存的**（`album_audio_id` 与 `audio_id`）。
    #[test]
    fn privilege_candidates_keep_the_server_side_album_audio_id() {
        // 构造一个 privilege 响应，variant 的 album_audio_id 与 Song 自己的不同
        let response = json!({
            "data": [{
                "quality": "128",
                "level": 2,
                "hash": "h_128",
                "album_audio_id": 330978610,
                "relate_goods": [],
            }]
        });
        let candidates = parse_quality_candidates(&response, "128");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].album_audio_id, "330978610");

        // fallback 那一支用的是 Song 自己的 hash（它不携带 album_audio_id）
        let mut song = Song {
            name: "x".to_string(),
            hash: "h_128".to_string(),
            album_id: String::new(),
            album_audio_id: 116104796,
            album_name: String::new(),
            singers: vec![],
            duration_ms: 0,
            cover: None,
            privilege: None,
            file_id: None,
            extra_hashes: Default::default(),
            source: crate::source::SourceKind::Kugou,
        };
        let _ = &mut song;
        assert_eq!(
            ApiClient::fallback_candidates(&song, "128")[0],
            ("h_128".to_string(), "128".to_string())
        );
    }

    /// 蝰蛇音质 vs 标准音质的分类不能错——否则降级逻辑会把标准 128 当蝰蛇
    /// 走兜底（用户根本没要求降级就被偷偷降级），或者反过来。
    #[test]
    fn viper_quality_classification() {
        assert!(VIPER_QUALITIES.contains(&"viper_clear"));
        assert!(VIPER_QUALITIES.contains(&"viper_atmos"));
        assert!(VIPER_QUALITIES.contains(&"viper_tape"));
        // 标准音质 / flac / high 不是蝰蛇——别误判
        assert!(!VIPER_QUALITIES.contains(&"flac"));
        assert!(!VIPER_QUALITIES.contains(&"high"));
        assert!(!VIPER_QUALITIES.contains(&"128"));
        assert!(!VIPER_QUALITIES.contains(&"320"));
    }

    /// `/artist/lists` **按首字母分组**返回：`data.info` 是 28 个
    /// `{title, singer: [...]}` 的组（热门 + A–Z + #），歌手在组内的 `singer` 数组里。
    ///
    /// 旧实现把组包装直接交给 `artist_from_json`——它认不出 `singer` 这层，
    /// 于是每一组都解析失败，最后掉进「兜底扫描」只命中**第一组**，
    /// 其余 27 组（A–Z 全表）被静默丢掉。线上日志里因此刷了 19 条
    /// 「响应未命中任何候选键……请核对字段布局」的警告。
    #[test]
    fn collects_artists_from_grouped_response() {
        // 结构照抄真实响应：热门组 + 两个字母组
        let root = json!({
            "data": {
                "info": [
                    {
                        "title": "热门",
                        "singer": [
                            {"singerid": 3520, "singername": "周杰伦", "fanscount": 25694934,
                             "imgurl": "http://singerimg.kugou.com/uploadpic/softhead/{size}/a.jpg"},
                            {"singerid": 3060, "singername": "薛之谦", "fanscount": 20017619}
                        ]
                    },
                    {
                        "title": "A",
                        "singer": [
                            {"singerid": 2, "singername": "阿杜", "fanscount": 1665651}
                        ]
                    },
                    {
                        "title": "B",
                        "singer": [
                            {"singerid": 3, "singername": "BEYOND", "fanscount": 100}
                        ]
                    }
                ]
            },
            "errcode": 0,
            "status": 1
        });

        let artists = collect_artists(&root);
        let names: Vec<&str> = artists.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["周杰伦", "薛之谦", "阿杜", "BEYOND"],
            "必须把每一组都收下，而不是只收第一组"
        );
        assert_eq!(artists[0].id, 3520);
        assert_eq!(artists[0].follower_count, Some(25_694_934));
    }

    /// 分组里可能出现空数组（实测 U / V 组就是空的），不能因此中断。
    #[test]
    fn collects_artists_skips_empty_groups() {
        let root = json!({
            "data": {
                "info": [
                    {"title": "热门", "singer": [{"singerid": 1, "singername": "甲"}]},
                    {"title": "U", "singer": []},
                    {"title": "V", "singer": []},
                    {"title": "W", "singer": [{"singerid": 2, "singername": "乙"}]}
                ]
            }
        });
        let names: Vec<String> = collect_artists(&root).into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["甲", "乙"]);
    }

    /// 扁平布局（条目本身就是歌手）仍要能解析——上游可能改回去。
    #[test]
    fn collects_artists_from_flat_response() {
        let root = json!({
            "data": {
                "info": [
                    {"singerid": 3520, "singername": "周杰伦"},
                    {"singerid": 3060, "singername": "薛之谦"}
                ]
            }
        });
        let names: Vec<String> = collect_artists(&root).into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["周杰伦", "薛之谦"]);
    }
}
