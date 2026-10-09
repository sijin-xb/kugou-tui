//! 退出时把「正在听什么」存下来，下次启动原样恢复。
//!
//! 关掉终端再打开，队列和进度都不该丢——这是播放器该有的基本行为。
//! 存在 `~/.cache/kugou-tui/session.json`，和配置文件分开：配置是用户手改的
//! 长期设置，会话是程序自己写的瞬时状态，混在一起会互相干扰（比如用户改了
//! 配置却被程序覆写）。

use serde::{Deserialize, Serialize};

use crate::api::model::Song;

/// 一个可恢复的播放会话。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    /// 队列里的全部歌曲，按顺序。
    pub queue: Vec<Song>,
    /// 队列游标：当前停在哪个位置。
    pub cursor: Option<usize>,
    /// 当前这首歌播到几毫秒。
    pub position_ms: u64,
    /// 上一次领取「概念版」当天 VIP 的日期（`2026-09-23`）。
    ///
    /// 存下来是为了**每天只领一次**：上游文档明确写着「尽量别频繁调用」，
    /// 而这个接口还带风控。没有它的话，一天里开几次程序就打几次。
    ///
    /// `#[serde(default)]` 是必须的：老版本写下的 `session.json` 里没有这个字段，
    /// 不给默认值会让整个文件解析失败，用户会莫名其妙丢掉一次「上次听到哪」。
    #[serde(default)]
    pub vip_claimed_day: Option<String>,
}

impl Session {
    /// 会话文件的位置。
    pub fn path() -> std::path::PathBuf {
        crate::config::default_cache_dir().join("session.json")
    }

    /// 读上次会话。文件不存在、损坏、字段缺失都当作「没有上次」——会话丢了
    /// 只是少恢复一次，不该让程序起不来。
    pub fn load() -> Option<Self> {
        let text = std::fs::read_to_string(Self::path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// 写会话。失败只记日志：存不下会话不该影响退出。
    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            crate::logger::tlog!(crate::logger::LEVEL_WARN, "创建会话目录失败：{error}");
            return;
        }
        match serde_json::to_string(self) {
            Ok(text) => {
                if let Err(error) = std::fs::write(&path, text) {
                    crate::logger::tlog!(crate::logger::LEVEL_WARN, "保存会话失败：{error}");
                }
            }
            Err(error) => {
                crate::logger::tlog!(crate::logger::LEVEL_WARN, "序列化会话失败：{error}")
            }
        }
    }

    /// 这个会话有没有值得恢复的东西。
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 老版本写下的 `session.json` 里**没有** `vip_claimed_day`。
    ///
    /// 加了新字段之后它必须还能解析——否则用户升级一次就莫名其妙丢掉「上次听到
    /// 哪」，而且看不出原因。这条就是钉住那个 `#[serde(default)]`。
    #[test]
    fn legacy_session_without_the_vip_field_still_parses() {
        let legacy = r#"{"queue":[],"cursor":null,"position_ms":0}"#;
        let session: Session = serde_json::from_str(legacy).expect("老会话文件应当仍可解析");
        assert_eq!(session.vip_claimed_day, None);
        assert_eq!(session.position_ms, 0);
    }

    /// 领取日期能存能读——「每天只领一次」全靠它。
    #[test]
    fn vip_claimed_day_round_trips() {
        let session = Session {
            vip_claimed_day: Some("2026-09-23".to_string()),
            ..Default::default()
        };
        let text = serde_json::to_string(&session).expect("序列化不该失败");
        let back: Session = serde_json::from_str(&text).expect("反序列化不该失败");
        assert_eq!(back.vip_claimed_day.as_deref(), Some("2026-09-23"));
    }

    /// 老版本写下的队列条目**没有** `source`。升级后必须仍能解析，且缺字段的
    /// 歌回落为默认音源（酷狗）——绝不能因为旧文件缺字段而丢队列或 panic。
    #[test]
    fn legacy_session_queue_entries_default_to_kugou() {
        let song = Song {
            name: "老歌".to_string(),
            hash: "abc".to_string(),
            source: crate::source::SourceKind::Netease,
            ..Song::default()
        };
        let mut value = serde_json::to_value(Session {
            queue: vec![song],
            cursor: Some(0),
            position_ms: 1234,
            vip_claimed_day: None,
        })
        .expect("序列化不该失败");

        // 模拟老版本写下的文件：队列条目没有 `source`，顶层也没有 `vip_claimed_day`
        let object = value.as_object_mut().expect("会话是对象");
        object.remove("vip_claimed_day");
        for entry in object
            .get_mut("queue")
            .and_then(|queue| queue.as_array_mut())
            .expect("queue 是数组")
        {
            entry.as_object_mut().expect("歌曲是对象").remove("source");
        }

        let session: Session = serde_json::from_value(value).expect("旧队列应仍可解析");
        assert_eq!(session.queue.len(), 1);
        assert_eq!(session.queue[0].name, "老歌");
        assert_eq!(session.queue[0].source, crate::source::SourceKind::Kugou);
        assert_eq!(session.vip_claimed_day, None);
    }
}
