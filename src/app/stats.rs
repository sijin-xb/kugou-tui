//! 本地听歌统计。
//!
//! 只记两样东西：**播完了几首**（`TrackFinished`，试听片段不算）和**听了多久**
//! （播放中每拍累加）。按「天 + 歌手」两个维度聚合，都有界：天保留 365 个，
//! 歌手保留播放次数前 [`ARTIST_LIMIT`] 名。歌曲级数据需要更重的去重与裁剪，
//! 先不做——「本周听了多少、最常听谁」这个粒度已经能回答日常的问题。
//!
//! 数据落在 `~/.cache/kugou-tui/stats.json`，和会话文件同一个目录：它同样是
//! 程序自己写的运行数据，不是用户手改的配置。日期取不到（`time` 在多线程下
//! 可能拒绝推断本地偏移）就跳过当次记录——少记一笔远好于记错一天。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 保留多少天的逐日数据。
const DAY_LIMIT: usize = 365;

/// 保留多少个歌手的播放计数。
const ARTIST_LIMIT: usize = 300;

/// 一天的统计。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct DayStat {
    #[serde(default)]
    pub plays: u64,
    /// 收听时长（毫秒）。
    #[serde(default)]
    pub listen_ms: u64,
}

/// 本地听歌统计。落盘格式见 [`Self::path`]。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListeningStats {
    #[serde(default)]
    days: BTreeMap<String, DayStat>,
    /// 歌手 → 播完次数。
    #[serde(default)]
    artists: BTreeMap<String, u64>,
}

impl ListeningStats {
    /// 统计文件的位置。
    pub fn path() -> std::path::PathBuf {
        crate::config::default_cache_dir().join("stats.json")
    }

    /// 读统计。文件不存在、损坏、字段缺失都当作「没有统计」——统计丢了只是
    /// 从零再记，不该让程序起不来。
    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// 写统计。失败只记日志：存不下统计不该影响退出。
    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            crate::logger::tlog!(crate::logger::LEVEL_WARN, "创建统计目录失败：{error}");
            return;
        }
        match serde_json::to_string(self) {
            Ok(text) => {
                if let Err(error) = std::fs::write(&path, text) {
                    crate::logger::tlog!(crate::logger::LEVEL_WARN, "保存统计失败：{error}");
                }
            }
            Err(error) => {
                crate::logger::tlog!(crate::logger::LEVEL_WARN, "序列化统计失败：{error}")
            }
        }
    }

    /// 记一次「完整听完」。
    pub fn record_complete(&mut self, artist: &str) {
        self.record_complete_on(crate::util::today_local(), artist);
    }

    /// 累加一段收听时长（毫秒）。
    pub fn add_listen(&mut self, ms: u64) {
        self.add_listen_on(crate::util::today_local(), ms);
    }

    pub(crate) fn record_complete_on(&mut self, today: Option<String>, artist: &str) {
        if let Some(today) = today {
            self.days.entry(today).or_default().plays += 1;
        }
        if !artist.is_empty() {
            *self.artists.entry(artist.to_string()).or_default() += 1;
        }
        self.prune();
    }

    pub(crate) fn add_listen_on(&mut self, today: Option<String>, ms: u64) {
        if ms == 0 {
            return;
        }
        if let Some(today) = today {
            self.days.entry(today).or_default().listen_ms += ms;
        }
    }

    /// 裁剪：天保留最近 [`DAY_LIMIT`] 天，歌手保留播放次数前 [`ARTIST_LIMIT`] 名。
    ///
    /// BTreeMap 的 key 是 ISO 日期，字典序即时间序，丢的永远是「最旧的那天」。
    fn prune(&mut self) {
        while self.days.len() > DAY_LIMIT {
            let Some(oldest) = self.days.keys().next().cloned() else {
                break;
            };
            self.days.remove(&oldest);
        }
        if self.artists.len() > ARTIST_LIMIT {
            let entries: Vec<(String, u64)> =
                std::mem::take(&mut self.artists).into_iter().collect();
            let mut entries = entries;
            entries.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            self.artists = entries.into_iter().take(ARTIST_LIMIT).collect();
        }
    }

    /// 首页资料区的一行摘要；没有任何记录时返回 `None`（不占版面）。
    pub fn summary_line(&self) -> Option<String> {
        self.summary_line_on(week_start_key().as_deref())
    }

    /// 今天听了多久、几首；今天还没有记录时返回 `None`。
    ///
    /// 播放条的上下文行用它兜底：这首歌没歌词可显示的时候，那行也有内容可看。
    pub fn today_summary(&self) -> Option<String> {
        let stat = self.days.get(&crate::util::today_local()?)?;
        if stat.listen_ms == 0 {
            return None;
        }
        Some(format!(
            "今天已听 {} · {} 首",
            format_listen(stat.listen_ms),
            stat.plays
        ))
    }

    pub(crate) fn summary_line_on(&self, week_start: Option<&str>) -> Option<String> {
        let (week_plays, week_listen_ms) = self.week_stats_on(week_start)?;
        if week_plays == 0 && week_listen_ms == 0 {
            return None;
        }
        let listen = format_listen(week_listen_ms);
        match self.top_artist() {
            Some(artist) => Some(format!("本周 {week_plays} 首 · {listen} · 最常听 {artist}")),
            None => Some(format!("本周 {week_plays} 首 · {listen}")),
        }
    }

    /// 最近 7 天的（播放总数, 收听总时长）。日期 key 是 ISO 格式，字典序比较
    /// 即时间比较，`range(start..)` 一趟扫完。
    fn week_stats_on(&self, week_start: Option<&str>) -> Option<(u64, u64)> {
        let start = week_start?;
        let mut plays = 0;
        let mut listen_ms = 0;
        for stat in self.days.range(start.to_string()..).map(|(_, stat)| stat) {
            plays += stat.plays;
            listen_ms += stat.listen_ms;
        }
        Some((plays, listen_ms))
    }

    /// 播放次数最多的歌手；并列时取名字序靠前的（稳定、可复现）。
    pub fn top_artist(&self) -> Option<&str> {
        self.artists
            .iter()
            .max_by_key(|(_, plays)| **plays)
            .map(|(artist, _)| artist.as_str())
    }
}

/// 「本周」的起点：今天往前数 6 天。算不出来时调用方放弃本次统计。
fn week_start_key() -> Option<String> {
    let today = time::OffsetDateTime::now_local().ok()?.date();
    let start = today.checked_sub(time::Duration::days(6))?;
    Some(format!(
        "{:04}-{:02}-{:02}",
        start.year(),
        u8::from(start.month()),
        start.day()
    ))
}

/// 收听时长的人类说法：一小时以上给一位小数的「小时」，以下给整数「分钟」。
fn format_listen(ms: u64) -> String {
    let minutes = ms / 60_000;
    if minutes >= 60 {
        format!("{:.1} 小时", minutes as f64 / 60.0)
    } else {
        format!("{minutes} 分钟")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(plays: u64, listen_ms: u64) -> DayStat {
        DayStat { plays, listen_ms }
    }

    /// 播完计数进当天，也进歌手榜；日期取不到时只丢当天计数，歌手榜照记。
    #[test]
    fn completions_count_into_the_day_and_the_artist_board() {
        let mut stats = ListeningStats::default();
        stats.record_complete_on(Some("2026-11-05".into()), "周杰伦");
        stats.record_complete_on(Some("2026-11-05".into()), "周杰伦");
        stats.record_complete_on(Some("2026-11-05".into()), "陈奕迅");
        // 空歌手（比如拿不到名字）不计入歌手榜，但当天次数照记
        stats.record_complete_on(Some("2026-11-05".into()), "");

        assert_eq!(stats.days["2026-11-05"].plays, 4);
        assert_eq!(stats.artists["周杰伦"], 2);
        assert_eq!(stats.artists.len(), 2);
    }

    /// 收听时长只进当天；零时长不记。
    #[test]
    fn listen_time_accumulates_into_the_day() {
        let mut stats = ListeningStats::default();
        stats.add_listen_on(Some("2026-11-05".into()), 30_000);
        stats.add_listen_on(Some("2026-11-05".into()), 45_000);
        stats.add_listen_on(Some("2026-11-05".into()), 0);
        assert_eq!(stats.days["2026-11-05"].listen_ms, 75_000);
        assert_eq!(stats.days.len(), 1);
    }

    /// 裁剪：天超限丢最旧的，歌手超限按播放次数留前 N 名。
    #[test]
    fn pruning_keeps_the_newest_days_and_the_top_artists() {
        let mut stats = ListeningStats::default();
        for index in 0..(DAY_LIMIT + 10) {
            let key = format!("20{:02}-01-{:02}", 20 + index / 360, index % 360 + 1);
            stats.days.insert(key, day(1, 0));
        }
        stats.prune();
        assert_eq!(stats.days.len(), DAY_LIMIT);
        // 丢的是字典序最旧的那批
        let newest = format!(
            "20{}-01-{:02}",
            20 + (DAY_LIMIT + 9) / 360,
            (DAY_LIMIT + 9) % 360 + 1
        );
        assert!(stats.days.contains_key(&newest));

        for index in 0..(ARTIST_LIMIT + 5) {
            stats.artists.insert(format!("歌手{index}"), 1);
        }
        stats.artists.insert("顶流".into(), 999);
        stats.prune();
        assert_eq!(stats.artists.len(), ARTIST_LIMIT);
        assert!(stats.artists.contains_key("顶流"), "播放最多的不能被裁掉");
    }

    /// 摘要行：本周有数据才出现，格式是「本周 N 首 · 时长 · 最常听 X」。
    #[test]
    fn summary_line_only_appears_with_data() {
        let mut stats = ListeningStats::default();
        assert_eq!(stats.summary_line_on(Some("2026-10-30")), None);

        // 全在「本周」窗口内
        stats.record_complete_on(Some("2026-11-05".into()), "周杰伦");
        stats.record_complete_on(Some("2026-11-04".into()), "周杰伦");
        stats.record_complete_on(Some("2026-11-04".into()), "陈奕迅");
        stats.add_listen_on(Some("2026-11-05".into()), 90 * 60_000);

        let line = stats
            .summary_line_on(Some("2026-10-30"))
            .expect("有数据应当有摘要");
        assert!(line.contains("本周 3 首"), "{line}");
        assert!(line.contains("1.5 小时"), "{line}");
        assert!(line.contains("最常听 周杰伦"), "{line}");

        // 一周前的不算进「本周」
        let mut old = ListeningStats::default();
        old.record_complete_on(Some("2026-10-01".into()), "老歌歌手");
        assert_eq!(
            old.summary_line_on(Some("2026-10-30")),
            None,
            "窗口外的记录不该出现在摘要里"
        );
    }

    /// serde 往返：字段一个不丢（落盘格式一旦写坏，用户的历史就没了）。
    #[test]
    fn stats_round_trip_through_serde() {
        let mut stats = ListeningStats::default();
        stats.record_complete_on(Some("2026-11-05".into()), "周杰伦");
        stats.add_listen_on(Some("2026-11-05".into()), 123_000);

        let text = serde_json::to_string(&stats).expect("序列化不该失败");
        let back: ListeningStats = serde_json::from_str(&text).expect("反序列化不该失败");
        assert_eq!(back.artists["周杰伦"], 1);
        assert_eq!(back.days["2026-11-05"].listen_ms, 123_000);
    }

    /// 老版本文件里缺字段（或空对象）也能解析——统计文件是长期数据，
    /// 不能因为结构演进一次就整体作废。
    #[test]
    fn legacy_stats_with_missing_fields_still_parse() {
        let stats: ListeningStats = serde_json::from_str("{}").expect("空对象应当可解析");
        assert_eq!(stats.days.len(), 0);
        assert_eq!(stats.artists.len(), 0);
    }
}
