//! 系统托盘（KDE/MATE 风格的 StatusNotifierItem）。
//!
//! 把播放器注册成 DBus 上的 `org.kde.StatusNotifierItem`，让 waybar / Quickshell /
//! KDE 之类能识别并显示它。宿主收到右键（中右键在 KDE 里走 SecondaryActivate，
//! 部分宿主走 ContextMenu）会派发一次 `PlayPause` 动作进事件总线，等同于按空格。
//!
//! # 自适配：三层降级，每层都静默跳过、不影响播放
//!
//! 1. 没有图形会话（无 `WAYLAND_DISPLAY` 也无 `DISPLAY`）→ 完全不连 DBus；
//! 2. session bus 不可用（纯 tty、容器里没挂 bus）→ zbus 报 Err，记 WARN；
//! 3. 没有 `StatusNotifierWatcher`（非 KDE/Quickshell 桌面）→
//!    `RegisterStatusNotifierItem` 调用失败，记 WARN。
//!
//! 任一情况 `TrayHandle::is_connected()` 都会返回 false，主循环跳过同步。
//!
//! # 不实现菜单
//!
//! DBusMenu 是 `com.canonical.dbusmenu`，要单独注册一个接口并实现几百行 XML 树。
//! 那个体量对「最小化托盘」过头了。`Menu` 属性返回 `/` 表示无菜单，左键 `Activate`
//! 按用户要求留空。把交互都收在 `SecondaryActivate` / `ContextMenu`，让中右键
//! 触发播放/暂停——比「什么都不做」更趁手、又不至于把菜单那套拉进来。

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

/// 嵌入的图标（64×64 RGBA，源 SVG 在 `assets/tray.svg`）。运行时再缩放到目标尺寸。
const ICON_PNG: &[u8] = include_bytes!("../assets/tray-icon.png");

/// 提供给宿主挑选的多尺寸图标。22 是 KDE「最小托盘尺寸」，64 是 Quickshell 在高分
/// 屏下的常用值——宿主挑一个合适的，剩下的忽略。少给尺寸会让某些宿主退回到空白。
const ICON_SIZES: [u32; 2] = [22, 64];

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
}

/// 主循环持有它，每帧调 [`Self::update`] 刷一次。
#[derive(Clone)]
pub struct TrayHandle {
    info: Arc<Mutex<TrayInfo>>,
    /// 注册是否走到了 watcher 注册那一步。`spawn` 失败时为 false。
    connected: Arc<AtomicBool>,
}

impl TrayHandle {
    pub fn update(&self, info: TrayInfo) {
        if let Ok(mut guard) = self.info.lock() {
            *guard = info;
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
    /// 启动时一次性预渲染的多尺寸图标。`IconPixmap` 属性直接返回它，零拷贝是不可能的
    /// （`Vec<u8>` 得克隆出去给 zbus），但启动期以外不会改。
    pixmaps: Vec<Pixmap>,
    /// ToolTip 用的单个图标。规范 ToolTip 只放一份图标，64 看着比 22 清楚。
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

    #[zbus(property)]
    async fn icon_pixmap(&self) -> Vec<Pixmap> {
        self.pixmaps.clone()
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

    /// 滚轮。最常见的语义是音量，但项目里音量按 `=`/`-`/数字，没在滚轮上做。
    async fn scroll(&self, _delta: i32, _direction: &str) {}

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

/// 一条菜单项：`(id, 标签, 点击后派发的动作)`。id 从 1 起——0 是 DBusMenu
/// 规定的虚拟根节点。
type MenuEntry = (i32, &'static str, Action);

/// 菜单项。
///
/// `window_control` 为假（当前会话不是 niri）时**最后一项根本不出现**：
/// 一个点了没反应的菜单项比没有它更糟。
///
/// 没做成动态（跟着播放状态把措辞在「播放 / 暂停」之间切）：那要维护 revision
/// 并广播 `ItemsPropertiesUpdated`，而「播放 / 暂停」这个说法两种状态下都成立。
fn menu_entries(window_control: bool) -> Vec<MenuEntry> {
    let mut entries: Vec<MenuEntry> = vec![
        (1, "播放 / 暂停", Action::PlayPause),
        (2, "上一首", Action::Prev),
        (3, "下一首", Action::Next),
    ];
    if window_control {
        // 一个开关项而不是「最小化」「显示」两项：niri 的 toggle 一次搞定，
        // 菜单里也不用让用户先判断当前是哪种状态。
        entries.push((4, "最小化 / 显示窗口", Action::ToggleWindow));
    }
    entries
}

struct Menu {
    bus: EventBus,
    entries: Vec<MenuEntry>,
}

impl Menu {
    fn find(&self, id: i32) -> Option<&MenuEntry> {
        self.entries.iter().find(|entry| entry.0 == id)
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
            self.entries
                .iter()
                .map(|(id, label, _)| layout_node(*id, label))
                .collect()
        } else {
            Vec::new()
        };
        // revision 恒为 0：树是静态的，宿主取一次就够
        (0, (parent_id, HashMap::new(), children))
    }

    async fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        ids.into_iter()
            .filter_map(|id| {
                self.find(id)
                    .map(|(id, label, _)| (*id, item_properties(label)))
            })
            .collect()
    }

    async fn get_property(&self, id: i32, name: String) -> OwnedValue {
        self.find(id)
            .and_then(|(_, label, _)| item_properties(label).remove(&name))
            .unwrap_or_else(|| owned_str(""))
    }

    /// 点击。宿主只发 `clicked` 这一个事件 id，其余（`opened` / `closed` 等）忽略。
    async fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }
        if let Some((_, _, action)) = self.find(id) {
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

/// 一条菜单项的属性。`label` / `enabled` / `visible` 缺一个，多数宿主就不显示它。
fn item_properties(label: &str) -> HashMap<String, OwnedValue> {
    let mut map = HashMap::new();
    map.insert("label".to_string(), owned_str(label));
    map.insert("enabled".to_string(), owned_bool(true));
    map.insert("visible".to_string(), owned_bool(true));
    map
}

/// 包成 DBusMenu 的布局节点 `(ia{sv}av)`——没有子菜单，第三项给空数组。
///
/// 必须手工拼 `Value::Structure`：zvariant 只给**固定几个**元组实现了 `Type`，
/// `(i32, a{sv}, av)` 这种嵌套组合不在其中，直接 `try_from` 元组会编译不过。
fn layout_node(id: i32, label: &str) -> OwnedValue {
    // 字段必须给**具体类型**。写成 `Value::from(id)` 之类的话，每个字段自己又是个
    // variant，整个节点会变成 `(vvv)` 而不是规范要的 `(ia{sv}av)`——宿主按规范去解析
    // 会拿到对不上的类型，实测 Quickshell 直接崩（不是报错，是进程挂掉）。
    let structure = Structure::from((id, item_properties(label), Vec::<OwnedValue>::new()));
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
    let info_thread = Arc::clone(&info);
    let connected_thread = Arc::clone(&connected);
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
            // 菜单和托盘项共用一条事件总线：点菜单项要能把动作投进主循环
            let menu = Menu {
                bus: bus.clone(),
                entries: menu_entries(crate::window::available()),
            };

            let item = Item {
                info: Arc::clone(&info_thread),
                bus,
                pixmaps: pixmaps.clone(),
                tooltip_pixmap: tooltip_pixmap.clone(),
            };

            let result: zbus::Result<()> = async {
                let connection = ConnectionBuilder::session()?
                    .name(bus_name.as_str())?
                    .serve_at(OBJECT_PATH, item)?
                    .serve_at(MENU_PATH, menu)?
                    .build()
                    .await?;

                // 向 watcher 报到。失败常见原因：Quickshell 没跑 / KDE plasma 进程没起。
                // connection 已建立，但没人知道我们——直接走完信号循环也只会空转，
                // 所以 watcher 失败视为整体失败，drop connection 释放 bus name。
                connection
                    .call_method(
                        Some(WATCHER_DEST),
                        WATCHER_PATH,
                        Some(WATCHER_IFACE),
                        "RegisterStatusNotifierItem",
                        &(bus_name.as_str(),),
                    )
                    .await?;

                connected_thread.store(true, Ordering::Relaxed);

                let iface = connection
                    .object_server()
                    .interface::<_, Item>(OBJECT_PATH)
                    .await?;

                let mut last_status = String::new();
                let mut last_tooltip = String::new();

                loop {
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

                    // 状态变了 → 发 `NewStatus` 信号，宿主可能换 active/passive 颜色；
                    // tooltip 变了 → 发 `NewToolTip`，鼠标悬停才会更新。
                    // 频率 0.5s：托盘本身不需要更实时，省点锁开销。
                    if cur_status != last_status {
                        last_status = cur_status.clone();
                        let ctxt = iface.signal_emitter();
                        // `#[zbus(signal)]` 宏把信号方法生成在 `ItemSignals` trait 上，
                        // 默认不可见的关联函数。直接按 `Item::new_status(ctxt, ...)`
                        // 静态调用最简洁。
                        let _ = Item::new_status(ctxt, cur_status).await;
                    }
                    if cur_tooltip != last_tooltip {
                        last_tooltip = cur_tooltip;
                        let ctxt = iface.signal_emitter();
                        let _ = Item::new_tool_tip(ctxt).await;
                    }

                    tokio::time::sleep(Duration::from_millis(500)).await;
                }

                #[allow(unreachable_code)]
                Ok(())
            }
            .await;

            if let Err(error) = result {
                crate::logger::tlog!(
                    crate::logger::LEVEL_WARN,
                    "系统托盘注册失败（不影响播放）：{error}"
                );
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
    #[test]
    fn menu_gains_the_window_entry_only_when_controllable() {
        let plain = menu_entries(false);
        let full = menu_entries(true);

        assert_eq!(plain.len(), 3, "不是 niri 时只有播放控制三项");
        assert_eq!(full.len(), 4);
        assert_eq!(plain, full[..3].to_vec(), "前三项不该受窗口控制能力影响");

        let (id, label, action) = full[3];
        assert_eq!(id, 4, "新项 id 要接在现有项之后，别和前三项撞");
        assert_eq!(label, "最小化 / 显示窗口");
        assert_eq!(action, Action::ToggleWindow);

        // id 必须唯一：重复 id 会让宿主的菜单项互相覆盖
        let mut ids: Vec<i32> = full.iter().map(|(id, _, _)| *id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), full.len(), "菜单项 id 不能重复");
    }

    /// `GetLayout` 的节点必须是 `(ia{sv}av)`。
    ///
    /// 这条是防崩溃的：字段若写成 `Value::from(..)`，整个节点会退化成 `(vvv)`，
    /// 宿主按规范解析时会**直接崩**（实测 Quickshell 挂掉，不是报错）。签名比对
    /// 能在测试里拦住这种错，不用等真实状态栏炸一次才发现。
    #[test]
    fn menu_node_has_the_spec_signature() {
        let node = layout_node(1, "播放 / 暂停");
        assert_eq!(
            node.value_signature().to_string(),
            "(ia{sv}av)",
            "布局节点的类型签名必须与 com.canonical.dbusmenu 规范一致"
        );
    }

    /// 图标字节序：SNI 要 BGRA，image 给的是 RGBA。
    ///
    /// 错了不会报错、图标也不会消失，只会**变色**（而且是那种「看着像渲染
    /// 问题」的变色），是最难靠肉眼定位的一类 bug。所以这里逐字节比对。
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

        // 64 那一份与源图同尺寸，可以逐字节比对
        let source = image::load_from_memory(ICON_PNG)
            .expect("源 PNG 应能解码")
            .into_rgba8()
            .into_raw();
        let (_, _, bytes) = pixmaps
            .iter()
            .find(|(width, _, _)| *width == 64)
            .expect("应有 64 尺寸的图标");
        assert_eq!(bytes.len(), source.len());

        for (index, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
            let src = &source[index * 4..index * 4 + 4];
            assert_eq!(chunk[0], src[2], "第 {index} 个像素：B 位应取自源的 R");
            assert_eq!(chunk[2], src[0], "第 {index} 个像素：R 位应取自源的 B");
            assert_eq!(chunk[1], src[1], "第 {index} 个像素：G 位不变");
            assert_eq!(chunk[3], src[3], "第 {index} 个像素：A 位不变");
        }

        // 兜底：如果整张图 R 恒等于 B，上面那条断言等于什么都没验证
        assert!(
            source
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[0] != pixel[2]),
            "图标里应当有非灰阶像素，否则字节序断言形同虚设"
        );
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
        assert_eq!(pixmaps.first().map(|entry| entry.0), Some(22));
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
        assert_eq!(revision, 0, "静态菜单的 revision 恒为 0");
        assert_eq!(root_id, 0, "根节点 id 按规范是 0");
        // 菜单项在 spawn 时按「能不能控制窗口」定下来了，这里取同一份来对照。
        let entries = menu_entries(crate::window::available());
        assert_eq!(children.len(), entries.len(), "菜单项数量应一致");

        // 逐项读 label：这是宿主真正画出来的文字，缺了就是一条空白。
        for (id, label, _) in &entries {
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
            assert_eq!(&got, label, "id {id} 的 label");
        }

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
