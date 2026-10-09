//! 登录态相关接口：设备指纹、云端歌单增删。
//!
//! # 云端歌单同步怎么工作
//!
//! 酷狗的「云端歌单」就是账号下的普通歌单。同步 = 调 `/playlist/tracks/add`
//! 把本地队列写进去。写操作需要登录 cookie，并且需要数字 `listid`
//! （不是公开歌单的 `global_collection_id`）。
//!
//! 因此 [`NodeApi::user_playlists`](crate::api::node::NodeApi::user_playlists) 返回的
//! 歌单里只有 [`Playlist::is_writable`] 为真的才能作为同步目标。

use serde_json::Value;

use crate::api::data_of;
use crate::api::model::{Song, pick_i64, pick_string};
use crate::api::node::NodeApi;
use crate::error::{AppError, Result};

/// 会员形态。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum VipKind {
    /// 未识别到有效会员。
    #[default]
    None,
    /// 标准版豪华 VIP（顶层 `is_vip = 1`）。
    Standard,
    /// 酷狗概念版会员（`busi_vip` 里 `busi_type = "concept"`）。
    Concept,
    /// 其它形态，`0` 号元素是 `busi_type` 原文。
    Other(String),
}

/// 当前登录用户的公开资料（`/user/detail`）。
///
/// 接口文档里没写这个接口的返回字段，实际返回里昵称叫 `nickname`、头像叫
/// `pic`、等级叫 `p_grade`。成功标志是 `status == 1` 而不是 `code == 200`——
/// 酷狗这批接口用自己的 `status` 字段。
#[derive(Debug, Clone, Default)]
pub struct UserInfo {
    /// 昵称。
    pub nickname: String,
    /// 头像图片地址。
    pub pic: Option<String>,
    /// 用户等级（`p_grade`）。
    pub grade: Option<u32>,
    /// 累计听歌时长（**分钟**）。
    ///
    /// 单位是从实测反推的：真实时长 1320 小时 39 分 = 79239 分钟，而接口给的
    /// `duration` 正是 79239。之前当成秒，算出来是「22 小时 0 分」——差了 60 倍。
    /// 酷狗这个字段没有文档，只有对得上的那个单位才是对的。
    pub duration_min: Option<u64>,
}

impl UserInfo {
    /// 听歌时长的人类可读形式：「1320 小时 39 分」这种。
    ///
    /// 输入是分钟（见 `duration_min` 的注释），所以直接 /60 和 %60 就够，
    /// 不用再换算秒。只保留两级单位：秒级精度对「听了多久」没有意义。
    pub fn duration_text(&self) -> Option<String> {
        let total_min = self.duration_min?;
        if total_min == 0 {
            return None;
        }
        let hours = total_min / 60;
        let minutes = total_min % 60;
        Some(if hours > 0 {
            format!("{hours} 小时 {minutes} 分")
        } else {
            format!("{minutes} 分")
        })
    }
}

/// 会员信息摘要，用于界面显示。
#[derive(Debug, Clone, Default)]
pub struct VipInfo {
    pub kind: VipKind,
    /// 产品类型，如 `svip` / `tvip` / `VIP`。
    pub product: String,
    /// 到期时间原文（服务端给的是 `YYYY-MM-DD HH:MM:SS`）。
    pub end_time: String,
}

impl VipInfo {
    /// 是否有有效会员。
    pub fn is_vip(&self) -> bool {
        !matches!(self.kind, VipKind::None)
    }

    /// 会员形态的中文名（`豪华` / `概念版` / 上游给的名字）。
    fn kind_text(&self) -> &str {
        match &self.kind {
            VipKind::Standard => "豪华",
            VipKind::Concept => "概念版",
            VipKind::Other(name) => name.as_str(),
            VipKind::None => "",
        }
    }

    /// 到期日，形如 `09-21`。取不到就是空串。
    ///
    /// 只取日期部分：服务端给的是 `YYYY-MM-DD HH:MM:SS`，界面上一行放不下。
    fn end_date(&self) -> String {
        match self.end_time.split(' ').next() {
            Some(date) if date.len() >= 10 => date[5..10].to_string(),
            _ => String::new(),
        }
    }

    /// 界面用的一行摘要，例如「概念版 SVIP · 至 09-21」。
    pub fn label(&self) -> String {
        if !self.is_vip() {
            return "非会员".to_string();
        }

        let kind = self.kind_text();
        let product = self.product.to_uppercase();
        let end = self.end_date();

        if end.is_empty() {
            format!("{kind} {product}")
        } else {
            format!("{kind} {product} · 至 {end}")
        }
    }

    /// 窄侧边栏用的一行摘要，例如「概念版 09-28」。
    ///
    /// 侧边栏可用宽度最窄只有 20 列，`  会员 ` 这个前缀就占 7 列，留给值的只剩
    /// 13 列——完整形态（「概念版 TVIP · 至 09-28」= 21 列）必然折行，折出来的
    /// 第二行没有缩进，看着像排版坏了。所以这里连「至」字都省掉。产品名在侧边栏
    /// 的「音源」那一行已经有了，完整形态在首页「我的资料」里。
    pub fn short_label(&self) -> String {
        if !self.is_vip() {
            return "非会员".to_string();
        }

        let kind = self.kind_text();
        let end = self.end_date();
        if end.is_empty() {
            kind.to_string()
        } else {
            format!("{kind} {end}")
        }
    }
}

/// 二维码扫码状态。
///
/// 取值来自 `/login/qr/check` 的 `data.status`：
/// `0` 过期 / `1` 等待扫码 / `2` 待确认 / `4` 授权成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrStatus {
    Expired,
    Waiting,
    Pending,
    Success,
}

/// 一次扫码状态查询的结果。
pub struct QrCheck {
    pub status: QrStatus,
    /// 授权成功后才有的登录令牌。
    pub token: Option<String>,
    pub userid: Option<String>,
    /// 服务端随响应下发的登录 cookie（网易云走这条路）。
    ///
    /// ⚠️ 网易云的登录态**就是这个 cookie**——不存下来，之后的请求没有任何
    /// 身份信息，`/user/playlist` 永远拿不到 uid。客户端自己造不出它，只能
    /// 从服务端的响应里接住。
    pub cookie: Option<String>,
}

impl NodeApi {
    /// 二维码登录第 1 步：取 key。
    pub async fn login_qr_key(&self) -> Result<String> {
        let root = self.get_json_uncached("/login/qr/key", &[]).await?;
        pick_string(data_of(&root), &["qrcode", "key"])
            .ok_or_else(|| AppError::NotFound("`/login/qr/key` 未返回 key".to_string()))
    }

    /// 第 2 步：取二维码内容（一段 URL，由客户端自己渲染成图片）。
    ///
    /// 接口同时会返回 `base64` 的 PNG，但终端里显示不了图片，所以只取 `url` 自行编码。
    pub async fn login_qr_create(&self, key: &str) -> Result<String> {
        let root = self
            .get_json_uncached("/login/qr/create", &[("key", key.to_string())])
            .await?;
        pick_string(data_of(&root), &["url", "qrcode", "qrurl"])
            .ok_or_else(|| AppError::NotFound("`/login/qr/create` 未返回二维码内容".to_string()))
    }

    /// 第 3 步：轮询扫码状态。
    pub async fn login_qr_check(&self, key: &str) -> Result<QrCheck> {
        let root = self
            .get_json_uncached("/login/qr/check", &[("key", key.to_string())])
            .await?;
        let data = data_of(&root);
        let status = match pick_i64(data, &["status", "code"]) {
            Some(0) => QrStatus::Expired,
            Some(2) => QrStatus::Pending,
            Some(4) => QrStatus::Success,
            _ => QrStatus::Waiting,
        };
        Ok(QrCheck {
            status,
            token: pick_string(data, &["token"]),
            userid: pick_string(data, &["userid"]),
            // 酷狗走 token + userid，不用服务端 cookie
            cookie: None,
        })
    }

    /// 查当前账号的会员信息（`/user/vip/detail`）。
    ///
    /// # 为什么必须看 `busi_vip`
    ///
    /// 顶层 `is_vip` 只反映**标准版豪华 VIP**。酷狗把「概念版」等其它形态的会员放在
    /// `data.busi_vip[]` 里，每项带 `busi_type`（如 `concept`）与 `product_type`
    /// （如 `svip` / `tvip`）。只认顶层字段会把真正的会员判成「没会员」——
    /// 实测某账号顶层 `is_vip: 0`，但 `busi_vip` 里概念版 SVIP 仍在有效期内。
    /// 当前登录用户的公开资料。
    ///
    /// 成功标志是 \`status == 1\`（不是 \`code == 200\`）。取不到不影响听歌，
    /// 调用方静默降级即可。
    pub async fn user_detail(&self) -> Result<UserInfo> {
        let root = self.get_json_uncached("/user/detail", &[]).await?;
        let data = data_of(&root);

        Ok(UserInfo {
            nickname: pick_string(data, &["nickname"]).unwrap_or_default(),
            pic: pick_string(data, &["pic"]).filter(|url| !url.trim().is_empty()),
            grade: pick_i64(data, &["p_grade"]).and_then(|v| u32::try_from(v).ok()),
            duration_min: pick_i64(data, &["duration"]).and_then(|v| u64::try_from(v).ok()),
        })
    }

    pub async fn user_vip_detail(&self) -> Result<VipInfo> {
        let root = self.get_json_uncached("/user/vip/detail", &[]).await?;
        let data = data_of(&root);

        let mut info = VipInfo::default();

        // 标准版豪华 VIP
        if pick_i64(data, &["is_vip", "vip_type"]) == Some(1) {
            info.kind = VipKind::Standard;
            info.product = "VIP".to_string();
            info.end_time = pick_string(data, &["vip_end_time"]).unwrap_or_default();
            return Ok(info);
        }

        // 其它形态的会员（概念版等）
        if let Some(entries) = data.get("busi_vip").and_then(Value::as_array) {
            for entry in entries {
                if pick_i64(entry, &["is_vip"]) != Some(1) {
                    continue;
                }
                let busi_type = pick_string(entry, &["busi_type"]).unwrap_or_default();
                info.kind = if busi_type == "concept" {
                    VipKind::Concept
                } else {
                    VipKind::Other(busi_type)
                };
                info.product = pick_string(entry, &["product_type"]).unwrap_or_default();
                info.end_time = pick_string(entry, &["vip_end_time"]).unwrap_or_default();
                return Ok(info);
            }
        }

        Ok(info)
    }

    /// 领取「概念版」某一天的 VIP。
    ///
    /// `receive_day` 是**要领取的那一天**（`2026-09-23`），不是「今天」——传过去的
    /// 日期就是领到的那天，所以补领也行。官方文档同时提醒「建议不要领太多天」。
    ///
    /// # 仅概念版可用
    ///
    /// 上游是概念版接口（`source_id: 90139`）。标准版账号会拿到业务错误码，由
    /// [`check_error_code`] 抛出——调用方要先按音源挡掉，别让用户白点。
    ///
    /// 返回原始响应：上游在「今天已经领过」「账号被风控」这类情况下**不一定**给
    /// 非零 `error_code`，把响应交给调用方才能如实描述结果，而不是替它断言成功。
    pub async fn claim_day_vip(&self, receive_day: &str) -> Result<Value> {
        self.get_json_mutating(
            "/youth/day/vip",
            &[("receive_day", receive_day.to_string())],
        )
        .await
    }

    /// 把刚领到的一天 VIP 升级成「畅听 VIP」。
    ///
    /// **必须先 [`Self::claim_day_vip`]**——官方文档写明「需要先领取一天 VIP」。
    /// 上游用 cookie 里的 userid 定位账号，这里不需要额外参数。
    pub async fn upgrade_day_vip(&self) -> Result<Value> {
        self.get_json_mutating("/youth/day/vip/upgrade", &[]).await
    }

    /// 已经领取过 VIP 的日期（`2026-09-23` 这种）。
    ///
    /// # 为什么需要它
    ///
    /// 领取接口对「今天已经领过」**只回一个 `error_code`、不给描述**，实测就是
    /// 这样：界面只能显示「服务端未提供错误描述」，用户看着像程序坏了。
    /// 而「今天到底领过没有」是能直接问到的——这个只读接口返回最近约三个月的
    /// 每日记录，比本地记一个日期可靠得多：你可能是在**手机端或另一台机器**上领的。
    ///
    /// 只统计 `receive_vip == 1` 的日子；返回的列表按服务端顺序，调用方只需要
    /// 「今天在不在里面」。
    pub async fn claimed_vip_days(&self) -> Result<Vec<String>> {
        let root = self
            .get_json_uncached("/youth/month/vip/record", &[])
            .await?;
        let list = data_of(&root)
            .get("list")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();

        Ok(list
            .iter()
            .filter(|entry| pick_i64(entry, &["receive_vip"]) == Some(1))
            .filter_map(|entry| pick_string(entry, &["day"]))
            .collect())
    }

    /// 获取设备指纹 `dfid`。
    ///
    /// `/song/url` 缺了这个参数会返回「本次请求需要验证」。拿到后应回写配置，
    /// 后续启动就不必再请求。
    pub async fn fetch_device_fingerprint(&self) -> Result<String> {
        let root = self.get_json_uncached("/register/dev", &[]).await?;
        let data = data_of(&root);

        pick_string(data, &["dfid", "DFID"])
            .or_else(|| pick_string(&root, &["dfid"]))
            .ok_or_else(|| AppError::NotFound("`/register/dev` 未返回 dfid".to_string()))
    }

    /// 检查云端写操作（加歌 / 删歌）的响应。
    ///
    /// # 为什么不能只 `?` 掉
    ///
    /// 实测往**别人的**歌单加歌时，接口 HTTP 是 200，但业务上失败：
    /// ```json
    /// {"status":0,"error_code":30205}
    /// ```
    /// 只 `?`（我们原来的写法）会把它当成成功，于是界面提示「已收藏」而歌单里
    /// 根本没有这首歌——用户以为程序坏了，其实是它撒了谎。
    ///
    /// 另外这些错误码**没有**附带描述（没有 `error_msg`），所以这里把实测遇到的
    /// 几个翻译成人话，别让用户对着一串数字猜。
    pub(crate) fn check_write_result(path: &str, root: &Value) -> Result<()> {
        let Some(code) = root.get("error_code").and_then(Value::as_i64) else {
            return Ok(());
        };
        if code == 0 {
            return Ok(());
        }
        let message = match code {
            // 实测：歌单的 `list_create_userid` 不是自己时（收藏的别人的歌单）返回它
            30205 => "该歌单不是你自己的，无法往别人的歌单里加歌",
            _ => "服务端未提供错误描述",
        };
        Err(AppError::Api {
            path: path.to_string(),
            code,
            message: message.to_string(),
        })
    }

    /// 批量把歌曲加入云端歌单。
    ///
    /// 返回实际提交的歌曲数量，便于界面给出「已同步 N 首」的反馈。
    pub async fn add_tracks_to_playlist(
        &self,
        source: crate::source::SourceKind,
        list_id: i64,
        songs: &[Song],
    ) -> Result<usize> {
        if songs.is_empty() {
            return Ok(0);
        }
        // 网易云走的是另一套接口（`/playlist/track/add` 的 `pid` + `ids`），
        // 与酷狗的「歌名|hash|专辑id」完全不通，必须按音源分派。
        if let crate::source::SourceKind::Netease = source {
            return crate::source::netease::add_tracks_to_playlist(
                &crate::api::ApiClient::Node(self.clone()),
                list_id,
                songs,
            )
            .await;
        }

        // 服务端按逗号分隔多首、按竖线分隔字段，单次提交太多会被截断
        const BATCH_SIZE: usize = 20;
        let mut written = 0usize;

        for chunk in songs.chunks(BATCH_SIZE) {
            let payload = chunk
                .iter()
                .map(encode_track_entry)
                .collect::<Vec<_>>()
                .join(",");

            let root = self
                .get_json_uncached_mutating(
                    "/playlist/tracks/add",
                    &[("listid", list_id.to_string()), ("data", payload)],
                )
                .await?;
            Self::check_write_result("/playlist/tracks/add", &root)?;

            written += chunk.len();
        }

        Ok(written)
    }

    /// 从云端歌单移除歌曲。
    ///
    /// `file_ids` 是**歌单条目的 `fileid`**，不是歌曲 hash —— 传 hash 会静默删不掉。
    /// fileid 只在歌单接口的返回里才有（见 `Song::file_id`）。
    ///
    /// 传 `Song` 而不是 id 列表，是因为两个音源定位一首歌用的东西不同：
    /// 酷狗要歌单条目的 `file_id`，网易云要**歌曲 id**（存在 `Song::hash`）。
    pub async fn remove_tracks_from_playlist(
        &self,
        source: crate::source::SourceKind,
        list_id: i64,
        songs: &[Song],
    ) -> Result<usize> {
        if songs.is_empty() {
            return Ok(0);
        }

        if let crate::source::SourceKind::Netease = source {
            return crate::source::netease::remove_tracks_from_playlist(
                &crate::api::ApiClient::Node(self.clone()),
                list_id,
                songs,
            )
            .await;
        }

        let file_ids: Vec<i64> = songs.iter().filter_map(|song| song.file_id).collect();
        if file_ids.is_empty() {
            // 酷狗必须有 fileid 才能定位歌单条目，搜索结果里的歌没有它
            return Ok(0);
        }

        let payload = file_ids
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");

        let root = self
            .get_json_uncached_mutating(
                "/playlist/tracks/del",
                &[("listid", list_id.to_string()), ("fileids", payload)],
            )
            .await?;
        Self::check_write_result("/playlist/tracks/del", &root)?;

        Ok(file_ids.len())
    }

    /// 删除（或取消收藏）一个云端歌单。
    pub async fn delete_playlist(
        &self,
        source: crate::source::SourceKind,
        list_id: i64,
    ) -> Result<()> {
        if let crate::source::SourceKind::Netease = source {
            return crate::source::netease::delete_playlist(
                &crate::api::ApiClient::Node(self.clone()),
                list_id,
            )
            .await;
        }
        self.get_json_uncached_mutating("/playlist/del", &[("listid", list_id.to_string())])
            .await?;
        Ok(())
    }

    /// 新建一个云端歌单。返回新歌单的 listid（服务端没给时返回 `None`）。
    ///
    /// 只传 `name` 与 `type=0`：文档把 `list_create_userid` / `list_create_listid` 列为必选，
    /// 但那两个是「收藏他人歌单」(`type=1`) 用的，自建歌单不适用。
    pub async fn create_playlist(
        &self,
        source: crate::source::SourceKind,
        name: &str,
    ) -> Result<Option<i64>> {
        if let crate::source::SourceKind::Netease = source {
            return crate::source::netease::create_playlist(
                &crate::api::ApiClient::Node(self.clone()),
                name,
            )
            .await;
        }
        let root = self
            .get_json_uncached_mutating(
                "/playlist/add",
                &[("name", name.to_string()), ("type", "0".to_string())],
            )
            .await?;
        Ok(pick_i64(data_of(&root), &["listid", "list_id", "id"]))
    }
}

/// 从 `/playlist/add` 的响应里取新建歌单的 listid。
///
/// 服务端把结果埋在 `data.info` 下（`data` 顶层**没有** `listid`），只扫顶层会恒
/// 返回 `None`：调用方只能显示「已新建」却拿不到 id，后续加歌也就没有目标。
/// 真实响应形如 `{"data":{"info":{"listid":5,…},"list_count":5},"status":1,"error_code":0}`。
pub(crate) fn created_listid(root: &Value) -> Option<i64> {
    let data = data_of(root);
    pick_i64(data, &["listid", "list_id", "id"]).or_else(|| {
        data.get("info")
            .and_then(|info| pick_i64(info, &["listid", "list_id", "id"]))
    })
}

/// 拼 `/playlist/tracks/add` 的 `data` 参数：`歌名|hash|专辑id|album_audio_id`。
///
/// 歌名里混入 `|` 或 `,` 会破坏分隔结构（用户搜到的歌名完全可能带逗号），
/// 所以先做替换。
pub(crate) fn encode_track_entry(song: &Song) -> String {
    let safe_name = song.name.replace(['|', ','], " ");
    format!(
        "{}|{}|{}|{}",
        safe_name, song.hash, song.album_id, song.album_audio_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 短形态必须省掉产品名与「至」字，且**真的放得进最窄的侧边栏**。
    ///
    /// 侧边栏最窄 24 列 → 可用 20 列，`  会员 ` 前缀占 7 列，留给值的只有 13 列。
    /// 完整形态 21 列，加上前缀必然折行，折出来的第二行没有缩进，看着像排版坏了。
    /// 这条测试就是锁住「短形态放得下」这个前提。
    #[test]
    fn short_label_fits_the_narrowest_sidebar() {
        let info = VipInfo {
            kind: VipKind::Concept,
            product: "tvip".to_string(),
            end_time: "2026-09-28 12:00:00".to_string(),
        };

        assert_eq!(info.label(), "概念版 TVIP · 至 09-28");
        assert_eq!(info.short_label(), "概念版 09-28");

        // 侧边栏 24 列 − 边框 2 − 左右内边距 2 = 20 列可用
        let room = 20 - crate::ui::widgets::display_width("  会员 ");
        let width = crate::ui::widgets::display_width(&info.short_label());
        assert!(
            width <= room,
            "短形态 {width} 列，最窄的侧边栏只剩 {room} 列"
        );
    }

    /// 非会员与取不到到期日时也不能崩、不能吐出半截字符串。
    #[test]
    fn labels_degrade_without_an_end_date() {
        let no_date = VipInfo {
            kind: VipKind::Standard,
            product: "svip".to_string(),
            end_time: String::new(),
        };
        assert_eq!(no_date.label(), "豪华 SVIP");
        assert_eq!(no_date.short_label(), "豪华");

        let none = VipInfo::default();
        assert_eq!(none.label(), "非会员");
        assert_eq!(none.short_label(), "非会员");
    }

    /// HTTP 200 但业务失败时必须报错——否则界面会谎报「已收藏」。
    #[test]
    fn write_result_rejects_non_zero_error_code() {
        let root = json!({"status": 0, "error_code": 30205});
        let error = NodeApi::check_write_result("/playlist/tracks/add", &root)
            .expect_err("30205 必须被当成失败");
        assert!(
            error.user_hint().contains("不是你自己的"),
            "错误提示要说清原因，实际：{}",
            error.user_hint()
        );
    }

    /// 成功（`error_code: 0`）不能误判成失败。
    #[test]
    fn write_result_accepts_zero_error_code() {
        let root = json!({"status": 1, "error_code": 0, "data": {}});
        NodeApi::check_write_result("/playlist/tracks/add", &root).expect("0 表示成功");
    }

    /// 没有 `error_code` 字段的响应（部分接口只给 `data`）不该被拦下。
    #[test]
    fn write_result_tolerates_missing_error_code() {
        let root = json!({"data": {"status": 1}});
        NodeApi::check_write_result("/playlist/tracks/del", &root).expect("缺字段视为成功");
    }

    #[test]
    fn created_listid_reads_the_nested_info_object() {
        // 真实响应形状：listid 在 data.info 里，data 顶层没有。
        let root = json!({
            "data": {"info": {"listid": 5, "name": "x"}, "list_count": 5},
            "status": 1,
            "error_code": 0
        });
        assert_eq!(created_listid(&root), Some(5));
    }

    #[test]
    fn created_listid_still_accepts_a_flat_data_object() {
        let root = json!({"data": {"listid": 7}, "status": 1});
        assert_eq!(created_listid(&root), Some(7));
    }

    #[test]
    fn created_listid_is_none_when_the_server_omits_it() {
        let root = json!({"data": {"info": {"name": "x"}}, "status": 1, "error_code": 0});
        assert_eq!(created_listid(&root), None);
    }

    #[test]
    fn encodes_track_entry_with_expected_field_order() {
        let song = Song {
            name: "海阔天空".to_string(),
            hash: "ABC".to_string(),
            album_id: "123".to_string(),
            album_audio_id: 456,
            ..Song::default()
        };
        assert_eq!(encode_track_entry(&song), "海阔天空|ABC|123|456");
    }

    #[test]
    fn sanitizes_separators_inside_song_name() {
        let song = Song {
            name: "Hello, World|Live".to_string(),
            hash: "H".to_string(),
            album_id: "1".to_string(),
            album_audio_id: 2,
            ..Song::default()
        };
        let encoded = encode_track_entry(&song);
        assert_eq!(encoded, "Hello  World Live|H|1|2");
        // 字段数必须仍然是 4
        assert_eq!(encoded.split('|').count(), 4);
    }

    /// 批量加歌走的是「逗号分隔多首、竖线分隔字段」的裸格式，任何残留的分隔符都会
    /// 让服务端解析错位。这里把所有名字里的分隔符都替换掉，保证批量插入顺序不错乱。
    #[test]
    fn batch_payload_keeps_field_count_stable() {
        let names = ["a", "b,c", "d|e", "f,g|h"];
        let songs: Vec<Song> = names
            .iter()
            .map(|name| Song {
                name: (*name).to_string(),
                hash: "H".to_string(),
                album_id: "1".to_string(),
                album_audio_id: 2,
                ..Song::default()
            })
            .collect();

        let payload = songs
            .iter()
            .map(encode_track_entry)
            .collect::<Vec<_>>()
            .join(",");

        // 逗号只能出现在「歌曲之间」，竖线只能出现在「字段之间」
        assert_eq!(payload.matches(',').count(), names.len() - 1);
        for entry in payload.split(',') {
            assert_eq!(entry.split('|').count(), 4);
        }
    }

    #[test]
    fn listen_duration_is_minutes_not_seconds() {
        // 实测：真实听歌时长 1320 小时 39 分，接口 /user/detail 给的
        // `duration` 是 79239 —— 正好是分钟数（1320*60 + 39）。
        // 之前当成秒，算出来是「22 小时 0 分」，差 60 倍。
        let info = UserInfo {
            duration_min: Some(79239),
            ..Default::default()
        };
        assert_eq!(info.duration_text().as_deref(), Some("1320 小时 39 分"));
    }

    #[test]
    fn listen_duration_edge_cases() {
        // 0 表示没数据，不显示
        assert_eq!(
            UserInfo {
                duration_min: Some(0),
                ..Default::default()
            }
            .duration_text(),
            None
        );
        assert_eq!(
            UserInfo {
                duration_min: None,
                ..Default::default()
            }
            .duration_text(),
            None
        );
        // 不足一小时只显示分钟
        assert_eq!(
            UserInfo {
                duration_min: Some(39),
                ..Default::default()
            }
            .duration_text()
            .as_deref(),
            Some("39 分")
        );
        // 整小时
        assert_eq!(
            UserInfo {
                duration_min: Some(120),
                ..Default::default()
            }
            .duration_text()
            .as_deref(),
            Some("2 小时 0 分")
        );
    }
}
