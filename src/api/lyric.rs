//! 歌词获取与 LRC 解析。
//!
//! # 两步走
//!
//! 酷狗取歌词要两个请求：
//!
//! 1. `GET /search/lyric?hash=&keywords=` → 拿到 `(id, accesskey)`
//! 2. `GET /lyric?id=&accesskey=&fmt=krc&decode=true` → 拿到歌词正文
//!
//! 服务端在 `decode=true` 时会把结果放进 `decodeContent`；如果只给了 base64 的
//! `content`，本地再解一次。
//!
//! # 关于 KRC
//!
//! 酷狗原生的逐字歌词格式是 KRC（`[起始毫秒,持续毫秒]字<偏移,时长,0>…`）。
//! 本项目要逐字高亮，所以请求 `fmt=krc`，解析器解析 `<本行内偏移,持续,0>`
//! 标记（见 `parse_krc_words`）；翻译与音译只存在于 KRC 的 `[language:]` 标签里，
//! 请求 lrc 就拿不到。解析器同时兼容纯 LRC 的时间标签，服务端万一回落到 LRC
//! 也不会整篇解析失败——只是退化成逐行。

use base64::Engine;
use serde_json::Value;

use crate::api::data_of;
use crate::api::model::{Lyric, LyricLine, LyricWord, Song, pick_string};
use crate::api::node::NodeApi;
use crate::error::{AppError, Result};

/// 取歌词要用的两个上游请求。两个后端各自实现：`NodeApi` 打本机服务，
/// `NativeApi` 直连 `lyrics.kugou.com`。**取歌词的算法本身只此一份**，
/// 见 [`fetch_lyric_via`]。
///
/// 与 [`crate::api::catalog::StreamSource`] 同样的分层：只抽象到「发哪一个请求」，
/// 不抽象参数拼装——native 要签名、要 `clearDefaultParams`，硬凑共同签名只会
/// 两边都别扭。
#[allow(async_fn_in_trait)] // 与 MusicApi 一致：只用泛型静态分发，不做 dyn
pub(crate) trait LyricSource {
    /// 第一步：按 hash 找歌词候选，返回 `/search/lyric` 的原始响应。
    async fn search_lyric(&self, song: &Song) -> Result<Value>;

    /// 第二步：按候选的 `(id, accesskey)` 取歌词正文。
    ///
    /// 返回**原始响应体**（可能是 JSON，也可能是纯文本），由 [`fetch_lyric_via`]
    /// 统一解释。native 在这一步就把 KRC 解好并写进 `decodeContent`，
    /// 与 Node 服务端 `module/lyric.js` 的 `decode` 分支等价。
    async fn lyric_body(&self, lyric_id: &str, access_key: &str) -> Result<String>;
}

impl LyricSource for NodeApi {
    async fn search_lyric(&self, song: &Song) -> Result<Value> {
        self.get_json(
            "/search/lyric",
            &[
                ("hash", song.hash.clone()),
                (
                    "keywords",
                    format!("{} - {}", song.singer_text(), song.name),
                ),
                ("duration", song.duration_ms.to_string()),
                ("man", "yes".to_string()),
            ],
        )
        .await
    }

    async fn lyric_body(&self, lyric_id: &str, access_key: &str) -> Result<String> {
        self.get_text(
            "/lyric",
            &[
                ("id", lyric_id.to_string()),
                ("accesskey", access_key.to_string()),
                // 必须是 krc：翻译与音译只在 KRC 的 [language:] 标签里，lrc 没有。
                ("fmt", "krc".to_string()),
                ("decode", "true".to_string()),
                ("charset", "utf8".to_string()),
            ],
        )
        .await
    }
}

/// 取某首歌歌词的共享实现。
///
/// # 为什么要试多个候选
///
/// 同一个 hash 酷狗往往提供多个 KRC 变体：**有的只带罗马音，有的带真正的中文译文**。
/// 只取第一个的话，日语歌很容易拿到罗马音版——界面就显示成一串拼音，等于没有翻译。
///
/// 所以这里遍历候选，优先采用「[language:] 里有 CJK 译文」的那个；
/// 都没有才退回第一个能解析出内容的。
///
/// # 取不到歌词不是错误路径
///
/// 上游 `decodeLyrics` 解不开 KRC 时返回空字符串（不抛异常），空文本解析出空歌词，
/// 这个候选被跳过，最终走到「歌词为空」。native 的 `krc::decode` 返回 `Err`，
/// 但调用点在写 `decodeContent` 时把 `Err` 折成空串（见
/// [`crate::api::native::inject_decoded_lyric`]），所以两端的最终表现一致：
/// 都是 `AppError::NotFound`，界面只记一条 WARN，不弹错误、不影响播放。
pub(crate) async fn fetch_lyric_via<S: LyricSource>(source: &S, song: &Song) -> Result<Lyric> {
    let root = source.search_lyric(song).await?;

    let candidates = crate::api::extract_list(&root, &["candidates"], |value| {
        let id = pick_string(value, &["id", "lyric_id"])?;
        let access_key = pick_string(value, &["accesskey", "access_key"])?;
        Some(LyricCandidate {
            id,
            access_key,
            duration_ms: pick_duration(value),
            singer: pick_string(value, &["singer", "singername", "author"]),
            title: pick_string(value, &["song", "songname", "title"]),
        })
    });
    // `man=yes` 才会返回多个版本。上限 6 个：再往后质量通常更差，
    // 而每多一个候选就多一次请求。
    let candidates: Vec<LyricCandidate> = candidates.into_iter().take(6).collect();

    if candidates.is_empty() {
        return Err(AppError::NotFound(format!("未找到《{}》的歌词", song.name)));
    }

    let mut fallback: Option<Lyric> = None;
    let mut skipped_mismatched = 0usize;

    for candidate in candidates {
        // 候选自带 `duration`、`singer`、`song`。酷狗的歌词检索并不可靠：对冷门/翻唱/
        // 纯音乐曲目常把**另一首歌**当成候选返回。实测两种翻车：
        //   * 时长明显对不上（《记忆停留的地方》的多个版本拿到的是 320 秒的古典钢琴曲）；
        //   * 时长只差零点几秒、但歌手与歌名完全是另一首歌（July 的纯音乐
        //     《기억이 머물던 곳》拿到的是 MC晚安《错过》）。
        // 时长不符、或歌手与歌名都对不上的候选一律跳过——宁可显示「暂无歌词」，
        // 也不显示别的歌的词。
        if !duration_matches(candidate.duration_ms, song.duration_ms)
            || !candidate_identity_matches(
                candidate.singer.as_deref(),
                candidate.title.as_deref(),
                song,
            )
        {
            skipped_mismatched += 1;
            continue;
        }
        let LyricCandidate {
            id: lyric_id,
            access_key,
            ..
        } = candidate;

        // 该接口在 `decode=true` 下返回 JSON；偶尔直接吐纯文本，两种都接住。
        let body = match source.lyric_body(&lyric_id, &access_key).await {
            Ok(body) => body,
            Err(_) => continue, // 这个候选取不到，试下一个
        };
        let text = match serde_json::from_str::<Value>(&body) {
            Ok(root) => {
                if crate::api::model::check_error_code("/lyric", &root).is_err() {
                    continue;
                }
                extract_lyric_text(&root)
            }
            Err(_) => body,
        };

        let mut lyric = parse_lrc(&text);
        if lyric.is_empty() {
            continue;
        }

        // Second gate: the candidate metadata checked above can be wrong; the
        // KRC body carries the authoritative audio hash. See `krc_hash_matches`.
        if !krc_hash_matches(&text, song) {
            skipped_mismatched += 1;
            continue;
        }

        attach_translations(&mut lyric, &text);
        // 剔掉「歌名 - 歌手」标题行与制作人员名单行——它们不是歌词，
        // 但酷狗常把它们当作普通定时行塞进 KRC（《记忆停留》开头十几秒全是名单）。
        drop_non_lyric_lines(&mut lyric, song);
        if lyric.is_empty() {
            continue;
        }

        // 命中「有 CJK 译文」的候选，直接用它
        if translation_block_has_cjk(&text) {
            return Ok(lyric);
        }
        // 否则留作兜底（只留第一个，避免覆盖成更差的）
        if fallback.is_none() {
            fallback = Some(lyric);
        }
    }

    if fallback.is_none() && skipped_mismatched > 0 {
        return Err(AppError::NotFound(format!(
            "未找到《{}》匹配的歌词（已忽略 {} 个不匹配的候选）",
            song.name, skipped_mismatched
        )));
    }
    fallback.ok_or_else(|| AppError::NotFound(format!("《{}》的歌词为空", song.name)))
}

/// 一个 `/search/lyric` 候选。`duration_ms`/`singer`/`title` 用于校验它是否真的是本曲。
struct LyricCandidate {
    id: String,
    access_key: String,
    duration_ms: Option<u64>,
    singer: Option<String>,
    title: Option<String>,
}

/// 从候选 JSON 里读时长（可能是数字，也可能是字符串）。
fn pick_duration(value: &Value) -> Option<u64> {
    let raw = value.get("duration").or_else(|| value.get("duration_ms"))?;
    raw.as_u64()
        .or_else(|| raw.as_str().and_then(|text| text.trim().parse().ok()))
}

/// 候选时长与本曲时长是否可能是同一首歌。
///
/// 任一未知（0 或缺失）就不据此否决；已知时允许 5 秒或 10% 的偏差。
fn duration_matches(candidate_ms: Option<u64>, song_ms: u64) -> bool {
    match candidate_ms {
        Some(candidate) if candidate > 0 && song_ms > 0 => {
            let diff = candidate.abs_diff(song_ms);
            diff <= 5_000 || diff.saturating_mul(100) <= song_ms.saturating_mul(10)
        }
        _ => true,
    }
}

/// 候选的歌手 / 歌名是否与当前曲目对得上。
///
/// 只要**歌手或歌名任一**对得上就接受：同一首歌的歌词常由不同上传者提供，
/// 标题或演唱者偶有简写；但两者**都对不上**时，几乎可以断定是另一首歌。
/// 两个字段都缺失（上游没给）时不据此否决。
fn candidate_identity_matches(singer: Option<&str>, title: Option<&str>, song: &Song) -> bool {
    let singer = singer.map(str::trim).filter(|text| !text.is_empty());
    let title = title.map(str::trim).filter(|text| !text.is_empty());
    if singer.is_none() && title.is_none() {
        return true;
    }
    if let Some(singer) = singer
        && text_matches_any_singer(singer, song)
    {
        return true;
    }
    if let Some(title) = title
        && text_matches(title, &song.name)
    {
        return true;
    }
    false
}

/// 归一化后互相包含即视为同一名称（忽略大小写、空白与常见分隔符）。
fn text_matches(a: &str, b: &str) -> bool {
    let a = normalize_name(a);
    let b = normalize_name(b);
    if a.chars().count() < 2 || b.chars().count() < 2 {
        return false;
    }
    a.contains(&b) || b.contains(&a)
}

/// 候选歌手是否匹配本曲的任一演唱者（含用「、」拼接的整体串）。
fn text_matches_any_singer(candidate: &str, song: &Song) -> bool {
    if text_matches(candidate, &song.singer_text()) {
        return true;
    }
    song.singers
        .iter()
        .any(|singer| text_matches(candidate, &singer.name))
}

/// KRC 正文头部自带的 `[hash:]` 是否与本曲（含 `extra_hashes`）一致。
///
/// 候选的 `singer`/`song`/`duration` 可以整体错标，KRC 正文里的 `[hash:]` 才是
/// 这条歌词真正归属的音频，因此比元数据更权威。
///
/// 没有 `[hash:]` 标签时返回 true（不否决）：存在没有 hash 的正确歌词，强制要求
/// 会把它们误杀成「暂无歌词」。只有**有 hash 且对不上**才拒绝。
fn krc_hash_matches(krc_text: &str, song: &Song) -> bool {
    let Some(candidate) = krc_tag_value(krc_text, "hash") else {
        return true;
    };
    let candidate = candidate.trim().to_ascii_lowercase();
    if candidate.is_empty() {
        return true;
    }
    let song_hash = song.hash.trim().to_ascii_lowercase();
    if !song_hash.is_empty() && candidate == song_hash {
        return true;
    }
    song.extra_hashes
        .values()
        .any(|hash| hash.trim().to_ascii_lowercase() == candidate)
}

/// 取出 KRC **头部元信息区**形如 `[key:value]` 的标签值（去 BOM）。
///
/// 只扫到第一个定时行（`[1234,567]` 或 `[00:01.00]`，以数字开头）为止——
/// 元信息标签的键是字母（`id`/`ar`/`ti`/`hash`…），定时行的方括号里以数字开头。
/// 不设这个边界的话，歌词正文里万一出现 `[hash:...]` 字样会被误当元信息。
fn krc_tag_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("[{key}:");
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}').trim_start();
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        // 以数字开头的是定时行，元信息区到此结束
        if rest.starts_with(|ch: char| ch.is_ascii_digit()) {
            break;
        }
        if let Some(value) = line.strip_prefix(prefix.as_str()) {
            return value.split(']').next();
        }
    }
    None
}

/// 名称归一化：小写、去掉空白与括号/连接符等标点，便于宽松比对。
fn normalize_name(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_whitespace())
        .filter(|ch| {
            !matches!(
                ch,
                '(' | ')' | '[' | ']' | '（' | '）' | '【' | '】' | '-' | '—' | '_' | '·' | '・'
            )
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// 剔掉非歌词行：`歌名 - 歌手` 标题行，以及 `词：`/`曲：`/`编曲：`… 这类名单行。
fn drop_non_lyric_lines(lyric: &mut Lyric, song: &Song) {
    lyric
        .lines
        .retain(|line| !is_non_lyric_line(&line.text, song));
}

/// 这一行是不是「不是歌词」的元信息。
fn is_non_lyric_line(text: &str, song: &Song) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    // 标题行：`{歌名} - {歌手}` 或 `{歌手} - {歌名}`
    let name = song.name.trim();
    let singer = song.singer_text();
    let singer = singer.trim();
    if !name.is_empty() && !singer.is_empty() {
        let name_first = format!("{name} - {singer}");
        let singer_first = format!("{singer} - {name}");
        if trimmed == name_first || trimmed == singer_first {
            return true;
        }
    }

    credit_prefix(trimmed).is_some()
}

/// 形如「词：」「作曲：」「制作发行：」的制作人员行前缀。
///
/// 判据是「关键词后紧跟冒号」：`曲终人散`、`词不达意` 这类正常歌词不会命中。
fn credit_prefix(text: &str) -> Option<&'static str> {
    const KEYS: &[&str] = &[
        "制作发行",
        "制作人",
        "母带工程师",
        "录音师",
        "混音师",
        "出品人",
        "作词",
        "作曲",
        "编曲",
        "混音",
        "监制",
        "和音",
        "和声",
        "策划",
        "统筹",
        "制作",
        "发行",
        "出品",
        "录音",
        "母带",
        "吉他",
        "贝斯",
        "键盘",
        "弦乐",
        "配唱",
        "人声",
        "合唱",
        "伴唱",
        "演唱",
        "原唱",
        "封面",
        "设计",
        "文案",
        "推广",
        "营销",
        "词",
        "曲",
        "鼓",
        "OP",
        "SP",
    ];

    for key in KEYS {
        if let Some(rest) = text.strip_prefix(key) {
            let rest = rest.trim_start();
            if rest.starts_with(':') || rest.starts_with('：') {
                return Some(key);
            }
        }
    }
    None
}

/// `[language:]` 里是否存在**真正的 CJK 译文**块（而不是只有罗马音）。
///
/// 判定方式与桌面歌词脚本一致：逐块看有没有任一行含 CJK 字符。
/// 罗马音是纯拉丁，会被排除；中文译文含汉字，会命中。
fn translation_block_has_cjk(krc_text: &str) -> bool {
    let Some(payload) = extract_language_payload(krc_text) else {
        return false;
    };
    let Some(content) = payload.get("content").and_then(|value| value.as_array()) else {
        return false;
    };

    for block in content {
        let Some(items) = block.get("lyricContent").and_then(|value| value.as_array()) else {
            continue;
        };
        for entry in items {
            let Some(text) = flatten_lyric_content(entry) else {
                continue;
            };
            // CJK 统一表意文字 + 扩展 A 区
            if text.chars().any(|ch| matches!(ch, '\u{3400}'..='\u{9fff}')) {
                return true;
            }
        }
    }
    false
}

/// 从 `/lyric` 的 JSON 响应里取出 LRC 正文。
fn extract_lyric_text(root: &Value) -> String {
    let data = data_of(root);

    if let Some(text) = pick_string(data, &["decodeContent", "lyric", "lrc"])
        .or_else(|| pick_string(root, &["decodeContent", "lyric", "lrc"]))
    {
        return text;
    }

    // 只有 base64 的 `content` 时本地解码
    if let Some(encoded) =
        pick_string(data, &["content"]).or_else(|| pick_string(root, &["content"]))
    {
        if let Ok(decoded) = decode_base64_text(&encoded) {
            return decoded;
        }
        return encoded;
    }

    String::new()
}

fn decode_base64_text(encoded: &str) -> std::result::Result<String, base64::DecodeError> {
    let compact: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD.decode(compact.as_bytes())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// 解析 LRC 歌词。
///
/// 支持：
/// * 一行多个时间标签：`[00:01.00][00:05.00]副歌`（会展开成两行）
/// * 毫秒位数不定：`[00:01]` / `[00:01.5]` / `[00:01.23]` / `[00:01.234]`
/// * KRC 时间标签：`[1234,567]`
/// * 元信息行（`[ti:]` `[ar:]` `[language:...]`）自动跳过
///
/// 结果按时间升序排列，供 [`Lyric::index_at`] 做二分查找。
pub fn parse_lrc(text: &str) -> Lyric {
    let mut lines: Vec<LyricLine> = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() {
            continue;
        }

        let (timestamps, remainder) = consume_time_tags(line);
        if timestamps.is_empty() {
            continue;
        }

        // 「这行算不算歌词」只在这里判断一次，判据是 `has_lyric_content`——
        // `attach_translations` 用的是同一个函数。两边各判一次就会串位。
        if !has_lyric_content(remainder) {
            continue;
        }

        // 逐字时间戳是相对**本行起始**的，所以每个重复时间标签都要各解析一次
        for time_ms in timestamps {
            let (content, words) = parse_krc_words(remainder, time_ms);
            lines.push(LyricLine {
                time_ms,
                text: content,
                translation: None,
                romanization: None,
                words,
            });
        }
    }

    lines.sort_by_key(|line| line.time_ms);
    lines.dedup_by(|a, b| a.time_ms == b.time_ms && a.text == b.text);

    Lyric {
        text: text.to_string(),
        lines,
    }
}

/// 吃掉行首连续的 `[...]` 时间标签，返回 (时间戳列表, 剩余文本)。
fn consume_time_tags(line: &str) -> (Vec<u64>, &str) {
    let mut timestamps = Vec::new();
    let mut offset = 0usize;

    while line[offset..].starts_with('[') {
        let Some(close) = line[offset..].find(']') else {
            break;
        };
        let tag = &line[offset + 1..offset + close];

        // 元信息标签（ti/ar/al/by/language…）不是时间戳，遇到就停止扫描
        let Some(time_ms) = parse_time_tag(tag) else {
            break;
        };

        timestamps.push(time_ms);
        offset += close + 1;
    }

    (timestamps, &line[offset..])
}

/// 解析单个时间标签。
fn parse_time_tag(tag: &str) -> Option<u64> {
    if tag.contains(':') {
        return parse_lrc_tag(tag);
    }
    // KRC：`[起始毫秒,持续毫秒]`
    let (start, _duration) = tag.split_once(',')?;
    start.trim().parse::<u64>().ok()
}

/// 解析 `mm:ss.fff` 形式的标签。
fn parse_lrc_tag(tag: &str) -> Option<u64> {
    let (minutes, rest) = tag.split_once(':')?;
    let minutes: u64 = minutes.trim().parse().ok()?;

    let (seconds_text, fraction_text) = match rest.split_once('.') {
        Some((seconds, fraction)) => (seconds, Some(fraction)),
        None => (rest, None),
    };
    let seconds: u64 = seconds_text.trim().parse().ok()?;
    if seconds >= 60 {
        // 秒数越界说明这不是合法时间标签（可能是 `[ar:xxx:yyy]`）
        return None;
    }

    let millis = fraction_text.map(parse_fraction).unwrap_or(0);
    Some(
        minutes
            .saturating_mul(60_000)
            .saturating_add(seconds.saturating_mul(1_000))
            .saturating_add(millis),
    )
}

/// 把小数部分归一化成毫秒。位数不定，按位权补零。
fn parse_fraction(text: &str) -> u64 {
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    let value: u64 = digits.parse().unwrap_or(0);
    match digits.len() {
        0 => 0,
        1 => value * 100,
        2 => value * 10,
        3 => value,
        // 超过 3 位就截断，不四舍五入——歌词对齐差 1ms 无感
        _ => digits[..3].parse().unwrap_or(0),
    }
}

/// 拆出纯文本，并保留每个字的时间戳。
///
/// KRC 的逐字信息写在一行里：`这<0,200,0>是<200,300,0>测试`
/// ——`<本行内偏移毫秒,持续毫秒,0>` 跟在它所描述的那个字**后面**。
///
/// # 字数与标记数必须一致才返回逐字信息
///
/// 少一个标记就没法确定剩下那些字的时间。这种情况返回**空** `words`
/// （调用方退回整行高亮），而不是猜一个——猜出来的时间会让整行歌词
/// 唱得和声音对不上，比没有效果更糟。
fn parse_krc_words(text: &str, line_start_ms: u64) -> (String, Vec<LyricWord>) {
    let mut output = String::with_capacity(text.len());
    let mut words: Vec<LyricWord> = Vec::new();

    // 整行没有任何 `<…>`（普通 LRC）：原样返回文本、逐字信息留空。
    // 少了这一步会把纯 LRC 的歌词清成空字符串——歌词直接消失。
    if !text.contains('<') {
        return (decode_html_entities(text.trim()), Vec::new());
    }

    // 逐字单元写作 `<本行内偏移,持续,0>文本`——**标记在文本前面**（KRC 标准写法，
    // 与 MoeKoeMusic 的 /<(\d+),(\d+),\d+>([^<]+)/g 一致）。
    //
    // 文本可以不止一个字符：实测见过 `<1600,160,0>Jay`，所以按「一段标记 + 它后面
    // 直到下一个 `<` 之前的所有字符」来切，同一段里的字符共用一组时间
    // （MoeKoeMusic 也是把 `Jay` 当一个单元整体高亮）。
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        // 标记**之前**的字符也是正文。KRC 的标准写法里这里是空的（每行以标记开头），
        // 但歌里出现「有 `<` 却没有配对 `>`」的文本时（比如 `宝贝<3`），不收下就会
        // 把这一行的字整段丢掉——那一行会直接不显示。
        //
        // 这些字没有逐字时间（标记缺失），所以下面那条「字数与标记数必须一致」的
        // 检查会把 `words` 清空，退回整行高亮。这正是想要的行为。
        let prefix = decode_html_entities(&rest[..open]);
        for character in prefix.chars() {
            output.push(character);
        }

        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('>') else {
            break;
        };
        let mut parts = after_open[..close].split(',');
        let offset: u64 = parts
            .next()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let duration: u64 = parts
            .next()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);

        // 标记之后、下一个 `<`（或行尾）之前的所有字符都属于这一单元
        let body = &after_open[close + 1..];
        let stop = body.find('<').unwrap_or(body.len());
        let segment = &body[..stop];

        let start_ms = line_start_ms + offset;
        let end_ms = start_ms + duration;
        // 实体解码会改变字符数（`&apos;` 一个变一个、`&#39;` 四个变一个），
        // 所以逐字时间按**解码后**的字符分配。字数对不上时下面会把 `words` 清空、
        // 退回整行高亮——宁可少个效果，也不能让高亮和声音错位。
        let segment = decode_html_entities(segment);
        for character in segment.chars() {
            output.push(character);
            words.push(LyricWord { start_ms, end_ms });
        }
        rest = &body[stop..];
    }

    // 一个逐字单元都没有（普通 LRC、或这行没标记）就返回空，
    // 调用方退回整行高亮——宁可少个效果，也不能让歌词和时间错位。
    let trimmed = output.trim().to_string();
    if words.is_empty() || words.len() != trimmed.chars().count() {
        words.clear();
    }
    (trimmed, words)
}

/// 一行时间标签之后的文本算不算「有内容」。
///
/// # 判据只能有一处
///
/// `parse_lrc`（决定哪些行进 `Lyric`）与 `attach_translations`（算这些行在**原始定时行**
/// 里的序号，好去语言轨里取译文）必须用**同一个**判据。两边筛掉的行不一样，序号就会
/// 整体错开一位，之后每一行的译文都往后串——用户看到「译文和原文对不上」，没有任何报错。
///
/// 曾经是两处各写一份：这边看 `clean_krc_markup` 的结果，那边看 `parse_krc_words`
/// 的返回值。在「`<` 之前有字、却没有配对的 `>`」的行上（比如歌里写了 `宝贝<3`）
/// 两者分道扬镳，译文整体串位。回归测试见
/// `a_line_with_a_stray_angle_bracket_does_not_shift_translations`。
///
/// 现在两边都调这一个函数，`parse_lrc` 不再自己看 `parse_krc_words` 的输出。
/// **这条不变量要守住**：任何一边自己判断「这行算不算数」，串位就会回来。
fn has_lyric_content(remainder: &str) -> bool {
    !clean_krc_markup(remainder).is_empty()
}

/// 去掉 KRC 的内联标记 `<起始偏移,持续时长,0>`，并把残留的 HTML 实体还原。
///
/// # 顺序不能换
///
/// 必须先剥标记、再解实体。反过来的话，`&lt;` 解出的 `<` 会被当成标记起点，
/// 之后整行文字都会被当成标记吞掉——歌词直接消失。服务端下发的 `&lt;` 是
/// 真实字符（歌里写了「<3」），不是标记。
fn clean_krc_markup(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut depth = 0usize;

    for character in text.chars() {
        match character {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => output.push(character),
            _ => {}
        }
    }

    decode_html_entities(output.trim())
}

/// 还原服务端下发的 HTML 实体。
///
/// 酷狗歌词接口把 `'` 写成 `&apos;`（截图里 `I&apos;m sorry 我不是坏男孩`），
/// 客户端不解码就会把实体原样显示出来。
///
/// `&amp;` 必须**最后**处理：先解它的话，`&amp;lt;` 会变成 `&lt;`，再被后续规则
/// 解成 `<`，把一个转义过的实体凭空变成真字符。
fn decode_html_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }

    const NAMED: &[(&str, &str)] = &[
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&#39;", "'"),
        ("&#x27;", "'"),
        ("&nbsp;", " "),
    ];

    let mut output = text.to_string();
    for (entity, replacement) in NAMED {
        if output.contains(entity) {
            output = output.replace(entity, replacement);
        }
    }
    // 数值实体 `&#NNN;` / `&#xHH;`：只处理十进制与十六进制两种写法
    output = decode_numeric_entities(&output);
    // 必须最后
    output.replace("&amp;", "&")
}

/// 还原 `&#123;` / `&#x1F600;` 形式的数值实体。非法的原样保留。
fn decode_numeric_entities(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find("&#") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find(';') else {
            output.push_str(&rest[start..]);
            return output;
        };
        let body = &after[..end];
        let code = if let Some(hex) = body.strip_prefix('x').or_else(|| body.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            body.parse::<u32>().ok()
        };
        match code.and_then(char::from_u32) {
            Some(character) => output.push(character),
            None => output.push_str(&rest[start..start + 2 + end + 1]),
        }
        rest = &after[end + 1..];
    }

    output.push_str(rest);
    output
}

/// 从 KRC 的 `[language:base64]` 标签里取出译文，按行挂到歌词上。
///
/// 标签形如：
///
/// ```text
/// [language:eyJjb250ZW50IjpbeyJsYW5ndWFnZSI6MCwibHlyaWNDb250ZW50Ijpb...]]
/// ```
///
/// base64 解开后是：
///
/// ```json
/// {"content":[{"type":1,"lyricContent":["译文1","译文2"]},{"type":0,"lyricContent":["音译1"]}]}
/// ```
///
/// `type` 为 1 是翻译、0 是音译，两条轨各自填写、互不影响。
fn attach_translations(lyric: &mut Lyric, krc_text: &str) {
    let Some(payload) = extract_language_payload(krc_text) else {
        return;
    };
    let Some(content) = payload.get("content").and_then(|value| value.as_array()) else {
        return;
    };

    // 各轨按 `type` 编号，但含义在不同歌里不固定（Bad Apple!! 的 type=1 是罗马音、
    // type=2 才是中文译文）。所以**不能**硬编码 type 取轨，要按「中文字符密度」动态挑——
    // 密度最高的是译文，最低的是音译/罗马音。
    //
    // `language` 字段同理不能用（实测两轨 language 都是 0），但跟我们的挑法无关。
    let mut tracks: Vec<(i64, String)> = Vec::new();
    for section in content {
        let Some(kind) = section.get("type").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        let lines = section
            .get("lyricContent")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(flatten_lyric_content)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let joined: String = lines.join("\n");
        if !joined.trim().is_empty() {
            tracks.push((kind, joined));
        }
    }
    if tracks.is_empty() {
        return;
    }

    // 汉字（Han）占比。注意**汉字分不清中文和日文**——日文也用汉字。
    let hanzi_ratio = |text: &str| -> f64 {
        let (chars, hanzi) =
            text.chars()
                .filter(|ch| !ch.is_whitespace())
                .fold((0usize, 0usize), |(c, h), ch| {
                    (
                        c + 1,
                        h + if matches!(ch, '\u{4e00}'..='\u{9fff}') {
                            1
                        } else {
                            0
                        },
                    )
                });
        if chars == 0 {
            0.0
        } else {
            hanzi as f64 / chars as f64
        }
    };

    // 是否含假名（平假名 / 片假名）。有假名就说明这是**日文**，
    // 不能当中文译文——否则日文歌会把「日文原文」当成译文，显示出来跟原文重复。
    let has_kana =
        |text: &str| -> bool { text.chars().any(|ch| matches!(ch, '\u{3040}'..='\u{30ff}')) };

    // 判定「这是一条中文译文轨」：有汉字、且不含假名。
    // 密度阈值取 0.2：中文译文几乎全是汉字，日文原文因为夹杂大量假名通常低于此值，
    // 取宽松一点避免漏掉夹杂少量假名的译文（比如引用原句时）。
    let is_chinese_track = |text: &str| -> bool { !has_kana(text) && hanzi_ratio(text) >= 0.2 };

    // 排序：中文轨排最前（按汉字密度降序），其余（日文原文 / 罗马音）排后面。
    // 罗马音密度接近 0，自然落在最后，正好当音译。
    tracks.sort_by(|a, b| {
        let a_cn = is_chinese_track(&a.1);
        let b_cn = is_chinese_track(&b.1);
        match (a_cn, b_cn) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => hanzi_ratio(&b.1)
                .partial_cmp(&hanzi_ratio(&a.1))
                .unwrap_or(std::cmp::Ordering::Equal),
        }
    });

    // 有中文轨才把它当译文；否则留空，让界面回退显示音译（罗马音）——
    // 日语歌常常只有「日文原文 + 罗马音」两条轨，此时显示罗马音比重复原文有用得多，
    // 用户至少能跟着念。
    let chinese_count = tracks.iter().filter(|(_, t)| is_chinese_track(t)).count();

    // 译文位：只有存在中文轨时才填；没有就留空，界面会退回显示音译。
    // （用 first 是因为排序已把中文轨排到最前）
    let (translation_kind, translation_text) = match tracks.first().cloned() {
        // `chinese_count > 0` 已经保证 tracks 非空，但这里仍然显式匹配而不 unwrap——
        // 项目约定非测试代码不得出现裸 unwrap（见 src/error.rs 模块文档）。
        Some(first) if chinese_count > 0 => first,
        _ => (i64::MAX, String::new()),
    };

    // 音译位：取**汉字密度最低**的那条轨，也就是罗马音。
    //
    // 排序后中文在最前、其余按密度降序，所以最低密度的落在末尾。
    // 不能取第一条——没有中文轨时第一条是日文原文，而原文已经显示在上面了，
    // 再显示一遍毫无意义；罗马音至少能让人跟着念。
    let romanization = tracks
        .last()
        .cloned()
        .filter(|(kind, _)| *kind != translation_kind);

    // ---- 行号对齐（关键）----
    //
    // 各语言轨的 lyricContent 是按**原始 KRC 定时行**顺序排列的，而 parse_lrc 会
    // 跳过空内容的行（间奏之类的空行）。两边序号会错位：歌词第 5 行可能对应轨道的第 8 项。
    // 不对齐的话译文会取到空值，界面就退回去显示音译——这正是「翻译显示成音译」的根因。
    //
    // 所以这里复刻 parse_lrc 的筛选逻辑，算出每条保留下来的歌词行在定时行中的真实序号。
    let mut ordinals: Vec<usize> = Vec::new();
    let mut timed_seen = 0usize;
    for raw in krc_text.lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            continue;
        }
        let (timestamps, remainder) = consume_time_tags(line);
        if timestamps.is_empty() {
            continue; // 元信息行（[id:] [ti:] [language:] 等）
        }
        let ordinal = timed_seen;
        timed_seen += 1;
        // 判据必须与 `parse_lrc` 一致，否则下面的 `ordinals` 会错位——
        // 见 `has_lyric_content` 的说明
        if has_lyric_content(remainder) {
            ordinals.push(ordinal);
        }
    }

    // 预先切好行，避免在内层循环里反复 split（也顺带解决借用/move 的麻烦）
    let translation_lines: Vec<&str> = translation_text.lines().collect();
    // 先把音译轨的整段文本取出来（拥有所有权），再按行切片，
    // 否则引用的是闭包里的临时变量，编译不过
    let romanization_text: Option<String> = match romanization {
        Some((other_kind, other_text)) if other_kind != translation_kind => Some(other_text),
        _ => None,
    };
    let romanization_lines: Vec<&str> = romanization_text
        .as_deref()
        .map(|text| text.lines().collect())
        .unwrap_or_default();

    for (index, line) in lyric.lines.iter_mut().enumerate() {
        // 歌词行 → 它在定时行中的序号 → 再到轨道里取对应项
        let source = ordinals.get(index).copied().unwrap_or(index);
        if let Some(text) = translation_lines.get(source)
            && !text.trim().is_empty()
        {
            line.translation = Some(decode_html_entities(text.trim()));
        }
        if let Some(text) = romanization_lines.get(source)
            && !text.trim().is_empty()
        {
            line.romanization = Some(decode_html_entities(text.trim()));
        }
    }
}

/// 取出 `[language:...]` 里的载荷字符串。
fn extract_language_payload(text: &str) -> Option<serde_json::Value> {
    let start = text.find("[language:")? + "[language:".len();
    let end = text[start..].find(']')? + start;
    let raw = &text[start..end];

    // 载荷里偶尔混进换行等字符，先清掉再补 padding
    let cleaned: String = raw.chars().filter(|ch| !ch.is_whitespace()).collect();
    let mut padded = cleaned;
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }

    let engine = base64::engine::general_purpose::STANDARD;
    let decoded = engine.decode(padded).ok()?;
    serde_json::from_slice(&decoded).ok()
}

/// `lyricContent` 的元素可能是字符串，也可能是 `["原文","译文"]` 这样的数组。
fn flatten_lyric_content(entry: &serde_json::Value) -> Option<String> {
    match entry {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(parts) => {
            let text: String = parts
                .iter()
                .filter_map(|part| part.as_str())
                .collect::<Vec<_>>()
                .join("");
            Some(text)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 译文与音译的提取。
    /// 日语歌：只有「日文原文 + 罗马音」两条轨，没有中文译文时的行为。
    ///
    /// 锁住两点。
    ///
    /// 一是**不能把日文原文当成译文**：日文也用汉字，光看汉字密度会误判，
    /// 必须靠「含假名」把日文排除掉。
    ///
    /// 二是没有中文轨时译文留空、音译位放**罗马音**（密度最低那条），
    /// 而不是再显示一遍日文原文——原文已经在上面显示过了，重复毫无意义。
    #[test]
    fn japanese_song_without_chinese_uses_romaji() {
        use base64::Engine;

        let payload = r#"{"content":[
            {"language":0,"type":0,"lyricContent":[["","na ga re te ku to ki no na ka de de mo"]]},
            {"language":0,"type":1,"lyricContent":[["","流れてく時の中ででも"]]}
        ]}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
        let krc = format!("[id:1]\n[language:{encoded}]\n[0,1000]流れてく時の中ででも\n");

        let mut lyric = parse_lrc(&krc);
        attach_translations(&mut lyric, &krc);

        assert_eq!(lyric.lines.len(), 1);
        // 含假名 → 不算中文译文，译文留空
        assert_eq!(
            lyric.lines[0].translation, None,
            "日文原文（含假名）不能被当成中文译文"
        );
        // 音译位应是罗马音，不是日文原文
        assert_eq!(
            lyric.lines[0].romanization.as_deref(),
            Some("na ga re te ku to ki no na ka de de mo"),
            "没有中文轨时，音译位应放罗马音"
        );
    }

    /// **回归测试**：`parse_lrc` 与 `attach_translations` 必须用**同一个**判据判断
    /// 「这一行算不算歌词」，否则译文会整体串位。
    ///
    /// 两处原本各写了一份：`parse_lrc` 看 `parse_krc_words(...)` 的返回值是否为空，
    /// `attach_translations` 看 `clean_krc_markup(...)` 是否为空。绝大多数输入上两者一致，
    /// 但只要出现一行「`<` 之前有字、却没有配对的 `>`」的歌词（比如歌里写了 `宝贝<3`），
    /// 两者就分道扬镳：`parse_krc_words` 只输出**标记之后**的字符，所以那行整行算空、
    /// 被丢掉；`clean_krc_markup` 只按深度过滤，`<` 前面的字照样留下，所以那行算有内容。
    /// 于是 `ordinals` 多出一项，**之后每一行的译文都往后串一位**——用户看到的是
    /// 「译文和原文对不上」，而且没有任何报错。
    ///
    /// 这条测试同时钉住两件事：那一行不再被丢掉（`<` 之前的字是正文），
    /// 以及剩下的行与译文逐行对齐（中间那行空行仍然会被跳过）。
    #[test]
    fn a_line_with_a_stray_angle_bracket_does_not_shift_translations() {
        use base64::Engine;

        let payload = r#"{"content":[
            {"language":0,"type":2,"lyricContent":[
                ["","译1"],["","译2"],["","译3"],["","译4"]
            ]}
        ]}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
        let krc = format!(
            "[id:1]\n[language:{encoded}]\n\
             [0,1000]第一句\n\
             [1000,1000]   \n\
             [2000,1000]宝贝<3\n\
             [3000,1000]第四句\n"
        );

        let mut lyric = parse_lrc(&krc);
        attach_translations(&mut lyric, &krc);

        let texts: Vec<&str> = lyric.lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["第一句", "宝贝", "第四句"],
            "空行该跳过；`宝贝<3` 这一行的字是正文，不该丢"
        );

        // 关键：剩下三行拿到的译文必须与它们在**原始定时行**里的位置对应
        let translations: Vec<Option<&str>> = lyric
            .lines
            .iter()
            .map(|line| line.translation.as_deref())
            .collect();
        assert_eq!(
            translations,
            vec![Some("译1"), Some("译3"), Some("译4")],
            "译文串位了：宝贝那一行应当拿第 3 条译文"
        );
    }

    #[test]
    fn attaches_translation_and_romanization() {
        use base64::Engine;

        let payload = r#"{"content":[
            {"language":0,"type":1,"lyricContent":[["","no mi ko"]]},
            {"language":0,"type":2,"lyricContent":[["","就算身处流逝的时光里"]]}
        ]}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
        // 一行 KRC：时间标签形如 [起始毫秒,持续毫秒]
        let krc = format!("[id:1]\n[language:{encoded}]\n[0,1000]日文原文\n");

        let mut lyric = parse_lrc(&krc);
        attach_translations(&mut lyric, &krc);

        assert_eq!(lyric.lines.len(), 1, "应解析出 1 行");
        assert_eq!(lyric.lines[0].text, "日文原文");
        assert_eq!(
            lyric.lines[0].translation.as_deref(),
            Some("就算身处流逝的时光里"),
            "中文密度高的轨应被选为译文（Bad Apple!! 是 type=2）"
        );
        assert_eq!(
            lyric.lines[0].romanization.as_deref(),
            Some("no mi ko"),
            "拉丁字符多的轨应被选为音译（Bad Apple!! 是 type=1）"
        );
    }

    #[test]
    fn parses_plain_lrc() {
        let text = "[ti:测试]\n[ar:歌手]\n[00:01.00]第一句\n[00:05.50]第二句\n";
        let lyric = parse_lrc(text);
        assert_eq!(lyric.lines.len(), 2);
        assert_eq!(lyric.lines[0].time_ms, 1_000);
        assert_eq!(lyric.lines[1].time_ms, 5_500);
        assert_eq!(lyric.lines[1].text, "第二句");
        // 原文要原样留着：WebSocket 的 `lyricsData` 发的是它，不是解析后的 `lines`。
        // 解析会丢掉逐字标记与 `[ti:]` 这类标签，而那正是第三方客户端排版要用的。
        assert_eq!(lyric.text, text);
    }

    #[test]
    fn expands_multiple_tags_on_one_line() {
        let lyric = parse_lrc("[00:01.00][00:05.00]副歌\n");
        assert_eq!(lyric.lines.len(), 2);
        assert_eq!(lyric.lines[0].time_ms, 1_000);
        assert_eq!(lyric.lines[1].time_ms, 5_000);
        assert_eq!(lyric.lines[0].text, lyric.lines[1].text);
    }

    #[test]
    fn handles_varying_fraction_widths() {
        assert_eq!(parse_time_tag("00:01"), Some(1_000));
        assert_eq!(parse_time_tag("00:01.5"), Some(1_500));
        assert_eq!(parse_time_tag("00:01.23"), Some(1_230));
        assert_eq!(parse_time_tag("00:01.234"), Some(1_234));
        assert_eq!(parse_time_tag("01:02.3456"), Some(62_345));
    }

    #[test]
    fn rejects_metadata_tags() {
        assert_eq!(parse_time_tag("ti:标题"), None);
        assert_eq!(parse_time_tag("ar:歌手"), None);
        // `[language:base64...]` 是 MoeKoeMusic 用的翻译元信息
        assert_eq!(parse_time_tag("language:eyJhbGciOi"), None);
        // 秒数越界
        assert_eq!(parse_time_tag("00:99.00"), None);
    }

    #[test]
    fn parses_krc_style_timestamps() {
        let lyric = parse_lrc("[1234,567]逐字歌词\n");
        assert_eq!(lyric.lines.len(), 1);
        assert_eq!(lyric.lines[0].time_ms, 1_234);
    }

    #[test]
    fn strips_krc_inline_markup() {
        // 真实 KRC：标记在**字前面**
        let lyric = parse_lrc("[1000,500]<100,200,0>海<300,200,0>阔天空\n");
        assert_eq!(lyric.lines[0].text, "海阔天空");
    }

    #[test]
    fn decodes_html_entities_in_plain_lrc() {
        // 服务端把 `'` 下发成 `&apos;`，截图里就是 `I&apos;m sorry 我不是坏男孩`
        let lyric = parse_lrc("[00:01.00]I&apos;m sorry 我不是坏男孩\n");
        assert_eq!(lyric.lines[0].text, "I'm sorry 我不是坏男孩");
    }

    #[test]
    fn decodes_html_entities_in_krc_markup() {
        let lyric = parse_lrc("[1000,500]<100,200,0>I&apos;m<300,200,0> sorry\n");
        assert_eq!(lyric.lines[0].text, "I'm sorry");
        // 逐字时间按解码后的字符分配：`&apos;` 六个字符解成一个
        assert_eq!(lyric.lines[0].words.len(), 9);
    }

    #[test]
    fn escaped_angle_brackets_survive_markup_stripping() {
        // `&lt;` 是真实字符（歌里写了「<3」），不是 KRC 标记。
        // 若先解实体再剥标记，这个 `<` 会开启深度计数、把后面的字全吞掉。
        let lyric = parse_lrc("[1000,500]<100,200,0>&lt;3 你\n");
        assert_eq!(lyric.lines[0].text, "<3 你");
    }

    #[test]
    fn amp_is_decoded_last() {
        // `&amp;lt;` 是「字面的 &lt;」，只该解一层。先解 `&amp;` 会连锁解成 `<`。
        assert_eq!(decode_html_entities("&amp;lt;"), "&lt;");
        assert_eq!(decode_html_entities("&amp;amp;"), "&amp;");
        // 单独的 `&` 原样保留，不能被吃掉
        assert_eq!(decode_html_entities("A & B"), "A & B");
    }

    #[test]
    fn decodes_numeric_entities() {
        assert_eq!(decode_html_entities("&#39;"), "'");
        assert_eq!(decode_html_entities("&#x27;"), "'");
        assert_eq!(decode_html_entities("&#20320;"), "你");
        // 非法数值原样保留，不能吞字符
        assert_eq!(decode_html_entities("&#xZZ;"), "&#xZZ;");
        assert_eq!(decode_html_entities("&#999999999;"), "&#999999999;");
        // 没有分号的裸 `&#` 原样保留
        assert_eq!(decode_html_entities("&#"), "&#");
    }

    #[test]
    fn decodes_entities_in_translation_track() {
        use base64::Engine;

        let payload = r#"{"content":[
            {"language":0,"type":1,"lyricContent":[["","他&apos;说没关系"]]}
        ]}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
        let krc = format!("[id:1]\n[language:{encoded}]\n[0,1000]hello\n");

        let mut lyric = parse_lrc(&krc);
        attach_translations(&mut lyric, &krc);

        assert_eq!(
            lyric.lines[0].translation.as_deref(),
            Some("他'说没关系"),
            "译文轨也要解实体"
        );
    }

    #[test]
    fn skips_lines_without_text() {
        let lyric = parse_lrc("[00:01.00]\n[00:02.00]有词\n");
        assert_eq!(lyric.lines.len(), 1);
        assert_eq!(lyric.lines[0].text, "有词");
    }

    #[test]
    fn sorts_out_of_order_lines() {
        let lyric = parse_lrc("[00:05.00]后\n[00:01.00]前\n");
        assert_eq!(lyric.lines[0].text, "前");
    }

    #[test]
    fn extracts_decoded_content_from_json() {
        let root = json!({"status": 1, "decodeContent": "[00:01.00]嗨\n"});
        assert_eq!(extract_lyric_text(&root), "[00:01.00]嗨");
    }

    #[test]
    fn decodes_base64_content_when_decode_content_absent() {
        // "abc" 的 base64
        let root = json!({"content": "YWJj"});
        assert_eq!(extract_lyric_text(&root), "abc");
    }

    /// 逐字时间戳：`<本行内偏移,持续,0>` 跟在它描述的那个字后面，
    /// 绝对时间 = 行起始 + 偏移。
    #[test]
    fn parses_per_word_timestamps() {
        // 真实 KRC 的写法：标记在**字前面**。行起始 1000ms。
        let (text, words) =
            parse_krc_words("<0,200,0>这<200,300,0>是<500,200,0>测<700,200,0>试", 1000);
        assert_eq!(text, "这是测试");
        assert_eq!(words.len(), text.chars().count());
        assert_eq!(
            words[0],
            LyricWord {
                start_ms: 1000,
                end_ms: 1200
            }
        );
        assert_eq!(
            words[1],
            LyricWord {
                start_ms: 1200,
                end_ms: 1500
            }
        );
        assert_eq!(
            words[2],
            LyricWord {
                start_ms: 1500,
                end_ms: 1700
            }
        );
        assert_eq!(
            words[3],
            LyricWord {
                start_ms: 1700,
                end_ms: 1900
            }
        );
    }

    /// 一个标记可以管好几个字符（真实数据里见过 `<1600,160,0>Jay`），
    /// 同一段内的字符共用一组时间——和 MoeKoeMusic 的整体高亮保持一致。
    #[test]
    fn a_single_tag_covers_multiple_characters() {
        let (text, words) = parse_krc_words("<0,300,0>周<300,300,0>Jay", 2000);
        assert_eq!(text, "周Jay");
        assert_eq!(words.len(), text.chars().count());
        // "周" 单独一个单元
        assert_eq!(
            words[0],
            LyricWord {
                start_ms: 2000,
                end_ms: 2300
            }
        );
        // "Jay" 三个字符共用同一组时间
        for word in &words[1..] {
            assert_eq!(
                *word,
                LyricWord {
                    start_ms: 2300,
                    end_ms: 2600
                }
            );
        }
    }

    /// 照抄真实响应里的一行（晴天的第一句），确保端到端格式对得上。
    #[test]
    fn parses_a_real_krc_line() {
        let line = "[0,2250]<0,160,0>晴<160,160,0>天<320,160,0> <480,160,0>-<640,160,0> <800,160,0>周<960,160,0>杰<1120,160,0>伦";
        let (timestamps, remainder) = consume_time_tags(line);
        assert_eq!(timestamps, vec![0]);
        let (text, words) = parse_krc_words(remainder, 0);
        assert_eq!(text, "晴天 - 周杰伦");
        assert_eq!(words.len(), text.chars().count());
        // 第一个字 0~160ms，第二个 160~320ms
        assert_eq!(
            words[0],
            LyricWord {
                start_ms: 0,
                end_ms: 160
            }
        );
        assert_eq!(
            words[1],
            LyricWord {
                start_ms: 160,
                end_ms: 320
            }
        );
    }

    /// 普通 LRC（没有任何逐字标记）→ 逐字信息为空，调用方退回整行高亮。
    #[test]
    fn plain_text_has_no_word_timestamps() {
        let (text, words) = parse_krc_words("这是一句普通的歌词", 1000);
        assert_eq!(text, "这是一句普通的歌词");
        assert!(words.is_empty());
    }

    /// 字数与标记数对不上就**整体放弃**逐字信息——猜出来的时间会让歌词
    /// 唱得和声音错位，比没效果更糟。调用方会退回整行高亮。
    #[test]
    fn words_always_line_up_with_text() {
        // 真实格式下每个字符都会配到一组时间，所以 text 与 words 必须严格
        // 一一对应——这是渲染逐字高亮的前提，错位就会「唱的和亮的对不上」。
        let (text, words) = parse_krc_words("<0,200,0>这<200,300,0>是测试", 1000);
        assert_eq!(text, "这是测试");
        assert_eq!(words.len(), text.chars().count());
        // "是测试" 三个字共用了第二段的时间
        for word in &words[1..] {
            assert_eq!(
                *word,
                LyricWord {
                    start_ms: 1200,
                    end_ms: 1500
                }
            );
        }
    }

    /// 一个字在不同进度下的比例：未唱 0.0、正在唱介于两者之间、已唱 1.0。
    #[test]
    fn word_progress_follows_playback_position() {
        let word = LyricWord {
            start_ms: 1000,
            end_ms: 1200,
        };
        assert_eq!(word.progress_at(900), 0.0, "还没到");
        assert_eq!(word.progress_at(1000), 0.0, "刚起头");
        assert_eq!(word.progress_at(1100), 0.5, "唱到一半");
        assert_eq!(word.progress_at(1200), 1.0, "唱完");
        assert_eq!(word.progress_at(9999), 1.0, "过去很久仍然是 1，不能溢出");
    }

    /// 坏数据不能让逐字推进炸掉：`end_ms <= start_ms` 时不能除零。
    #[test]
    fn zero_length_word_does_not_divide_by_zero() {
        let degenerate = LyricWord {
            start_ms: 1000,
            end_ms: 1000,
        };
        assert_eq!(degenerate.progress_at(999), 0.0);
        assert_eq!(degenerate.progress_at(1000), 1.0);
        assert!(degenerate.progress_at(1500).is_finite());

        // 倒挂的时间戳同样不能出 NaN / inf
        let reversed = LyricWord {
            start_ms: 2000,
            end_ms: 1000,
        };
        assert!(reversed.progress_at(1500).is_finite());
    }

    /// 解不开的候选要被**跳过并继续**，不能把整首歌的歌词判死。
    ///
    /// 上游 `decodeLyrics` 解不开时返回空字符串；native 的 `krc::decode` 返回 `Err`，
    /// 但 `inject_decoded_lyric` 把它折成空串，于是这个候选解析出空歌词、`continue`，
    /// 后面的候选照样有机会——这正是两端表现一致的地方。
    ///
    /// 这里用一个假的 `LyricSource` 直接锁住 `fetch_lyric_via` 的控制流：
    /// 第一个候选正文解不出内容，第二个候选是好的，结果必须是第二个的歌词。
    struct FakeLyricSource {
        bodies: Vec<String>,
    }

    impl LyricSource for FakeLyricSource {
        async fn search_lyric(&self, _song: &Song) -> Result<Value> {
            Ok(json!({
                "status": 1,
                "candidates": [
                    {"id": "bad", "accesskey": "k1"},
                    {"id": "good", "accesskey": "k2"},
                ],
            }))
        }

        async fn lyric_body(&self, lyric_id: &str, _access_key: &str) -> Result<String> {
            let index = if lyric_id == "bad" { 0 } else { 1 };
            Ok(self.bodies[index].clone())
        }
    }

    fn fake_song() -> Song {
        Song {
            name: "测试曲".to_string(),
            hash: "deadbeef".to_string(),
            duration_ms: 1000,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn undecodable_candidate_is_skipped_and_next_one_is_used() {
        let source = FakeLyricSource {
            bodies: vec![
                // 第一个：JSON 合法但解不出歌词（等价于 KRC 解密失败折成空串）
                json!({"status": 200, "contenttype": 0, "decodeContent": ""}).to_string(),
                // 第二个：正常歌词
                json!({"status": 200, "decodeContent": "[0,1000]好歌词\n"}).to_string(),
            ],
        };

        let lyric = fetch_lyric_via(&source, &fake_song()).await.unwrap();
        assert_eq!(lyric.lines.len(), 1);
        assert_eq!(lyric.lines[0].text, "好歌词");
    }

    /// 所有候选都解不出内容时，最终是 `NotFound`（不是 panic、不是空歌词对象）——
    /// 界面据此只记一条 WARN，播放不受影响。
    #[tokio::test]
    async fn all_undecodable_candidates_end_as_not_found() {
        let source = FakeLyricSource {
            bodies: vec![
                json!({"status": 200, "decodeContent": ""}).to_string(),
                json!({"status": 200, "decodeContent": ""}).to_string(),
            ],
        };

        let error = fetch_lyric_via(&source, &fake_song()).await.unwrap_err();
        assert!(error.to_string().contains("歌词为空"), "实际：{error}");
    }

    /// 候选自带的时长与本曲对不上时必须跳过——否则会把**另一首歌**的歌词
    /// 当成这首歌的歌词（实测《记忆停留的地方》的冷门版本拿到的是古典钢琴曲）。
    #[tokio::test]
    async fn candidates_with_mismatched_duration_are_skipped() {
        struct Source {
            candidates: serde_json::Value,
            bodies: Vec<String>,
        }

        impl LyricSource for Source {
            async fn search_lyric(&self, _song: &Song) -> Result<Value> {
                Ok(self.candidates.clone())
            }

            async fn lyric_body(&self, lyric_id: &str, _access_key: &str) -> Result<String> {
                let index = if lyric_id == "wrong" { 0 } else { 1 };
                Ok(self.bodies[index].clone())
            }
        }

        let source = Source {
            candidates: json!({
                "status": 1,
                "candidates": [
                    // 另一首歌：320 秒，与本曲（60 秒）对不上
                    {"id": "wrong", "accesskey": "k1", "duration": 320317},
                    {"id": "right", "accesskey": "k2", "duration": 60000},
                ],
            }),
            bodies: vec![
                json!({"status": 200, "decodeContent": "[0,1000]别人的歌词\n"}).to_string(),
                json!({"status": 200, "decodeContent": "[0,1000]本曲歌词\n"}).to_string(),
            ],
        };

        let song = Song {
            name: "记忆停留的地方".to_string(),
            hash: "h".to_string(),
            duration_ms: 60_000,
            ..Default::default()
        };

        let lyric = fetch_lyric_via(&source, &song).await.unwrap();
        assert_eq!(lyric.lines.len(), 1);
        assert_eq!(lyric.lines[0].text, "本曲歌词");
    }

    /// 所有候选的时长都对不上时，结果是 `NotFound`（界面显示「暂无歌词」），
    /// 而不是硬塞一首别的歌的歌词。
    #[tokio::test]
    async fn all_mismatched_candidates_end_as_not_found() {
        struct Source;
        impl LyricSource for Source {
            async fn search_lyric(&self, _song: &Song) -> Result<Value> {
                Ok(json!({
                    "status": 1,
                    "candidates": [
                        {"id": "a", "accesskey": "k", "duration": 320317},
                    ],
                }))
            }
            async fn lyric_body(&self, _id: &str, _key: &str) -> Result<String> {
                Ok(json!({"status": 200, "decodeContent": "[0,1000]纯音乐，请欣赏\n"}).to_string())
            }
        }

        let song = Song {
            name: "记忆停留的地方".to_string(),
            duration_ms: 60_000,
            ..Default::default()
        };
        let error = fetch_lyric_via(&Source, &song).await.unwrap_err();
        assert!(error.to_string().contains("匹配的歌词"), "实际：{error}");
    }

    /// 中文 KRC 常把「歌名 - 歌手」标题行与制作人员名单当成定时行塞进来，
    /// 这些行必须被剔除，不能当作歌词显示。
    #[test]
    fn credit_and_title_lines_are_dropped() {
        let song = Song {
            name: "记忆停留".to_string(),
            singers: vec![crate::api::model::Singer {
                id: 0,
                name: "蓝心羽".to_string(),
            }],
            duration_ms: 174_000,
            ..Default::default()
        };
        let krc = "[0,1000]记忆停留 - 蓝心羽\n\
                   [1000,1000]词：陈大创\n\
                   [2000,1000]曲：陈大创\n\
                   [3000,1000]编曲：KEYI\n\
                   [4000,1000]制作发行：极韵文化/BLUE STUDIO\n\
                   [5000,1000]想要捕捉爱的瞬间\n\
                   [6000,1000]曲终人散也要说再见\n";

        let mut lyric = parse_lrc(krc);
        drop_non_lyric_lines(&mut lyric, &song);

        let texts: Vec<&str> = lyric.lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["想要捕捉爱的瞬间", "曲终人散也要说再见"],
            "标题行与名单行该剔除；以「曲」开头但没有冒号的正常歌词要保留"
        );
    }

    /// 时长校验：未知时长不否决；同一首歌允许小幅偏差；差得离谱才拒绝。
    #[test]
    fn duration_match_tolerates_small_drift() {
        assert!(duration_matches(None, 60_000), "未知时长不否决");
        assert!(duration_matches(Some(0), 60_000), "0 视为未知");
        assert!(duration_matches(Some(60_628), 60_000), "628ms 偏差可接受");
        assert!(duration_matches(Some(174_628), 174_000), "628ms 偏差可接受");
        assert!(
            !duration_matches(Some(320_317), 60_000),
            "差 260 秒必须拒绝"
        );
        assert!(
            !duration_matches(Some(320_317), 160_000),
            "差 160 秒必须拒绝"
        );
    }

    /// 候选时长对得上、但歌手与歌名都是另一首歌时必须跳过——实测 July 的纯音乐
    /// 《기억이 머물던 곳》拿到的是 MC晚安《错过》，两者时长只差 0.6 秒。
    #[tokio::test]
    async fn candidates_with_mismatched_identity_are_skipped() {
        struct Source {
            candidates: serde_json::Value,
            bodies: Vec<String>,
        }

        impl LyricSource for Source {
            async fn search_lyric(&self, _song: &Song) -> Result<Value> {
                Ok(self.candidates.clone())
            }

            async fn lyric_body(&self, lyric_id: &str, _access_key: &str) -> Result<String> {
                let index = if lyric_id == "wrong" { 0 } else { 1 };
                Ok(self.bodies[index].clone())
            }
        }

        let source = Source {
            candidates: json!({
                "status": 1,
                "candidates": [
                    // 时长相近（203598 vs 203000），但歌手与歌名完全是另一首歌
                    {"id": "wrong", "accesskey": "k1", "duration": 203598, "singer": "MC晚安", "song": "错过"},
                    {"id": "right", "accesskey": "k2", "duration": 203000, "singer": "July", "song": "기억이 머물던 곳"},
                ],
            }),
            bodies: vec![
                json!({"status": 200, "decodeContent": "[0,1000]想想真是可惜呀\n"}).to_string(),
                json!({"status": 200, "decodeContent": "[0,1000]本曲歌词\n"}).to_string(),
            ],
        };

        let song = Song {
            name: "기억이 머물던 곳".to_string(),
            duration_ms: 203_000,
            singers: vec![crate::api::model::Singer {
                id: 0,
                name: "July".to_string(),
            }],
            ..Default::default()
        };

        let lyric = fetch_lyric_via(&source, &song).await.unwrap();
        assert_eq!(lyric.lines.len(), 1);
        assert_eq!(lyric.lines[0].text, "本曲歌词");
    }

    /// 名称比对要宽松：忽略大小写、空白与常见分隔符/括号；但不同的名字不能误判为相同。
    #[test]
    fn name_matching_is_lenient_but_not_wrong() {
        assert!(text_matches("July", "july"));
        assert!(text_matches("记忆停留", "记忆停留 (治愈版)"));
        assert!(text_matches(
            "Where memory had stopped",
            "Wherememoryhadstopped"
        ));
        assert!(!text_matches("MC晚安", "July"));
        assert!(!text_matches("错过", "기억이 머물던 곳"));
        // 太短的名字不做包含判断，避免误判
        assert!(!text_matches("a", "July"));
    }

    /// 歌手 / 歌名任一匹配即接受；两者都缺失时不否决；两者都对不上才拒绝。
    #[test]
    fn candidate_identity_matches_uses_singer_or_title() {
        let song = Song {
            name: "记忆停留".to_string(),
            singers: vec![crate::api::model::Singer {
                id: 0,
                name: "蓝心羽".to_string(),
            }],
            ..Default::default()
        };
        assert!(
            candidate_identity_matches(None, None, &song),
            "缺字段不否决"
        );
        assert!(
            candidate_identity_matches(Some("蓝心羽"), Some("别的"), &song),
            "歌手对得上"
        );
        assert!(
            candidate_identity_matches(Some("别人"), Some("记忆停留"), &song),
            "歌名对得上"
        );
        assert!(
            !candidate_identity_matches(Some("别人"), Some("别的歌"), &song),
            "都对不上则拒绝"
        );
    }

    /// KRC 内部 `[hash:]` 与本曲不符的候选必须跳过，即便候选元数据
    /// （singer/song/duration）全对——元数据本身可以错标。
    #[tokio::test]
    async fn candidate_with_mismatched_krc_hash_is_skipped() {
        struct Source {
            candidates: serde_json::Value,
            bodies: Vec<String>,
        }

        impl LyricSource for Source {
            async fn search_lyric(&self, _song: &Song) -> Result<Value> {
                Ok(self.candidates.clone())
            }

            async fn lyric_body(&self, lyric_id: &str, _access_key: &str) -> Result<String> {
                let index = if lyric_id == "wrong" { 0 } else { 1 };
                Ok(self.bodies[index].clone())
            }
        }

        let source = Source {
            candidates: json!({
                "status": 1,
                "candidates": [
                    // 元数据全对（歌手/歌名/时长都与本曲一致），但正文是别的歌
                    {"id": "wrong", "accesskey": "k1", "duration": 243000,
                     "singer": "July", "song": "바람에 쓰는 편지"},
                    {"id": "right", "accesskey": "k2", "duration": 243000,
                     "singer": "July", "song": "바람에 쓰는 편지"},
                ],
            }),
            bodies: vec![
                json!({"status": 200, "decodeContent":
                    "\u{feff}[ar:Mc小贱杰]\r\n[ti:说唱]\r\n[hash:90e67ade8426e017f71e6def7deb9b44]\r\n[0,1000]第一次喊说唱\n"
                }).to_string(),
                json!({"status": 200, "decodeContent":
                    "\u{feff}[ar:July]\r\n[ti:바람에 쓰는 편지]\r\n[hash:6af00fbd4d444a82c005843eef9dc2d4]\r\n[0,1000]纯音乐，请欣赏\n"
                }).to_string(),
            ],
        };

        let song = Song {
            name: "Letter That Writing in the Wind (写在风中的信)".to_string(),
            hash: "6AF00FBD4D444A82C005843EEF9DC2D4".to_string(),
            duration_ms: 243_722,
            singers: vec![crate::api::model::Singer {
                id: 18967,
                name: "July".to_string(),
            }],
            ..Default::default()
        };

        let lyric = fetch_lyric_via(&source, &song).await.unwrap();
        assert_eq!(lyric.lines.len(), 1);
        assert_eq!(
            lyric.lines[0].text, "纯音乐，请欣赏",
            "KRC 内部 hash 对不上的候选必须跳过"
        );
    }

    /// 没有 `[hash:]` 标签的候选不能因此被拒——存在没有 hash 的正确歌词。
    #[tokio::test]
    async fn candidate_without_krc_hash_is_kept() {
        struct Source;
        impl LyricSource for Source {
            async fn search_lyric(&self, _song: &Song) -> Result<Value> {
                Ok(json!({
                    "status": 1,
                    "candidates": [{"id": "ok", "accesskey": "k"}],
                }))
            }
            async fn lyric_body(&self, _id: &str, _key: &str) -> Result<String> {
                Ok(json!({"status": 200, "decodeContent":
                    "\u{feff}[ti:晴天]\r\n[ar:周杰伦]\r\n[0,1000]故事的小黄花\n"
                })
                .to_string())
            }
        }

        let song = Song {
            name: "晴天".to_string(),
            hash: "B3A52A7A958BF0AED0EBFBA2E9A818B7".to_string(),
            duration_ms: 269_000,
            singers: vec![crate::api::model::Singer {
                id: 6452,
                name: "周杰伦".to_string(),
            }],
            ..Default::default()
        };

        let lyric = fetch_lyric_via(&Source, &song).await.unwrap();
        assert_eq!(lyric.lines[0].text, "故事的小黄花");
    }
}
