//! 系统托盘（KDE/MATE 风格的 StatusNotifierItem）。
//!
//! 把播放器注册成 DBus 上的 `org.kde.StatusNotifierItem`，让 waybar / Quickshell /
//! KDE 之类能识别并显示它。全部交互最终都折算成 [`Action`] 投进事件总线，等同按键：
//!
//! * **滚轮**（垂直）调音量、（水平）快进 / 快退——托盘图标上不用展开菜单就能操作；
//! * **右键菜单**（DBusMenu）：播放 / 暂停（措辞跟着状态走）、上一首 / 下一首、
//!   静音、退出；跑在 niri 下时多一项「最小化 / 显示窗口」；
//! * **中键**（KDE 走 `SecondaryActivate`）与部分宿主的 `ContextMenu` → 播放 / 暂停。
//!
//! # 图标跟随播放状态
//!
//! 播放 / 加载时用正常的音符图标；暂停 / 停止时换**调暗**的同一张图（只压 alpha，
//! 颜色不变），并广播 `NewIcon`。托盘上「还在不在放」不用点开就知道。
//!
//! # 自适配：三层降级，每层都静默跳过、不影响播放
//!
//! 1. 没有图形会话（无 `WAYLAND_DISPLAY` 也无 `DISPLAY`）→ 完全不连 DBus；
//! 2. session bus 不可用（纯 tty、容器里没挂 bus）→ zbus 报 Err，记 WARN；
//! 3. 没有 `StatusNotifierWatcher`（非 KDE/Quickshell 桌面）→
//!    注册失败，**每 5 秒重试一次**而不是放弃——面板常常比播放器后启动。
//!
//! 后两种情况 `TrayHandle::is_connected()` 返回 false，主循环跳过同步；
//! 一旦 watcher 出现并注册成功，它会翻回 true，托盘自动出现。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zbus::connection::Builder as ConnectionBuilder;
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Structure, Value};

use crate::audio::engine::PlaybackState;
use crate::event::{Event, EventBus};
use crate::keymap::Action;

/// 嵌入的图标（256×256 RGBA，源 SVG 在 `assets/tray.svg`）。运行时再缩放到目标尺寸。
const ICON_PNG: &[u8] = include_bytes!("../assets/tray-icon.png");

/// 提供给宿主挑选的多尺寸图标。源图 256px，往下的每一档都从它重采样。
///
/// 覆盖三档主流托盘的常用值：16 / 22 / 24（KDE「最小托盘尺寸」与 waybar 常见的
/// 22、GNOME 扩展偏好的 16）、32 / 48（普通与高分屏面板）、64（Quickshell 高分屏）。
/// 多给只是启动期多几次重采样，宿主挑一个合适的，剩下的忽略；
/// 少给尺寸会让某些宿主拿最接近的一档硬放大，糊成一团。
const ICON_SIZES: [u32; 6] = [16, 22, 24, 32, 48, 64];

/// 暂停 / 停止时图标整体压暗到这个不透明度。
///
/// 取 0.45：在深浅面板上都看得出「变灰了」，又没暗到像程序退出了。
const DIM_ALPHA: f32 = 0.45;

/// 一张图标位图：`(宽, 高, ARGB32 像素)`，对应 SNI 的 `a(iiay)`。
///
/// 抽成别名是因为同一串类型在属性签名里要写四遍（clippy 的
/// `type_complexity` 会拦），更重要的是：有名字之后它读起来像「一张图」，
/// 而不像「一个恰好有三个元素的元组」。
type Pixmap = (i32, i32, Vec<u8>);

/// 本进程内已注册的托盘项个数，给 bus name 编实例号用（见 `spawn`）。
static INSTANCE: AtomicU32 = AtomicU32::new(1);

/// DBusMenu 的一个布局节点：`(id, 属性, 子节点)`，对应规范里的 `(ia{sv}av)`。
type MenuNode = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

/// `GetLayout` 的返回：`(revision, 节点)`。别名是为了让 clippy 的
/// `type_complexity` 闭嘴，顺带让签名读起来像「一份菜单」而不是一堆括号。
type MenuLayout = (u32, MenuNode);

const OBJECT_PATH: &str = "/StatusNotifierItem";
const WATCHER_DEST: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";

/// 推给托盘线程的状态快照——比 MPRIS 那个更小，只装播放状态和当前曲目。
///
/// 由主线程写入、托盘线程读取，所以套 `Mutex`；锁持有时间不超过一次字段拷贝，
/// 竞争可忽略。
#[derive(Debug, Clone, Default)]
pub struct TrayInfo {
    pub title: String,
    pub artists: Vec<String>,
    pub status: PlaybackState,
    /// 是否静音。托盘菜单的「静音 / 取消静音」措辞跟着它走。
    pub muted: bool,
    /// 当前曲目的标识（酷狗的 hash）。只用来判断「换歌了没有」——换歌才需要
    /// 重建 `title` / `artists`，其余每拍只刷新 `status` / `muted`。
    pub track_id: String,
}

/// 主循环持有它，每帧调 [`Self::update_track`] 刷一次。
#[derive(Clone)]
pub struct TrayHandle {
    info: Arc<Mutex<TrayInfo>>,
    /// 注册是否走到了 watcher 注册那一步。`spawn` 失败时为 false。
    connected: Arc<AtomicBool>,
}

impl TrayHandle {
    /// 刷新快照：`status` / `muted` 每拍都写，元数据只在换歌时重建。
    ///
    /// `build` 里是歌名 / 歌手名的克隆，只在 `track_id` 变化时才需要重算。
    pub fn update_track(
        &self,
        track_id: &str,
        status: PlaybackState,
        muted: bool,
        build: impl FnOnce() -> TrayInfo,
    ) {
        let Ok(mut guard) = self.info.lock() else {
            return;
        };
        if guard.track_id != track_id {
            let mut info = build();
            info.status = status;
            info.muted = muted;
            info.track_id = track_id.to_string();
            *guard = info;
        } else {
            guard.status = status;
            guard.muted = muted;
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

/// SNI 接口实现。
struct Item {
    info: Arc<Mutex<TrayInfo>>,
    bus: EventBus,
    /// 启动时一次性预渲染的多尺寸图标（播放 / 加载态）。`IconPixmap` 属性直接返回它，
    /// 零拷贝是不可能的（`Vec<u8>` 得克隆出去给 zbus），但启动期以外不会改。
    pixmaps_bright: Vec<Pixmap>,
    /// 同一套图的调暗版（暂停 / 停止态）：只压 alpha，颜色不变。
    pixmaps_dim: Vec<Pixmap>,
    /// ToolTip 用的单个图标。规范 ToolTip 只放一份图标，用最大那档看着最清楚。
    tooltip_pixmap: Vec<Pixmap>,
}

impl Item {
    /// 拿快照。锁中毒时取 inner——信号循环绝不能因为别处 panic 就退出。
    fn with_info<R>(&self, f: impl FnOnce(&TrayInfo) -> R) -> R {
        match self.info.lock() {
            Ok(guard) => f(&guard),
            Err(poisoned) => f(&poisoned.into_inner()),
        }
    }

    /// 当前该用亮图标还是暗图标。
    fn icon_is_dim(&self) -> bool {
        self.with_info(|info| icon_should_dim(&info.status))
    }
}

/// 暂停 / 停止时托盘图标换调暗版。
///
/// `Loading` 算「即将出声」，跟播放一样用亮图——缓冲的那两秒图标灰掉，
/// 看起来反而像程序卡住。
fn icon_should_dim(state: &PlaybackState) -> bool {
    matches!(state, PlaybackState::Paused | PlaybackState::Stopped)
}

/// 把一整套图标压暗：RGB 原样，alpha 乘 [`DIM_ALPHA`]。
fn dim_pixmaps(pixmaps: &[Pixmap]) -> Vec<Pixmap> {
    pixmaps
        .iter()
        .map(|(width, height, bytes)| {
            let mut bytes = bytes.clone();
            for chunk in bytes.as_chunks_mut::<4>().0 {
                chunk[3] = (f32::from(chunk[3]) * DIM_ALPHA).round() as u8;
            }
            (*width, *height, bytes)
        })
        .collect()
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    /// 应用在系统里的稳定标识。规范说"重启之间要保持不变"，但只用来去重显示
    /// ——kugou-tui 同一时刻只会有一份，连重启也是同一个名字，刚好。
    #[zbus(property)]
    async fn id(&self) -> String {
        "kugou-tui".to_string()
    }

    /// 类别。播放器属于「应用状态」——Quickshell 会据此把它和其他托盘项分开渲染。
    #[zbus(property)]
    async fn category(&self) -> String {
        "ApplicationStatus".to_string()
    }

    /// 是否需要用户注意。规范里 `Active`/`Passive`/`NeedsAttention` 三档——
    /// 播放中用 Active，宿主可能加一点高亮；其它情况 Passive，不打扰。
    #[zbus(property)]
    async fn status(&self) -> String {
        let active = self.with_info(|info| info.status == PlaybackState::Playing);
        if active { "Active" } else { "Passive" }.to_string()
    }

    /// 应用名。鼠标悬停与右键菜单可能用到，固定返回。
    #[zbus(property)]
    async fn title(&self) -> String {
        "kugou-tui".to_string()
    }

    // --- 图标 ---
    //
    // 规范要求同名 IconName + IconPixmap 都返回；IconName 留空表示「不用主题图标，
    // 看 Pixmap」。这样不依赖任何系统图标主题——apt/Flatpak/容器里都最稳。

    #[zbus(property)]
    async fn icon_name(&self) -> String {
        String::new()
    }

    /// 图标跟着播放状态走：播放 / 加载亮，暂停 / 停止暗。
    #[zbus(property)]
    async fn icon_pixmap(&self) -> Vec<Pixmap> {
        if self.icon_is_dim() {
            self.pixmaps_dim.clone()
        } else {
            self.pixmaps_bright.clone()
        }
    }

    // 下面三组属性规范列了但我们用不到——全部返回空，避免宿主读到奇怪默认值。

    #[zbus(property)]
    async fn attention_icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    async fn attention_icon_pixmap(&self) -> Vec<Pixmap> {
        Vec::new()
    }

    #[zbus(property)]
    async fn attention_movie_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    async fn overlay_icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    async fn overlay_icon_pixmap(&self) -> Vec<Pixmap> {
        Vec::new()
    }

    /// 菜单路径，指向本连接上的 [`MENU_PATH`]。
    ///
    /// **不能返回 `/`**。虽然 SNI 规范说 `/` 表示无菜单，但实测 Quickshell 会据此
    /// 把右键整个跳过——想要右键有反应，这里就必须是个真实路径。
    #[zbus(property)]
    async fn menu(&self) -> zbus::zvariant::OwnedObjectPath {
        zbus::zvariant::OwnedObjectPath::try_from(MENU_PATH).expect("菜单路径是合法 DBus 路径")
    }

    #[zbus(property)]
    async fn item_is_menu(&self) -> bool {
        false
    }

    /// ToolTip 是个 4 元组，对应 SNI 规范的 `(sa(iiay)ss)`：
    /// `(icon_name, icon_pixmap, title, description)`。
    ///
    /// zvariant 给元组实现了 `Type`（最多 12 个），只要每个元素都实现了——这里全是
    /// 标准类型，没有自定义结构。
    #[zbus(property)]
    async fn tool_tip(&self) -> (String, Vec<Pixmap>, String, String) {
        let description = self.with_info(|info| format_tooltip(&info.title, &info.artists));
        (
            String::new(),
            self.tooltip_pixmap.clone(),
            "kugou-tui".to_string(),
            description,
        )
    }

    // --- 鼠标 / 触控事件 ---

    /// 左键点击。用户偏好「什么都不做」，按约定保留空实现。
    async fn activate(&self, _x: i32, _y: i32) {}

    /// 中键（在 KDE 里）和右键（在其它宿主里）的统一入口。
    async fn secondary_activate(&self, _x: i32, _y: i32) {
        self.bus.send(Event::Action(Action::PlayPause));
    }

    /// 右键显式菜单调用——没有菜单的话也要响应，否则部分宿主会以为是「菜单坏了」。
    async fn context_menu(&self, _x: i32, _y: i32) {
        self.bus.send(Event::Action(Action::PlayPause));
    }

    /// 滚轮：垂直调音量，水平快进 / 快退。只看符号——宿主给的 delta 绝对值
    /// 含义不一（有的 ±1，有的 ±120），按格数缩放只会时大时小。
    async fn scroll(&self, delta: i32, direction: &str) {
        let action = match (direction, delta) {
            ("vertical", d) if d > 0 => Action::VolumeUp,
            ("vertical", d) if d < 0 => Action::VolumeDown,
            ("horizontal", d) if d > 0 => Action::SeekForward,
            ("horizontal", d) if d < 0 => Action::SeekBackward,
            _ => return, // 斜向滚轮与零位移不做任何事
        };
        self.bus.send(Event::Action(action));
    }

    async fn open(&self, _uri: &str) {}

    // --- 信号 ---
    //
    // `#[zbus(property)]` 自动生成的 `*_changed` 走的是标准
    // `org.freedesktop.DBus.Properties.PropertiesChanged`。KDE 状态栏实际监听的是
    // `org.kde.StatusNotifierItem.New*` 这一组**自定义信号**——所以得手动声明，
    // 名字才会按方法名首字母大写映射到 `NewIcon` / `NewStatus` / `NewTitle` /
    // `NewToolTip` 等。
    #[zbus(signal)]
    async fn new_icon(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_status(ctxt: &SignalEmitter<'_>, status: String) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_title(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_tool_tip(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_attention_icon(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_overlay_icon(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// 右键菜单的对象路径。宿主从 `Menu` 属性拿到它，再来连 `com.canonical.dbusmenu`。
///
/// **必须给一个真实路径**：Quickshell（至少 end4 那份配置）右键前先判 `hasMenu`，
/// 而那个值就是从 `Menu` 属性来的——返回 `/`（SNI 里「无菜单」的写法）时右键被
/// 整个跳过。这正是「点了没反应」的直接原因。
const MENU_PATH: &str = "/StatusNotifierItem/menu";

/// 菜单项 id。固定常量而不是散落的字面量：`entry_label` 与信号循环都按 id
/// 找「播放 / 暂停」「静音」这两项动态标签，写错一个数字就是菜单点错动作。
const ID_PLAY_PAUSE: i32 = 1;
const ID_PREV: i32 = 2;
const ID_NEXT: i32 = 3;
const ID_SEPARATOR: i32 = 4;
const ID_MUTE: i32 = 5;
const ID_WINDOW: i32 = 6;
/// 「退出」的 id 取决于有没有窗口项：两项都要出现，id 就得接在不同位置——
/// 重复 id 会让宿主的菜单项互相覆盖。
const ID_QUIT_AFTER_WINDOW: i32 = 7;
const ID_QUIT: i32 = 6;

/// 一条菜单项：`(id, 静态文案, 点击后派发的动作)`。
///
/// `action` 为 `None` 表示**分隔线**（属性 `type = separator`），不派发任何动作。
/// 静态文案只是兜底：id [`ID_PLAY_PAUSE`] 与 [`ID_MUTE`] 的实际显示由
/// [`entry_label`] 按当前播放状态算（播放↔暂停、静音↔取消静音）。
type MenuEntry = (i32, &'static str, Option<Action>);

/// 菜单项。
///
/// `window_control` 为假（当前会话不是 niri）时**「最小化 / 显示窗口」根本不出现**：
/// 一个点了没反应的菜单项比没有它更糟。「退出」用 [`Action::Quit`] 走正常退出
/// 流程（先保存配置与播放进度），与按 `q` 等价。
fn menu_entries(window_control: bool) -> Vec<MenuEntry> {
    let mut entries: Vec<MenuEntry> = vec![
        (ID_PLAY_PAUSE, "播放 / 暂停", Some(Action::PlayPause)),
        (ID_PREV, "上一首", Some(Action::Prev)),
        (ID_NEXT, "下一首", Some(Action::Next)),
        (ID_SEPARATOR, "", None),
        (ID_MUTE, "静音", Some(Action::ToggleMute)),
    ];
    if window_control {
        entries.push((ID_WINDOW, "最小化 / 显示窗口", Some(Action::ToggleWindow)));
        entries.push((ID_QUIT_AFTER_WINDOW, "退出", Some(Action::Quit)));
    } else {
        entries.push((ID_QUIT, "退出", Some(Action::Quit)));
    }
    entries
}

/// 一条菜单项**实际显示**的标签。
///
/// 「播放 / 暂停」「静音」跟着状态切换措辞，其余原样返回静态文案。
fn entry_label(entry: &MenuEntry, info: &TrayInfo) -> String {
    match entry.0 {
        ID_PLAY_PAUSE => {
            if info.status == PlaybackState::Playing {
                "暂停"
            } else {
                "播放"
            }
        }
        ID_MUTE => {
            if info.muted {
                "取消静音"
            } else {
                "静音"
            }
        }
        _ => entry.1,
    }
    .to_string()
}

/// 只有这两个 id 的标签是动态的。信号循环广播 `ItemsPropertiesUpdated` 时按它过滤，
/// 状态没变就不打扰宿主。
const DYNAMIC_LABEL_IDS: [i32; 2] = [ID_PLAY_PAUSE, ID_MUTE];

struct Menu {
    bus: EventBus,
    entries: Vec<MenuEntry>,
    /// 与 [`Item`] 共享的状态快照：动态标签从这里读。
    info: Arc<Mutex<TrayInfo>>,
    /// 布局版本号。标签变化时 +1 并广播 `LayoutUpdated`，宿主才知道要重拉。
    revision: Arc<AtomicU32>,
}

impl Menu {
    fn find(&self, id: i32) -> Option<&MenuEntry> {
        self.entries.iter().find(|entry| entry.0 == id)
    }

    /// 拿快照。与 `Item::with_info` 同一套锁中毒策略。
    fn with_info<R>(&self, f: impl FnOnce(&TrayInfo) -> R) -> R {
        match self.info.lock() {
            Ok(guard) => f(&guard),
            Err(poisoned) => f(&poisoned.into_inner()),
        }
    }
}

#[interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[zbus(property)]
    async fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    async fn text_direction(&self) -> String {
        "ltr".to_string()
    }

    #[zbus(property)]
    async fn status(&self) -> String {
        "normal".to_string()
    }

    #[zbus(property)]
    async fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    /// 整棵树一次给全：`parent_id = 0` 要全部菜单项，其余 id 没有子菜单。
    async fn get_layout(
        &self,
        parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> MenuLayout {
        let children: Vec<OwnedValue> = if parent_id == 0 {
            self.with_info(|info| {
                self.entries
                    .iter()
                    .map(|entry| layout_node(entry, info))
                    .collect()
            })
        } else {
            Vec::new()
        };
        // revision 跟着动态标签走：状态没变时它不动，宿主取一次就够；
        // 变了信号循环会 +1 并广播 `LayoutUpdated`。
        (
            self.revision.load(Ordering::Relaxed),
            (parent_id, HashMap::new(), children),
        )
    }

    async fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        self.with_info(|info| {
            ids.into_iter()
                .filter_map(|id| {
                    self.find(id)
                        .map(|entry| (entry.0, item_properties(entry, info)))
                })
                .collect()
        })
    }

    async fn get_property(&self, id: i32, name: String) -> OwnedValue {
        self.with_info(|info| {
            self.find(id)
                .and_then(|entry| item_properties(entry, info).remove(&name))
                .unwrap_or_else(|| owned_str(""))
        })
    }

    /// 点击。宿主只发 `clicked` 这一个事件 id，其余（`opened` / `closed` 等）忽略。
    /// 分隔线（`action` 为 `None`）点不出事件，天然落在 `find` 的 `None` 分支里。
    async fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }
        if let Some((_, _, Some(action))) = self.find(id) {
            self.bus.send(Event::Action(*action));
        }
    }

    async fn about_to_show(&self, _id: i32) -> bool {
        false // 不需要宿主再重新拉一次布局
    }

    async fn about_to_show_group(&self, ids: Vec<i32>) -> Vec<(i32, bool)> {
        ids.into_iter().map(|id| (id, false)).collect()
    }

    #[zbus(signal)]
    async fn layout_updated(
        ctxt: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn items_properties_updated(
        ctxt: &SignalEmitter<'_>,
        updated: Vec<(i32, HashMap<String, OwnedValue>)>,
        removed: Vec<(i32, Vec<String>)>,
    ) -> zbus::Result<()>;
}

/// 一条菜单项的属性。`label` / `enabled` / `visible` 缺一个，多数宿主就不显示它；
/// 分隔线按规范只认 `type = separator`，宿主画横线、忽略其余属性。
fn item_properties(entry: &MenuEntry, info: &TrayInfo) -> HashMap<String, OwnedValue> {
    let mut map = HashMap::new();
    if entry.2.is_none() {
        map.insert("type".to_string(), owned_str("separator"));
        map.insert("enabled".to_string(), owned_bool(false));
    } else {
        map.insert("label".to_string(), owned_str(&entry_label(entry, info)));
        map.insert("enabled".to_string(), owned_bool(true));
    }
    map.insert("visible".to_string(), owned_bool(true));
    map
}

/// 包成 DBusMenu 的布局节点 `(ia{sv}av)`——没有子菜单，第三项给空数组。
///
/// 必须手工拼 `Value::Structure`：zvariant 只给**固定几个**元组实现了 `Type`，
/// `(i32, a{sv}, av)` 这种嵌套组合不在其中，直接 `try_from` 元组会编译不过。
fn layout_node(entry: &MenuEntry, info: &TrayInfo) -> OwnedValue {
    // 字段必须给**具体类型**。写成 `Value::from(id)` 之类的话，每个字段自己又是个
    // variant，整个节点会变成 `(vvv)` 而不是规范要的 `(ia{sv}av)`——宿主按规范去解析
    // 会拿到对不上的类型，实测 Quickshell 直接崩（不是报错，是进程挂掉）。
    let structure = Structure::from((
        entry.0,
        item_properties(entry, info),
        Vec::<OwnedValue>::new(),
    ));
    OwnedValue::try_from(Value::Structure(structure)).unwrap_or_else(|_| owned_str(""))
}

fn owned_str(value: &str) -> OwnedValue {
    OwnedValue::from(zbus::zvariant::Str::from(value))
}

fn owned_bool(value: bool) -> OwnedValue {
    OwnedValue::from(value)
}

/// 启动托盘服务。
///
/// 失败（没图形会话 / 没 bus / 没 watcher）→ 返回 `None`、记一条 WARN 日志，
/// 不阻塞播放。和 mpris.rs 一样的取舍：桌面集成是「有更好」，不是「没不行」。
pub fn spawn(bus: EventBus) -> Option<TrayHandle> {
    if !environment_supports_tray() {
        crate::logger::tlog!(
            crate::logger::LEVEL_INFO,
            "无图形会话（WAYLAND/DISPLAY 未设置），跳过系统托盘"
        );
        return None;
    }

    // 解码 + 多尺寸预渲染。失败一般是文件本身坏了或 image 缺 feature——
    // 任何一种都说明构建出了问题，托盘没图标也不好用，直接放弃。
    let pixmaps = match build_pixmaps() {
        Ok(pixmaps) => pixmaps,
        Err(error) => {
            crate::logger::tlog!(
                crate::logger::LEVEL_WARN,
                "托盘图标解码失败，跳过系统托盘：{error}"
            );
            return None;
        }
    };

    // ToolTip 的图标从这堆里挑最大的那一份，规范只放一份——外面套个 `Vec` 凑齐
    // SNI 规范的 `a(iiay)` 形状。`unwrap_or_default` 退化成空数组，宿主极少见。
    let tooltip_pixmap = pixmaps
        .iter()
        .max_by_key(|(width, _, _)| *width)
        .cloned()
        .map(|entry| vec![entry])
        .unwrap_or_default();

    let info = Arc::new(Mutex::new(TrayInfo::default()));
    let connected = Arc::new(AtomicBool::new(false));
    let revision = Arc::new(AtomicU32::new(0));
    let info_thread = Arc::clone(&info);
    let connected_thread = Arc::clone(&connected);
    let revision_thread = Arc::clone(&revision);
    let pid = std::process::id();
    // 规范给的 bus name 形状是 `...-<pid>-<n>`。实例号不是摆设：同一进程里注册
    // 第二份（测试并发跑、或将来真的有多个托盘项）时，少了它第二个会撞名、
    // 注册静默失败——表现是「明明 spawn 了，托盘就是不出现」。
    let instance = INSTANCE.fetch_add(1, Ordering::Relaxed);
    let bus_name = format!("org.kde.StatusNotifierItem-{pid}-{instance}");

    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => return,
        };

        runtime.block_on(async move {
            // 断了就重连（与 mpris.rs 同一套路）：session bus 重启或总线名被别的实例
            // 抢走之后，旧连接上的信号会一直失败，而 `connected` 仍是 true，托盘就
            // 永远停在最后一帧。只有重建连接才能真的恢复。
            loop {
                // 菜单和托盘项共用一条事件总线：点菜单项要能把动作投进主循环
                let entries = menu_entries(crate::window::available());
                let menu = Menu {
                    bus: bus.clone(),
                    entries: entries.clone(),
                    info: Arc::clone(&info_thread),
                    revision: Arc::clone(&revision_thread),
                };

                let item = Item {
                    info: Arc::clone(&info_thread),
                    bus: bus.clone(),
                    pixmaps_bright: pixmaps.clone(),
                    pixmaps_dim: dim_pixmaps(&pixmaps),
                    tooltip_pixmap: tooltip_pixmap.clone(),
                };

                let result: zbus::Result<()> = async {
                    let connection = ConnectionBuilder::session()?
                        .name(bus_name.as_str())?
                        .serve_at(OBJECT_PATH, item)?
                        .serve_at(MENU_PATH, menu)?
                        .build()
                        .await?;

                    // watcher 代理是惰性的：此刻面板没起也不会失败，真正的探测在下面的循环里。
                    let watcher =
                        zbus::Proxy::new(&connection, WATCHER_DEST, WATCHER_PATH, WATCHER_IFACE)
                            .await?;

                    let item_iface = connection
                        .object_server()
                        .interface::<_, Item>(OBJECT_PATH)
                        .await?;
                    let menu_iface = connection
                        .object_server()
                        .interface::<_, Menu>(MENU_PATH)
                        .await?;

                    let mut last_status = String::new();
                    let mut last_tooltip = String::new();
                    let mut last_icon_dim = false;
                    // 动态菜单标签跟随的两个开关：（在播放, 已静音）
                    let mut last_menu_state = (false, false);
                    // 注册状态只记录**翻转**：面板没起的 5 秒一轮重试不刷日志
                    let mut was_connected = false;
                    // 连续发送失败计数。单次失败可能只是对端一时忙，连着几次还发不出去
                    // 就不是偶然了——与 mpris.rs 同一判据，够 3 次就交回外层重建连接。
                    let mut failures = 0u32;
                    // 自愈节拍：每 HEAL_EVERY 拍（0.5s × 10 = 5s）对一次账。第 1 拍立即注册，
                    // 正常启动时托盘不比原来慢。
                    const HEAL_EVERY: u64 = 10;
                    let mut ticks: u64 = 0;

                    loop {
                        ticks += 1;

                        // ---- 注册与自愈 ----
                        //
                        // 三种情况都要（重）注册：启动时面板还没起；面板重启把 watcher
                        // 连带换了一轮；watcher 无声地把我们丢了。判据是
                        // `RegisteredStatusNotifierItems` 里还有没有自己的 bus name——
                        // 已在列表里就不再调 `RegisterStatusNotifierItem`，重复注册是
                        // 多余调用，还可能让 watcher 发重复信号。
                        if ticks % HEAL_EVERY == 1 {
                            let known = watcher
                                .get_property::<Vec<String>>("RegisteredStatusNotifierItems")
                                .await
                                .map(|items| items.iter().any(|item| item == &bus_name))
                                .unwrap_or(false);
                            if !known {
                                match watcher
                                    .call_method(
                                        "RegisterStatusNotifierItem",
                                        &(bus_name.as_str(),),
                                    )
                                    .await
                                {
                                    Ok(_) => {
                                        if !was_connected {
                                            crate::logger::tlog!(
                                                crate::logger::LEVEL_INFO,
                                                "系统托盘已注册：{bus_name}"
                                            );
                                        }
                                        was_connected = true;
                                        connected_thread.store(true, Ordering::Relaxed);
                                    }
                                    Err(_) => {
                                        // 常见原因：Quickshell / KDE 的托盘宿主还没起。
                                        // 静默重试，只在翻转时记一条。
                                        if was_connected {
                                            crate::logger::tlog!(
                                                crate::logger::LEVEL_WARN,
                                                "托盘宿主已消失，转入后台重连"
                                            );
                                        }
                                        was_connected = false;
                                        connected_thread.store(false, Ordering::Relaxed);
                                    }
                                }
                            }
                        }

                        let current = match info_thread.lock() {
                            Ok(guard) => Some(guard.clone()),
                            Err(poisoned) => Some(poisoned.into_inner().clone()),
                        };
                        let current = match current {
                            Some(info) => info,
                            None => {
                                tokio::time::sleep(Duration::from_millis(500)).await;
                                continue;
                            }
                        };

                        let cur_status = status_label(&current.status).to_string();
                        let cur_tooltip = format_tooltip(&current.title, &current.artists);
                        let cur_icon_dim = icon_should_dim(&current.status);
                        let cur_menu_state =
                            (current.status == PlaybackState::Playing, current.muted);

                        // 状态变了 → 发 `NewStatus`，宿主可能换 active/passive 颜色；
                        // 图标档位变了 → `NewIcon`，暂停 / 停止时托盘换调暗的那套图；
                        // tooltip 变了 → `NewToolTip`，鼠标悬停才会更新。
                        // 频率 0.5s：托盘本身不需要更实时，省点锁开销。
                        let mut failed = false;
                        let mut sent = false;
                        if cur_status != last_status {
                            sent = true;
                            last_status = cur_status.clone();
                            let ctxt = item_iface.signal_emitter();
                            // `#[zbus(signal)]` 宏把信号方法生成在 `ItemSignals` trait 上，
                            // 默认不可见的关联函数。直接按 `Item::new_status(ctxt, ...)`
                            // 静态调用最简洁。
                            failed |= Item::new_status(ctxt, cur_status).await.is_err();
                        }
                        if cur_tooltip != last_tooltip {
                            sent = true;
                            last_tooltip = cur_tooltip;
                            let ctxt = item_iface.signal_emitter();
                            failed |= Item::new_tool_tip(ctxt).await.is_err();
                        }
                        if cur_icon_dim != last_icon_dim {
                            sent = true;
                            last_icon_dim = cur_icon_dim;
                            let ctxt = item_iface.signal_emitter();
                            failed |= Item::new_icon(ctxt).await.is_err();
                        }

                        // 动态菜单标签：播放↔暂停、静音↔取消静音。只给这两个 id 发
                        // `ItemsPropertiesUpdated`，再广播一次 `LayoutUpdated` 让宿主知道
                        // revision 变了——两者缺一，有的宿主只认其中一个。
                        if cur_menu_state != last_menu_state {
                            sent = true;
                            last_menu_state = cur_menu_state;
                            let new_revision = revision_thread.fetch_add(1, Ordering::Relaxed) + 1;
                            let updated: Vec<(i32, HashMap<String, OwnedValue>)> = entries
                                .iter()
                                .filter(|entry| DYNAMIC_LABEL_IDS.contains(&entry.0))
                                .map(|entry| {
                                    let mut props = HashMap::new();
                                    props.insert(
                                        "label".to_string(),
                                        owned_str(&entry_label(entry, &current)),
                                    );
                                    (entry.0, props)
                                })
                                .collect();
                            let ctxt = menu_iface.signal_emitter();
                            failed |= Menu::items_properties_updated(ctxt, updated, Vec::new())
                                .await
                                .is_err();
                            failed |= Menu::layout_updated(ctxt, new_revision, 0).await.is_err();
                        }

                        // 只有真的发过信号才更新计数：没信号可发的一拍既不算失败也不该清零，
                        // 否则「偶尔变一次、次次失败」会被间隔里的空拍抹平，永远凑不够 3 次。
                        if sent {
                            if failed {
                                failures += 1;
                                if failures >= 3 {
                                    return Err(zbus::Error::Failure(
                                        "托盘信号连续发送失败".to_string(),
                                    ));
                                }
                            } else {
                                failures = 0;
                            }
                        }

                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }

                    #[allow(unreachable_code)]
                    Ok(())
                }
                .await;

                match result {
                    Ok(()) => break,
                    Err(error) => {
                        connected_thread.store(false, Ordering::Relaxed);
                        crate::logger::tlog!(
                            crate::logger::LEVEL_WARN,
                            "系统托盘连接中断（不影响播放），5 秒后重连：{error}"
                        );
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        });
    });

    Some(TrayHandle { info, connected })
}

/// 图形会话是否存在。SSH 进 tty 但转发 X/Wayland 时这两个变量会被 sshd 设上，
/// 所以单看变量就够；不需要再去 `xdg_runtime_dir` 之类的地方猜。
fn environment_supports_tray() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

fn status_label(state: &PlaybackState) -> &'static str {
    match state {
        PlaybackState::Playing => "Active",
        _ => "Passive",
    }
}

fn format_tooltip(title: &str, artists: &[String]) -> String {
    match (title.is_empty(), artists.is_empty()) {
        (true, _) => "kugou-tui".to_string(),
        (false, true) => title.to_string(),
        (false, false) => format!("{title} - {}", artists.join(", ")),
    }
}

/// 把嵌入的 PNG 解出来、按 [`ICON_SIZES`] 缩放成多份 ARGB32 数据。
///
/// SNI 规范要求每个像素按 BGRA 字节序排（little-endian 上是 u32 LE 0xAARRGGBB），
/// 而 image crate 默认 RGBA——所以每个像素 R/B 互换一次。一次性成本，spawn 时算。
fn build_pixmaps() -> anyhow::Result<Vec<Pixmap>> {
    let dynamic = image::load_from_memory(ICON_PNG)?;
    let source_width = dynamic.width();
    let source_height = dynamic.height();

    let mut pixmaps = Vec::with_capacity(ICON_SIZES.len());
    for size in ICON_SIZES {
        let sized = if source_width == size && source_height == size {
            dynamic.clone()
        } else {
            dynamic.resize_exact(size, size, image::imageops::FilterType::Lanczos3)
        };
        let rgba = sized.into_rgba8();
        let mut bytes = rgba.into_raw();
        // BGRA 转换：每个 4 字节块的第 0、2 位互换。
        for chunk in bytes.as_chunks_mut::<4>().0 {
            chunk.swap(0, 2);
        }
        pixmaps.push((size as i32, size as i32, bytes));
    }
    Ok(pixmaps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_lists_title_and_artists() {
        let artists = vec!["A".to_string(), "B".to_string()];
        assert_eq!(format_tooltip("歌名", &artists), "歌名 - A, B");
    }

    #[test]
    fn tooltip_falls_back_gracefully() {
        assert_eq!(format_tooltip("", &[]), "kugou-tui");
        assert_eq!(format_tooltip("纯曲", &[]), "纯曲");
    }

    #[test]
    fn status_label_reflects_playing() {
        assert_eq!(status_label(&PlaybackState::Playing), "Active");
        assert_eq!(status_label(&PlaybackState::Paused), "Passive");
        assert_eq!(status_label(&PlaybackState::Stopped), "Passive");
    }

    /// 菜单项：能控制窗口时多一项「最小化 / 显示窗口」，不能时**整项不出现**。
    ///
    /// 「点了没反应」比「没有这一项」更糟，所以不可用时是去掉而不是置灰。
    /// 「退出」两种形态下都有——托盘上是唯一的图形化退出入口，不能跟着窗口
    /// 控制一起消失。
    #[test]
    fn menu_gains_the_window_entry_only_when_controllable() {
        let plain = menu_entries(false);
        let full = menu_entries(true);

        assert_eq!(
            plain.len(),
            6,
            "不带窗口控制：播放三项 + 分隔线 + 静音 + 退出"
        );
        assert_eq!(full.len(), 7);
        // 前 5 项（含分隔线）不受窗口控制能力影响
        assert_eq!(plain[..5], full[..5]);

        // full 里多出的是「最小化 / 显示窗口」+「退出」，plain 里直接是「退出」
        assert_eq!(
            full[5],
            (ID_WINDOW, "最小化 / 显示窗口", Some(Action::ToggleWindow))
        );
        assert_eq!(full[6].2, Some(Action::Quit), "最后一项应是退出");
        assert_eq!(plain[5].2, Some(Action::Quit), "最后一项应是退出");

        // 两条链路的分隔线都在「下一首」与「静音」之间，且不派发动作
        assert_eq!(plain[3], (ID_SEPARATOR, "", None));
        assert_eq!(full[3], (ID_SEPARATOR, "", None));

        // id 必须唯一：重复 id 会让宿主的菜单项互相覆盖
        for entries in [&plain, &full] {
            let mut ids: Vec<i32> = entries.iter().map(|(id, _, _)| *id).collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), entries.len(), "菜单项 id 不能重复");
        }
    }

    /// 「播放 / 暂停」「静音」的标签必须跟着状态走，其余项原样。
    #[test]
    fn play_pause_and_mute_labels_follow_the_state() {
        let mut info = TrayInfo::default();
        let entries = menu_entries(false);
        let find = |id: i32| entries.iter().find(|entry| entry.0 == id).unwrap();

        info.status = PlaybackState::Playing;
        assert_eq!(entry_label(find(ID_PLAY_PAUSE), &info), "暂停");
        info.status = PlaybackState::Paused;
        assert_eq!(entry_label(find(ID_PLAY_PAUSE), &info), "播放");
        info.status = PlaybackState::Loading;
        assert_eq!(entry_label(find(ID_PLAY_PAUSE), &info), "播放");

        assert_eq!(entry_label(find(ID_MUTE), &info), "静音");
        info.muted = true;
        assert_eq!(entry_label(find(ID_MUTE), &info), "取消静音");

        let prev = find(ID_PREV);
        assert_eq!(entry_label(prev, &info), "上一首", "静态项不受状态影响");
    }

    /// 分隔线必须带 `type = separator`，普通项必须带 label。
    ///
    /// 少了 `type`，宿主会把分隔线画成一条空白（更糟的是有的宿主当成可点的
    /// 空项）；少了 `label`，普通项整条消失。
    #[test]
    fn separator_is_typed_and_plain_items_are_labelled() {
        let info = TrayInfo::default();
        let entries = menu_entries(false);

        let separator = entries
            .iter()
            .find(|entry| entry.0 == ID_SEPARATOR)
            .unwrap();
        let props = item_properties(separator, &info);
        assert_eq!(
            props
                .get("type")
                .and_then(|value| String::try_from(value.clone()).ok()),
            Some("separator".to_string()),
            "分隔线必须声明 type = separator"
        );

        let play = entries
            .iter()
            .find(|entry| entry.0 == ID_PLAY_PAUSE)
            .unwrap();
        let props = item_properties(play, &info);
        assert!(
            props.contains_key("label"),
            "普通项必须有 label，否则宿主不显示"
        );
        assert!(
            !props.contains_key("type"),
            "普通项不能带 type，否则会被当成特殊项"
        );
    }

    /// `GetLayout` 的节点必须是 `(ia{sv}av)`。
    ///
    /// 这条是防崩溃的：字段若写成 `Value::from(..)`，整个节点会退化成 `(vvv)`，
    /// 宿主按规范解析时会**直接崩**（实测 Quickshell 挂掉，不是报错）。签名比对
    /// 能在测试里拦住这种错，不用等真实状态栏炸一次才发现。
    #[test]
    fn menu_node_has_the_spec_signature() {
        let info = TrayInfo::default();
        let entry = (ID_PLAY_PAUSE, "播放 / 暂停", Some(Action::PlayPause));
        let node = layout_node(&entry, &info);
        assert_eq!(
            node.value_signature().to_string(),
            "(ia{sv}av)",
            "布局节点的类型签名必须与 com.canonical.dbusmenu 规范一致"
        );
    }

    /// 图标字节序：SNI 要 BGRA，image 给的是 RGBA。
    ///
    /// 错了不会报错、图标也不会消失，只会**变色**（而且是那种「看着像渲染
    /// 问题」的变色），是最难靠肉眼定位的一类 bug。所以这里逐字节比对——
    /// 每一档尺寸都和「源图走同一条缩放管线」的结果对照。
    #[test]
    fn pixmaps_are_bgra_and_sized_as_requested() {
        let pixmaps = build_pixmaps().expect("嵌入的 PNG 应当能解码");
        assert_eq!(pixmaps.len(), ICON_SIZES.len());

        for (index, (width, height, bytes)) in pixmaps.iter().enumerate() {
            let size = ICON_SIZES[index] as i32;
            assert_eq!(*width, size, "宽度应与请求的尺寸一致");
            assert_eq!(*height, size, "高度应与请求的尺寸一致");
            assert_eq!(
                bytes.len(),
                (size * size * 4) as usize,
                "每个像素 4 字节（ARGB32）"
            );
        }

        let source = image::load_from_memory(ICON_PNG).expect("源 PNG 应能解码");
        for (width, height, bytes) in pixmaps.iter() {
            let expected = source
                .resize_exact(
                    *width as u32,
                    *height as u32,
                    image::imageops::FilterType::Lanczos3,
                )
                .into_rgba8()
                .into_raw();
            assert_eq!(bytes.len(), expected.len());
            for (index, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
                let src = &expected[index * 4..index * 4 + 4];
                assert_eq!(
                    chunk[0], src[2],
                    "{width}px 第 {index} 个像素：B 位应取自源的 R"
                );
                assert_eq!(
                    chunk[2], src[0],
                    "{width}px 第 {index} 个像素：R 位应取自源的 B"
                );
                assert_eq!(chunk[1], src[1], "{width}px 第 {index} 个像素：G 位不变");
                assert_eq!(chunk[3], src[3], "{width}px 第 {index} 个像素：A 位不变");
            }
        }

        // 兜底：如果整张图 R 恒等于 B，上面那条断言等于什么都没验证
        assert!(
            image::load_from_memory(ICON_PNG)
                .expect("源 PNG 应能解码")
                .into_rgba8()
                .into_raw()
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[0] != pixel[2]),
            "图标里应当有非灰阶像素，否则字节序断言形同虚设"
        );
    }

    /// 调暗版只压 alpha，RGB 一个字节都不能动——颜色变了就成了另一张图。
    #[test]
    fn dim_pixmaps_shrink_alpha_and_keep_colors() {
        let bright = build_pixmaps().expect("嵌入的 PNG 应当能解码");
        let dim = dim_pixmaps(&bright);

        assert_eq!(bright.len(), dim.len(), "尺寸档数不变");
        let mut darkened = false;
        for ((bw, bh, bb), (dw, dh, db)) in bright.iter().zip(&dim) {
            assert_eq!((bw, bh), (dw, dh));
            for (b, d) in bb.as_chunks::<4>().0.iter().zip(db.as_chunks::<4>().0) {
                assert_eq!(&b[..3], &d[..3], "RGB 必须原样");
                let expected = (f32::from(b[3]) * DIM_ALPHA).round() as u8;
                assert_eq!(d[3], expected, "alpha 应乘 {DIM_ALPHA}");
                if b[3] != d[3] {
                    darkened = true;
                }
            }
        }
        assert!(darkened, "至少要有一个像素被压暗，否则调暗等于没调");
    }

    /// 图标档位：播放 / 加载亮，暂停 / 停止暗。
    #[test]
    fn icon_dims_only_when_playback_is_idle() {
        assert!(!icon_should_dim(&PlaybackState::Playing));
        assert!(!icon_should_dim(&PlaybackState::Loading));
        assert!(icon_should_dim(&PlaybackState::Paused));
        assert!(icon_should_dim(&PlaybackState::Stopped));
    }

    /// 端到端：真在 session bus 上注册一次，再从 watcher 那边把名字查回来。
    ///
    /// 没有图形会话或没有 watcher（CI / 纯 tty / 非 KDE 桌面）时直接跳过——
    /// 这个测试验证的是「有 watcher 时确实注册成功」，不是「任何环境都能注册」。
    /// 真机上跑过：Quickshell 提供的 watcher 立刻接受了注册。
    #[test]
    fn registers_with_the_watcher_when_one_is_present() {
        if !environment_supports_tray() {
            return;
        }

        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => return,
        };

        let Ok(connection) = runtime.block_on(zbus::connection::Connection::session()) else {
            return;
        };
        let Ok(watcher) = runtime.block_on(zbus::Proxy::new(
            &connection,
            WATCHER_DEST,
            WATCHER_PATH,
            WATCHER_IFACE,
        )) else {
            return;
        };
        // watcher 不在就跳过：下面的断言在没托盘的环境里会误报失败。
        if runtime
            .block_on(watcher.get_property::<Vec<String>>("RegisteredStatusNotifierItems"))
            .is_err()
        {
            return;
        }

        let (bus, receiver) = EventBus::new();
        // spawn 内部还会再探一次环境变量，可能被并发的
        // `environment_probe_handles_missing_vars` 短暂改动——那种情况跳过而不是失败。
        let Some(handle) = spawn(bus) else {
            return;
        };

        // 注册是异步的：本机实测几十毫秒，给 3 秒上限。
        let mut connected = false;
        for _ in 0..30 {
            if handle.is_connected() {
                connected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(connected, "3 秒内应完成 watcher 注册");

        // 直接读这条 item 的属性——比查 watcher 列表更硬：
        // 列表用 well-known 还是 unique 名字记录取决于 watcher 实现，而
        // 「按我们注册的 bus name 能读到规范要求的属性」是托盘能显示的充要条件。
        let expected = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        let Ok(item) = runtime.block_on(zbus::Proxy::new(
            &connection,
            expected.as_str(),
            OBJECT_PATH,
            "org.kde.StatusNotifierItem",
        )) else {
            panic!("应能连上自己注册的 item：{expected}");
        };

        let category: String = runtime
            .block_on(item.get_property("Category"))
            .expect("Category 属性应当可读");
        assert_eq!(category, "ApplicationStatus");

        let title: String = runtime
            .block_on(item.get_property("Title"))
            .expect("Title 属性应当可读");
        assert_eq!(title, "kugou-tui");

        // `Menu` 必须是真实路径：返回 `/` 时 Quickshell 会把右键整个跳过。
        let menu: zbus::zvariant::OwnedObjectPath = runtime
            .block_on(item.get_property("Menu"))
            .expect("Menu 属性应当可读");
        assert_eq!(menu.as_str(), MENU_PATH, "Menu 应指向 DBusMenu 对象");

        // `IconPixmap` 是 `a(iiay)`：形状错了部分宿主干脆不显示图标。
        let pixmaps: Vec<Pixmap> = runtime
            .block_on(item.get_property("IconPixmap"))
            .expect("IconPixmap 属性应当可读");
        assert_eq!(pixmaps.len(), ICON_SIZES.len(), "应提供多尺寸图标");
        assert_eq!(
            pixmaps.first().map(|entry| entry.0),
            Some(16),
            "最小一档是 16"
        );
        assert!(
            pixmaps
                .iter()
                .all(|(width, height, bytes)| bytes.len() == (*width * *height * 4) as usize),
            "每个像素 4 字节 ARGB32"
        );

        // `ToolTip` 是 4 元组 `(sa(iiay)ss)`：验证能原样序列化回来。
        let tooltip: (String, Vec<Pixmap>, String, String) = runtime
            .block_on(item.get_property("ToolTip"))
            .expect("ToolTip 属性应当可读");
        assert_eq!(tooltip.2, "kugou-tui");
        assert_eq!(tooltip.3, "kugou-tui", "没在播放时 tooltip 退回应用名");

        // 右键链路：宿主点右键调的是 `ContextMenu`（部分宿主走中键的
        // `SecondaryActivate`）。两个都得真的把动作投进事件总线——收到
        // 「点了没反应」的反馈时，先在这里确认方法名和派发都对。
        let xml = runtime
            .block_on(connection.call_method(
                Some(expected.as_str()),
                OBJECT_PATH,
                Some("org.freedesktop.DBus.Introspectable"),
                "Introspect",
                &(),
            ))
            .expect("Introspect 应可调用")
            .body()
            .deserialize::<String>()
            .expect("Introspect 应返回 XML");
        assert!(
            xml.contains("ContextMenu"),
            "宿主应能看到 ContextMenu：\n{xml}"
        );
        assert!(
            xml.contains("SecondaryActivate"),
            "宿主应能看到 SecondaryActivate：\n{xml}"
        );

        for method in ["ContextMenu", "SecondaryActivate"] {
            runtime
                .block_on(connection.call_method(
                    Some(expected.as_str()),
                    OBJECT_PATH,
                    Some("org.kde.StatusNotifierItem"),
                    method,
                    &(0i32, 0i32),
                ))
                .unwrap_or_else(|error| panic!("{method} 应可调用：{error}"));

            match receiver.recv_timeout(Duration::from_secs(1)) {
                Ok(Event::Action(Action::PlayPause)) => {}
                other => panic!("{method} 应派发 PlayPause，实际：{other:?}"),
            }
        }

        // DBusMenu：宿主右键时先 `GetLayout` 拉树、再 `Event(id, "clicked")` 回点击。
        // 这两个跑不通，表现就是「菜单弹不出来」或者「弹出来点了没反应」。
        let layout = runtime
            .block_on(connection.call_method(
                Some(expected.as_str()),
                MENU_PATH,
                Some("com.canonical.dbusmenu"),
                "GetLayout",
                &(0i32, -1i32, Vec::<String>::new()),
            ))
            .expect("GetLayout 应可调用");
        let (revision, (root_id, _root_props, children)): MenuLayout =
            layout.body().deserialize().expect("GetLayout 应返回布局");
        assert_eq!(revision, 0, "默认状态（停止、未静音）下 revision 还是初值");
        assert_eq!(root_id, 0, "根节点 id 按规范是 0");
        // 菜单项在 spawn 时按「能不能控制窗口」定下来了，这里取同一份来对照。
        let entries = menu_entries(crate::window::available());
        assert_eq!(children.len(), entries.len(), "菜单项数量应一致");

        // 逐项读 label：这是宿主真正画出来的文字，缺了就是一条空白。
        // 播放 / 暂停、静音两项的标签由 `entry_label` 按状态解析——默认停止、
        // 未静音，应分别显示「播放」「静音」。
        let snapshot = TrayInfo::default();
        for (id, _, _) in &entries {
            let reply = runtime
                .block_on(connection.call_method(
                    Some(expected.as_str()),
                    MENU_PATH,
                    Some("com.canonical.dbusmenu"),
                    "GetProperty",
                    &(*id, "label"),
                ))
                .expect("GetProperty 应可调用");
            let got: OwnedValue = reply.body().deserialize().expect("label 应是变体");
            let got = String::try_from(got).expect("label 应是字符串");
            let entry = entries
                .iter()
                .find(|entry| entry.0 == *id)
                .expect("id 应在菜单里");
            assert_eq!(&got, &entry_label(entry, &snapshot), "id {id} 的 label");
        }

        // 分隔线必须能读到 type = separator，宿主才画得出横线。
        let reply = runtime
            .block_on(connection.call_method(
                Some(expected.as_str()),
                MENU_PATH,
                Some("com.canonical.dbusmenu"),
                "GetProperty",
                &(ID_SEPARATOR, "type"),
            ))
            .expect("GetProperty 应可调用");
        let got: OwnedValue = reply.body().deserialize().expect("type 应是变体");
        assert_eq!(
            String::try_from(got).as_deref(),
            Ok("separator"),
            "分隔线的 type 属性"
        );

        // 点第一项（播放 / 暂停）应当派发 PlayPause。
        runtime
            .block_on(connection.call_method(
                Some(expected.as_str()),
                MENU_PATH,
                Some("com.canonical.dbusmenu"),
                "Event",
                &(entries[0].0, "clicked", Value::from(0i32), 0u32),
            ))
            .expect("Event 应可调用");
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::Action(Action::PlayPause)) => {}
            other => panic!("点「播放 / 暂停」应派发 PlayPause，实际：{other:?}"),
        }

        // 点「退出」应当派发 Quit——托盘菜单是唯一的图形化退出入口。
        let quit_id = entries.last().expect("菜单不应为空").0;
        runtime
            .block_on(connection.call_method(
                Some(expected.as_str()),
                MENU_PATH,
                Some("com.canonical.dbusmenu"),
                "Event",
                &(quit_id, "clicked", Value::from(0i32), 0u32),
            ))
            .expect("Event 应可调用");
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::Action(Action::Quit)) => {}
            other => panic!("点「退出」应派发 Quit，实际：{other:?}"),
        }

        // 滚轮：垂直方向调音量（只看符号），宿主给的 delta 绝对值不统一。
        for (delta, wanted) in [(5i32, Action::VolumeUp), (-5, Action::VolumeDown)] {
            runtime
                .block_on(connection.call_method(
                    Some(expected.as_str()),
                    OBJECT_PATH,
                    Some("org.kde.StatusNotifierItem"),
                    "Scroll",
                    &(delta, "vertical"),
                ))
                .unwrap_or_else(|error| panic!("Scroll 应可调用：{error}"));
            match receiver.recv_timeout(Duration::from_secs(1)) {
                Ok(Event::Action(action)) if action == wanted => {}
                other => panic!("向上/向下滚应派发 {wanted:?}，实际：{other:?}"),
            }
        }

        // 水平滚轮是快进 / 快退；零位移不派发任何动作。
        runtime
            .block_on(connection.call_method(
                Some(expected.as_str()),
                OBJECT_PATH,
                Some("org.kde.StatusNotifierItem"),
                "Scroll",
                &(2i32, "horizontal"),
            ))
            .expect("Scroll 应可调用");
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::Action(Action::SeekForward)) => {}
            other => panic!("向右滚应派发 SeekForward，实际：{other:?}"),
        }
        runtime
            .block_on(connection.call_method(
                Some(expected.as_str()),
                OBJECT_PATH,
                Some("org.kde.StatusNotifierItem"),
                "Scroll",
                &(0i32, "vertical"),
            ))
            .expect("Scroll 应可调用");
        assert!(
            receiver.recv_timeout(Duration::from_millis(300)).is_err(),
            "零位移的滚动不该派发动作"
        );
    }

    /// 不在图形会话里跑也能正常工作——避免有人在 CI 里跑测试时整个进程崩溃。
    /// 这里只验证「没设变量」返回 false；设了的情况取决于测试时的环境。
    #[test]
    fn environment_probe_handles_missing_vars() {
        // SAFETY：测试串行执行；同进程内其他测试不应依赖这两个变量。
        let prev_wayland = std::env::var_os("WAYLAND_DISPLAY");
        let prev_display = std::env::var_os("DISPLAY");
        unsafe {
            std::env::remove_var("WAYLAND_DISPLAY");
            std::env::remove_var("DISPLAY");
        }
        assert!(!environment_supports_tray());
        // 复原：测试不该污染后续测试的环境。
        if let Some(value) = prev_wayland {
            unsafe {
                std::env::set_var("WAYLAND_DISPLAY", value);
            }
        }
        if let Some(value) = prev_display {
            unsafe {
                std::env::set_var("DISPLAY", value);
            }
        }
    }
}
