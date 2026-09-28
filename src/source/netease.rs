//! 网易云音乐音源（NeteaseCloudMusicApi / api-enhanced）。
//!
//! 部署方式与 KuGouMusicApi 一致：`git clone` + `npm install` + `node app.js`，
//! 默认 `http://127.0.0.1:3002`。
//!
//! # 与酷狗最大的差异：登录态归谁保管
//!
//! 酷狗的 token 由**客户端**保存，每次请求带上即可。网易云是**服务端**保管
//! cookie——`/login/qr/check` 成功时把它放在响应体的 `cookie` 字段里，客户端必须
//! 接住、存好、之后每次请求带回去。服务端进程一换（重启、换端口），身份就只剩
//! 客户端手里这一份了。
//!
//! ⚠️ 那个字段是**整段 `Set-Cookie` 响应头**（含 `Max-Age` / `Expires` / `Path`
//! 属性，多个 cookie 之间用 `;;` 连接），而服务端读回时只认「分号 + 空格」这一种
//! 分隔（`server.js` 里那条 `/;\s+|(?<!\s)\s+$/g`）。原样回传会让 `MUSIC_U` 被
//! 并进前一个属性段里，服务端就认不出登录态——界面显示「登录成功」，云端歌单却
//! 永远报「尚未登录」。所以存入前必须过一遍
//! [`crate::util::normalize_cookie_header`]。
//!
//! # 已实现的能力
//!
//! 搜索、播放直链、歌词（含译文）、封面、扫码登录、歌单广场、歌手、排行榜，
//! 以及云端歌单的读（列表 + 曲目）与写（加歌 / 删歌 / 新建 / 删除）。
//!
//! 不提供会员信息：服务端没有对应端点，见 [`crate::source::Capability::vip`]。
//!
//! # 验证状态
//!
//! 端点与响应形状已对着真实服务核对（`api-enhanced` 4.30.1）。解析仍全部走
//! 防御式取值：缺字段只丢那一条，不会 panic。

use serde_json::Value;

use crate::api::client::ApiClient;
use crate::api::model::{Lyric, Singer, Song, pick_i64, pick_string, pick_u32, pick_u64};
use crate::api::{data_of, extract_list};
use crate::error::Result;

/// 搜索结果里每页取多少条。与酷狗保持一致，便于 UI 分页逻辑复用。
const DEFAULT_PAGE_SIZE: u32 = 30;

/// 取歌单 / 榜单曲目时最多翻多少页。
///
/// 榜单一般就 100 首（一页够），歌单可能上千首。上限取 20 页 × 500 = 10000 首，
/// 再大的歌单也够用了，同时避免服务端分页异常时无限翻下去。
const MAX_TRACK_PAGES: u32 = 20;

/// 单曲搜索。
///
/// 接口 `GET /search`，关键词参数名是 `keywords`（不是酷狗的 `keywords` 之外的
/// 其它叫法），分页用 `offset` + `limit` 而不是 `page` + `pagesize`。
pub async fn search_songs(
    client: &ApiClient,
    keyword: &str,
    page: u32,
    page_size: u32,
) -> Result<Vec<Song>> {
    let limit = if page_size == 0 {
        DEFAULT_PAGE_SIZE
    } else {
        page_size
    };
    // 第一页 offset 为 0
    let offset = page.saturating_sub(1).saturating_mul(limit);

    let root = client
        .get_json_uncached(
            "/search",
            &[
                ("keywords", keyword.to_string()),
                ("limit", limit.to_string()),
                ("offset", offset.to_string()),
            ],
        )
        .await?;

    let songs = extract_list(data_of(&root), &["songs"], song_from_json);
    Ok(songs)
}

/// 从一条歌曲 JSON 里解析出 [`Song`]。
///
/// # 两套布局都得认
///
/// 网易云用**两套完全不同的字段名**描述同一首歌，取决于走哪个接口（实测 4.30.1）：
///
/// | 接口 | 歌手 | 专辑 | 时长 |
/// |---|---|---|---|
/// | `/search`（搜索） | `artists` | `album` | `duration` |
/// | `/playlist/track/all`（歌单 / 榜单） | `ar` | `al` | `dt` |
/// | `/artists` → `hotSongs`（歌手热歌） | `ar` | `al` | `dt` |
///
/// 只认第一套的后果很隐蔽：**搜索页一切正常，歌单页与歌手页里的歌却全部没有
/// 歌手、没有专辑、时长显示 `00:00`**（封面也会退化成再打一次 `/song/detail`）。
/// 只看搜索页很难发现，所以这里两套都试，并留了单元测试钉住两种布局。
///
/// 字段都是嵌套的：歌手在 `artists[]` / `ar[]`、专辑在 `album{}` / `al{}`。
/// 全部走 `Option` 取值，任何一段缺失只丢这一条。
fn song_from_json(value: &Value) -> Option<Song> {
    // 网易云用数字 id 标识歌曲，它是取播放链接与歌词的唯一依据
    let id = pick_i64(value, &["id"])?;
    let name = pick_string(value, &["name"]).unwrap_or_else(|| "未知曲目".to_string());

    // 专辑：搜索给 `album`，歌单/歌手给 `al`（见上面的布局对照表）
    let album = value.get("album").or_else(|| value.get("al"));
    let cover = album
        .and_then(|album| pick_string(album, &["picUrl", "pic", "img1v1Url"]))
        // 网易云的封面 URL 可以带尺寸参数，缩到 300 够终端与桌面控件用
        .map(|url| {
            if url.contains('?') {
                url
            } else {
                format!("{url}?param=300y300")
            }
        });

    Some(Song {
        name,
        // 复用酷狗的 hash 字段存放 id：它是本音源取链接与歌词的主键，
        // 与酷狗的 FileHash 在各自音源内语义等价（都唯一标识一首歌）。
        hash: id.to_string(),
        album_id: album
            .and_then(|album| pick_i64(album, &["id"]))
            .map(|id| id.to_string())
            .unwrap_or_default(),
        // 毫秒，与领域模型一致，无需换算。搜索是 `duration`，歌单/歌手是 `dt`。
        duration_ms: pick_u64(value, &["duration", "dt"]).unwrap_or_default(),
        singers: parse_singers(value),
        album_name: album
            .and_then(|album| pick_string(album, &["name"]))
            .unwrap_or_default(),
        cover,
        // 网易云不返回版权标记，按可播处理；取不到链接时会有明确报错
        privilege: None,
        album_audio_id: 0,
        extra_hashes: Default::default(),
        file_id: None,
        source: crate::source::SourceKind::Netease,
    })
}

/// 歌手数组：搜索是 `artists[]`，歌单/歌手是 `ar[]`（见 [`song_from_json`] 的布局对照表）。
fn parse_singers(value: &Value) -> Vec<Singer> {
    let Some(artists) = value
        .get("artists")
        .or_else(|| value.get("ar"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    artists
        .iter()
        .filter_map(|artist| {
            Some(Singer {
                id: pick_i64(artist, &["id"]).unwrap_or_default(),
                name: pick_string(artist, &["name"])?,
            })
        })
        .collect()
}

/// 取播放直链。
///
/// `br` 是码率，按配置里的音质映射过去；拿不到 `url` 时把服务端给的原因透出去。
///
/// 网易云没有酷狗那样的「试听片段」降级机制，取不到就是取不到，因此
/// `is_trial` 恒为 `false`。
pub async fn song_stream_url(
    client: &ApiClient,
    song: &Song,
    quality: &str,
) -> Result<crate::api::catalog::StreamUrl> {
    let bitrate = bitrate_for(quality);
    let root = client
        .get_json_uncached(
            "/song/url",
            &[("id", song.hash.clone()), ("br", bitrate.to_string())],
        )
        .await?;

    let data = data_of(&root);
    // 响应是 { data: [ { id, url, ... } ] }
    let url = data
        .get(0)
        .or(Some(data))
        .and_then(|entry| pick_string(entry, &["url"]))
        .filter(|url| !url.trim().is_empty());

    match url {
        Some(url) => Ok(crate::api::catalog::StreamUrl {
            url,
            is_trial: false,
            reason: None,
        }),
        None => Err(crate::error::AppError::NotFound(format!(
            "《{}》没有可用的播放地址（可能需要 VIP 或已下架）",
            song.name
        ))),
    }
}

/// 音质 → 码率。网易云用具体码率而不是酷狗那样的档位名。
fn bitrate_for(quality: &str) -> u32 {
    match quality {
        "320" => 320_000,
        "flac" | "super" | "high" => 999_000,
        // 默认 128：体积最小、成功率最高
        _ => 128_000,
    }
}

/// 取歌词。
///
/// 网易云把译文单独放在 `tlyric`，这里按行挂到 [`LyricLine::translation`]，
/// 与酷狗从 KRC 里提取译文后的结果一致，UI 不需要区分音源。
pub async fn fetch_lyric(client: &ApiClient, song: &Song) -> Result<Lyric> {
    let root = client
        .get_json_uncached("/lyric", &[("id", song.hash.clone())])
        .await?;
    let data = data_of(&root);

    let main = data
        .get("lrc")
        .and_then(|lrc| pick_string(lrc, &["lyric"]))
        .unwrap_or_default();
    if main.trim().is_empty() {
        return Err(crate::error::AppError::NotFound(format!(
            "未找到《{}》的歌词",
            song.name
        )));
    }

    let mut lyric = crate::api::lyric::parse_lrc(&main);
    attach_netease_translation(&mut lyric, data);
    Ok(lyric)
}

/// 把 `tlyric` 的译文按行挂上去。
///
/// 译文与原文都是 LRC 文本，时间标签一一对应；按时间戳配对比按行号配对稳，
/// 能容忍译文缺行。
fn attach_netease_translation(lyric: &mut Lyric, data: &Value) {
    let Some(translation) = data
        .get("tlyric")
        .and_then(|tlyric| pick_string(tlyric, &["lyric"]))
    else {
        return;
    };
    let translated = crate::api::lyric::parse_lrc(&translation);
    if translated.is_empty() {
        return;
    }

    for line in lyric.lines.iter_mut() {
        let Some(found) = translated
            .lines
            .iter()
            .find(|candidate| candidate.time_ms == line.time_ms)
        else {
            continue;
        };
        if !found.text.trim().is_empty() {
            line.translation = Some(found.text.clone());
        }
    }
}

// ============================================================================
// 扫码登录
// ============================================================================
//
// NeteaseCloudMusicApi 的扫码三步：
//   1. `/login/qr/key`            → unikey
//   2. `/login/qr/create?key=...` → 二维码内容（这里要的是文本，见下）
//   3. `/login/qr/check?key=...`  → 状态
//
// ⚠️ 与本模块其它方法一样**未经真实服务验证**。状态码语义是公开的：
// 800 过期 / 801 等待扫码 / 802 待确认 / 803 已授权。

/// 取二维码 key（unikey）。
pub async fn login_qr_key(client: &ApiClient) -> Result<String> {
    let root = client.get_json_uncached("/login/qr/key", &[]).await?;
    let data = data_of(&root);
    pick_string(data, &["unikey", "key"]).ok_or_else(|| {
        crate::error::AppError::Other("取登录二维码 key 失败：响应里没有 unikey".to_string())
    })
}

/// 取二维码内容。
///
/// 酷狗那套接口返回的就是二维码文本（由 TUI 自己渲染成方块）；网易云这个接口
/// 默认返回**图片链接**（`qrimg`），要文本得显式带上 `qrimg=false`。
/// 统一取文本，渲染交给 `qr_lines()`，与酷狗共用一套画法。
///
/// `platform=web` 让服务端在 URL 上拼 `chainId`，让 web / App / 二维码扫描器
/// 都能识别这个登录入口（默认 pc 路径不带 chainId，纯 web 扫码会触发不了授权）。
pub async fn login_qr_create(client: &ApiClient, key: &str) -> Result<String> {
    let root = client
        .get_json_uncached(
            "/login/qr/create",
            &[
                ("key", key.to_string()),
                ("qrimg", "false".to_string()),
                ("platform", "web".to_string()),
            ],
        )
        .await?;
    let data = data_of(&root);

    pick_string(data, &["qrurl", "url", "qrCode"])
        .or_else(|| {
            data.get("data")
                .and_then(|inner| pick_string(inner, &["qrurl", "url"]))
        })
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| {
            crate::error::AppError::Other("生成登录二维码失败：响应里没有二维码内容".to_string())
        })
}

/// 轮询扫码状态。
pub async fn login_qr_check(client: &ApiClient, key: &str) -> Result<crate::api::cloud::QrCheck> {
    use crate::api::cloud::{QrCheck, QrStatus};

    let root = client
        .get_json_uncached("/login/qr/check", &[("key", key.to_string())])
        .await?;
    let data = data_of(&root);
    let code = pick_i64(data, &["code"]).unwrap_or(800);

    // 授权成功时响应里没有 token / userid 可给客户端——身份是服务端下发的
    // 那串 cookie，所以这里 token 留 None，由上层改走 `apply_server_cookie` 那条路。
    let status = match code {
        800 => QrStatus::Expired,
        801 => QrStatus::Waiting,
        802 => QrStatus::Pending,
        803 => QrStatus::Success,
        // 其它一律当过期，避免出现「一直卡在等待中」的假象
        _ => QrStatus::Expired,
    };

    // 身份在响应的 `cookie` 字段里（含 MUSIC_U）。必须接住并交给上层存起来，
    // 否则本次「登录成功」只是个谎言；而且**得先规范化**——服务端给的是一整段
    // `Set-Cookie`，原样回传它自己认不出来（见本模块头部说明）。
    let cookie = pick_string(&root, &["cookie"])
        .or_else(|| pick_string(data, &["cookie"]))
        .and_then(|raw| crate::util::normalize_cookie_header(&raw));

    Ok(QrCheck {
        status,
        token: None,
        userid: None,
        cookie,
    })
}

/// 取封面 URL。
///
/// ⚠️ 实测：这个版本的搜索响应里 album **只有 picId 没有 picUrl**，
/// 拿不到地址，必须再查一次 \`/song/detail\`（那里是 \`al.picUrl\`）。
/// 多数第三方 API 都在搜索结果里直接给 URL，这里是例外。
pub async fn cover_url(client: &ApiClient, song: &Song) -> Result<Option<String>> {
    // 已经有了就别多问一次
    if let Some(cover) = song.cover.as_ref() {
        return Ok(Some(cover.clone()));
    }
    if song.hash.is_empty() {
        return Ok(None);
    }

    let root = client
        .get_json_uncached("/song/detail", &[("ids", song.hash.clone())])
        .await?;
    let data = data_of(&root);

    // 实测响应形状：{ songs: [ { al: { picUrl } } ] }
    let url = data
        .get("songs")
        .and_then(Value::as_array)
        .and_then(|songs| songs.first())
        .and_then(|song| song.get("al").or_else(|| song.get("album")))
        .and_then(|album| pick_string(album, &["picUrl", "pic"]));

    Ok(url)
}

// ============================================================================
// 云端歌单（读取）
// ============================================================================
//
// ⚠️ 与酷狗最大的差别：网易云的登录态由**服务端**持有，客户端必须先问
// `/login/status` 才知道「当前是谁」，拿到 uid 才能查这个人的歌单。
// 酷狗则是客户端自己保存 token，请求时带上即可。
//
// 实测（未登录时）：`/login/status` 返回 `{ code: 200, account: null, profile: null }`，
// 此时取不到 uid，必须让用户先按 L 扫码。

/// 当前登录用户的 uid。未登录返回 `Ok(None)`。
async fn current_uid(client: &ApiClient) -> Result<Option<String>> {
    let root = client.get_json_uncached("/login/status", &[]).await?;
    let data = data_of(&root);

    // 未登录时 account / profile 都是 null，取值会拿到 None
    let uid = data
        .get("profile")
        .and_then(|profile| pick_i64(profile, &["userId"]))
        .or_else(|| {
            data.get("account")
                .and_then(|account| pick_i64(account, &["id"]))
        });

    Ok(uid.map(|id| id.to_string()))
}

/// 取当前登录用户的资料。
///
/// 必须先有 uid：`/user/detail` 不带 uid 只会得到 `{"code":400,"message":"参数错误"}`，
/// 而客户端手上没有 uid（见 [`current_uid`]），所以这里先问一次 `/login/status`。
///
/// `duration_min` 留空：这个接口给的 `listenSongs` 是**听过的歌曲数**，不是时长。
/// 把「376 首」当成「376 分钟」显示出去，比不显示更糟。
pub async fn user_detail(client: &ApiClient) -> Result<crate::api::cloud::UserInfo> {
    let Some(uid) = current_uid(client).await? else {
        return Err(crate::error::AppError::Other(
            "网易云尚未登录，请先按 L 选择「网易云」扫码".to_string(),
        ));
    };

    let root = client
        .get_json_uncached("/user/detail", &[("uid", uid)])
        .await?;
    check_api_code("/user/detail", &root)?;

    // 实测形状：顶层给 `level` / `listenSongs`，昵称与头像在 `profile` 里
    let data = data_of(&root);
    let profile = data.get("profile");

    Ok(crate::api::cloud::UserInfo {
        nickname: profile
            .and_then(|profile| pick_string(profile, &["nickname"]))
            .unwrap_or_default(),
        pic: profile
            .and_then(|profile| pick_string(profile, &["avatarUrl", "picUrl"]))
            .filter(|url| !url.trim().is_empty()),
        grade: pick_u32(data, &["level"]),
        duration_min: None,
    })
}

/// 取当前用户的云端歌单。
///
/// 未登录时给出明确的「先去登录」，而不是返回空列表——空列表会让人以为
/// 是自己没有歌单。
pub async fn user_playlists(client: &ApiClient) -> Result<Vec<crate::api::model::Playlist>> {
    let Some(uid) = current_uid(client).await? else {
        return Err(crate::error::AppError::Other(
            "网易云尚未登录，请先按 L 选择「网易云」扫码".to_string(),
        ));
    };

    let root = client
        .get_json_uncached("/user/playlist", &[("uid", uid)])
        .await?;

    Ok(extract_list(
        data_of(&root),
        &["playlist"],
        playlist_from_json,
    ))
}

/// 从 `/user/playlist` 的一条记录解析出歌单。
fn playlist_from_json(value: &Value) -> Option<crate::api::model::Playlist> {
    let id = pick_i64(value, &["id"])?;
    Some(crate::api::model::Playlist {
        id: id.to_string(),
        // 网易云的歌单 id 同时就是写操作要用的 list_id
        list_id: Some(id),
        name: pick_string(value, &["name"]).unwrap_or_else(|| "未命名歌单".to_string()),
        cover: pick_string(value, &["coverImgUrl"]),
        song_count: pick_u64(value, &["trackCount"]).unwrap_or(0) as u32,
        creator: value
            .get("creator")
            .and_then(|creator| pick_string(creator, &["nickname"])),
        description: pick_string(value, &["description"]),
        is_own: true,
    })
}

/// 取歌单的**一页**曲目。
///
/// 首屏用：先给一页让界面立刻有内容，剩下的交给 [`playlist_tracks_all`] 在后台补齐。
///
/// 网易云没有「自己的歌单 / 公开歌单」两套端点（酷狗有），两者都按歌单 id 取，
/// 所以调用方不必区分。
pub async fn playlist_tracks_page(
    client: &ApiClient,
    playlist_id: &str,
    page: u32,
    page_size: u32,
) -> Result<Vec<Song>> {
    let limit = if page_size == 0 {
        DEFAULT_PAGE_SIZE
    } else {
        page_size
    };
    // 第一页 offset 为 0
    let offset = page.saturating_sub(1).saturating_mul(limit);

    let root = client
        .get_json_uncached(
            "/playlist/track/all",
            &[
                ("id", playlist_id.to_string()),
                ("limit", limit.to_string()),
                ("offset", offset.to_string()),
            ],
        )
        .await?;

    Ok(extract_list(data_of(&root), &["songs"], song_from_json))
}

// ==================================================================
// 云端歌单的写操作
//
// 网易云的接口与酷狗完全不是一套：`/playlist/track/add` 传 `pid` + `ids`，
// 而删歌用 `/playlist/track/delete` 且参数名是 `id`（**不是** `pid`）——
// 同一个模块的两个接口参数名不一致，写反了不会报错，只会静默删不掉。
//
// 另外这里用的是**歌曲 id**（存在 `Song::hash` 里），不是酷狗歌单条目的
// `fileid`：网易云的歌单里一首歌就是按歌曲 id 定位的。
//
// 四个写接口都走 `get_json_uncached_mutating`（**不重试**）。重试的前提是「同一
// 请求重发不会改变结果」，写操作不满足：删歌第一次其实成功了、只是响应丢了的话，
// 重发会得到「歌不存在」——用户看到一句失败提示，而歌其实已经删掉了。
// 这与酷狗侧的约定一致，理由详见 `api/client.rs` 的 `get_json_mutating`。
// ==================================================================

/// 检查 NeteaseCloudMusicApi 的业务状态码。
///
/// 它用 `code` 表示成败（`200` 成功），失败时给描述。**必须检查**：它在参数不对时
/// 也返回 HTTP 200，只看 HTTP 状态会误判成成功——写接口上就是「界面提示已收藏、
/// 歌单里却没有」；读接口上则是把失败当成空数据，界面上看起来像「你确实没有歌单」。
///
/// 与酷狗那个同名概念（[`crate::api::ApiClient::check_write_result`]）不是一回事：
/// 酷狗用 `error_code`，网易云用 `code`，两边的码值也各自独立。
fn check_api_code(path: &str, root: &Value) -> Result<()> {
    let code = root.get("code").and_then(Value::as_i64).unwrap_or(200);
    if code == 200 {
        return Ok(());
    }
    // 先取 `msg`：NeteaseCloudMusicApi 失败时 `message` 常是没用的「系统错误」，
    // 具体原因在 `msg` 里（例如「需要登录」）。取错字段用户就只能对着废话猜。
    let message = pick_string(root, &["msg", "message"])
        .unwrap_or_else(|| "服务端未提供错误描述".to_string());
    Err(crate::error::AppError::Api {
        path: path.to_string(),
        code,
        message,
    })
}

/// 把歌曲加入歌单，返回提交的歌曲数。
pub async fn add_tracks_to_playlist(
    client: &ApiClient,
    list_id: i64,
    songs: &[Song],
) -> Result<usize> {
    if songs.is_empty() {
        return Ok(0);
    }
    // 一次可以传多首（逗号分隔），但歌单容量与 URL 长度都有限，分批提交
    const BATCH_SIZE: usize = 20;
    let mut written = 0usize;
    for chunk in songs.chunks(BATCH_SIZE) {
        let ids = chunk
            .iter()
            .map(|song| song.hash.clone())
            .collect::<Vec<_>>()
            .join(",");
        let root = client
            .get_json_uncached_mutating(
                "/playlist/track/add",
                &[("pid", list_id.to_string()), ("ids", ids)],
            )
            .await?;
        check_api_code("/playlist/track/add", &root)?;
        written += chunk.len();
    }
    Ok(written)
}

/// 从歌单移除歌曲。
///
/// 注意参数名是 `id`（歌单），不是加歌那个 `pid`。
pub async fn remove_tracks_from_playlist(
    client: &ApiClient,
    list_id: i64,
    songs: &[Song],
) -> Result<usize> {
    if songs.is_empty() {
        return Ok(0);
    }
    let ids = songs
        .iter()
        .map(|song| song.hash.clone())
        .collect::<Vec<_>>()
        .join(",");
    let root = client
        .get_json_uncached_mutating(
            "/playlist/track/delete",
            &[("id", list_id.to_string()), ("ids", ids)],
        )
        .await?;
    check_api_code("/playlist/track/delete", &root)?;
    Ok(songs.len())
}

/// 新建歌单，返回新歌单的 id（服务端没给时返回 `None`）。
pub async fn create_playlist(client: &ApiClient, name: &str) -> Result<Option<i64>> {
    let root = client
        .get_json_uncached_mutating("/playlist/create", &[("name", name.to_string())])
        .await?;
    check_api_code("/playlist/create", &root)?;
    Ok(pick_i64(&root, &["id"]).or_else(|| pick_i64(data_of(&root), &["id"])))
}

/// 删除（或取消收藏）歌单。
pub async fn delete_playlist(client: &ApiClient, list_id: i64) -> Result<()> {
    let root = client
        .get_json_uncached_mutating("/playlist/delete", &[("id", list_id.to_string())])
        .await?;
    check_api_code("/playlist/delete", &root)?;
    Ok(())
}

// ==================================================================
// 目录类（歌单广场 / 歌手 / 排行榜）
//
// 网易云的这几个接口跟酷狗不是一套：分页用 `offset` + `limit` 而不是
// `page` + `pagesize`，歌单主键是**数字 id 的字符串**（`/playlist/detail`
// 之类的接口都按数字 id 取），榜单本身就是一张歌单。
// ==================================================================

/// 歌单广场（热门歌单）。
pub async fn plaza_playlists(
    client: &ApiClient,
    _category_id: i64,
    page: u32,
    page_size: u32,
) -> Result<Vec<crate::api::model::Playlist>> {
    let limit = if page_size == 0 {
        DEFAULT_PAGE_SIZE
    } else {
        page_size
    };
    let offset = page.saturating_sub(1).saturating_mul(limit);

    let root = client
        .get_json_uncached(
            "/top/playlist",
            &[("limit", limit.to_string()), ("offset", offset.to_string())],
        )
        .await?;

    Ok(extract_list(
        data_of(&root),
        &["playlists"],
        plaza_playlist_from_json,
    ))
}

fn plaza_playlist_from_json(value: &Value) -> Option<crate::api::model::Playlist> {
    let id = pick_i64(value, &["id"])?;
    Some(crate::api::model::Playlist {
        id: id.to_string(),
        // 广场上的歌单都是别人的，没有可写的 listid
        list_id: None,
        name: pick_string(value, &["name"]).unwrap_or_else(|| "未命名歌单".to_string()),
        cover: pick_string(value, &["coverImgUrl", "picUrl"]),
        song_count: pick_u32(value, &["trackCount"]).unwrap_or(0),
        creator: value
            .get("creator")
            .and_then(|creator| pick_string(creator, &["nickname"])),
        description: pick_string(value, &["description"]),
        is_own: false,
    })
}

/// 热门歌手。
pub async fn artist_list(
    client: &ApiClient,
    _kind: i64,
    hot_size: u32,
) -> Result<Vec<crate::api::model::Artist>> {
    let limit = if hot_size == 0 {
        DEFAULT_PAGE_SIZE
    } else {
        hot_size
    };
    let root = client
        .get_json_uncached(
            "/top/artists",
            &[("limit", limit.to_string()), ("offset", "0".to_string())],
        )
        .await?;

    Ok(extract_list(data_of(&root), &["artists"], artist_from_json))
}

fn artist_from_json(value: &Value) -> Option<crate::api::model::Artist> {
    let id = pick_i64(value, &["id"])?;
    Some(crate::api::model::Artist {
        id,
        name: pick_string(value, &["name"]).unwrap_or_else(|| "未知歌手".to_string()),
        avatar: pick_string(value, &["picUrl", "img1v1Url"]).map(|url| {
            if url.contains('?') {
                url
            } else {
                format!("{url}?param=300y300")
            }
        }),
        // `song_count` 会被渲染成「N 首」，所以只能取**歌曲数** `musicSize`。
        // 网易云的 `albumSize` 是专辑数——把它填在这里会让每个歌手后面挂一个
        // 类型正确、语义错误的数字，而且不会有任何异常暴露。
        // 拿不到就填 None：副标题会整段省掉，比显示一个错的强。
        song_count: pick_u32(value, &["musicSize"]),
        follower_count: None,
    })
}

/// 歌手的全部歌曲。
///
/// `/artists` 一次最多给 50 首（`hotSongs`），要全得翻 `/artist/songs`。
/// 这里先给热门的 50 首——比直接报「不支持」有用，也不至于为了一个歌手
/// 翻几十页。
pub async fn artist_tracks_all(client: &ApiClient, artist_id: i64) -> Result<Vec<Song>> {
    let root = client
        .get_json_uncached("/artists", &[("id", artist_id.to_string())])
        .await?;
    let songs = extract_list(data_of(&root), &["hotSongs"], song_from_json);
    Ok(songs)
}

/// 排行榜列表。
pub async fn rank_boards(client: &ApiClient) -> Result<Vec<crate::api::model::RankBoard>> {
    let root = client.get_json_uncached("/toplist", &[]).await?;
    Ok(extract_list(data_of(&root), &["list"], rank_from_json))
}

fn rank_from_json(value: &Value) -> Option<crate::api::model::RankBoard> {
    let id = pick_i64(value, &["id"])?;
    Some(crate::api::model::RankBoard {
        id,
        name: pick_string(value, &["name"]).unwrap_or_else(|| "未命名榜单".to_string()),
        cover: pick_string(value, &["coverImgUrl"]),
        update_frequency: pick_string(value, &["updateFrequency"]),
    })
}

/// 榜单歌曲。榜单本身就是一张歌单，按歌单 id 取曲目。
pub async fn rank_tracks_all(client: &ApiClient, rank_id: i64) -> Result<Vec<Song>> {
    playlist_tracks_all(client, &rank_id.to_string()).await
}

/// 翻页还要不要继续：**只有空页才说明真没了**。
///
/// 判据只能看「这一页是不是空的」，**不能**看「这一页满不满」。`song_from_json`
/// 会过滤条目（缺 hash、字段类型不对），500 的页剩 499 条是常事，按「不满页」停
/// 会把整表静默截断在那一页——用户看到的是一个短了一截的歌单，没有任何报错。
/// 酷狗那侧（`api::catalog::collect_all_pages`）用的是同一个判据。
///
/// 签名里**故意没有** `page_size`：页大小一旦进入判据，上面的截断就回来了。
///
/// 多打一次越界请求是安全的，2026-09-28 在 NeteaseCloudMusicApi 上实测：
/// `/playlist/track/all?id=3778678&limit=500&offset=500`（该歌单 200 首）与
/// `offset=999999` 都返回 `{"songs":[],"privileges":[],"code":200}`——是**空数组、
/// 不是报错**，也没有把 offset 夹回最后一页再返回重复内容。代价只是整表加载末尾
/// 多一个请求，而这个接口一次就要几秒，多一个请求换「列表不再被截断」值得。
fn should_continue_paging(got: usize) -> bool {
    got > 0
}

/// 取一个歌单 / 榜单的**全部**曲目。
///
/// 翻页直到拿到空页。单页取 500：**这个接口本身很慢**（实测 145 首要 2.4~3.4 秒，
/// 跟 limit 关系不大，是服务端在逐个补全曲目信息），所以优化点是「少发几次请求」，
/// 而不是「每次少拿一点」。400 首的歌单这样一次就够。
///
/// 「自己的歌单」与「公开歌单」在这里是同一个端点，调用方传 id 字符串即可。
pub async fn playlist_tracks_all(client: &ApiClient, playlist_id: &str) -> Result<Vec<Song>> {
    const PAGE: u32 = 500;
    let mut all: Vec<Song> = Vec::new();

    for page in 1..=MAX_TRACK_PAGES {
        let songs = playlist_tracks_page(client, playlist_id, page, PAGE).await?;
        let got = songs.len();
        all.extend(songs);
        if !should_continue_paging(got) {
            break;
        }
    }

    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 翻页只有**空页**才停：不满一页也必须继续翻。
    ///
    /// 这条是 2026-09-28 那次修复的回归测试。旧判据是「`got < PAGE` 就停」，
    /// 于是只要某页被 `song_from_json` 过滤掉一条（缺 hash、字段类型不对），
    /// 整表就静默截断在那一页——歌单看着少了一截，而不报任何错。
    ///
    /// 退回旧判据的话，下面 `499` 那条会立刻失败。
    #[test]
    fn paging_stops_only_on_an_empty_page() {
        const PAGE: u32 = 500;

        // 空页 → 停。这是唯一的停止条件。
        assert!(!should_continue_paging(0), "空页必须停");

        // 只要还有一条就得继续翻。
        for got in [1usize, 2, 29, 499, 500] {
            assert!(should_continue_paging(got), "{got} 条也该继续翻页");
        }

        // 与旧判据的对照：这些场景下旧判据会**误停**，把整表截断。
        // 把它写在测试里，是为了让人一眼看出「旧的那条为什么错」，
        // 而不是只看到一行 `got > 0`。
        for got in [1usize, 29, 499] {
            let old_rule_would_have_stopped = got < PAGE as usize;
            assert!(
                old_rule_would_have_stopped && should_continue_paging(got),
                "{got} 条：旧判据会停（截断列表），新判据必须继续"
            );
        }

        // 满页两者一致：这个场景旧判据也是对的，别把它一起改坏。
        assert!(should_continue_paging(PAGE as usize), "满页当然要继续");
    }

    /// 越界 offset 的行为是这次敢改判据的前提，把实测结论钉在这里。
    ///
    /// 实测（2026-09-28，NeteaseCloudMusicApi，歌单 3778678 共 200 首）：
    /// `offset=500` 与 `offset=999999` 都返回 `{"songs":[],"code":200}`，
    /// 即**空数组而不是报错**。所以「继续翻到空页」最多多花一个请求，
    /// 不会把「加载列表」变成「加载失败」。
    ///
    /// 这条断言的是**我们对服务端行为的理解**，不是代码逻辑：哪天上游改成
    /// 越界就报 400/非 200，`playlist_tracks_all` 的 `?` 会把错误抛上去，
    /// 那时这里要跟着改（并重新实测），别只改代码不改这条结论。
    #[test]
    fn an_out_of_range_page_is_empty_and_therefore_ends_paging() {
        // 服务端返回空页 → 解析出来 0 条 → 停
        let songs = extract_list(
            data_of(&json!({"code": 200, "songs": [], "privileges": []})),
            &["songs"],
            song_from_json,
        );
        assert!(songs.is_empty(), "越界页解析出来应当是 0 条");
        assert!(!should_continue_paging(songs.len()), "0 条就该停了");
    }

    /// 歌手的「N 首」必须是**歌曲数**，不能是专辑数。
    ///
    /// 网易云 `/top/artists` 同时给 `albumSize`（专辑数）和 `musicSize`（歌曲数）。
    /// 早先这里取的是 `albumSize`，于是每个歌手后面挂着一个类型正确、语义错误的
    /// 数字——没有任何异常会暴露它，只能靠这条测试钉住。
    #[test]
    fn artist_song_count_uses_music_size_not_album_size() {
        let value = json!({
            "id": 2116,
            "name": "陈奕迅",
            "picUrl": "http://p/x.jpg",
            "albumSize": 37,
            "musicSize": 821,
        });
        let artist = artist_from_json(&value).expect("应当解析出歌手");
        assert_eq!(artist.song_count, Some(821), "不能拿专辑数当歌曲数");

        // 没有这个字段时宁可不显示，也不要退回去用专辑数
        let bare = json!({"id": 1, "name": "x", "albumSize": 9});
        assert_eq!(
            artist_from_json(&bare).expect("应当解析出歌手").song_count,
            None
        );
    }

    /// HTTP 200 但 code 不是 200 时必须报错——否则界面会谎报「已收藏」。
    #[test]
    fn write_result_rejects_non_200_code() {
        let root = json!({"code": 301, "message": "系统错误", "msg": "需要登录"});
        let error = check_api_code("/playlist/track/add", &root).expect_err("301 必须被当成失败");
        // 具体原因在 msg 里，别给用户那句没用的「系统错误」
        assert!(
            error.user_hint().contains("需要登录"),
            "错误提示要说清原因，实际：{}",
            error.user_hint()
        );
    }

    #[test]
    fn write_result_accepts_200() {
        let root = json!({"code": 200, "id": 123});
        check_api_code("/playlist/create", &root).expect("200 表示成功");
    }

    /// 搜索布局：`artists` / `album` / `duration`。
    #[test]
    fn song_parses_the_search_layout() {
        let value = json!({
            "id": 123,
            "name": "想你就写信 (Live)",
            "duration": 238698,
            "artists": [{"id": 6452, "name": "周杰伦"}],
            "album": {"id": 999, "name": "中国新歌声第二季 第13期"}
        });

        let song = song_from_json(&value).expect("应当能解析");
        assert_eq!(song.name, "想你就写信 (Live)");
        assert_eq!(song.hash, "123");
        assert_eq!(song.duration_ms, 238_698);
        assert_eq!(song.album_name, "中国新歌声第二季 第13期");
        assert_eq!(song.album_id, "999");
        assert_eq!(song.singers.len(), 1);
        assert_eq!(song.singers[0].name, "周杰伦");
    }

    /// 「歌曲详情」布局：`ar` / `al` / `dt`。歌单、榜单、歌手热歌都走这套。
    ///
    /// 这个测试是防回归的关键：只认搜索布局时，搜索页照常正常，而歌单页与歌手页
    /// 里的歌会全部退化成「无歌手、无专辑、时长 00:00」——肉眼很难第一眼发现。
    #[test]
    fn song_parses_the_detail_layout() {
        let value = json!({
            "id": 1440570723,
            "name": "Normal No More",
            "dt": 199578,
            "ar": [{"id": 1234, "name": "TYSM"}],
            "al": {
                "id": 5678,
                "name": "Normal No More",
                "picUrl": "https://p1.music.126.net/x.jpg"
            }
        });

        let song = song_from_json(&value).expect("应当能解析");
        assert_eq!(song.name, "Normal No More");
        assert_eq!(song.duration_ms, 199_578, "时长应当取 dt");
        assert_eq!(song.album_name, "Normal No More", "专辑名应当取 al.name");
        assert_eq!(song.album_id, "5678", "专辑 id 应当取 al.id");
        assert_eq!(song.singers.len(), 1, "歌手应当取 ar");
        assert_eq!(song.singers[0].name, "TYSM");
        assert_eq!(
            song.cover.as_deref(),
            Some("https://p1.music.126.net/x.jpg?param=300y300"),
            "封面应当取 al.picUrl 并缩到 300"
        );
    }

    /// 没有 id 就丢这一条；字段缺一半也不能 panic。
    #[test]
    fn song_tolerates_missing_fields() {
        assert!(song_from_json(&json!({"name": "没有 id"})).is_none());
        assert!(
            song_from_json(&json!({"id": 1})).is_some(),
            "只有 id 也算一首"
        );
        assert!(
            song_from_json(&json!({"id": 1, "ar": [], "al": null})).is_some(),
            "空歌手与空专辑不该 panic"
        );
    }

    /// 响应里没有 code 字段时按成功处理（部分接口只给数据）。
    #[test]
    fn write_result_tolerates_missing_code() {
        let root = json!({"playlist": {"id": 1}});
        check_api_code("/playlist/create", &root).expect("缺 code 视为成功");
    }
}
