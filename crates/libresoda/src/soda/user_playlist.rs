//! 「我的歌单」（对应上游 `soda/user_playlist.go`），以及客户端侧边栏用的
//! 「我创建的歌单」列表（`GetMyPlaylists`，上游没有）。

use super::playlist::build_playlist_from_user_item;
use super::types::{PCMeResponse, UserPlaylistItem, UserPlaylistResponse};
use super::Soda;
use crate::error::{Result, SodaError};
use crate::http;
use crate::model::Playlist;

/// 客户端 `GetMyPlaylists` 的 PC 路径（侧边栏「创建的歌单」）。
pub const MY_PLAYLISTS_PATH: &str = "/luna/pc/me/playlist";

/// 「我创建的歌单」一页（客户端 `useSidebarPlaylistsQuery` 的翻页语义）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MyPlaylistsPage {
    pub playlists: Vec<Playlist>,
    pub has_more: bool,
    pub next_cursor: String,
}

/// 拉取「我创建的歌单」（客户端 `GetMyPlaylists`，`GET /luna/pc/me/playlist`）。
///
/// 客户端传的查询参数只有 `cursor` + `count`（默认 50）；IDL 里还有一个 `ab_param`，
/// 但它只出现在 body 里，GET 时不会发出去。
pub fn get_my_playlists(soda: &Soda, cursor: &str, count: i64) -> Result<MyPlaylistsPage> {
    if !soda.has_cookie() {
        return Err(SodaError::invalid_input("soda my playlists require cookie"));
    }
    let count = if count <= 0 { 50 } else { count.min(100) };
    let value = super::pc_get_json(
        soda,
        MY_PLAYLISTS_PATH,
        &[
            ("cursor", cursor.trim().to_string()),
            ("count", count.to_string()),
        ],
    )?;
    Ok(parse_my_playlists(&value))
}

/// 解析「我创建的歌单」回包。
///
/// 列表字段缺失（无歌单时服务端只回 `status_info`）当作空列表，不报错。
pub fn parse_my_playlists(value: &serde_json::Value) -> MyPlaylistsPage {
    let items = parse_my_playlist_items(value);
    MyPlaylistsPage {
        playlists: items,
        has_more: value
            .get("has_more")
            .and_then(|flag| flag.as_bool())
            .unwrap_or(false),
        next_cursor: value
            .get("next_cursor")
            .and_then(|cursor| cursor.as_str())
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

fn parse_my_playlist_items(value: &serde_json::Value) -> Vec<Playlist> {
    let raw = match value.get("playlists").and_then(|list| list.as_array()) {
        Some(list) => list,
        None => return Vec::new(),
    };
    let mut out: Vec<Playlist> = Vec::new();
    for item in raw {
        // 歌单实体与 /luna/pc/playlist/detail 的 playlist 同构，直接复用解析。
        let Ok(parsed) = serde_json::from_value::<UserPlaylistItem>(item.clone()) else {
            continue;
        };
        let playlist = build_playlist_from_user_item(&parsed, "", "");
        if playlist.id.is_empty() {
            continue;
        }
        out.push(playlist);
    }
    out
}

/// 等价 `GetUserPlaylists`：先取用户 id，再分页拉取歌单并做切片。
pub fn get_user_playlists(soda: &Soda, page: i64, limit: i64) -> Result<Vec<Playlist>> {
    if !soda.has_cookie() {
        return Err(SodaError::invalid_input(
            "soda user playlists require cookie",
        ));
    }
    let page = if page < 1 { 1 } else { page };
    let limit = if limit <= 0 {
        30
    } else if limit > 100 {
        100
    } else {
        limit
    };

    let me = fetch_pc_me(soda)?;
    let user_id = me.my_info.id.trim().to_string();
    if user_id.is_empty() {
        return Err(SodaError::invalid_input(
            "soda user playlists require logged-in user id",
        ));
    }

    let target_count = page * limit;
    let request_count = target_count.clamp(50, 100);

    let mut cursor = String::new();
    let mut seen_cursors: Vec<String> = Vec::new();
    let mut seen_playlists: Vec<String> = Vec::new();
    // 防御：target_count 来自调用方的 page*limit，可能极大；
    // 每页最多 100 条、最多 20 轮，容量上限按 2048 兜底即可。
    let mut playlists: Vec<Playlist> = Vec::with_capacity(target_count.clamp(0, 2048) as usize);

    let mut attempts = 0;
    while attempts < 20 && (playlists.len() as i64) < target_count {
        attempts += 1;
        let response = fetch_user_playlist_page(soda, &user_id, &cursor, request_count)?;
        for item in &response.playlists {
            let playlist = build_playlist_from_user_item(item, &user_id, &me.my_info.nickname);
            if playlist.id.is_empty() || seen_playlists.contains(&playlist.id) {
                continue;
            }
            seen_playlists.push(playlist.id.clone());
            playlists.push(playlist);
        }

        let next_cursor = response.next_cursor.trim().to_string();
        if next_cursor.is_empty() || next_cursor == cursor || seen_cursors.contains(&next_cursor) {
            break;
        }
        if !response.has_more && (response.playlists.len() as i64) < request_count {
            break;
        }
        seen_cursors.push(next_cursor.clone());
        cursor = next_cursor;
    }

    let start = (page - 1) * limit;
    if start >= playlists.len() as i64 {
        return Ok(Vec::new());
    }
    let mut end = start + limit;
    if end > playlists.len() as i64 {
        end = playlists.len() as i64;
    }
    Ok(playlists[start as usize..end as usize].to_vec())
}

/// 等价 `fetchPCMe`。
pub fn fetch_pc_me(soda: &Soda) -> Result<PCMeResponse> {
    let body = http::get(&super::pc_me_url(), &super::pc_request_options(soda))?;
    let response: PCMeResponse = serde_json::from_slice(&body)
        .map_err(|err| SodaError::json(format!("soda me json parse error: {err}")))?;
    if response.status_code != 0 {
        return Err(SodaError::Api {
            status_code: response.status_code,
            status_msg: if response.status_info.status_msg.trim().is_empty() {
                "unknown error".to_string()
            } else {
                response.status_info.status_msg.clone()
            },
        });
    }
    Ok(response)
}

/// 等价 `fetchUserPlaylistPage`。
pub fn fetch_user_playlist_page(
    soda: &Soda,
    user_id: &str,
    cursor: &str,
    count: i64,
) -> Result<UserPlaylistResponse> {
    let url = super::pc_user_playlist_url(user_id, cursor, count);
    let body = http::get(&url, &super::pc_request_options(soda))?;
    let response: UserPlaylistResponse = serde_json::from_slice(&body)
        .map_err(|err| SodaError::json(format!("soda user playlist json parse error: {err}")))?;
    if response.status_code != 0 {
        return Err(SodaError::Api {
            status_code: response.status_code,
            status_msg: if response.status_info.status_msg.trim().is_empty() {
                "unknown error".to_string()
            } else {
                response.status_info.status_msg.clone()
            },
        });
    }
    Ok(response)
}

impl Soda {
    /// 我创建的歌单（客户端侧边栏用；`cursor` 传空串取第一页）。
    pub fn get_my_playlists(&self, cursor: &str, count: i64) -> Result<MyPlaylistsPage> {
        get_my_playlists(self, cursor, count)
    }

    /// 等价 `(*Soda).GetUserPlaylists`。
    pub fn get_user_playlists(&self, page: i64, limit: i64) -> Result<Vec<Playlist>> {
        get_user_playlists(self, page, limit)
    }
}
