//! 按键 → 语义动作的映射。
//!
//! 分成两套键表，由 [`KeyMode`] 选择：
//!
//! * [`KeyMode::Normal`] —— 列表浏览态，字母键是快捷键（`q` 退出、`n` 下一首……）。
//! * [`KeyMode::TextInput`] —— 输入框获得焦点，字母键必须原样插入文本，
//!   否则用户就没法搜索带 `q`、`j` 的歌名了。
//!
//! 之所以把「模式判断」放在这里而不是让 UI 层各自处理，是为了让快捷键表只有一份，
//! 帮助面板与真实行为不会漂移。

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMode {
    /// 浏览态：字母键触发快捷键。
    Normal,
    /// 输入态：字母键插入文本。
    TextInput,
}

/// 与具体按键解耦的语义动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    // ---- 全局 ----
    /// 退出（会先保存配置与播放进度）。
    Quit,
    /// 强制退出，不保存。
    ForceQuit,
    /// 打开帮助面板。
    Help,
    /// 切换窗口的最小化状态（仅 niri）。目前由托盘菜单触发，默认不占键位。
    ToggleWindow,

    // ---- 列表导航 ----
    MoveUp,
    MoveDown,
    MoveTop,
    MoveBottom,
    PageUp,
    PageDown,
    /// 在左侧导航栏与右侧内容区之间切换焦点。
    FocusNext,
    FocusPrev,
    /// 激活当前项 / 提交输入。
    Submit,
    /// 返回上一层 / 取消输入。
    Cancel,
    /// 打开当前歌曲的右键菜单（等同鼠标右键）。
    ContextMenu,

    // ---- 文本编辑 ----
    Char(char),
    Backspace,
    Delete,
    CursorLeft,
    CursorRight,
    CursorHome,
    CursorEnd,

    // ---- 播放控制 ----
    PlayPause,
    Next,
    Prev,
    SeekForward,
    SeekBackward,
    /// 相对跳转指定毫秒（正负皆可）。
    ///
    /// MPRIS 的 `Seek` 给的是任意大小的偏移量，不能用「N 次 ±5 秒」去凑——
    /// 拖一次 1 小时的进度条会往事件队列里灌 720 条消息，主循环单帧只消费
    /// 64 条，界面会明显卡住。
    ///
    /// 只有 MPRIS 会构造它（键盘没有「跳 12345 毫秒」这种按键，见 `action_from_name`
    /// 里只映射了 `seek_forward` / `seek_backward`），所以非 Unix 上它是死的。
    /// 留 `allow` 而不是 cfg 掉：动作表在两边保持同一形状，
    /// `handle_action` 的 `match` 才能一直是穷尽的。
    #[cfg_attr(not(unix), allow(dead_code))]
    SeekBy(i64),
    /// 搜索结果「加载更多」：追加下一页。
    LoadMoreSearch,
    /// 绝对定位到指定毫秒。MPRIS 的 SetPosition 需要它——桌面组件拖进度条是"跳到某处"，
    /// 不是"前进/后退几秒"，只有 SeekForward/Backward 是做不到的。
    ///
    /// 同 [`Action::SeekBy`]，只有 MPRIS 会构造。
    #[cfg_attr(not(unix), allow(dead_code))]
    SeekTo(u64),
    VolumeUp,
    VolumeDown,
    ToggleMute,
    /// 顺序 → 单曲循环 → 随机 → 列表循环
    CyclePlaybackMode,
    /// 在支持的音质档位之间循环（下一首生效）。
    CycleQuality,
    /// 打开/关闭歌词面板。
    ToggleLyricPanel,
    /// 歌词整体延后 100ms。
    LyricDelay,
    /// 歌词整体提前 100ms。
    LyricAdvance,

    // ---- 业务 ----
    /// 打开搜索输入框。
    OpenSearch,
    /// 打开设置页（直达键；它同时也够得到数字键 9）。
    OpenSettings,
    /// 下载当前播放歌曲到设置页里选的目录。
    DownloadCurrent,
    /// 重新拉取当前视图数据。
    Reload,
    /// 把当前选中歌曲追加到播放队列。
    QueueAppend,
    /// 把当前列表**全部**追加到播放队列（数百首时用它，避免逐首添加。
    AddAllToQueue,
    /// 把当前选中歌曲插到下一首播放。
    QueuePlayNext,
    /// 把播放队列里选中的歌曲移出队列。
    RemoveFromQueue,
    /// 清空整个播放队列（需二次确认）。
    ClearQueue,
    /// 清空音频缓存目录。
    ClearCache,
    /// 切换歌曲列表的排列方向：倒序（最后一首在最上）↔ 正序。
    ToggleSortOrder,
    /// 打开排行榜视图。
    OpenRanks,
    /// 打开云端歌单视图。
    OpenCloud,
    /// 扫码登录（应用内渲染二维码）。
    Login,
    /// 领取「概念版」当天 VIP（领一天 → 升级成畅听 VIP）。
    ClaimVip,
    /// 在歌手列表里轮换地区筛选：全部 → 华语 → 欧美 → 日韩 → 其他。
    CycleArtistFilter,
    /// 把当前播放队列同步到指定的云端歌单。
    SyncToCloud,
    /// 把选中歌曲加入云端歌单。
    AddToCloud,
    /// 把选中歌曲**从**云端歌单移除（按歌单条目的 fileid）。
    RemoveFromCloud,
    /// 删除（取消收藏）当前选中的云端歌单。
    DeleteCloudPlaylist,
    /// 弹出输入框，用输入的名字新建云端歌单。
    NewCloudPlaylist,
    /// 切换左侧导航栏的可见性（小窗口下腾出空间）。
    ToggleSidebar,
    /// 打开音源管理页。
    SwitchSource,
    /// 音源管理页：把选中音源设为默认。
    SetDefaultSource,
    /// 音源管理页：把选中音源的优先级往上 / 往下调。
    RaiseSourcePriority,
    LowerSourcePriority,
    /// 按下了数字 1-9。语义由 App 按当前焦点决定：
    /// 侧边栏 → 切标签页。数字键只做这一件事，不会去选中列表项。
    Digit(u8),

    /// 未绑定。
    None,
}

/// 把一个按键事件翻译成语义动作。
/// 自定义键位表。启动时一次性装好，之后只读。
///
/// 用 `OnceLock` 而不是把它塞进 `AppState`：`resolve` 是个纯函数式的入口，
/// 被输入处理链路直接调用，为它单独传递上下文要改动所有调用点，不划算。
static CUSTOM: OnceLock<HashMap<(KeyCode, KeyModifiers), Action>> = OnceLock::new();

/// 安装配置文件里的自定义键位。
///
/// # 冲突与非法
///
/// * **按键冲突**（两个动作绑到同一个键）：后配置的覆盖先配置的，并记 WARN——
///   静默丢一个会让用户以为是程序坏了。
/// * **非法**：动作名不存在、按键名无法解析、或动作带参数（`SwitchTab` 等
///   需要数字的动作）→ 跳过该条并记 WARN，**不中断启动**。键位错了只是不顺手，
///   不该让程序起不来。
///
/// 返回实际生效的条数，便于启动时在日志里核对。
pub fn install_custom(bindings: &BTreeMap<String, String>) -> usize {
    let mut table: HashMap<(KeyCode, KeyModifiers), Action> = HashMap::new();

    for (action_name, key_name) in bindings {
        let action = match action_from_name(action_name) {
            Some(action) => action,
            None => {
                crate::logger::tlog!(
                    crate::logger::LEVEL_WARN,
                    "键位配置：未知动作 {action_name:?}（键 {key_name:?}），已忽略"
                );
                continue;
            }
        };
        let (code, modifiers) = match parse_key(key_name) {
            Some(parsed) => parsed,
            None => {
                crate::logger::tlog!(
                    crate::logger::LEVEL_WARN,
                    "键位配置：无法解析按键 {key_name:?}（动作 {action_name}），已忽略"
                );
                continue;
            }
        };

        if let Some(previous) = table.insert((code, modifiers), action) {
            crate::logger::tlog!(
                crate::logger::LEVEL_WARN,
                "键位配置：{key_name:?} 同时绑定了 {action_name} 与之前的 {previous:?}，以 {action_name} 为准"
            );
        }
    }

    let installed = table.len();
    if installed > 0 {
        let _ = CUSTOM.set(table);
        crate::logger::tlog!(crate::logger::LEVEL_INFO, "已加载 {installed} 条自定义键位");
    }
    installed
}

/// 动作名（snake_case）→ [`Action`]。
///
/// 带参数的动作（`SwitchTab` / `SeekTo` / `SeekBy` / `Char`）不在其中：
/// 它们的值来自运行时，配置文件里写不出完整语义。
pub fn action_from_name(name: &str) -> Option<Action> {
    Some(match name {
        "quit" => Action::Quit,
        "force_quit" => Action::ForceQuit,
        "help" => Action::Help,
        "toggle_window" => Action::ToggleWindow,
        "move_up" => Action::MoveUp,
        "move_down" => Action::MoveDown,
        "move_top" => Action::MoveTop,
        "move_bottom" => Action::MoveBottom,
        "page_up" => Action::PageUp,
        "page_down" => Action::PageDown,
        "focus_next" => Action::FocusNext,
        "focus_prev" => Action::FocusPrev,
        "submit" => Action::Submit,
        "cancel" => Action::Cancel,
        "backspace" => Action::Backspace,
        "delete" => Action::Delete,
        "cursor_left" => Action::CursorLeft,
        "cursor_right" => Action::CursorRight,
        "cursor_home" => Action::CursorHome,
        "cursor_end" => Action::CursorEnd,
        "play_pause" => Action::PlayPause,
        "next" => Action::Next,
        "prev" => Action::Prev,
        "seek_forward" => Action::SeekForward,
        "seek_backward" => Action::SeekBackward,
        "load_more_search" => Action::LoadMoreSearch,
        "volume_up" => Action::VolumeUp,
        "volume_down" => Action::VolumeDown,
        "toggle_mute" => Action::ToggleMute,
        "cycle_playback_mode" => Action::CyclePlaybackMode,
        "cycle_quality" => Action::CycleQuality,
        "toggle_lyric_panel" => Action::ToggleLyricPanel,
        "lyric_delay" => Action::LyricDelay,
        "lyric_advance" => Action::LyricAdvance,
        "open_search" => Action::OpenSearch,
        "open_settings" => Action::OpenSettings,
        "download_current" => Action::DownloadCurrent,
        "reload" => Action::Reload,
        "queue_append" => Action::QueueAppend,
        "add_all_to_queue" => Action::AddAllToQueue,
        "queue_play_next" => Action::QueuePlayNext,
        "remove_from_queue" => Action::RemoveFromQueue,
        "clear_queue" => Action::ClearQueue,
        "clear_cache" => Action::ClearCache,
        "toggle_sort_order" => Action::ToggleSortOrder,
        "open_ranks" => Action::OpenRanks,
        "open_cloud" => Action::OpenCloud,
        "login" => Action::Login,
        "claim_vip" => Action::ClaimVip,
        "cycle_artist_filter" => Action::CycleArtistFilter,
        "sync_to_cloud" => Action::SyncToCloud,
        "add_to_cloud" => Action::AddToCloud,
        "remove_from_cloud" => Action::RemoveFromCloud,
        "delete_cloud_playlist" => Action::DeleteCloudPlaylist,
        "new_cloud_playlist" => Action::NewCloudPlaylist,
        "toggle_sidebar" => Action::ToggleSidebar,
        "switch_source" => Action::SwitchSource,
        "set_default_source" => Action::SetDefaultSource,
        "raise_source_priority" => Action::RaiseSourcePriority,
        "lower_source_priority" => Action::LowerSourcePriority,
        _ => return None,
    })
}

/// 解析按键名 → (键码, 修饰键)。
///
/// 支持单字符（`q` / `Q` / `/`）、命名键（`space` / `enter` / `up` / `f1`…）
/// 以及 `ctrl+` / `alt+` / `shift+` 前缀。
fn parse_key(text: &str) -> Option<(KeyCode, KeyModifiers)> {
    let text = text.trim();
    let (modifiers, key) = if let Some(rest) = text.strip_prefix("ctrl+") {
        (KeyModifiers::CONTROL, rest)
    } else if let Some(rest) = text.strip_prefix("alt+") {
        (KeyModifiers::ALT, rest)
    } else if let Some(rest) = text.strip_prefix("shift+") {
        (KeyModifiers::SHIFT, rest)
    } else {
        (KeyModifiers::NONE, text)
    };

    let code = match key {
        "space" | " " => KeyCode::Char(' '),
        "enter" | "return" | "cr" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        // 单个字符**必须先判**，否则会被下面的 F1~F12 分支吃掉：
        // `f` 也满足「长度 ≤ 3 且以 f 开头」，于是 `f[1..]` 是空串、parse 失败、
        // 整条返回 None —— 结果是「歌手地区筛选」的 `f` 唯独没法通过配置重绑，
        // 而其余单字母都行。这是写测试时才发现的。
        single if single.chars().count() == 1 => KeyCode::Char(single.chars().next()?),
        // f1 ~ f12
        function if function.len() <= 3 && function.starts_with('f') => {
            let number: u8 = function[1..].parse().ok()?;
            if (1..=12).contains(&number) {
                KeyCode::F(number)
            } else {
                return None;
            }
        }
        _ => return None,
    };

    Some((code, modifiers))
}

/// 帮助面板里的写法 → [`parse_key`] 认的写法。
///
/// 面板是给人看的：`←` 比 `left` 直观、`S-Tab` 比 `backtab` 眼熟、`1..9` 一看
/// 就懂。但这些都不是 `parse_key` 的语法，要拿它们查 `parse_key` 必须先翻译。
///
/// 返回 `None` 表示「这个 token 不是一个具体的键」（如 `1..9` 的范围写法），
/// 调用方应跳过它而不是当成错误。
pub fn display_token_to_key_name(token: &str) -> Option<String> {
    // 别名要先判：`←` 也是一个「单字符」，不特殊处理就会被下面那条单字符
    // 捷径截走，变成 `Char('←')`——而 `resolve_normal` 绑的是 `KeyCode::Left`。
    if let Some(name) = match token {
        "←" => Some("left"),
        "→" => Some("right"),
        "↑" => Some("up"),
        "↓" => Some("down"),
        "S-Tab" => Some("backtab"),
        _ => None,
    } {
        return Some(name.to_string());
    }
    // `1..9` 是个范围，不是单个键，没法喂给 parse_key
    if token.contains("..") {
        return None;
    }
    // 单个字符必须**保留大小写**：`q` 与 `Q` 是两个不同的动作
    if token.chars().count() == 1 {
        return Some(token.to_string());
    }
    Some(token.to_lowercase())
}

/// 把面板上那串显示键按用户的自定义键位改写。
///
/// 例：面板写 `n / p`，用户把 `prev` 改成了 `N` → 返回 `n / N`；
/// 没被改过的 token 原样保留。
///
/// 为什么需要它：`CHEATSHEET` 是静态表（默认键位），而用户很可能在 `[keymap]`
/// 里换过键。不改写的话，帮助面板会一本正经地告诉用户去按一个已经不生效的键
/// ——比不写更糟，因为用户会以为程序坏了。
///
/// 每个 token 独立处理：同一个键在两行被用（如 `R`→`r` 的同时 `r`→`m`）
/// 不会互相干扰，因为这里是「按键找新动作」的单项替换，不做全局重映射。
pub fn display_keys_with_custom(tokens: &str) -> String {
    display_keys_with(tokens, CUSTOM.get())
}

/// [`display_keys_with_custom`] 的内部实现：把键表当参数传，便于单测。
///
/// 生产侧的 `CUSTOM` 是 `OnceLock`（启动装一次、之后只读），测试里装不进去；
/// 把「查表」这一步抽成参数，核心替换逻辑就能直接测，不必为了测试把
/// 生产代码换成 `RwLock`。`None` 表示没自定义键位。
fn display_keys_with(
    tokens: &str,
    table: Option<&HashMap<(KeyCode, KeyModifiers), Action>>,
) -> String {
    let custom_key_for = |action: Action| -> Option<String> {
        let table = table?;
        table
            .iter()
            .find(|(_, bound)| **bound == action)
            .map(|((code, modifiers), _)| render_key_name(*code, *modifiers))
    };

    tokens
        .split(" / ")
        .map(|token| {
            let token = token.trim();
            if token.is_empty() {
                return String::new();
            }
            // 知道这个 token 在当前键表下是什么动作；不知道就原样显示
            let Some(action) = resolve_displayed_token(token) else {
                return token.to_string();
            };
            // 用户把这个动作改到别的键上了就显示新键，否则保持原样
            custom_key_for(action).unwrap_or_else(|| token.to_string())
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

/// 取某个动作在**当前键位**下该显示成什么键，用于 UI 里的行内提示。
///
/// 例：`key_hint_for("R")` → 用户把刷新改到 `r` 了就返回 `"r"`，否则返回 `"R"`。
///
/// 为什么提示文字也要走这里：界面里的「按 X 重试」是硬编码的，而用户
/// 完全可能在 `[keymap]` 里换过键——提示就变成了「叫用户按一个没用的键」，
/// 比不提示更让人迷惑。传默认键进来，拿回去的是当前生效的那个。
pub fn key_hint_for(default_key: &str) -> String {
    display_keys_with_custom(default_key)
}

/// 面板 token 在默认键表下对应的动作（供反查用）。
fn resolve_displayed_token(token: &str) -> Option<Action> {
    let name = display_token_to_key_name(token)?;
    let (code, modifiers) = parse_key(&name)?;
    let action = resolve(KeyEvent::new(code, modifiers), KeyMode::Normal);
    (action != Action::None).then_some(action)
}

/// 按键 → 面板上显示的名字。与 [`display_token_to_key_name`] 是反方向。
fn render_key_name(code: KeyCode, modifiers: KeyModifiers) -> String {
    let base = match code {
        KeyCode::Left => "←".to_string(),
        KeyCode::Right => "→".to_string(),
        KeyCode::Up => "↑".to_string(),
        KeyCode::Down => "↓".to_string(),
        KeyCode::BackTab => "S-Tab".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Insert => "Insert".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PgUp".to_string(),
        KeyCode::PageDown => "PgDn".to_string(),
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(number) => format!("F{number}"),
        other => format!("{other:?}"),
    };

    if modifiers.contains(KeyModifiers::CONTROL) {
        format!("ctrl+{base}")
    } else if modifiers.contains(KeyModifiers::ALT) {
        format!("alt+{base}")
    } else if modifiers.contains(KeyModifiers::SHIFT) && base.chars().count() > 1 {
        format!("shift+{base}")
    } else {
        base
    }
}

/// 查自定义键位。未安装或没命中返回 `None`。
fn custom_action(key: KeyEvent) -> Option<Action> {
    let table = CUSTOM.get()?;
    table
        .get(&(key.code, key.modifiers))
        // 终端对 Shift+字母 通常报「大写 Char + SHIFT」，而配置里写的可能是
        // 不带修饰的 "Q"。回退一次，两种写法都能命中。
        .or_else(|| table.get(&(key.code, KeyModifiers::NONE)))
        .copied()
}

pub fn resolve(key: KeyEvent, mode: KeyMode) -> Action {
    // Ctrl+C 在任何模式下都表示「立刻退出」，且不可被自定义覆盖：
    // 它是唯一的强制退出通道，被绑走会让用户在异常时出不来。
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::ForceQuit;
    }

    // 输入态下的「裸字符键」必须让给输入框。
    //
    // 为什么这条要排在自定义键位**前面**：自定义表是全局的，不分模式——
    // 用户按 `seek_forward = "l"` 之后，在搜索框里打 `l` 会触发快进而不是
    // 输入字母，`m`/`h`/`N` 等同理。默认键表本来是对的（`resolve_text_input`
    // 里 `Char(c) if !CONTROL => Action::Char(c)` 吃掉所有字符），但自定义表
    // 先一步拦走了它们，于是“自定义字母键在输入框里全不能打”。
    //
    // 只放行**无修饰键**的字符：`ctrl+n` / `alt+1` 这类仍然走自定义表，
    // 让输入框里的组合键快捷方式继续可用。
    if mode == KeyMode::TextInput
        && matches!(key.code, KeyCode::Char(_))
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return resolve_text_input(key);
    }

    // 自定义键位优先于默认键表
    if let Some(action) = custom_action(key) {
        return action;
    }

    match mode {
        KeyMode::TextInput => resolve_text_input(key),
        KeyMode::Normal => resolve_normal(key),
    }
}

/// 输入态键表：文本编辑 + 提交/取消 + 上下移动列表。
fn resolve_text_input(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => Action::Char(c),
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Left => Action::CursorLeft,
        KeyCode::Right => Action::CursorRight,
        KeyCode::Home => Action::CursorHome,
        KeyCode::End => Action::CursorEnd,
        KeyCode::Enter => Action::Submit,
        KeyCode::Esc => Action::Cancel,
        // 单行输入框里上下键没有编辑含义。早期把它们映射成 `None`，结果是
        // 用户在输入态按 ↑/↓「完全没有反应」，误以为程序卡死。改为移动列表选中：
        // 边输入边用方向键挑结果，本来就该是顺手的操作。
        KeyCode::Up => Action::MoveUp,
        KeyCode::Down => Action::MoveDown,
        _ => Action::None,
    }
}

/// 浏览态键表。
fn resolve_normal(key: KeyEvent) -> Action {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    match key.code {
        // ---- 退出与帮助 ----
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('Q') => Action::ForceQuit,
        KeyCode::Char('?') | KeyCode::Char('h') | KeyCode::F(1) => Action::Help,

        // ---- 导航：同时提供 vim 风格与方向键 ----
        KeyCode::Char('k') | KeyCode::Up => Action::MoveUp,
        KeyCode::Char('j') | KeyCode::Down => Action::MoveDown,
        KeyCode::Char('g') | KeyCode::Home => Action::MoveTop,
        KeyCode::Char('G') | KeyCode::End => Action::MoveBottom,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Tab => Action::FocusNext,
        KeyCode::BackTab => Action::FocusPrev,
        KeyCode::Enter => Action::Submit,
        KeyCode::Esc => Action::Cancel,

        // ---- 播放控制 ----
        KeyCode::Char(' ') => Action::PlayPause,
        KeyCode::Char('n') => Action::Next,
        KeyCode::Char('p') => Action::Prev,
        // 左右方向键做 5 秒快进/快退，与大多数播放器一致
        KeyCode::Right => Action::SeekForward,
        KeyCode::Left => Action::SeekBackward,
        KeyCode::Char('+') | KeyCode::Char('=') => Action::VolumeUp,
        KeyCode::Char('-') => Action::VolumeDown,
        KeyCode::Char('m') => Action::ToggleMute,
        KeyCode::Char('r') => Action::CyclePlaybackMode,
        // y = quality：换个不冲突的键，r 已经被播放模式占了
        KeyCode::Char('y') => Action::CycleQuality,
        KeyCode::Char('l') => Action::ToggleLyricPanel,
        // 歌词微调：方括号在键盘上紧邻，符合直觉
        KeyCode::Char(']') => Action::LyricAdvance,
        KeyCode::Char('[') => Action::LyricDelay,

        // ---- 业务 ----
        KeyCode::Char('/') => Action::OpenSearch,
        // 设置页的直达键。它现在也够得到数字键 9，但逗号仍在多数键盘上挨着
        // m/n 那一排、不与任何现有键冲突，留着当第二条路径。
        KeyCode::Char(',') => Action::OpenSettings,
        KeyCode::Char('R') => Action::Reload,
        KeyCode::Char('a') => Action::QueueAppend,
        KeyCode::Char('A') => Action::AddAllToQueue,
        KeyCode::Char('i') => Action::QueuePlayNext,
        KeyCode::Char('x') => Action::RemoveFromQueue,
        KeyCode::Char('X') => Action::ClearQueue,
        KeyCode::Char('C') => Action::ClearCache,
        // 右键菜单的键盘入口。m 已被静音占用，用分号——它在多数程序里就是
        // 「命令/菜单」那个键
        KeyCode::Char(';') => Action::ContextMenu,
        KeyCode::Char('M') => Action::LoadMoreSearch,
        KeyCode::Char('o') => Action::ToggleSortOrder,
        KeyCode::Char('b') => Action::OpenRanks,
        KeyCode::Char('c') => Action::OpenCloud,
        KeyCode::Char('L') => Action::Login,
        // 大写 V：小写 v 已经是「切换音源」了，而领取 VIP 正是要配合音源用
        // （只有概念版能领），放同一个键上容易误触。
        KeyCode::Char('V') => Action::ClaimVip,
        KeyCode::Char('f') => Action::CycleArtistFilter,
        KeyCode::Char('S') => Action::SyncToCloud,
        KeyCode::Char('s') => Action::AddToCloud,
        KeyCode::Char('d') => Action::RemoveFromCloud,
        KeyCode::Char('D') => Action::DeleteCloudPlaylist,
        KeyCode::Char('N') => Action::NewCloudPlaylist,
        KeyCode::Char('\\') => Action::ToggleSidebar,
        // `v` 不再循环切换音源：改为打开音源管理页，在那里面挑更直观
        KeyCode::Char('v') => Action::SwitchSource,
        // 大写 W：下载当前播放歌曲到设置页里选的目录。
        // 用大写是为了避开键盘上更常用的小写字母，避免和未来可能加的快捷键冲突。
        KeyCode::Char('W') => Action::DownloadCurrent,
        // 音源管理页专用。用大写是为了避开已经占满的小写键位。
        KeyCode::Char('E') => Action::SetDefaultSource,
        KeyCode::Char('K') => Action::RaiseSourcePriority,
        KeyCode::Char('J') => Action::LowerSourcePriority,

        // 0-9 的语义按焦点决定：侧边栏里是切标签页，列表里是跳到对应项。
        // keymap 只产出「按了数字几」，具体含义交给 App 层判断。
        //
        // `0` 必须在内：它是第 10 个标签页（可视化）的键，`Tab::number_key()`
        // 会把它印在侧边栏上。原先这里写的是 `'1'..='9'`，于是侧边栏显示
        // 「0 可视化」、按下去却毫无反应——文档和界面都在骗人。
        KeyCode::Char(digit @ '0'..='9') => Action::Digit(digit as u8 - b'0'),
        // Shift 组合的字母键已被上面的显式分支吃掉，这里兜底避免误触发
        KeyCode::Char(_) if shift => Action::None,
        _ => Action::None,
    }
}

/// 供帮助面板展示的快捷键表：(按键, 说明, 分类)。
pub const CHEATSHEET: &[(&str, &str, &str)] = &[
    ("q / Q", "退出 / 强制退出", "全局"),
    ("? / F1", "打开本帮助", "全局"),
    ("\\", "显示/隐藏侧边栏", "全局"),
    ("v", "切换音源", "全局"),
    // 数字键 1-9 加 0 覆盖全部 10 个标签页，落点见 `Tab::ALL`。
    // **只切标签页，不做「列表内跳到第 N 项」**——焦点通常在歌曲列表上，
    // 若数字键改成跳列表项，最常用的「按数字切页」就没了。
    // 改标签页数量时必须同步这里，否则帮助面板会指向一个不存在的键，
    // 用户照着按却没反应，很难自查。
    ("1..9 / 0", "切换标签页", "导航"),
    ("Tab / S-Tab", "切换焦点区域", "导航"),
    ("j / k", "上下移动", "导航"),
    ("g / G", "跳到首行 / 末行", "导航"),
    ("PgUp / PgDn", "翻页", "导航"),
    ("Enter", "播放选中歌曲 / 进入", "导航"),
    ("Esc", "返回上一层", "导航"),
    ("/", "搜索", "业务"),
    (",", "打开设置页", "业务"),
    ("R", "刷新当前列表（绕过服务端 2 分钟缓存）", "业务"),
    ("M", "搜索结果加载下一页", "业务"),
    (";", "打开歌曲右键菜单（也可用鼠标右键）", "业务"),
    ("a / i", "加入队列 / 插播到下一首", "业务"),
    ("A", "把当前列表全部加入队列", "业务"),
    ("x / X", "移出队列 / 清空队列（需确认）", "业务"),
    ("C", "清空音频缓存（需确认）", "业务"),
    ("o", "正序 ↔ 倒序（最后一首在最上）", "业务"),
    ("y", "切换音质（下一首生效）", "业务"),
    ("b / c", "排行榜 / 云端歌单", "业务"),
    ("L", "扫码登录（酷狗 / 网易云）", "业务"),
    ("V", "领取今日概念版 VIP（仅概念版音源）", "业务"),
    ("f", "歌手地区筛选（在歌手页）", "业务"),
    ("E / J / K", "音源设为默认 / 调优先级（在音源页）", "业务"),
    ("s / S", "收藏单曲到云端 / 把整个队列同步到云端", "业务"),
    ("d / D", "从云端歌单移除 / 删除歌单（需确认）", "业务"),
    ("N", "新建云端歌单", "业务"),
    ("Space", "播放 / 暂停", "播放"),
    ("n / p", "下一首 / 上一首", "播放"),
    (
        "← / →",
        "列表内快退/快进 5 秒；设置页中则是修改选中项",
        "播放",
    ),
    ("+ / -", "音量增减 5%", "播放"),
    ("m", "静音开关", "播放"),
    ("r", "循环播放模式", "播放"),
    ("l", "歌词面板开关", "播放"),
    ("[ / ]", "歌词延后 / 提前 100ms", "播放"),
    ("W", "下载当前播放歌曲到设置里的目录", "播放"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_chars_named_keys_and_modifiers() {
        assert_eq!(
            parse_key("q"),
            Some((KeyCode::Char('q'), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key("Q"),
            Some((KeyCode::Char('Q'), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key("/"),
            Some((KeyCode::Char('/'), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key("space"),
            Some((KeyCode::Char(' '), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key("enter"),
            Some((KeyCode::Enter, KeyModifiers::NONE))
        );
        assert_eq!(parse_key("up"), Some((KeyCode::Up, KeyModifiers::NONE)));
        assert_eq!(parse_key("f1"), Some((KeyCode::F(1), KeyModifiers::NONE)));
        assert_eq!(
            parse_key("ctrl+n"),
            Some((KeyCode::Char('n'), KeyModifiers::CONTROL))
        );
        // 越界的功能键要拒绝，不能悄悄变成别的键
        assert_eq!(parse_key("f99"), None);
        assert_eq!(parse_key("不存在的键"), None);
    }

    /// `f` 必须解析成字母 `f`，不能掉进 F1~F12 那条分支。
    ///
    /// 实测踩到过：F 键那条判的是「长度 ≤ 3 且以 `f` 开头」，`f` 也满足，于是
    /// `f[1..]` 是空串、parse 失败、整条返回 `None`——**只有 `f`（歌手地区筛选）
    /// 没法通过配置重绑**，其余单字母都行。写「面板里的键都真绑过」那条测试时才发现。
    #[test]
    fn single_letter_f_is_not_swallowed_by_the_function_key_branch() {
        assert_eq!(
            parse_key("f"),
            Some((KeyCode::Char('f'), KeyModifiers::NONE)),
            "`f` 是普通字母键，不是功能键"
        );
        // 功能键本身不受影响
        assert_eq!(parse_key("f1"), Some((KeyCode::F(1), KeyModifiers::NONE)));
        assert_eq!(parse_key("f12"), Some((KeyCode::F(12), KeyModifiers::NONE)));
    }

    #[test]
    fn action_names_round_trip() {
        assert_eq!(action_from_name("quit"), Some(Action::Quit));
        assert_eq!(action_from_name("play_pause"), Some(Action::PlayPause));
        assert_eq!(
            action_from_name("set_default_source"),
            Some(Action::SetDefaultSource)
        );
        // 带参数的动作不在映射里：配置文件写不出完整语义
        assert_eq!(action_from_name("switch_tab"), None);
        assert_eq!(action_from_name("不存在的动作"), None);
    }

    /// Ctrl+C 是唯一的强制退出通道，自定义键位不能把它抢走。
    #[test]
    fn ctrl_c_cannot_be_rebound() {
        let forced = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(resolve(forced, KeyMode::Normal), Action::ForceQuit);
        assert_eq!(resolve(forced, KeyMode::TextInput), Action::ForceQuit);
    }

    /// 输入态下，裸字符键必须是「输入字符」，不能被快捷键表抢走。
    ///
    /// 曾经踩到的坑：自定义键位表（`[keymap]`）在模式判断**之前**拦截，
    /// 于是用户把 `seek_forward = "l"`、`prev = "N"` 之后，在搜索框里打
    /// `l` / `N` 变成了快进和上一首——字母根本打不进去。默认键表本来就没事
    /// （`resolve_text_input` 用 `Char(c)` 接住所有字符），是自定义表先一步
    /// 把它们吃掉的。这条测试用默认键表隔着把“输入态优先”这个顺序钉住：
    /// `l` 在浏览态是歌词，在输入态必须是字符 `l`。
    #[test]
    fn text_input_mode_gives_plain_characters_to_the_editor() {
        for c in [
            'h', 'l', 'm', 'n', 'N', 'r', 't', 'u', 'w', 'y', 'q', 'j', 'k', '?', '/',
        ] {
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            assert_eq!(
                resolve(key, KeyMode::TextInput),
                Action::Char(c),
                "输入态下 {c:?} 应当原样插入文本"
            );
        }
    }

    /// 输入态仍要放行带修饰键的绑定：`ctrl+n` 这类不抢字符，照旧生效。
    #[test]
    fn text_input_mode_still_allows_modified_keys() {
        let ctrl_l = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        // 没有自定义表时控制键落入 `resolve_text_input` 的兵底分支；
        // 关键是不能被当成字符 `l`（那会把组合键也当输入）。
        assert_ne!(resolve(ctrl_l, KeyMode::TextInput), Action::Char('l'));
    }

    /// 编辑动作在输入态下必须是编辑语义，而不是播放控制。
    ///
    /// 新建歌单弹窗也归入输入态（`AppState::is_editing`），而它的 buffer 挂在
    /// `TextInput` 上——靠的就是 `←`/`→` 在这里产出 `CursorLeft`/`CursorRight`，
    /// 否则左方向键会变成快退、光标根本动不了。
    #[test]
    fn text_input_mode_maps_arrows_to_cursor_moves() {
        let pairs = [
            (KeyCode::Left, Action::CursorLeft),
            (KeyCode::Right, Action::CursorRight),
            (KeyCode::Home, Action::CursorHome),
            (KeyCode::End, Action::CursorEnd),
            (KeyCode::Delete, Action::Delete),
            (KeyCode::Backspace, Action::Backspace),
        ];
        for (code, expected) in pairs {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            assert_eq!(
                resolve(key, KeyMode::TextInput),
                expected,
                "输入态下 {code:?} 应当是编辑动作"
            );
        }
    }

    /// 10 个数字键必须**全部**能产出 `Action::Digit`，包括 `0`。
    ///
    /// 原先的分支写的是 `'1'..='9'`，把 `0` 漏掉了：侧边栏印着「0 可视化」、
    /// 文档写着「1–9、0」、`Tab::number_key()` 也照常返回 `'0'`，但按下去
    /// 什么都不会发生——界面和文档一起骗人，而且没有任何报错。
    /// 这条测试按 `Tab::ALL` 的落点逐个验，改标签页数量时也会一起被钉住。
    #[test]
    fn every_digit_key_reaches_a_tab() {
        for digit in '0'..='9' {
            let event = KeyEvent::new(KeyCode::Char(digit), KeyModifiers::NONE);
            assert_eq!(
                resolve(event, KeyMode::Normal),
                Action::Digit(digit as u8 - b'0'),
                "「{digit}」应当是切标签页"
            );
        }
    }

    /// 领取 VIP 是**大写** `V`，小写 `v` 仍然是切换音源。
    ///
    /// 两个键挨着、又都和音源相关，最容易在改键位时被合并成一个。
    #[test]
    fn capital_v_claims_vip_and_lowercase_v_switches_source() {
        let capital = KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE);
        assert_eq!(resolve(capital, KeyMode::Normal), Action::ClaimVip);

        let lower = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE);
        assert_eq!(resolve(lower, KeyMode::Normal), Action::SwitchSource);
    }

    /// 帮助面板里写出来的每个键，都必须是**真的绑过的**。
    ///
    /// 面板是用户唯一的速查入口，写一个按下去没反应的键比不写更糟。这条测试把
    /// `CHEATSHEET` 逐条喂给 `resolve`，对不上就失败——加键位时忘了同步面板、
    /// 或面板里留了个早就删掉的键，都会在这里现形。
    ///
    /// **反方向测不了**：`resolve_normal` 是个大 match（还有区间与 guard 分支），
    /// 枚举不出它到底绑了哪些键，所以「绑了但面板没写」只能靠人查。真要做成
    /// 自动的，得先把那张表改成数据——代价大于收益，先不划算。
    #[test]
    fn every_shortcut_shown_in_help_is_actually_bound() {
        let mut unbound = Vec::new();
        for (keys, description, _) in CHEATSHEET {
            // 用 `" / "` 分隔而不是 `"/"`：条目 12 的键就是 `/`（搜索），
            // 按单斜杠切会把它切成两个空串，等于跳过不查。
            for token in keys.split(" / ").map(str::trim).filter(|t| !t.is_empty()) {
                let Some(name) = display_token_to_key_name(token) else {
                    continue;
                };
                let Some((code, modifiers)) = parse_key(&name) else {
                    unbound.push(format!("「{token}」({description}) 连写法都不认识"));
                    continue;
                };
                if resolve(KeyEvent::new(code, modifiers), KeyMode::Normal) == Action::None {
                    unbound.push(format!("「{token}」({description})"));
                }
            }
        }

        assert!(
            unbound.is_empty(),
            "帮助面板里这些键其实没绑定：{}",
            unbound.join("、")
        );
    }

    /// 没装自定义表时，改写函数必须原样返回，不做任何猜测。
    ///
    /// 测试环境里 `CUSTOM` 永远是空的（`OnceLock` 只在 `App::new` 里装一次），
    /// 这正好是「用户没写 `[keymap]`」那条路；有自定义键位的那条路只能在
    /// 真实运行里验，单测装不进表。
    #[test]
    fn display_keys_are_unchanged_without_custom_bindings() {
        for (keys, _, _) in CHEATSHEET {
            assert_eq!(
                display_keys_with_custom(keys),
                *keys,
                "没有自定义键位时「{keys}」不该被改写"
            );
        }
    }

    /// 自定义键位生效时，面板要显示用户实际在按的键。
    ///
    /// 这里直接构造键表喂给内部实现（生产侧的 `CUSTOM` 是 `OnceLock`，
    /// 测试装不进去）；用的是真实配置里那几条，对得上就跑得通。
    #[test]
    fn display_keys_follow_custom_bindings() {
        use std::collections::HashMap;
        let table: HashMap<(KeyCode, KeyModifiers), Action> = [
            (
                (KeyCode::Char('m'), KeyModifiers::NONE),
                Action::CyclePlaybackMode,
            ),
            (
                (KeyCode::Char('u'), KeyModifiers::NONE),
                Action::CycleQuality,
            ),
            (
                (KeyCode::Char('t'), KeyModifiers::NONE),
                Action::NewCloudPlaylist,
            ),
            ((KeyCode::Char('N'), KeyModifiers::NONE), Action::Prev),
            ((KeyCode::Char('r'), KeyModifiers::NONE), Action::Reload),
            (
                (KeyCode::Char('h'), KeyModifiers::NONE),
                Action::SeekBackward,
            ),
            (
                (KeyCode::Char('l'), KeyModifiers::NONE),
                Action::SeekForward,
            ),
            (
                (KeyCode::Char('y'), KeyModifiers::NONE),
                Action::ToggleLyricPanel,
            ),
            ((KeyCode::Char('w'), KeyModifiers::NONE), Action::ToggleMute),
        ]
        .into_iter()
        .collect();

        let cases = [
            ("R", "r", "刷新改成小写 r"),
            ("y", "u", "音质改成 u"),
            ("N", "t", "新建歌单改成 t"),
            ("m", "w", "静音改成 w"),
            ("r", "m", "循环模式改成 m"),
            ("l", "y", "歌词面板改成 y"),
            ("n / p", "n / N", "只有上一首被改，下一首保持 n"),
            ("← / →", "h / l", "快退快进改成 h / l"),
        ];
        for (panel, expected, why) in cases {
            assert_eq!(
                display_keys_with(panel, Some(&table)),
                expected,
                "{why}：面板「{panel}」应显示为「{expected}」"
            );
        }

        // 没被改过的条目不能被动到
        assert_eq!(display_keys_with("Space", Some(&table)), "Space");
        assert_eq!(display_keys_with("/", Some(&table)), "/");
    }

    /// `key_hint_for` 同样要在无自定义表时退回默认键。
    #[test]
    fn key_hint_falls_back_to_the_default_key() {
        assert_eq!(key_hint_for("R"), "R");
        assert_eq!(key_hint_for("Space"), "Space");
        assert_eq!(key_hint_for("/"), "/");
    }

    /// 面板 token 的翻译规则：别名、范围、保留大小写。
    #[test]
    fn display_tokens_translate_to_keymap_spelling() {
        assert_eq!(display_token_to_key_name("←").as_deref(), Some("left"));
        assert_eq!(
            display_token_to_key_name("S-Tab").as_deref(),
            Some("backtab")
        );
        assert_eq!(display_token_to_key_name("Enter").as_deref(), Some("enter"));
        // 单个字符保留大小写（`q` 与 `Q` 是两个动作）
        assert_eq!(display_token_to_key_name("Q").as_deref(), Some("Q"));
        // 范围写法不是具体按键，必须跳过
        assert_eq!(display_token_to_key_name("1..9"), None);
    }

    /// 按键 → 显示名的反方向渲染（供反查自定义键位时展示）。
    #[test]
    fn key_names_render_back_to_display_form() {
        assert_eq!(render_key_name(KeyCode::Left, KeyModifiers::NONE), "←");
        assert_eq!(
            render_key_name(KeyCode::BackTab, KeyModifiers::NONE),
            "S-Tab"
        );
        assert_eq!(
            render_key_name(KeyCode::Char(' '), KeyModifiers::NONE),
            "Space"
        );
        assert_eq!(render_key_name(KeyCode::Char('r'), KeyModifiers::NONE), "r");
        assert_eq!(render_key_name(KeyCode::F(1), KeyModifiers::NONE), "F1");
        assert_eq!(
            render_key_name(KeyCode::Char('n'), KeyModifiers::CONTROL),
            "ctrl+n"
        );
    }
}
