//! 桌面集成：MPRIS、系统托盘、窗口控制、WebSocket 推送。
//!
//! 从 `update.rs` 里切出来的第四块（见 `docs/MAINTENANCE.md` §1.8）。这里的代码
//! **不碰播放、不碰网络、不改业务状态**：它只把 `AppState` 里的东西翻译成桌面
//! 组件要的快照，或者发一条 compositor IPC。
//!
//! 三个 `sync_*` 由 `update.rs` 的 `tick()` 每拍调一次。其中 `sync_mpris` 与
//! `sync_tray` **只在 Unix 上存在**（MPRIS 与 StatusNotifierItem 都是 D-Bus
//! 接口，Windows 上没有 session bus，也没有认这两个接口的宿主）；`sync_ws` 跨
//! 平台（WebSocket 只依赖 tokio 与 rustls）。`toggle_window` 两个平台都编，
//! 只是不在 niri 下会回一句「不支持」。
//!
//! 失败一律只提示、不影响播放：窗口操作不成功不该让音乐停下来，桌面组件连不上
//! 也只是没有集成。

use crate::app::App;
use crate::audio::engine::PlaybackState;

/// 桌面组件（MPRIS）用的封面像素尺寸。控件显示得不大，没必要拉原图。
#[cfg(unix)]
const MPRIS_COVER_SIZE: u32 = 400;

impl App {
    /// 把当前播放信息推给 MPRIS，供桌面组件显示。
    ///
    /// 每次 tick 调一次。开销就是一次互斥锁写入，可忽略；桌面组件的轮询
    /// 频率远低于此，没必要更高频。
    #[cfg(unix)]
    pub(super) fn sync_mpris(&mut self) {
        // D-Bus 注册是异步的，尚未成功（或压根没有 session bus）时没必要每帧
        // 构造一份快照——那只是白白做几次字符串克隆。
        let Some(handle) = self.mpris.as_ref().filter(|handle| handle.is_connected()) else {
            return;
        };

        let info = match self.state.current.as_ref() {
            Some(song) => crate::mpris::TrackInfo {
                title: song.name.clone(),
                artists: song
                    .singers
                    .iter()
                    .map(|singer| singer.name.clone())
                    .collect(),
                album: song.album_name.clone(),
                // 在这里展开 {size}：MprisSnapshot 只存最终可用的地址，
                // 展开规则收敛到 Song::cover_url，避免各调用点各写一份
                art_url: song.cover_url(MPRIS_COVER_SIZE),
                // 曲目标识必须随歌走：客户端靠 `mpris:trackid` 判断「换歌了没有」，
                // 给固定值会让标题与封面停在上一首（见 `mpris::TrackInfo::track_id`）。
                track_id: song.hash.clone(),
                position_us: (self.state.position_ms as i64) * 1_000,
                duration_us: (self.state.duration_ms as i64) * 1_000,
                status: self.state.playback,
            },
            None => crate::mpris::TrackInfo {
                status: self.state.playback,
                ..Default::default()
            },
        };

        handle.update(info);
    }

    /// 把当前播放信息推给系统托盘，让 ToolTip 和状态图标跟着变。
    ///
    /// 设计取舍和 [`Self::sync_mpris`] 一致：只把数据写到快照里，DBus 的属性刷新
    /// 和 `New*` 信号由托盘线程自己轮询+发，避免反向调用主线程。
    #[cfg(unix)]
    pub(super) fn sync_tray(&mut self) {
        // 没注册成功时跳过——还没连上 watcher 的进程每帧构造一次快照是白干。
        let Some(handle) = self.tray.as_ref().filter(|handle| handle.is_connected()) else {
            return;
        };

        let muted = self.state.is_muted();
        let info = match self.state.current.as_ref() {
            Some(song) => crate::tray::TrayInfo {
                title: song.name.clone(),
                artists: song
                    .singers
                    .iter()
                    .map(|singer| singer.name.clone())
                    .collect(),
                status: self.state.playback,
                muted,
            },
            None => crate::tray::TrayInfo {
                status: self.state.playback,
                muted,
                ..Default::default()
            },
        };

        handle.update(info);
    }

    /// 把当前播放信息推给 WebSocket 客户端（桌面歌词 / 状态栏 / 遥控器）。
    ///
    /// 每次 tick 调一次，和 [`Self::sync_mpris`] 同一套路：这里只写一份快照，
    /// 真正的 JSON 序列化与广播在 `ws.rs` 的任务里做。快照内容没变时
    /// [`crate::ws::WsHandle::update`] 不做任何唤醒，所以暂停时这条调用近乎免费。
    pub(super) fn sync_ws(&mut self) {
        let Some(handle) = self.ws.as_ref().filter(|handle| handle.is_running()) else {
            return;
        };

        handle.update(crate::ws::Snapshot {
            song: self.state.current.clone(),
            // 没有歌词（还没取到 / 这首歌本来就没有）时为 `None`，此时只推播放状态。
            lyric_text: Some(self.state.lyric.lyric.text.clone()).filter(|text| !text.is_empty()),
            is_playing: self.state.playback == PlaybackState::Playing,
            position_ms: self.state.position_ms,
            duration_ms: self.state.duration_ms,
        });
    }

    /// 切换窗口的最小化状态（仅 niri，目前由托盘菜单触发）。
    ///
    /// 把 TUI 从平铺布局里收起来但**音乐照常播**——收起来之后键盘就够不着了，
    /// 这时托盘菜单与 MPRIS 是唯一的控制入口。
    ///
    /// 失败只提示一句 + 记日志：窗口操作不成功不该影响播放。
    pub(super) fn toggle_window(&mut self) {
        if !crate::window::available() {
            self.state.warn("当前环境不支持窗口控制（需要 niri）");
            return;
        }
        if crate::window::toggle_minimized() {
            self.state.info("已切换窗口最小化");
        } else {
            self.state.warn("窗口操作失败（详见日志）");
        }
    }
}
