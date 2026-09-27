//! 领域模型，以及酷狗接口响应的**防御性**解析。
//!
//! # 为什么要写得这么啰嗦
//!
//! KuGouMusicApi 是对官方接口的逆向封装，返回的是酷狗原始 JSON。它的字段有两个
//! 稳定特征：
//!
//! 1. **同一语义有多个键名**。歌单 id 可能是 `listid` / `list_id` / `specialid`；
//!    歌曲列表可能在 `songs` / `list` / `info` / `musiclist` 下。
//! 2. **数字与字符串混用**。`AlbumID` 有时是 `"123"`，有时是 `123`，有时是 `null`。
//!
//! 直接 `#[derive(Deserialize)]` 到强类型结构体，任何一个字段漂移都会让整条响应
//! 解析失败，表现为「界面一片空白」这种最难排查的故障。所以这里：
//!
//! * 用 [`Value`] 承接叶子字段，再用 [`value_to_string`] 一类工具做宽容转换；
//! * 用「候选键名列表」代替写死键名；
//! * 所有构造器返回 `Option`，单条数据坏掉只丢一条，不会连累整页。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{AppError, Result};

// ============================================================================
// 领域模型
// ============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Singer {
    pub id: i64,
    pub name: String,
}

/// 一首歌。整个程序内部只认这一种表示，API 层的各种原始形态都要归一到它。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Song {
    pub name: String,
    /// 音频文件 hash，是 `/song/url`、`/search/lyric` 的唯一标识。
    pub hash: String,
    pub album_id: String,
    /// `album_audio_id`（也叫 `MixSongID`）。取播放链接时要用它。
    pub album_audio_id: i64,
    pub album_name: String,
    pub singers: Vec<Singer>,
    pub duration_ms: u64,
    pub cover: Option<String>,
    /// 酷狗版权标记：`8` 通常表示可完整播放，`0` 表示需要 VIP 或已下架。
    ///
    /// 用 `Option` 是因为有些接口不返回这个字段，**把「没给」和「给了 0」
    /// 混为一谈会让客户端对一堆正常歌曲误报版权受限**。
    ///
    /// # 排行榜那个 `privilege_download` 为什么不算数
    ///
    /// 早先这里写的是「排行榜不返回这个字段」，实测**不准确**：`/rank/audio`
    /// 的条目里有 `privilege_download`，其 `.privilege` 取 `0` / `8` / `10`
    /// （TOP500 里恰好 2 首为 `0`，且与 `deprecated.pay_type == 0` 精确对应）。
    ///
    /// 但它**没有采用**：键名里的 `download` 说明它管的是「能否下载」，
    /// 与「能否播放」不是一回事；而取值 `0` 究竟意味着不可播还是仅不可下载，
    /// 在未登录的探测环境下无法证实（`/song/url` 一律回 `20028 需要验证`）。
    /// 拿不准就不猜——取链失败时给出的原因比一个可能错的「版权受限」标签更有用。
    pub privilege: Option<i64>,
    /// 歌单条目 id，仅从歌单接口返回，云端删歌时需要。
    pub file_id: Option<i64>,
    /// 同一首歌在不同接口里给出的**多个 hash**。
    ///
    /// 酷狗在搜索和歌单接口里给的 `FileHash`/`hash` 可能指向"已下架"的版本，
    /// 但同一首歌在 `audio_info.hash_128`/`hash_320`/... 里还有别的 hash——
    /// 那些 hash 调 `/song/url` 才能拿到链接。**把这一首**歌的每一个 hash 都
    /// 喂给 `/privilege/lite`，由它逐个回 variant 试，是唯一能绕开"搜索时给
    /// 的 hash 拿不到资源"的修复路径。
    ///
    /// 留空是合法状态：很多接口根本不返回 `audio_info`，搜不到就是空。
    #[serde(default)]
    pub extra_hashes: std::collections::BTreeMap<String, String>,
    /// 这首歌是从哪个音源取来的。
    ///
    /// **取播放链接时必须用它，而不是「当前音源」**：队列是可以跨音源的——
    /// 在酷狗搜几首入队、再切到网易云，那些酷狗的歌仍应能正常播放。
    /// 早先没有这个字段，播放时一律用当前音源去取链接，切过音源之后
    /// 队列里的旧歌就全部播不了（hash 在另一个平台的接口里根本查不到）。
    ///
    /// 默认值只为让反序列化/结构体更新语法不用到处改；真正的来源由分派层
    /// （`SourceKind` 上那些返回 `Vec<Song>` 的方法）统一盖章。
    #[serde(default)]
    pub source: crate::source::SourceKind,
}

impl Song {
    /// 封面地址，并把 `{size}` 占位符展开成具体像素值。
    ///
    /// 酷狗返回的 `sizable_cover` 形如
    /// `http://imge.kugou.com/stdmusic/{size}/20200819/xxx.jpg` ——
    /// **不替换的话它就是个 404**。早先这段展开只写在 mpris 里（给桌面组件用），
    /// 封面渲染那条路径漏了，结果封面永远加载不出来，界面只能一直显示占位。
    /// 收进这里，谁用谁展开，不会再漏。
    ///
    /// 生产代码里目前只有 MPRIS 那条路用它（界面上取封面走的是
    /// `source::cover_url`，还要过音源自己的过滤）。非 Unix 没有 MPRIS，
    /// 于是这里只在测试里被调用——留 `allow` 而不是 cfg 掉：模型层不该因为
    /// 某个平台没有桌面集成就换个形状。
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn cover_url(&self, size: u32) -> Option<String> {
        let url = self.cover.as_ref()?;
        Some(if url.contains("{size}") {
            url.replace("{size}", &size.to_string())
        } else {
            url.clone()
        })
    }

    /// 歌手名拼接，用于列表展示。
    pub fn singer_text(&self) -> String {
        if self.singers.is_empty() {
            return "未知歌手".to_string();
        }
        self.singers
            .iter()
            .map(|singer| singer.name.as_str())
            .collect::<Vec<_>>()
            .join("、")
    }

    /// 版权标记是否表示可播。
    ///
    /// 酷狗没有公开 privilege 的语义，只能靠实测分布倒推：取真实歌单 30 首，
    /// **29 首是 `10`、只有 1 首是 `0`**。把 `10` 当「不可播」会让整页正常歌曲误报
    /// 版权受限，所以这里只把明确的 `0` 视作受限，其余（含字段缺失）都按可播处理——
    /// 真正不能播时取链会失败并给出准确原因，比提前瞎猜好。
    pub fn looks_playable(&self) -> bool {
        match self.privilege {
            None => true,
            Some(code) => code != 0,
        }
    }

    /// 展示用时长，形如 `03:47`。
    pub fn duration_text(&self) -> String {
        format_duration_ms(self.duration_ms)
    }

    /// 缓存键：同一首歌的不同音质要分开缓存。
    pub fn cache_key(&self, quality: &str) -> String {
        format!("{}-{}", self.hash.to_lowercase(), quality)
    }

    /// 试听片段专用的缓存键。
    ///
    /// # 为什么片段不能用正常缓存键
    ///
    /// 缓存键只由 `hash-quality` 组成，不区分「完整版」和「试听片段」。若把 60 秒
    /// 片段按正常键存下来，之后即使会员生效，播放也会命中这个旧片段——表现就是
    /// 「明明已经是会员，这首歌还是只能听几十秒」，而且很难想到是缓存问题。
    ///
    /// 用独立键之后：播放只查正常键，查不到就重新取链；能拿到完整版就存正常键，
    /// 拿不到才存片段键。会员一生效，下一次播放自然就是完整版。
    pub fn trial_cache_key(&self, quality: &str) -> String {
        format!("{}-{}-trial", self.hash.to_lowercase(), quality)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Playlist {
    /// 歌单主键。公开歌单是 `global_collection_id` 字符串，自建歌单是数字 `listid`。
    pub id: String,
    /// 自建/收藏歌单的数字 `listid`，云端增删歌曲需要它。
    pub list_id: Option<i64>,
    pub name: String,
    pub cover: Option<String>,
    pub song_count: u32,
    pub creator: Option<String>,
    pub description: Option<String>,
    /// 是否是当前登录用户自己创建的歌单。
    pub is_own: bool,
}

impl Playlist {
    /// 能否对它做云端写操作（加歌/删歌）。
    pub fn is_writable(&self) -> bool {
        self.list_id.is_some() && self.is_own
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Artist {
    pub id: i64,
    pub name: String,
    pub avatar: Option<String>,
    pub song_count: Option<u32>,
    pub follower_count: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RankBoard {
    pub id: i64,
    pub name: String,
    pub cover: Option<String>,
    pub update_frequency: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LyricLine {
    /// 该行起始时间（毫秒）。
    pub time_ms: u64,
    pub text: String,
    /// 该行的译文（若有）。
    ///
    /// 酷狗把翻译/音译放在 KRC 的 `[language:base64]` 标签里（文档未记载）：
    /// base64 解开是 JSON，`content[].type` 为 1 是翻译、0 是音译，
    /// `lyricContent` 按行与主歌词一一对应。
    ///
    /// 注意区分翻译与音译的是 **`type`** 而不是 `language`——实测同一首歌里
    /// 两个轨道的 `language` 都是 0，只有 `type` 不同。
    pub translation: Option<String>,
    /// 该行的音译（罗马音，若有）。来自 `type == 0` 的轨道。
    pub romanization: Option<String>,
    /// 逐字时间戳（KRC 才有，与 [`Self::text`] 的字符**一一对应**）。
    ///
    /// 为空表示拿不到逐字信息（LRC 格式、或这行的标记数与字数对不上），
    /// 此时退回整行高亮——宁可少个效果，也不能让歌词和时间错位。
    #[serde(default)]
    pub words: Vec<LyricWord>,
}

/// 一个字的起止时间（毫秒，绝对时间轴）。
///
/// KRC 里写作 `字<该字在本行的偏移毫秒,该字持续毫秒,0>`，绝对时间 = 行起始 + 偏移。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricWord {
    pub start_ms: u64,
    pub end_ms: u64,
}

impl LyricWord {
    /// 该字在 `position_ms` 时刻**已经唱了多少**，`0.0` ~ `1.0`。
    ///
    /// 用连续比例而不是「未唱 / 正在唱 / 已唱」三档，是为了画出 Apple Music 那种
    /// 柔和的推进：边界字的颜色取「未唱色 → 已唱色」之间的插值，看上去是渐变扫过，
    /// 而不是一格一格硬跳。
    ///
    /// 真实数据里偶有 `end_ms <= start_ms` 的坏字（KRC 里见过），`max(1)` 保证
    /// 分母非零——那种字的行为退化成「唱到 start 就整个亮起来」。
    pub fn progress_at(self, position_ms: u64) -> f32 {
        if position_ms >= self.end_ms {
            return 1.0;
        }
        if position_ms <= self.start_ms {
            return 0.0;
        }
        let span = self.end_ms.saturating_sub(self.start_ms).max(1) as f32;
        (position_ms - self.start_ms) as f32 / span
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Lyric {
    pub lines: Vec<LyricLine>,
}

impl Lyric {
    /// 二分查找当前时间对应的歌词行下标。
    ///
    /// 返回 `None` 表示还没到第一句歌词（前奏阶段）。
    pub fn index_at(&self, position_ms: u64) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        if position_ms < self.lines[0].time_ms {
            return None;
        }
        // partition_point 返回第一个 time_ms > position 的位置，减一即为当前行
        let index = self
            .lines
            .partition_point(|line| line.time_ms <= position_ms);
        index.checked_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

// ============================================================================
// 宽容的 JSON 取值工具
// ============================================================================

/// 把 `Value` 转成 `String`，兼容数字与布尔。
pub fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// 把 `Value` 转成 `i64`，兼容 `"123"`、`123.0`、`null`。
pub fn value_to_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64)),
        Value::String(text) => {
            let trimmed = text.trim();
            trimmed
                .parse::<i64>()
                .ok()
                .or_else(|| trimmed.parse::<f64>().ok().map(|float| float as i64))
        }
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

pub fn value_to_u64(value: &Value) -> Option<u64> {
    value_to_i64(value).and_then(|number| u64::try_from(number).ok())
}

pub fn value_to_u32(value: &Value) -> Option<u32> {
    value_to_i64(value).and_then(|number| u32::try_from(number).ok())
}

/// 依次尝试多个键名，返回第一个能转成字符串的值。
///
/// 这是应对「同一语义多个键名」的主要手段。
pub fn pick_string(object: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(value_to_string))
}

pub fn pick_i64(object: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(value_to_i64))
}

pub fn pick_u64(object: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(value_to_u64))
}

/// 依次尝试多个键名，返回第一个是数组的值；都没有则返回空数组。
pub fn pick_array<'a>(object: &'a Value, keys: &[&str]) -> &'a [Value] {
    for key in keys {
        if let Some(array) = object.get(*key).and_then(Value::as_array) {
            return array.as_slice();
        }
    }
    &[]
}

/// 校验 KuGouMusicApi 的业务错误码。
///
/// # 为什么只认 `error_code`
///
/// 曾试图把 `errcode` 也纳入判断（`/song/url` 用它返回 20028「本次请求需要验证」），
/// 但实测发现**各接口的 `errcode` 语义不一致**：
///
/// * `/song/url` 失败：`errcode: 20028`
/// * `/search/lyric` 成功：`errcode: 200`（！）
///
/// 一刀切地「非 0 即错」会把每次歌词搜索都判成失败。所以这里只认语义统一的
/// `error_code`；`/song/url` 的错误由 `catalog::song_stream_url` 显式读 `error`
/// 字段处理——那里能拿到「本次请求需要验证」这样的可读原因，比错误码更有用。
///
/// 同理刻意**不**看 `status`：不同接口 1 / 200 / 2 都出现过，且含义相反。
pub fn check_error_code(path: &str, root: &Value) -> Result<()> {
    let Some(code) = root.get("error_code").and_then(value_to_i64) else {
        return Ok(());
    };
    if code == 0 {
        return Ok(());
    }
    let message = pick_string(root, &["error_msg", "errmsg", "msg", "message"])
        .filter(|text| !text.trim().is_empty())
        // 带上错误码：不少接口（例如领取概念版 VIP）失败时**只给码不给描述**，
        // 界面上一句「服务端未提供错误描述」等于什么都没说，用户和我们都无从下手。
        // 有码至少能拿去搜、能对照服务端日志。
        .unwrap_or_else(|| format!("服务端未提供错误描述（错误码 {code}）"));
    Err(AppError::Api {
        path: path.to_string(),
        code,
        message,
    })
}

// ============================================================================
// 歌曲解析
// ============================================================================

/// 从任意一种酷狗歌曲 JSON 中提取 [`Song`]。
///
/// 酷狗在不同接口里用**三套完全不同的布局**描述同一首歌，这里都要认：
///
/// * 搜索结果：`SongName` / `FileHash` / `Singer` 数组
/// * 歌单条目：`filename` / `hash` / `singername`
/// * 排行榜条目（`/rank/audio`）：顶层**没有** `hash`、`duration`、`album_name`，
///   音频信息全在 `audio_info`（`hash_128` / `hash_320` / `hash_flac`…），
///   专辑名在 `album_info.album_name`，歌手在 `authors[].author_name`
///
/// 缺少 hash 时返回 `None`——没有 hash 就无法取播放链接，这条数据没有意义。
pub fn song_from_json(value: &Value) -> Option<Song> {
    let hash = pick_string(value, &["FileHash", "hash", "Filehash", "file_hash"])
        .or_else(|| pick_audio_hash(value))?;

    let name = pick_string(
        value,
        &[
            // OriSongName 是干净歌名（如「晴天」），优先用它
            "OriSongName",
            "SongName",
            "songname",
            "audio_name",
            // 搜索结果用的是 FileName（大写 F/N），且形如「周杰伦 - 晴天」；
            // 歌单条目则可能是小写的 filename。两个都要认，大小写不能想当然。
            "FileName",
            "filename",
            "name",
        ],
    )
    .map(|name| strip_extension(&name))
    .unwrap_or_else(|| "未知曲目".to_string());

    let singers = parse_singers(value);
    // 歌单条目的 `name` 形如 `BIGBANG - Love Song`（歌手被塞进了名字），
    // 而列表里歌手本来就是单独一列，不去掉前缀就会显示两遍
    let name = strip_singer_prefix(&name, &singers);

    // `audio_info.hash_xxx` 一并收下——先收，后面的 Song 构造才能引用
    let extra_hashes = parse_extra_hashes(value, &hash);

    let duration_ms = pick_u64(value, &["Duration", "duration", "timelength", "timelen"])
        .map(normalize_duration)
        .or_else(|| pick_audio_duration(value))
        .unwrap_or_default();

    // `album_audio_id`：沿用原来「MixSongID 系」的候选键。
    //
    // 注意候选键里**刻意不放** `audio_id` ——它在歌单接口里才是正确值，
    // 而搜索接口里 `MixSongID` 才是。混在同一个字段里必然有一半接口取错，
    // 所以拆成两个字段分别存，取链接时挨个试。
    let album_audio_id = pick_i64(
        value,
        &[
            "AlbumAudioID",
            "album_audio_id",
            "MixSongID",
            "mixsongid",
            "EMixSongID",
        ],
    )
    .unwrap_or_default();

    // `audio_id`：歌单接口（`/playlist/track/all/new`）给的另一个标识。
    // 实测对下架歌曲它是唯一能拿到直链的那个。

    // 封面可能在顶层、在 `trans_param` 里，也可能在 `album_info` / `albuminfo` 里
    let cover = pick_string(value, &["Image", "img", "cover", "album_image"])
        .or_else(|| nested_string(value, "trans_param", &["union_cover", "cover"]))
        .or_else(|| nested_string(value, "album_info", &["sizable_cover", "cover"]))
        .or_else(|| nested_string(value, "albuminfo", &["cover", "sizable_cover"]));

    // 专辑名有三种藏法：顶层 `AlbumName`、排行榜的 `album_info.album_name`、
    // 歌单条目的 `albuminfo.name`（注意这个没有下划线，是另一套命名）
    let album_name = pick_string(value, &["AlbumName", "album_name"])
        .or_else(|| nested_string(value, "album_info", &["album_name", "name"]))
        .or_else(|| nested_string(value, "albuminfo", &["name", "album_name"]))
        .unwrap_or_default();

    Some(Song {
        name,
        hash,
        album_id: pick_string(value, &["AlbumID", "album_id"]).unwrap_or_default(),
        album_audio_id,
        album_name,
        singers,
        duration_ms,
        cover,
        // 缺失即「未知」，交给 looks_playable 决定要不要预警
        privilege: pick_i64(value, &["Privilege", "privilege", "pay_type"]),
        file_id: pick_i64(value, &["Fileid", "FileId", "fileid", "file_id"]),
        extra_hashes,
        // 标准版与概念版共用这套解析，具体来源由分派层盖章覆盖
        source: crate::source::SourceKind::Kugou,
    })
}

/// 从 `audio_info` 里取音频 hash。
///
/// 只有排行榜那类接口用这个布局：条目顶层没有 `hash`，各音质的 hash 分列在
/// `audio_info.hash_128` / `hash_320` / `hash_flac` 等字段里。按「默认音质优先、
/// 再退到任意可用」的顺序取——取哪个都只是播放时选的音质不同，不影响能不能播。
fn pick_audio_hash(value: &Value) -> Option<String> {
    let info = value.get("audio_info")?;
    pick_string(
        info,
        &[
            "hash_128",
            "hash_320",
            "hash_flac",
            "hash_high",
            "hash_super",
        ],
    )
}

/// 把 `audio_info` 里所有 hash_xxx 都收下，按音质映射成 BTreeMap。
///
/// 同一首歌在不同接口里给出的 `hash` 字段（`FileHash` / `hash`）可能指向
/// **已经下架**的版本，但 `audio_info.hash_128`、`hash_320` 等里通常还会给
/// 一组别的 hash——它们各自对应一个可播放的资源。**只有把这些都收下，**/
/// `privilege/lite` **才知道该问哪些 hash**。
///
/// 与顶层 hash 重复的项跳过，避免无意义地重发同一份请求。
fn parse_extra_hashes(value: &Value, primary: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    let Some(info) = value.get("audio_info") else {
        return map;
    };
    for quality in ["128", "320", "flac", "high", "super"] {
        let key = format!("hash_{quality}");
        let Some(hash) = info.get(&key).and_then(Value::as_str) else {
            continue;
        };
        if hash == primary {
            continue;
        }
        map.insert(quality.to_string(), hash.to_string());
    }
    map
}

/// 从 `audio_info` 里取时长。单位同样走 [`normalize_duration`] 归一。
fn pick_audio_duration(value: &Value) -> Option<u64> {
    let info = value.get("audio_info")?;
    pick_u64(
        info,
        &[
            "duration_128",
            "duration_320",
            "duration_flac",
            "duration_high",
            "duration_super",
        ],
    )
    .map(normalize_duration)
}

/// 从 `object[key]` 这个嵌套对象里按候选键名取字符串。
///
/// 酷狗把同一语义散在不同嵌套结构里（`album_info` / `albuminfo` / `trans_param`…），
/// 用这个省掉一长串 `.get(..).and_then(..)`。
fn nested_string(object: &Value, key: &str, candidates: &[&str]) -> Option<String> {
    object
        .get(key)
        .and_then(|inner| pick_string(inner, candidates))
}

/// 去掉歌名里重复的歌手前缀。
///
/// 歌单条目的 `name` 是 `BIGBANG - Love Song` 这种「歌手 - 歌名」，而列表里歌手
/// 本来就单独占一列，不去掉就会显示两遍。只在确实以已知歌手名开头时才动手，
/// 避免误伤真的以「某某 - 」开头的歌名。
fn strip_singer_prefix(name: &str, singers: &[Singer]) -> String {
    // 多位歌手时酷狗可能用「A、B」拼接，所以除了逐个歌手，也试一次拼接形式
    let joined = singers
        .iter()
        .map(|singer| singer.name.as_str())
        .collect::<Vec<_>>()
        .join("、");

    let candidates = singers
        .iter()
        .map(|singer| singer.name.as_str())
        .chain(std::iter::once(joined.as_str()));

    for candidate in candidates {
        if candidate.is_empty() {
            continue;
        }
        if let Some(rest) = name.strip_prefix(candidate)
            && let Some(title) = rest.strip_prefix(" - ")
        {
            let title = title.trim();
            if !title.is_empty() {
                return title.to_string();
            }
        }
    }

    name.to_string()
}

/// 去掉歌名末尾的音频扩展名。
///
/// 部分入口返回的歌名形如 `Overcast Sky.mp3`——扩展名对听歌没有任何帮助，却会挤占
/// 列表本来就紧张的列名宽度。这里只在「后缀确实是常见音频格式、且去掉后名字不为空」
/// 时才动手，避免误伤本来就以此结尾的正常歌名。
///
/// # 为什么不能按字节切片
///
/// 早先的写法是 `name.to_lowercase()` 之后按长度回切原串。但 `to_lowercase()`
/// **不保证字节长度不变**（如 `İ` U+0130 会变成 `i` + 组合用点，2 字节变 3 字节），
/// 回切时可能落在 UTF-8 字符边界之外直接 panic——而 release 是 `panic = "abort"`，
/// 等于解析一个歌名就能让进程死掉。所以这里全程按 `char` / `&str` 操作。
fn strip_extension(name: &str) -> String {
    const EXTENSIONS: [&str; 6] = ["mp3", "flac", "m4a", "wav", "ogg", "aac"];

    let Some((stem, extension)) = name.rsplit_once('.') else {
        return name.to_string();
    };
    if EXTENSIONS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(extension))
        && !stem.trim().is_empty()
    {
        return stem.trim_end().to_string();
    }
    name.to_string()
}

/// 解析歌手数组，退化时回落到 `SingerName` 字符串。
fn parse_singers(value: &Value) -> Vec<Singer> {
    let raw = pick_array(value, &["Singer", "singerinfo", "singers", "authors"]);
    let mut singers: Vec<Singer> = raw
        .iter()
        .filter_map(|entry| {
            let name = pick_string(entry, &["name", "Name", "singername", "author_name"])?;
            Some(Singer {
                id: pick_i64(entry, &["id", "Id", "singerid", "author_id"]).unwrap_or_default(),
                name,
            })
        })
        .collect();

    if singers.is_empty()
        && let Some(text) = pick_string(value, &["SingerName", "singername", "author_name"])
    {
        singers = split_singer_text(&text);
    }

    // `filename` 形如 "Beyond - 海阔天空"，歌手名可以从中兜底提取
    if singers.is_empty()
        && let Some(filename) = pick_string(value, &["filename", "audio_name"])
        && let Some((prefix, _)) = filename.split_once(" - ")
    {
        let prefix = prefix.trim();
        if !prefix.is_empty() && prefix != "未知" {
            singers.push(Singer {
                id: 0,
                name: prefix.to_string(),
            });
        }
    }

    singers
}

/// 把 `"Beyond、黄家驹"` / `"Beyond/黄家驹"` 这类文本拆成多个歌手。
fn split_singer_text(text: &str) -> Vec<Singer> {
    text.split(['、', '/', '&', ',', '，'])
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| Singer {
            id: 0,
            name: name.to_string(),
        })
        .collect()
}

/// 酷狗的 `Duration` 单位不稳定：有时是秒，有时是毫秒。
///
/// 用 10000 做分界——**大于等于** 10000 的只可能是毫秒（约 10 秒以上），
/// 因为不存在时长 10000 秒（2.7 小时）的歌曲。
///
/// 注意边界必须取 `>=` 而不是 `>`：取值恰好为 `10000` 时，真实含义是「10 秒的
/// 毫秒数」，走秒分支会被算成 10000 秒 ≈ 2.78 小时，进度条直接报废。
fn normalize_duration(raw: u64) -> u64 {
    if raw == 0 {
        0
    } else if raw >= 10_000 {
        raw
    } else {
        raw.saturating_mul(1_000)
    }
}

/// 在 `data` 下寻找歌曲数组。
///
/// 两种布局都认：`data` 本身是数组（`/artist/audios`），或 `data` 是个对象、
/// 歌曲数组挂在某个命名键下（`songs` / `songlist` / …）。
/// 候选键按「最可能」到「最兜底」排列，最后再扫描 `data` 本身。
pub fn extract_songs(data: &Value) -> Vec<Song> {
    // 少数接口（实测 `/artist/audios`）的 `data` **本身就是歌曲数组**，没有再包一层
    // 命名键——歌单是 `data.songs`、排行榜是 `data.songlist`，只有它是裸数组。
    // 不先认这一种，那些接口一首歌都取不到（歌手页点进去永远是空的）。
    if let Some(array) = data.as_array() {
        let songs: Vec<Song> = array.iter().filter_map(song_from_json).collect();
        if !songs.is_empty() {
            return songs;
        }
    }

    const CANDIDATES: &[&str] = &[
        "songs",
        "list",
        "info",
        "musiclist",
        "songlist",
        "audios",
        "lists",
        "data",
        "filelist",
    ];

    for key in CANDIDATES {
        let array = pick_array(data, &[key]);
        if array.is_empty() {
            continue;
        }
        let songs: Vec<Song> = array.iter().filter_map(song_from_json).collect();
        if !songs.is_empty() {
            return songs;
        }
    }
    Vec::new()
}

// ============================================================================
// 歌单 / 歌手 / 排行榜解析
// ============================================================================

pub fn playlist_from_json(value: &Value) -> Option<Playlist> {
    let list_id = pick_i64(value, &["listid", "list_id", "specialid", "special_id"]);
    let global_id = pick_string(value, &["global_collection_id", "global_id", "gid"]);

    // 至少要有一个 id，否则这个条目无法被打开
    let id = global_id
        .clone()
        .or_else(|| list_id.map(|number| number.to_string()))?;

    let name = pick_string(
        value,
        &["name", "listname", "specialname", "title", "collectname"],
    )?;

    Some(Playlist {
        id,
        list_id,
        name,
        cover: pick_string(
            value,
            &["pic", "imgurl", "img", "cover", "picurl", "banner"],
        ),
        song_count: pick_u32(
            value,
            &["songcount", "song_count", "count", "songnum", "total"],
        )
        .unwrap_or_default(),
        creator: pick_string(
            value,
            &["nickname", "username", "creator", "user_name", "author"],
        ),
        description: pick_string(value, &["introduction", "intro", "description", "desc"]),
        is_own: pick_i64(value, &["is_self", "isself", "is_mine"]).unwrap_or_default() == 1,
    })
}

pub fn artist_from_json(value: &Value) -> Option<Artist> {
    let name = pick_string(value, &["name", "author_name", "singername", "ArtistName"])?;
    Some(Artist {
        id: pick_i64(value, &["id", "author_id", "singerid", "ArtistId"]).unwrap_or_default(),
        name,
        avatar: pick_string(value, &["img", "avatar", "pic", "singer_img", "image"]),
        song_count: pick_u32(
            value,
            &["songcount", "song_count", "audio_count", "musicnum"],
        ),
        follower_count: pick_u32(value, &["fanscount", "fans_count", "fansnum"]),
    })
}

pub fn rank_board_from_json(value: &Value) -> Option<RankBoard> {
    let id = pick_i64(value, &["rankid", "rank_id", "id"])?;
    let name = pick_string(value, &["rankname", "rank_name", "name", "title"])?;
    Some(RankBoard {
        id,
        name,
        cover: pick_string(value, &["imgurl", "img", "pic", "cover", "banner"]),
        update_frequency: pick_string(value, &["update_frequency", "updatefrequency", "frequency"]),
    })
}

pub fn pick_u32(object: &Value, keys: &[&str]) -> Option<u32> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(value_to_u32))
}

// ============================================================================
// 通用格式化
// ============================================================================

/// 毫秒 → `mm:ss`（超过一小时则是 `h:mm:ss`）。
pub fn format_duration_ms(milliseconds: u64) -> String {
    let total_seconds = milliseconds / 1_000;
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = total_seconds / 3_600;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 搜索结果的解析（真实响应结构，含 FileName / OriSongName / FileHash / Duration）。
    ///
    /// 用真实结构而不是手写 mock，避免上游改字段名时测试还绿着。
    #[test]
    fn parses_search_result_payload() {
        let data = json!({
            "pagesize": 5,
            "page": 1,
            "total": 480,
            "correctiontype": 0,
            "lists": [
                {
                    "FileName": "Alstroemeria Records - Bad Apple!! (feat.nomico)",
                    "SingerName": "Alstroemeria Records",
                    "OriSongName": "Bad Apple!!",
                    "FileHash": "F20A1FDBE025D06207B6BC31F0699F0A",
                    "ExtName": "mp3",
                    "Duration": 317,
                    "AlbumID": 15130869,
                    "AlbumName": "10th Anniversary Bad Apple!! feat.nomico PHASE3",
                    "MixSongID": 130275462
                }
            ],
            "sec_aggre_v2": [],
            "istag": 0,
            "size": 0
        });

        let songs = extract_songs(&data);
        assert_eq!(songs.len(), 1, "应解析出 1 首，实际 {}", songs.len());

        let song = &songs[0];
        // OriSongName 是干净歌名，应优先于带歌手的 FileName
        assert_eq!(song.name, "Bad Apple!!");
        assert_eq!(song.hash, "F20A1FDBE025D06207B6BC31F0699F0A");
        assert_eq!(song.duration_ms, 317_000, "Duration 是秒，应换算成毫秒");
        assert_eq!(song.album_audio_id, 130_275_462);
    }

    #[test]
    fn parses_search_result_song() {
        let raw = json!({
            "SongName": "海阔天空",
            "FileHash": "ABC123",
            "AlbumID": 12345,
            "AlbumAudioID": "67890",
            "AlbumName": "乐与怒",
            "Duration": 326,
            "Privilege": 8,
            "Singer": [{"id": 42, "name": "Beyond"}]
        });

        let song = song_from_json(&raw).expect("应能解析");
        assert_eq!(song.name, "海阔天空");
        assert_eq!(song.hash, "ABC123");
        assert_eq!(song.album_id, "12345");
        assert_eq!(song.album_audio_id, 67_890);
        assert_eq!(song.duration_ms, 326_000);
        assert_eq!(song.singer_text(), "Beyond");
        assert!(song.looks_playable());
    }

    #[test]
    fn parses_playlist_entry_song() {
        let raw = json!({
            "filename": "Beyond - 海阔天空",
            "hash": "DEF456",
            "duration": 326000,
            "fileid": 999
        });

        let song = song_from_json(&raw).expect("应能解析");
        assert_eq!(song.hash, "DEF456");
        assert_eq!(song.duration_ms, 326_000);
        assert_eq!(song.file_id, Some(999));
        assert_eq!(song.singer_text(), "Beyond");
    }

    #[test]
    fn rejects_song_without_hash() {
        assert!(song_from_json(&json!({"SongName": "无 hash"})).is_none());
    }

    #[test]
    fn parses_rank_entry_with_nested_audio_info() {
        // 结构照抄 /rank/audio 的真实响应：顶层没有 hash / duration / album_name，
        // 音频信息在 audio_info、专辑名在 album_info、歌手在 authors
        let raw = json!({
            "songname": "甲乙丙丁 (你我怎么两清)",
            "author_name": "李佳薇",
            "authors": [{ "author_id": 83922, "author_name": "李佳薇" }],
            "audio_id": 1106816298,
            "album_id": 197648995,
            "album_audio_id": 920474385,
            "audio_info": {
                "hash_128": "213D580CA0BDCC28A5FDBA995FFDA106",
                "hash_320": "B7AC734C6806EFF90C22C74F1AFFA156",
                "hash_flac": "85479C21FADC65A7C495989D6FE9396D",
                "duration_128": 210000,
                "filesize_128": 3368531
            },
            "album_info": {
                "album_name": "甲乙丙丁",
                "sizable_cover": "http://imge.kugou.com/stdmusic/{size}/x.jpg"
            }
        });

        let song = song_from_json(&raw).expect("排行榜条目也应能解析");
        assert_eq!(song.hash, "213D580CA0BDCC28A5FDBA995FFDA106");
        assert_eq!(song.name, "甲乙丙丁 (你我怎么两清)");
        assert_eq!(song.album_name, "甲乙丙丁");
        assert_eq!(song.duration_ms, 210_000);
        assert_eq!(song.album_audio_id, 920_474_385);
        assert_eq!(song.singer_text(), "李佳薇");
        assert!(song.cover.is_some());
        // privilege 缺失 → 按可播处理，否则会对整页正常歌曲误报版权受限
        assert!(song.looks_playable());
        // 排行榜条目也照 `audio_info` 的多档 hash 收下——这是「同一首歌在
        // 某些接口的 hash 拿不到资源、其它接口的能拿到」的根本修复
        assert_eq!(
            song.extra_hashes.get("320").map(String::as_str),
            Some("B7AC734C6806EFF90C22C74F1AFFA156")
        );
        assert_eq!(
            song.extra_hashes.get("flac").map(String::as_str),
            Some("85479C21FADC65A7C495989D6FE9396D")
        );
        // 主 hash 与 `hash_128` 相同 —— 这种重复项必须跳过，否则 `/privilege/lite`
        // 会收到一堆 `resource`，白白浪费服务端资源
        assert!(!song.extra_hashes.contains_key("128"));
    }

    #[test]
    fn parses_playlist_entry_with_prefixed_name() {
        // 结构照抄 /playlist/track/all 的真实响应：时长字段是 `timelen`，
        // 专辑在 `albuminfo`（无下划线），而 `name` 里带着歌手前缀
        let raw = json!({
            "name": "BIGBANG - Love Song",
            "hash": "1B0772CFE733408D58B4EEE703E1EBAE",
            "timelen": 225854,
            "size": 3614319,
            "bitrate": 128,
            "extname": "mp3",
            "album_id": "537580",
            "mixsongid": 64542610,
            "fileid": 58,
            "privilege": 10,
            "singerinfo": [{ "name": "BIGBANG", "id": 84161, "type": 2 }],
            "albuminfo": { "name": "빅뱅 스페셜에디션", "id": 537580 },
            "cover": "http://imge.kugou.com/stdmusic/{size}/x.jpg"
        });

        let song = song_from_json(&raw).expect("歌单条目应能解析");
        assert_eq!(song.name, "Love Song", "应去掉与歌手列重复的前缀");
        assert_eq!(song.singer_text(), "BIGBANG");
        assert_eq!(song.duration_ms, 225_854, "`timelen` 是毫秒");
        assert_eq!(song.album_name, "빅뱅 스페셜에디션");
        assert_eq!(song.album_audio_id, 64_542_610);
        assert_eq!(song.file_id, Some(58));
        // 实测歌单里 29/30 首的 privilege 都是 10，必须当作可播
        assert_eq!(song.privilege, Some(10));
        assert!(song.looks_playable());
    }

    #[test]
    fn keeps_name_when_prefix_is_not_the_singer() {
        let raw = json!({
            "SongName": "Love - Actually",
            "FileHash": "H",
            "Singer": [{ "id": 1, "name": "Someone Else" }]
        });
        let song = song_from_json(&raw).expect("应能解析");
        assert_eq!(song.name, "Love - Actually", "前缀不是歌手名时不该动它");
    }

    #[test]
    fn missing_privilege_is_treated_as_playable() {
        let absent = song_from_json(&json!({"FileHash": "H"})).expect("应能解析");
        assert_eq!(absent.privilege, None);
        assert!(absent.looks_playable());

        let blocked = song_from_json(&json!({"FileHash": "H", "Privilege": 0})).expect("应能解析");
        assert_eq!(blocked.privilege, Some(0));
        assert!(!blocked.looks_playable());
    }

    /// 同一首歌在搜索结果里拿不到 URL、歌单里能拿到——根因是顶层 `FileHash`
    /// 指向的是已下架版本，`audio_info.hash_320/128` 才是当前还在用的。
    /// 这个测试**直接锁死**「多档 hash 必须收下、且不能与主 hash 重复」。
    #[test]
    fn collects_extra_hashes_from_audio_info() {
        let raw = json!({
            "SongName": "测试曲",
            "FileHash": "PRIMARY_HASH",
            "audio_info": {
                "hash_128": "PRIMARY_HASH",    // 与主 hash 重复 → 必须跳过
                "hash_320": "HASH_320",
                "hash_flac": "HASH_FLAC",
                "hash_high": "HASH_HIGH",
                "hash_super": "HASH_SUPER",
                "duration_320": 200000,        // duration_xxx 不是 hash → 忽略
            },
        });
        let song = song_from_json(&raw).expect("应能解析");
        assert_eq!(song.hash, "PRIMARY_HASH");
        assert_eq!(
            song.extra_hashes.get("320").map(String::as_str),
            Some("HASH_320")
        );
        assert_eq!(
            song.extra_hashes.get("flac").map(String::as_str),
            Some("HASH_FLAC")
        );
        assert_eq!(
            song.extra_hashes.get("high").map(String::as_str),
            Some("HASH_HIGH")
        );
        assert_eq!(
            song.extra_hashes.get("super").map(String::as_str),
            Some("HASH_SUPER")
        );
        assert!(
            !song.extra_hashes.contains_key("128"),
            "与主 hash 重复的 128 必须跳过"
        );
        assert_eq!(song.extra_hashes.len(), 4);
    }

    /// 没有 `audio_info` 时 `extra_hashes` 留空——大多数接口都不返回。
    #[test]
    fn extra_hashes_is_empty_when_audio_info_absent() {
        let raw = json!({"SongName": "测试曲", "FileHash": "H"});
        let song = song_from_json(&raw).expect("应能解析");
        assert!(song.extra_hashes.is_empty());
    }

    #[test]
    fn extracts_songs_from_nested_container() {
        let payload = json!({
            "data": {
                "info": [
                    {"SongName": "A", "FileHash": "h1"},
                    {"SongName": "B", "FileHash": "h2"}
                ]
            }
        });
        let songs = extract_songs(payload.get("data").expect("data"));
        assert_eq!(songs.len(), 2);
        assert_eq!(songs[1].name, "B");
    }

    #[test]
    fn finds_current_lyric_line() {
        let lyric = Lyric {
            lines: vec![
                LyricLine {
                    time_ms: 1_000,
                    text: "第一句".into(),
                    translation: None,
                    romanization: None,
                    words: Vec::new(),
                },
                LyricLine {
                    time_ms: 5_000,
                    text: "第二句".into(),
                    translation: None,
                    romanization: None,
                    words: Vec::new(),
                },
                LyricLine {
                    time_ms: 9_000,
                    text: "第三句".into(),
                    translation: None,
                    romanization: None,
                    words: Vec::new(),
                },
            ],
        };
        assert_eq!(lyric.index_at(500), None);
        assert_eq!(lyric.index_at(1_000), Some(0));
        assert_eq!(lyric.index_at(7_000), Some(1));
        assert_eq!(lyric.index_at(60_000), Some(2));
    }

    /// 锁住「封面模板 URL 必须展开 {size}」这条规则。
    ///
    /// 这个 bug 真实发生过：展开逻辑只写在 mpris 里，封面渲染那条路径漏了，
    /// 于是封面永远是个 404，界面一直显示占位图，看起来像「没有封面功能」。
    #[test]
    fn expands_cover_size_placeholder() {
        // 酷狗：模板 URL，必须替换
        let song = Song {
            cover: Some("http://imge.kugou.com/stdmusic/{size}/20200819/x.jpg".to_string()),
            ..Song::default()
        };
        assert_eq!(
            song.cover_url(400).as_deref(),
            Some("http://imge.kugou.com/stdmusic/400/20200819/x.jpg")
        );

        // 网易云/QQ 音乐：本来就是完整地址，原样返回
        let song = Song {
            cover: Some("https://p3.music.126.net/abc/1.jpg".to_string()),
            ..Song::default()
        };
        assert_eq!(
            song.cover_url(400).as_deref(),
            Some("https://p3.music.126.net/abc/1.jpg")
        );

        // 没有封面
        assert_eq!(Song::default().cover_url(400), None);
    }

    #[test]
    fn strips_audio_extension() {
        assert_eq!(strip_extension("Overcast Sky.mp3"), "Overcast Sky");
        assert_eq!(strip_extension("Song.FLAC"), "Song", "扩展名大小写无关");
        assert_eq!(strip_extension("No Extension"), "No Extension");
        // 去掉后为空的名字要保持原样（`.mp3` 本身可能是歌名）
        assert_eq!(strip_extension(".mp3"), ".mp3");
        assert_eq!(strip_extension("海阔天空.MP3"), "海阔天空");
    }

    /// 早先的实现先 `to_lowercase()` 再按长度回切原串。`to_lowercase()` 不保证
    /// 字节长度不变（`İ` 会变成 `i` + 组合用点），回切可能落在字符边界之外 → panic。
    /// 这里锁住「多字节歌名带扩展名」不会出问题。
    #[test]
    fn strip_extension_is_safe_for_multibyte_names() {
        assert_eq!(strip_extension("İ.mp3"), "İ");
        assert_eq!(strip_extension("straße.flac"), "straße");
    }

    #[test]
    fn normalizes_duration_units() {
        assert_eq!(normalize_duration(0), 0);
        assert_eq!(normalize_duration(317), 317_000, "秒 → 毫秒");
        assert_eq!(normalize_duration(210_000), 210_000, "毫秒原样保留");
        // 边界：恰好 10000 是「10 秒的毫秒数」。用 `>` 判定会被当成 10000 秒
        // （≈2.78 小时），进度条直接报废。
        assert_eq!(normalize_duration(10_000), 10_000);
        assert_eq!(normalize_duration(9_999), 9_999_000);
    }

    #[test]
    fn formats_duration() {
        assert_eq!(format_duration_ms(0), "00:00");
        assert_eq!(format_duration_ms(227_000), "03:47");
        assert_eq!(format_duration_ms(3_723_000), "1:02:03");
    }

    /// `/artist/audios` 的 `data` **本身就是歌曲数组**，没有再包一层命名键
    /// （歌单是 `data.songs`、排行榜是 `data.songlist`，只有这个接口是裸数组）。
    ///
    /// 旧实现只按 `songs` / `list` / … 这些键在 `data` 里找，数组本身永远命中不了，
    /// 于是 `artist_tracks` 恒返回空——「点进歌手却一首歌都没有」。
    /// 字段照抄真实响应（`audio_name` / `author_name` / `timelength` / `privilege`）。
    #[test]
    fn extract_songs_accepts_a_bare_array() {
        let data = json!([
            {
                "hash": "FF81E148CE0E2E809AC759ADF9A8E109",
                "audio_name": "红尘误此生",
                "author_name": "宋晓峰",
                "timelength": 225000,
                "privilege": 8,
                "album_id": "123456",
                "album_audio_id": 655142342,
                "album_name": "红尘误此生"
            },
            {
                "hash": "187D31F57E1B299AFA2880635B20F69A",
                "audio_name": "茶汤",
                "author_name": "郁可唯",
                "timelength": 308819,
                "privilege": 10,
                "album_id": "654321",
                "album_audio_id": 50570,
                "album_name": "茶汤"
            }
        ]);

        let songs = extract_songs(&data);
        assert_eq!(songs.len(), 2, "data 本身是数组时必须能解析出歌曲");
        assert_eq!(songs[0].name, "红尘误此生");
        assert_eq!(songs[0].hash, "FF81E148CE0E2E809AC759ADF9A8E109");
        assert_eq!(songs[0].duration_ms, 225_000);
        assert_eq!(songs[0].singer_text(), "宋晓峰");
        assert_eq!(songs[0].album_audio_id, 655_142_342);
        assert_eq!(songs[1].name, "茶汤");
    }

    /// 裸数组里的元素若解析不出歌曲，不能因此挡住后面「命名键」的布局。
    #[test]
    fn extract_songs_falls_through_a_non_song_array() {
        let data = json!({
            "tags": [{"id": 1, "name": "流行"}],
            "songs": [{"hash": "H", "audio_name": "真歌"}]
        });
        let songs = extract_songs(&data);
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].name, "真歌");
    }
}
