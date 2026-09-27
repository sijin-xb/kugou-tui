//! 应用状态。
//!
//! 状态是**纯数据**：不含网络句柄、不含渲染代码。`update` 负责改它，`ui` 负责读它。
//! 这条分界线让「按键 → 状态变化」和「状态 → 屏幕」都能单独推理。
//!
//! 列表选中态直接用 ratatui 的 [`ListState`]，因为滚动偏移需要跨帧保持——
//! 每帧重建状态会让长列表的滚动位置反复归零。

use ratatui::layout::Rect;
use ratatui::widgets::ListState;

use crate::api::model::{Artist, Lyric, Playlist, RankBoard, Song};
use crate::audio::engine::PlaybackState;
use crate::config::Config;

// ============================================================================
// 导航
// ============================================================================

/// 顶层标签页。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    /// 首页：正在播放的总览——封面 + 曲目信息 + 歌词。
    ///
    /// 这是默认落点，也是侧边栏的第一项：打开播放器最想看到的是「现在在放什么」，
    /// 而不是一个空的搜索框。
    #[default]
    Home,
    Search,
    Playlists,
    Artists,
    Ranks,
    Cloud,
    /// 播放队列。原先挤在侧边栏/弹窗里，独立成页后能看全、能翻页。
    Queue,
    /// 音频可视化。不承载列表，整块主区都用来画实时频谱。
    Visualizer,
    /// 音源管理：启用/禁用、设默认、调优先级、查看可用状态。
    Sources,
    /// 设置：主题、音质、播放模式、缓存……集中改配置的地方。
    Settings,
}

impl Tab {
    /// 全部标签页，顺序**就是数字键 1-9 与 0 的落点**。
    ///
    /// 原先「歌词」与「封面」占着下标 7、8（数字键 8、9），删掉这两页之后如果
    /// 直接顺延，`可视化` 会被从 `0` 顶到 `8`、`音源`/`设置` 也跟着往前挪——
    /// 老用户已经形成的肌肉记忆会全乱。所以这里把 `可视化` 留在下标 9（仍然是
    /// `0` 键），让 `音源`/`设置` 去填空出来的 8、9 两格：
    /// **1-7 与 0 一个都没动**，只有原先被两页占用的 8、9 换了主人。
    pub const ALL: [Tab; 10] = [
        Tab::Home,       // 1
        Tab::Search,     // 2
        Tab::Playlists,  // 3
        Tab::Artists,    // 4
        Tab::Ranks,      // 5
        Tab::Cloud,      // 6
        Tab::Queue,      // 7
        Tab::Sources,    // 8
        Tab::Settings,   // 9
        Tab::Visualizer, // 0
    ];

    /// 数字键能直接够到的标签页数量（1-9 加 0）。
    ///
    /// 目前正好等于 [`Self::ALL`] 的长度，所以每个标签都够得到；留着它是为了
    /// 以后再加标签页时，新页默认落在数字键之外，而不是悄悄挤掉某个键。
    pub const NUMBERED: usize = 10;

    /// 侧边栏的显示顺序：按 [`Self::group`] 归类排好。
    ///
    /// 与 `ALL` **故意不同**——`ALL` 的顺序决定数字键 1-9/0 的落点，动它会让
    /// 肌肉记忆全乱；而平铺 10 个标签的侧边栏像一堵文字墙。所以这里只改显示
    /// 顺序，并在每个标签前标出它的数字键，用户照着按不会错。
    ///
    /// 「正在播放」整组排在最前：首页是默认落点，也是用得最多的一页，压在
    /// 列表页下面每按一次上下键都要路过一串才发现「不太舒服」。
    pub const SIDEBAR_ORDER: [Tab; 10] = [
        // 正在播放
        Tab::Home,
        Tab::Visualizer,
        // 发现
        Tab::Search,
        Tab::Playlists,
        Tab::Artists,
        Tab::Ranks,
        // 我的
        Tab::Cloud,
        Tab::Queue,
        // 设置
        Tab::Sources,
        Tab::Settings,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Home => "首页",
            Self::Search => "搜索",
            Self::Playlists => "歌单",
            Self::Artists => "歌手",
            Self::Ranks => "排行榜",
            Self::Cloud => "云端",
            Self::Visualizer => "可视化",
            Self::Queue => "队列",
            Self::Sources => "音源",
            Self::Settings => "设置",
        }
    }

    /// 侧边栏图标。Nerd Font 与 ASCII 的取舍见 [`crate::ui::icons`]。
    pub fn icon(self) -> &'static str {
        use crate::ui::icons;
        match self {
            Self::Home => icons::home(),
            Self::Search => icons::search(),
            Self::Playlists => icons::playlists(),
            Self::Artists => icons::artist(),
            Self::Ranks => icons::rank(),
            Self::Cloud => icons::cloud(),
            Self::Queue => icons::queue(),
            Self::Visualizer => icons::visualizer(),
            Self::Sources => icons::sources(),
            Self::Settings => icons::settings(),
        }
    }

    /// 侧边栏分组标题。10 个标签平铺会让侧边栏像一堵文字墙，分组后才扫得动。
    ///
    /// 分组**不改变** `ALL` 的顺序——顺序决定数字键 1-9/0 的落点，动了会让
    /// 用户肌肉记忆全乱。这里只是显示时插一行标题。
    pub fn group(self) -> &'static str {
        match self {
            Self::Home | Self::Visualizer => "正在播放",
            Self::Search | Self::Playlists | Self::Artists | Self::Ranks => "发现",
            Self::Cloud | Self::Queue => "我的",
            Self::Sources | Self::Settings => "设置",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    /// 该标签对应的数字键（`1`..`9` / `0`），超出了返回 `None`。
    ///
    /// 侧边栏按分组重排后显示顺序与数字键落点不再一致，得把键标出来。
    pub fn number_key(self) -> Option<char> {
        let index = self.index();
        if index >= Self::NUMBERED {
            return None;
        }
        // 第 10 个（下标 9）是 0 键
        Some(if index == 9 {
            '0'
        } else {
            (b'1' + index as u8) as char
        })
    }

    /// 侧边栏与状态栏共用的「第 N 个标签」文本。
    pub fn position_text(self) -> String {
        format!("{}/{}", self.index() + 1, Self::ALL.len())
    }

    /// 数字键 → 标签页。`0` 表示第 10 个（可视化），其余按 1 基索引。
    pub fn from_number(number: u8) -> Option<Self> {
        let index = if number == 0 {
            9
        } else {
            number.checked_sub(1)? as usize
        };
        // 只够到 NUMBERED 范围内的页面；以后新增的标签页默认落在数字键之外
        if index >= Self::NUMBERED {
            return None;
        }
        Self::ALL.get(index).copied()
    }

    /// 侧边栏第 `index` 项对应的标签页（鼠标点击用）。
    ///
    /// 走的是**显示顺序** [`Self::SIDEBAR_ORDER`]，不是数字键落点：鼠标不受
    /// 键盘上那十个键的限制——列表里点得到第几项，就该切到第几页。
    pub fn from_sidebar_index(index: usize) -> Option<Self> {
        Self::SIDEBAR_ORDER.get(index).copied()
    }
}

/// 当前获得按键的面板。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// 左侧标签/状态栏。
    Sidebar,
    /// 主区域：搜索输入框，或歌单/歌手/榜单的条目列表。
    #[default]
    Primary,
    /// 次区域：歌曲列表。
    Secondary,
    /// 播放队列。
    Queue,
}

/// 鼠标可命中的区域。
///
/// 渲染时由 ui 层回填。主循环拿它把「屏幕坐标」翻译成「第几行」，从而支持
/// 点击选中、双击激活、滚轮翻页。之所以放在状态里而不是 ui 层内部，是因为
/// 事件处理在 app 层，必须能读到本帧的布局结果。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    /// 侧边栏标签，值为它在 [`Tab::SIDEBAR_ORDER`]（显示顺序）里的位置。
    Tab(usize),
    /// 主区的「条目列表」（歌单/歌手/榜单）。
    Entries,
    /// 主区的「歌曲列表」。
    Songs,
    /// 播放队列。
    Queue,
    /// 播放条的进度条区域：点击可跳转进度。
    Progress,
    /// 歌词面板的一行：点击跳转到这一句的起始时间。
    LyricLine,
    /// 设置页的条目列表。
    Settings,
    /// 「我的资料」里的「领取今日 VIP」那一行。
    VipClaim,
    /// 「我的资料」里资料载入失败后的重试提示行。
    ProfileRetry,
}

#[derive(Debug, Clone, Copy)]
pub struct HitZone {
    pub rect: Rect,
    pub target: HitTarget,
    /// 该列表当前显示的第一行数据下标（来自 ListState 的 offset）。
    pub first_index: usize,
    /// 列表总长度，防止点击到列表末尾之外的空白行时越界。
    pub len: usize,
}

impl HitZone {
    pub fn contains(&self, column: u16, row: u16) -> bool {
        self.rect
            .contains(ratatui::layout::Position::new(column, row))
    }

    /// 屏幕行号 → 数据下标。超出列表范围返回 None。
    pub fn index_at(&self, row: u16) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let offset = row.saturating_sub(self.rect.top()) as usize;
        Some((self.first_index + offset).min(self.len - 1))
    }
}

/// 状态栏消息级别，决定配色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusLevel {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

// ============================================================================
// 通用控件状态
// ============================================================================

/// 单行文本输入。
///
/// `cursor` 是**字符**下标而不是字节下标——中文歌名很常见，用字节下标会在
/// 中间截断 UTF-8 导致 panic。
#[derive(Debug, Default, Clone)]
pub struct TextInput {
    buffer: String,
    cursor: usize,
}

impl TextInput {
    pub fn text(&self) -> &str {
        &self.buffer
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// 光标处的字节偏移，供渲染时定位终端光标。
    pub fn cursor_byte_index(&self) -> usize {
        self.byte_index()
    }

    pub fn set(&mut self, text: impl Into<String>) {
        self.buffer = text.into();
        self.cursor = self.buffer.chars().count();
    }

    pub fn insert(&mut self, character: char) {
        // 换行与控制字符会破坏单行输入框的假设
        if character.is_control() {
            return;
        }
        let index = self.byte_index();
        self.buffer.insert(index, character);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let start = self.byte_index();
        let width = self.buffer[start..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or_default();
        self.buffer.replace_range(start..start + width, "");
    }

    pub fn delete(&mut self) {
        let start = self.byte_index();
        let Some(character) = self.buffer[start..].chars().next() else {
            return;
        };
        self.buffer
            .replace_range(start..start + character.len_utf8(), "");
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.buffer.chars().count());
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buffer.chars().count();
    }

    fn byte_index(&self) -> usize {
        self.buffer
            .char_indices()
            .nth(self.cursor)
            .map(|(index, _)| index)
            .unwrap_or(self.buffer.len())
    }
}

/// 一次列表载入的三态：空闲 / 载入中 / 载入失败。
///
/// # 为什么把 loading 和 error 绑在一起
///
/// 之前它们是散在两个字段上的（`loading: bool` 加渲染时的空态判断），而
/// `loading` 只在成功路径里清零。结果是：任何一次请求失败，面板就**永远停在
/// 「载入中…」**——状态栏报着错，面板里还在转圈，用户既不知道失败了、也不知道
/// 该按什么。三个状态必须由同一处代码迁移，才不会漏掉失败这条边。
#[derive(Debug, Default, Clone)]
pub struct LoadState {
    /// 是否正在载入。
    loading: bool,
    /// 上一次载入失败的原因（面向用户的一行）。
    error: Option<String>,
}

impl LoadState {
    /// 开始一次载入：清掉上一次的失败原因，立起「载入中」。
    pub fn begin(&mut self) {
        self.loading = true;
        self.error = None;
    }

    /// 载入成功：收掉「载入中」，清掉失败原因。
    pub fn succeed(&mut self) {
        self.loading = false;
        self.error = None;
    }

    /// 载入失败：收掉「载入中」，把原因留下来。
    ///
    /// 只写 `loading = false` 是不够的——面板会退回「暂无数据」，用户会以为这份
    /// 数据本来就是空的，而不是没取到。
    pub fn fail(&mut self, reason: impl Into<String>) {
        self.loading = false;
        self.error = Some(reason.into());
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// 上一次失败的原因。`None` 表示没失败过（不代表已经载入完成）。
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// 条目列表（歌单 / 歌手 / 排行榜），带持久化的滚动偏移。
#[derive(Debug)]
pub struct EntryList<T> {
    pub entries: Vec<T>,
    pub cursor: ListState,
    pub load: LoadState,
}

impl<T> Default for EntryList<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            cursor: ListState::default(),
            load: LoadState::default(),
        }
    }
}

impl<T> EntryList<T> {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 直接选中第 `index` 项（鼠标点击用）。下标越界时钳到末尾。
    pub fn select(&mut self, index: usize) {
        if self.entries.is_empty() {
            self.cursor.select(None);
            return;
        }
        self.cursor.select(Some(index.min(self.entries.len() - 1)));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.cursor
            .selected()
            .filter(|index| *index < self.entries.len())
    }

    pub fn selected(&self) -> Option<&T> {
        self.selected_index()
            .and_then(|index| self.entries.get(index))
    }

    /// 用新数据替换列表，并把选中项复位到第一行。
    pub fn replace(&mut self, entries: Vec<T>) {
        self.entries = entries;
        self.load.succeed();
        if self.entries.is_empty() {
            self.cursor.select(None);
        } else {
            self.cursor.select(Some(0));
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        move_selection(&mut self.cursor, self.entries.len(), delta);
    }

    pub fn select_first(&mut self) {
        select_first(&mut self.cursor, self.entries.len());
    }

    pub fn select_last(&mut self) {
        select_last(&mut self.cursor, self.entries.len());
    }
}

/// 歌曲列表。
#[derive(Debug, Default)]
pub struct SongList {
    /// 列表标题，形如「歌单名 · 128 首」。
    pub title: String,
    pub songs: Vec<Song>,
    pub cursor: ListState,
    pub load: LoadState,
    /// 空列表时的提示语。
    ///
    /// 各标签页空态的原因不同（没搜过 / 还没载入 / 筛选无结果），一律显示
    /// 「暂无数据」会让用户不知道下一步该按什么。
    pub empty_hint: String,
}

impl SongList {
    /// 设置空态提示语。载入数据不会覆盖它。
    pub fn set_empty_hint(&mut self, hint: impl Into<String>) {
        self.empty_hint = hint.into();
    }

    /// 当前应显示的空态文案。
    pub fn empty_text(&self) -> &str {
        if self.empty_hint.is_empty() {
            "暂无数据"
        } else {
            &self.empty_hint
        }
    }

    pub fn is_empty(&self) -> bool {
        self.songs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.songs.len()
    }

    /// 反转顺序，并让光标仍停在同一首歌上（下标取镜像）。
    ///
    /// 这里反转的是**存储**，因此播放队列与界面顺序永远一致。
    pub fn toggle_sort(&mut self) {
        self.songs.reverse();
        let len = self.songs.len();
        if len == 0 {
            self.cursor.select(None);
            return;
        }
        if let Some(index) = self.cursor.selected() {
            self.cursor.select(Some((len - 1 - index).min(len - 1)));
        }
    }

    /// 按当前排序方向整理新载入的歌曲。
    pub fn set_songs_sorted(
        &mut self,
        title: impl Into<String>,
        mut songs: Vec<Song>,
        sort_descending: bool,
    ) {
        if sort_descending {
            songs.reverse();
        }
        self.replace(title, songs);
    }

    pub fn select(&mut self, index: usize) {
        if self.songs.is_empty() {
            self.cursor.select(None);
            return;
        }
        self.cursor.select(Some(index.min(self.songs.len() - 1)));
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.cursor
            .selected()
            .filter(|index| *index < self.songs.len())
    }

    pub fn selected(&self) -> Option<&Song> {
        self.selected_index()
            .and_then(|index| self.songs.get(index))
    }

    pub fn replace(&mut self, title: impl Into<String>, songs: Vec<Song>) {
        self.title = title.into();
        self.songs = songs;
        self.load.succeed();
        if self.songs.is_empty() {
            self.cursor.select(None);
        } else {
            self.cursor.select(Some(0));
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        move_selection(&mut self.cursor, self.songs.len(), delta);
    }

    pub fn select_first(&mut self) {
        select_first(&mut self.cursor, self.songs.len());
    }

    pub fn select_last(&mut self) {
        select_last(&mut self.cursor, self.songs.len());
    }
}

/// 选中项移动，自动钳位到合法范围。
///
/// 还没有选中项时（列表刚加载完），向下移动落在首行、向上移动落在末行——
/// 这比「从 0 再位移」更符合直觉：按一下 `j` 就应该选中第一项。
pub fn move_selection(state: &mut ListState, len: usize, delta: isize) {
    if len == 0 {
        state.select(None);
        return;
    }

    let next = match state.selected() {
        None => {
            if delta >= 0 {
                0
            } else {
                len - 1
            }
        }
        Some(current) => (current as isize + delta).clamp(0, len as isize - 1) as usize,
    };

    state.select(Some(next));
}

pub fn select_first(state: &mut ListState, len: usize) {
    state.select((len > 0).then_some(0));
}

pub fn select_last(state: &mut ListState, len: usize) {
    state.select((len > 0).then(|| len - 1));
}

// ============================================================================
// 各标签页状态
// ============================================================================

/// 搜索页：输入框 + 结果列表。
#[derive(Debug, Default)]
pub struct SearchPane {
    pub input: TextInput,
    /// 输入框是否正在接收字符。
    pub editing: bool,
    /// 上次真正提交搜索的关键词，用于提示「结果对应的是哪个词」。
    pub submitted: String,
    pub results: SongList,
    /// 已加载到第几页。「加载更多」在此基础上 +1。
    ///
    /// 搜索刻意分页：酷狗只有第 1 页是精确匹配，深页是兜底内容，
    /// 一次取全会把相关结果淹没。想要更多就一页页追加。
    pub page: u32,
    /// 「加载更多」是否在飞。
    ///
    /// 单独一个标志，**不能复用 `results.load`**：那个状态一立起来，面板就会把
    /// 现有结果整屏换成「载入中…」（见 `ui/views/lists.rs`）——翻页时闪一下，
    /// 比多出一页重复的歌更糟。它的作用是挡住连按 `M`：两次按键会基于同一个
    /// `page` 各发一次请求，两条结果都追加就是重复的一页，晚到的那条还会让顺序倒过来。
    pub loading_more: bool,
}

#[derive(Debug, Default)]
pub struct PlaylistPane {
    pub list: EntryList<Playlist>,
    pub songs: SongList,
    /// 歌单广场分类 id，`0` 为推荐。
    pub category: i64,
    /// 当前打开的是哪个歌单。按 `R` 刷新时要连它的歌曲一起重载——只刷左侧
    /// 列表的话，右侧歌曲永远是旧的（与 [`CloudPane::open_playlist`] 同因）。
    pub open_playlist: Option<Playlist>,
}

#[derive(Debug, Default)]
pub struct ArtistPane {
    pub list: EntryList<Artist>,
    pub songs: SongList,
    /// 歌手分类：0 全部 / 1 华语 / 2 欧美 / 3 日韩。
    pub kind: i64,
    /// 当前打开的是哪位歌手。
    ///
    /// 迟到的结果要拿它比对：用户可能已经点了下一位，不比对的话右侧会变成
    /// 「左侧高亮 B、右侧列着 A 的歌」——而且没有任何提示，看起来就是数据错了。
    pub open_artist: Option<Artist>,
}

#[derive(Debug, Default)]
pub struct RankPane {
    pub list: EntryList<RankBoard>,
    pub songs: SongList,
    /// 当前打开的是哪个榜单，理由同 [`ArtistPane::open_artist`]。
    pub open_board: Option<RankBoard>,
}

#[derive(Debug, Default)]
pub struct CloudPane {
    pub list: EntryList<Playlist>,
    pub songs: SongList,
    /// 云端页当前**打开**的歌单。
    ///
    /// 两个用途，都为了「用户正在看哪个歌单就以哪个为准」：
    ///
    /// 1. 判断云端内容变了要不要重载歌曲——只有打开的就是变动的那个才重载，
    ///    否则会把用户正在看的另一个歌单给覆盖掉；
    /// 2. 按 `s` 收藏时的默认目标。**原先用的是 `sync_target`（上次选的那个），
    ///    它未必等于眼前这个歌单**，于是「在《我喜欢》里按 s，歌加到了别的歌单，
    ///    眼前这个当然纹丝不动」。现在优先用打开的这个。
    ///
    /// `None` 表示云端页还没打开任何歌单。
    pub open_playlist: Option<Playlist>,
}

/// 当前封面。
///
/// 刻意**不** derive Debug / Clone：`protocol` 是终端图形协议持有的可变图片
/// 状态，既打不出有用的调试信息，也不该被复制共享。
#[derive(Default)]
pub struct CoverArt {
    /// 封面属于哪首歌（用 hash 标识）。`None` 表示还没有封面。
    pub hash: Option<String>,
    /// 图片真实宽高比（宽/高）。
    ///
    /// 封面不都是正方形——单曲封面多为方图，但歌手照、歌单头图常有 16:9 之类
    /// 的比例。拿不到时按 1.0（方图）。
    pub aspect: f32,
    /// 解码后的原图。
    ///
    /// **必须留着**：封面要按目标区域重新裁剪，而区域会变——改窗口大小、切到别的
    /// 页面都会让它变。`Picker::new_resize_protocol` 会吃掉 image，所以用 `Arc`
    /// 存一份，重建时克隆。
    source: Option<std::sync::Arc<image::DynamicImage>>,
    /// 图形协议的图片状态，由 `ratatui-image` 管理。
    ///
    /// 有它就不用自己往 stdout 写转义序列了——widget 会把图片画进 ratatui 的
    /// Buffer，由框架的 diff 统一决定输出什么：既不会阻塞写入，也不会打乱
    /// 光标跟踪（这两点正是之前卡死与闪烁的根因），而且内容没变时一个字节
    /// 都不会重发（kitty 走「已传输图片 + 占位符」引用）。
    ///
    /// 由 [`Self::fit_to`] 按需构建，不是加载封面时就建好的。
    protocol: Option<ratatui_image::protocol::StatefulProtocol>,
    /// `protocol` 是按什么编出来的。
    ///
    /// 图片协议是按**目标区域的尺寸**编码的：区域或铺满方式一变就必须重编，
    /// 否则图还是上一次的尺寸，画出来会缩在区域一角（「泳池只给左上角注水」
    /// 说的就是这个）。有它才能判断「要不要重编」。
    build: Option<CoverBuild>,
}

/// [`CoverArt::fit_to`] 编协议时的入参与结果。
#[derive(Debug, Clone, Copy, PartialEq)]
struct CoverBuild {
    /// 铺满方式（来自 [`crate::config::CoverFill`]）。
    mode: crate::config::CoverFill,
    /// 调用方请求的区域。
    requested: Rect,
    /// 实际渲染进去的矩形。`CoverFill::Fit` 下它比 `requested` 小（图不铺满）。
    render: Rect,
}

impl CoverArt {
    /// 换一张封面。
    pub fn set_image(&mut self, hash: String, image: image::DynamicImage, aspect: f32) {
        self.hash = Some(hash);
        self.aspect = aspect;
        self.source = Some(std::sync::Arc::new(image));
        self.protocol = None;
        self.build = None;
    }

    /// 是否属于这首歌。
    pub fn belongs_to(&self, hash: &str) -> bool {
        self.hash.as_deref() == Some(hash)
    }

    /// 有没有可以画出来的内容。
    pub fn is_drawable(&self) -> bool {
        self.source.is_some()
    }

    /// 取一份「按 `mode` 铺进 `area`」的图片协议，必要时重新编码。
    ///
    /// 返回（协议, 真正要渲染进去的矩形）。没有原图时返回 `None`。
    ///
    /// 只在区域或铺满方式**变了**的时候重编：区域稳定时一帧都不会重发数据，
    /// 这是「不卡死、不闪」的前提。
    pub fn fit_to(
        &mut self,
        mode: crate::config::CoverFill,
        area: Rect,
        picker: &ratatui_image::picker::Picker,
    ) -> Option<(&mut ratatui_image::protocol::StatefulProtocol, Rect)> {
        let source = self.source.as_ref()?;

        let stale = self
            .build
            .is_none_or(|build| build.mode != mode || build.requested != area);
        if stale {
            let (image, render) =
                crate::ui::views::prepare_cover(source, area, picker.font_size(), mode);
            self.protocol = Some(picker.new_resize_protocol(image));
            self.build = Some(CoverBuild {
                mode,
                requested: area,
                render,
            });
        }

        let render = self.build?.render;
        Some((self.protocol.as_mut()?, render))
    }
}

/// 手动实现：`protocol` 是终端图形协议的状态，打不出有用的调试信息，
/// 其余字段照常输出。`AppState` 的 derive(Debug) 依赖这个。
impl std::fmt::Debug for CoverArt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverArt")
            .field("hash", &self.hash)
            .field("aspect", &self.aspect)
            .field("source", &self.source.is_some())
            .field("protocol", &self.protocol.is_some())
            .finish()
    }
}

/// 登录用户的头像。
///
/// 与 `CoverArt` 分开：封面是当前歌曲的专辑图，切歌就换；头像在登录期间不变。
#[derive(Default)]
pub struct Avatar {
    /// 图形协议的图片状态，由 `ratatui-image` 管理。没取到头像时为 None。
    pub protocol: Option<ratatui_image::protocol::StatefulProtocol>,
}

impl std::fmt::Debug for Avatar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Avatar")
            .field("protocol", &self.protocol.is_some())
            .finish()
    }
}

/// 帮助面板状态。
///
/// # 为什么要滚动
///
/// `CHEATSHEET` 有 38 条，34 行的终端只放得下 26 条。早先这里只有一个 `bool`，
/// 面板是模态的、按键全被吞掉，于是最后 12 条（Space / n·p / ←·→ / +·- / m / r /
/// l / [·] / W——**整块播放控制**）在常见尺寸下永远看不到，也没有任何提示说还有内容。
///
/// 偏移和「打开」这个动作绑在一起：面板没有关闭动画，忘了归零的话第二次打开会
/// 停在上次的位置。
#[derive(Debug, Default)]
pub struct HelpPane {
    open: bool,
    /// 首行在 `CHEATSHEET` 里的下标。
    offset: usize,
    /// 上一帧实际可见的行数，由渲染函数回填——滚动钳位要用它，而视口高度只有
    /// 渲染时才知道。
    viewport: usize,
}

impl HelpPane {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// 打开，并把滚动位置复位到顶部。
    pub fn open(&mut self) {
        self.open = true;
        self.offset = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// 滚动 `delta` 行。偏移会被钳到「最后一行刚好贴住视口底部」。
    pub fn scroll_by(&mut self, delta: isize, total: usize) {
        self.offset = clamp_offset(self.offset as isize + delta, total, self.viewport);
    }

    /// 翻一页。步长取「视口高度 - 1」——留一行重叠，翻页后视线有个锚点。
    pub fn scroll_page(&mut self, direction: isize, total: usize) {
        let step = self.viewport.saturating_sub(1).max(1) as isize;
        self.scroll_by(step * direction.signum(), total);
    }

    pub fn scroll_to_top(&mut self) {
        self.offset = 0;
    }

    pub fn scroll_to_bottom(&mut self, total: usize) {
        self.offset = clamp_offset(isize::MAX, total, self.viewport);
    }

    /// 渲染用：登记本帧的视口行数，返回应该显示的行区间。
    pub fn visible_range(&mut self, total: usize, viewport: usize) -> std::ops::Range<usize> {
        self.viewport = viewport;
        self.offset = clamp_offset(self.offset as isize, total, viewport);
        self.offset..(self.offset + viewport).min(total)
    }
}

/// 把滚动偏移钳进 `0..=(total - viewport)`。视口比内容长时只能贴顶。
fn clamp_offset(offset: isize, total: usize, viewport: usize) -> usize {
    let max = total.saturating_sub(viewport);
    offset.clamp(0, max as isize) as usize
}

/// 与 KuGouMusicApi 的连通性。
///
/// **只能由真实请求的结果驱动。** 启动那一刻程序一个请求都还没发过，所以初始值
/// 是 `Unknown`。早先这里没有这个概念，启动时直接打印「已连接 {base}」——接口
/// 全挂也照样这么说，用户看到「已连接」就把网络问题排除掉了，然后往别处找原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Connection {
    /// 还没发过请求，不知道。
    #[default]
    Unknown,
    /// 最近一次请求成功了（业务错误码也算——那说明服务是通的）。
    Connected,
    /// 最近一次请求是传输层失败：连接被拒 / 超时。
    Unreachable,
}

impl Connection {
    /// 侧边栏「连接」区块标题上的后缀。
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "未验证",
            Self::Connected => "已连通",
            Self::Unreachable => "未连通",
        }
    }
}

/// 歌词换行过渡：一次过渡最多跨多少行。
///
/// 拖动进度条、点歌词行跳转会一次跨几十行，逐行淡过去又慢又晕；超过这个跨度
/// 就直接吸附到目标行（见 [`LyricPane::transition_ms_for`]）。
const LYRIC_ANIM_MAX_LINES: usize = 3;

/// 过渡时长相对「该行到下一行的间隔」的比例。
///
/// 快歌的行只有几百毫秒，按配置上限走会出现「上一次过渡还没走完就该换下一行」——
/// 看着不是顺滑，是拖沓。取 0.55 是留出「过渡结束 → 稳定几帧 → 下一次换行」的
/// 呼吸感；再小就显得急促了。
const LYRIC_ANIM_SPAN_RATIO: f32 = 0.55;

/// 歌词面板状态。
#[derive(Debug, Default)]
pub struct LyricPane {
    /// 当前歌词属于哪首歌，用于丢弃过期的异步结果。
    pub hash: Option<String>,
    pub lyric: Lyric,
    pub load: LoadState,
    /// 已渲染过的当前行下标，用于只在换行时重新计算居中偏移。
    pub active_line: Option<usize>,

    // ---- 换行过渡 ----
    /// 过渡的**旧锚点**（歌词行下标）。`None` 表示这次过渡没有旧行可淡出
    /// （歌曲刚开始唱第一句）。
    pub prev_line: Option<usize>,
    /// 过渡已走过的毫秒数。
    pub transition_ms: f32,
    /// 这次过渡的总时长（毫秒）。`0` 表示不做过渡——此时
    /// [`Self::transition_progress`] 恒为 `1.0`，也就是稳态。
    ///
    /// 默认值就是 `0`，这一点是刻意的：所有既有的渲染测试都只设 `active_line`
    /// 而不碰过渡状态，于是它们断言的仍是稳态配色，一行都不用改。
    pub transition_total_ms: f32,

    /// 本帧显示的第 N 行对应哪个歌词行下标。
    ///
    /// 由 `render_lyric` 回填、[`AppState::begin_frame`] 清空——和 `hit_zones`
    /// 同一套约定：只有渲染层知道行几何（译文/音译会让显示行与歌词行不再一一对应），
    /// 而点击要的是歌词行。
    pub display_line_index: Vec<usize>,
}

impl LyricPane {
    /// 换行过渡的进度 `t ∈ [0, 1]`，`1.0` 表示稳态（没有过渡在进行）。
    ///
    /// 用 ease-out cubic：前 1/3 时间走完约 2/3 的视觉变化，之后缓慢收敛。
    /// 线性缓动在终端里「起步发木」——而这个动画总共只有几帧（200ms / 33ms），
    /// 起步那两帧的差别恰恰最明显。
    pub fn transition_progress(&self) -> f32 {
        if self.transition_total_ms <= 0.0 {
            return 1.0;
        }
        let raw = (self.transition_ms / self.transition_total_ms).clamp(0.0, 1.0);
        1.0 - (1.0 - raw).powi(3)
    }

    /// 推进过渡时钟。
    ///
    /// 时钟来自 `Event::Tick` 的 `elapsed` 累加，而不是 `Instant::now()`——
    /// 与 `advance_visualizer` 同一套做法，好处是测试能喂固定步长。
    pub fn advance_transition(&mut self, elapsed: std::time::Duration) {
        if self.transition_total_ms <= 0.0 || self.transition_ms >= self.transition_total_ms {
            return;
        }
        self.transition_ms += elapsed.as_secs_f32() * 1000.0;
        if self.transition_ms >= self.transition_total_ms {
            // 走完就收尾：清掉旧锚点，渲染层之后走稳态路径（不做浮点插值）。
            self.transition_ms = self.transition_total_ms;
            self.prev_line = None;
        }
    }

    /// 换行：记下旧锚点、把时钟归零。`total_ms` 为 `0` 表示不做过渡。
    pub fn retarget(&mut self, new_line: Option<usize>, total_ms: f32) {
        self.prev_line = self.active_line;
        self.active_line = new_line;
        self.transition_ms = 0.0;
        self.transition_total_ms = total_ms;
    }

    /// 本次换行该用多长的过渡（毫秒）。`0` 表示不做过渡。
    ///
    /// 四道否决，顺序即优先级：
    ///
    /// 1. 用户把 `lyric_anim_ms` 设成 `0`（或开了 `lite_mode` / 16 色模式）；
    /// 2. 新状态是「没有当前行」（停止、重放回前奏）——没有新行可点亮；
    /// 3. 跨行太多（拖动进度条、点行跳转）——逐行淡过去又慢又晕；
    /// 4. 其余按行距自适应，上限是用户配的值。
    pub fn transition_ms_for(&self, config: &Config, duration_ms: u64, next: Option<usize>) -> f32 {
        let cap = config.lyric_anim_ms as f32;
        // `lite_mode` 的卖点就是少重绘；16 色下 `theme::mix` 没有中间色阶可取
        // （只能二选一），淡入会退化成「t 过 0.5 时整块硬翻」，比不动画更怪。
        if cap <= 0.0 || config.lite_mode || config.basic_color {
            return 0.0;
        }
        let Some(next) = next else {
            return 0.0;
        };
        if let Some(prev) = self.active_line
            && next.abs_diff(prev) >= LYRIC_ANIM_MAX_LINES
        {
            return 0.0;
        }
        (self.line_span_ms(next, duration_ms) * LYRIC_ANIM_SPAN_RATIO).min(cap)
    }

    /// 第 `index` 行持续多久（到下一行的时间差）。最后一行用歌曲总时长兜底。
    fn line_span_ms(&self, index: usize, duration_ms: u64) -> f32 {
        let Some(line) = self.lyric.lines.get(index) else {
            return 0.0;
        };
        let next_ms = self
            .lyric
            .lines
            .get(index + 1)
            .map_or_else(|| duration_ms.max(line.time_ms + 1), |next| next.time_ms);
        next_ms.saturating_sub(line.time_ms) as f32
    }

    /// 显示行号 → 歌词行号。越界返回 `None`。
    pub fn line_index_at_display(&self, display: usize) -> Option<usize> {
        self.display_line_index.get(display).copied()
    }

    /// 切歌 / 清空歌词时复位过渡状态：新歌的第一句不该从上一首的某一行淡过来。
    pub fn reset_transition(&mut self) {
        self.active_line = None;
        self.prev_line = None;
        self.transition_ms = 0.0;
        self.transition_total_ms = 0.0;
    }
}

// ============================================================================
// 根状态
// ============================================================================

#[derive(Debug)]
pub struct AppState {
    // ---- 配置与连接 ----
    pub config: Config,
    /// 是否已配置登录 cookie。未登录时云端功能不可用。
    pub logged_in: bool,
    /// 与 API 服务的连通性。由真实请求的结果驱动，见 [`Connection`]。
    pub connection: Connection,
    /// 终端图形能力（封面 / 头像用哪种图片协议、单元格像素尺寸）。
    ///
    /// 放这里而不是 `App` 上：渲染封面时要按目标区域的像素尺寸裁图，而区域只有
    /// 渲染时才知道，所以渲染路径必须能拿到它。它不参与任何后台线程，
    /// 不违反「`Picker` 不是 `Send`」这条约束。
    pub picker: Option<ratatui_image::picker::Picker>,

    // ---- 界面 ----
    pub tab: Tab,
    pub focus: Focus,
    pub sidebar_visible: bool,
    pub help: HelpPane,
    pub show_lyric_panel: bool,
    /// 这一帧歌词**真的画到了屏幕上**。由 `render_lyric` 回填、`begin_frame` 复位。
    ///
    /// 逐字推进要 ~30fps 才顺滑，但默认刷新是 5fps。要不要提速取决于歌词到底
    /// 有没有显示——标签页、歌词面板开关、终端尺寸都会影响它，而渲染层是唯一
    /// 知道真相的地方。让它在渲染时回填，比在 `frame_interval` 里重推一遍布局可靠。
    pub lyric_visible: bool,
    pub should_quit: bool,
    /// 强制退出：跳过配置保存。
    pub force_quit: bool,

    // ---- 各标签页 ----
    pub search: SearchPane,
    pub playlists: PlaylistPane,
    pub artists: ArtistPane,
    pub ranks: RankPane,
    pub cloud: CloudPane,

    // ---- 播放 ----
    pub queue: crate::app::queue::PlayQueue,
    pub queue_cursor: ListState,
    /// 音源管理页里选中的行。
    pub sources_cursor: ListState,
    pub current: Option<Song>,
    /// 当前播放的是否为试听片段。
    ///
    /// 片段播完和整首播完必须区别对待：片段结束是「没权限」，不该被当成正常结束
    /// 而自动跳下一首——否则用户只会看到「听几十秒就跳歌」，不知道是会员没生效。
    pub current_is_trial: bool,
    pub playback: PlaybackState,
    pub position_ms: u64,
    pub duration_ms: u64,
    /// 鼠标当前位置（列, 行）。终端支持鼠标移动上报时才有值，用于悬停反馈。
    pub hover: Option<(u16, u16)>,

    /// 播放电平（0.0~1.0），来自音频线程的真实采样峰值。会随播放逐帧刷新。
    ///
    /// 这是**时域**的——最近若干格子的音量历史，侧边栏那条小跳动条用它。
    pub levels: Vec<f32>,
    /// 当前这段声音的频谱（0.0~1.0），由 FFT 分频得到，**频域**。
    pub spectrum: Vec<f32>,
    /// 平滑后的频谱。原始频谱每帧跳一次，直接画会明显抖动；这里做
    /// 「快起慢落」的缓动后，柱子才跟手又不抖。
    pub smooth_spectrum: Vec<f32>,
    /// 峰值保持：柱顶那条刻度线的高度，比柱子本身落得慢，形成经典频谱的观感。
    pub peak_spectrum: Vec<f32>,
    /// 实际打开的音频输出设备名，由音频线程上报。
    ///
    /// 要把它显示出来，是因为 Linux 上「进度在走、却一点声音都没有」几乎都是
    /// 声音去了另一张卡，而进程本身毫无异常（不报错、状态是播放中）。设备名
    /// 摆在界面上，用户一眼就能发现自己听的不是这张卡。
    pub output_device: String,
    /// 可选的输出设备名，设置页在这里挑。
    ///
    /// 启动时枚举一次就够：插拔声卡是少数情况，真换了大不了重开一次，
    /// 没必要为了它去轮询设备列表。
    pub audio_devices: Vec<String>,
    /// 当前音量，`0.0 ~ 1.0`。
    pub volume: f32,
    /// 静音前的音量，用于 `m` 键还原。
    pub volume_before_mute: Option<f32>,
    pub lyric: LyricPane,

    // ---- 云端同步 ----
    /// 同步目标歌单。在云端标签页选中歌单时自动设置。
    pub sync_target: Option<Playlist>,

    // ---- 状态栏 ----
    pub status: String,
    pub status_level: StatusLevel,
    /// 正在进行的后台任务描述，非空时状态栏显示进度指示。
    pub busy: Option<String>,
    /// 下载进度 `(已下载, 总大小)`。
    pub download_progress: Option<(u64, Option<u64>)>,
    /// 音频缓存已占用字节数。定期测量，不在每帧做目录扫描。
    pub cache_bytes: u64,
    /// 心跳计数，用于把「每 N 拍做一次」的低频任务错开。
    pub ticks: u64,
    /// 上一次取登录二维码 key 的时刻。
    ///
    /// 每次取 key 都会向网易云申请一个新的登录会话。短时间内反复申请（比如
    /// 二维码过期后连按几次 `L`）会被判为「登录频繁」，手机端直接扫不了——
    /// 实测就是这个提示。用它把申请间隔拉住，别把用户的账号搞限流。
    pub last_qr_key_at: Option<std::time::Instant>,
    /// 本帧的鼠标命中区，由 ui 层每帧清空后回填。
    pub hit_zones: Vec<HitZone>,
    /// 歌曲列表的排列顺序。
    ///
    /// `true` = 倒序，即**最后一首排在最上面**（`o` 键切换）。之所以把它做成状态而不是
    /// 在渲染时反转：反转后的顺序必须与播放队列一致，就地反转存储，所有现有的下标逻辑
    /// （选中、播放、鼠标点击）都不用改，不会出现「看到的顺序和播放的顺序不一致」。
    pub sort_descending: bool,
    /// 待确认的危险操作。非空时按键先走确认流程，避免误按一下就清空整个队列。
    pub pending_confirm: Option<ConfirmAction>,
    /// 应用内扫码登录；`None` 表示未在进行登录。
    pub login: Option<LoginState>,
    /// 登录前的音源选择器。非空时它是模态的，会拦截所有按键。
    pub login_picker: Option<LoginPicker>,
    /// 下载到文件夹时的音质选择框（模态）。
    pub quality_picker: Option<QualityPicker>,
    /// 文本输入弹窗；`None` 表示没有弹出的输入框。
    pub prompt: Option<PromptState>,
    /// 歌曲右键菜单；\`None\` 表示没开菜单。
    pub context_menu: Option<ContextMenu>,
    /// 设置页当前选中的条目下标。
    pub settings_cursor: usize,
    /// 当前封面，以及它属于哪首歌（避免切歌后继续显示上一张）。
    pub cover: CoverArt,
    /// 当前账号的会员信息，未登录或未取到时为 None。
    ///
    /// 存结构体而不是拼好的字符串：侧边栏窄、首页宽，两处要的形态不同
    /// （`VipInfo::short_label` / `VipInfo::label`），在这儿定型就没得挑了。
    pub vip_info: Option<crate::api::cloud::VipInfo>,
    /// 上一次领取「概念版」当天 VIP 的日期（`2026-09-23`），随会话持久化。
    ///
    /// 用来做到「每天只领一次」——上游文档写着「尽量别频繁调用」，接口还带风控。
    pub vip_claimed_day: Option<String>,
    /// 正在领 VIP。期间界面上那一行显示「领取中…」，并且挡住重复触发。
    pub vip_claiming: bool,
    /// 当前登录用户的资料（昵称 / 头像 / 等级 / 听歌时长）。
    pub user_info: Option<crate::api::cloud::UserInfo>,
    /// 用户资料的载入状态。
    ///
    /// 和 `user_info` 分开：取不到资料时 `user_info` 仍是 `None`，而界面必须能
    /// 区分「还在取」和「取失败了」——之前只有前者，接口挂掉时首页会永远显示
    /// 「加载中…」，用户没有任何线索。
    pub user_info_load: LoadState,
    /// 登录用户的头像。和 `cover`（当前歌曲专辑图）分开存。
    pub avatar: Avatar,
    /// 会话恢复待续播的位置：(歌曲 hash, 毫秒)。
    ///
    /// 只在播放**这首歌**时才用——用户要是先去播别的，这个位置就该作废，
    /// 否则下次停在任意一首上按播放都会从上次的位置开始。
    pub resume: Option<(String, u64)>,
    /// 上次鼠标点击命中的（区域, 数据下标）。
    last_click: Option<(HitTarget, Option<usize>)>,
    last_click_at: std::time::Instant,
}

/// 登录时的音源选择器。
///
/// 多个音源都能登录后，「按 L 登录哪个」就成了必须回答的问题——登录态是
/// 按音源分开存的，登录前必须选定目标，否则凭据会存错地方。
#[derive(Debug, Clone, Default)]
pub struct LoginPicker {
    /// 候选音源（只列支持登录的）。
    pub candidates: Vec<crate::source::SourceKind>,
    pub cursor: ListState,
}

impl LoginPicker {
    /// 当前选中的音源。
    pub fn selected(&self) -> Option<crate::source::SourceKind> {
        let index = self.cursor.selected().unwrap_or(0);
        self.candidates.get(index).copied()
    }

    /// 上下移动选中。
    pub fn move_by(&mut self, delta: isize) {
        if self.candidates.is_empty() {
            return;
        }
        let len = self.candidates.len();
        let current = self.cursor.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.cursor.select(Some(next));
    }
}

/// 下载歌曲时的音质选择框。
///
/// 和全局 `config.quality` 分开：全局那个是**播放**音质（要照顾流量和缓冲），
/// 下载到文件夹往往想要无损——每次为了下一首歌去改全局设置太别扭。
#[derive(Debug)]
pub struct QualityPicker {
    /// 要下载的歌。
    pub song: crate::api::model::Song,
    /// 候选音质，取自 `config::SUPPORTED_QUALITIES`。
    pub candidates: Vec<String>,
    pub cursor: ListState,
}

impl QualityPicker {
    /// 默认选中项落在**当前全局音质**上：多数人下载就是想要现在听的这个档。
    pub fn new(song: crate::api::model::Song, current: &str) -> Self {
        let candidates: Vec<String> = crate::config::SUPPORTED_QUALITIES
            .iter()
            .map(|quality| (*quality).to_string())
            .collect();
        let start = candidates
            .iter()
            .position(|quality| quality == current)
            .unwrap_or(0);
        let mut cursor = ListState::default();
        cursor.select(Some(start));
        Self {
            song,
            candidates,
            cursor,
        }
    }

    /// 当前选中的音质。
    pub fn selected(&self) -> Option<&str> {
        let index = self.cursor.selected().unwrap_or(0);
        self.candidates.get(index).map(String::as_str)
    }

    /// 上下移动选中。
    pub fn move_by(&mut self, delta: isize) {
        if self.candidates.is_empty() {
            return;
        }
        let len = self.candidates.len();
        let current = self.cursor.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.cursor.select(Some(next));
    }
}

/// 应用内扫码登录的状态。
#[derive(Debug, Clone, Default)]
pub struct LoginState {
    /// 二维码的显示行（每模块两字符宽以修正终端字符的高宽比）。
    pub qr: Vec<String>,
    /// 二维码 key，轮询时用。
    pub key: String,
    /// 当前提示语。
    pub message: String,
    /// 是否已结束（成功或失败）。结束后不再轮询。
    pub finished: bool,
    pub succeeded: bool,
}

impl LoginState {
    /// 弹窗所需的高度：二维码高度 + 提示与内边距。
    pub fn dialog_height(&self) -> u16 {
        let qr_height = self.qr.len() as u16;
        if qr_height == 0 { 5 } else { qr_height + 4 }
    }

    /// 弹窗所需的宽度。
    pub fn dialog_width(&self) -> u16 {
        let qr_width = self
            .qr
            .first()
            .map(|line| line.chars().count())
            .unwrap_or(0) as u16;
        if qr_width == 0 {
            44
        } else {
            qr_width.max(40) + 4
        }
    }
}

/// 需要二次确认的操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    /// 清空播放队列。
    ClearQueue,
    /// 删除（取消收藏）一个云端歌单。
    DeleteCloudPlaylist,
    /// 清空音频缓存目录。
    ClearCache,
    /// 已有登录态时再按 `L`：重新扫码会覆盖现有凭据。
    Relogin,
}

/// 右键菜单里的一项对歌曲的动作。
///
/// 每一项都有对应的快捷键，菜单只是把它们集中到光标处——所以菜单里**不出现
/// 没有对应键位的操作**，否则用户从菜单学会一个动作后，下次想用键盘却找不到。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// 播放（等同 `Enter`）。
    Play,
    /// 加到队列末尾（等同 `a`）。
    QueueAppend,
    /// 插播到下一首（等同 `i`）。
    QueuePlayNext,
    /// 收藏到云端歌单（等同 `s`）。
    AddToCloud,
    /// 下载到本地（等同 `w`）。
    Download,
    /// 从播放队列移除（等同 `x`）。
    RemoveFromQueue,
}

impl MenuAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Play => "播放",
            Self::QueueAppend => "加入队列",
            Self::QueuePlayNext => "插播下一首",
            Self::AddToCloud => "收藏到云端",
            Self::Download => "下载到本地",
            Self::RemoveFromQueue => "从队列移除",
        }
    }

    /// 右侧标出对应的键位，让用户知道下次可以直接按。
    pub fn key_hint(self) -> &'static str {
        match self {
            Self::Play => "Enter",
            Self::QueueAppend => "a",
            Self::QueuePlayNext => "i",
            Self::AddToCloud => "s",
            Self::Download => "W",
            Self::RemoveFromQueue => "x",
        }
    }

    /// 菜单项分两组装：队列内外能做的事不一样。
    ///
    /// `in_queue` 为 true 时（点在播放队列里），「加入队列」没有意义，换成
    /// 「从队列移除」。
    pub fn items_for(in_queue: bool) -> Vec<MenuAction> {
        if in_queue {
            vec![
                Self::Play,
                Self::QueuePlayNext,
                Self::RemoveFromQueue,
                Self::AddToCloud,
                Self::Download,
            ]
        } else {
            vec![
                Self::Play,
                Self::QueueAppend,
                Self::QueuePlayNext,
                Self::AddToCloud,
                Self::Download,
            ]
        }
    }
}

/// 打开的右键菜单。
#[derive(Debug, Clone)]
pub struct ContextMenu {
    /// 菜单作用在哪首歌上。存整首歌而不是下标：切页、刷新列表之后下标会失效，
    /// 而歌本身不会变。
    pub song: crate::api::model::Song,
    pub items: Vec<MenuAction>,
    pub cursor: usize,
}

impl ContextMenu {
    pub fn new(song: crate::api::model::Song, in_queue: bool) -> Self {
        Self {
            song,
            items: MenuAction::items_for(in_queue),
            cursor: 0,
        }
    }

    pub fn selected(&self) -> Option<MenuAction> {
        self.items.get(self.cursor).copied()
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let len = self.items.len() as isize;
        let next = (self.cursor as isize + delta).rem_euclid(len);
        self.cursor = next as usize;
    }
}

impl ConfirmAction {
    pub fn question(self) -> &'static str {
        match self {
            Self::ClearQueue => "清空整个播放队列并停止播放？",
            Self::DeleteCloudPlaylist => "删除这个云端歌单？该操作会取消收藏它。",
            Self::ClearCache => "清空音频缓存？已缓存的歌曲需要重新下载。",
            Self::Relogin => "已登录。重新扫码会覆盖当前凭据，确定要重新登录？",
        }
    }

    pub fn hint(self) -> &'static str {
        "Enter 确认 · Esc 取消"
    }
}

/// 文本输入弹窗。用于「新建歌单」这类需要用户输入名字的操作。
#[derive(Debug, Clone)]
pub struct PromptState {
    pub title: String,
    /// 输入框。用 [`TextInput`] 而不是裸 `String`：光标移动、Delete、Home/End
    /// 都已在里面实现，弹窗只需把 `Action` 转发过来，不必另写一套。
    pub buffer: TextInput,
    pub action: PromptAction,
}

/// 文本输入弹窗提交后的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAction {
    /// 用输入的名字新建云端歌单。
    CreateCloudPlaylist,
}

impl PromptState {
    pub fn new(title: impl Into<String>, action: PromptAction) -> Self {
        Self {
            title: title.into(),
            buffer: TextInput::default(),
            action,
        }
    }
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let logged_in = config.is_logged_in();
        let volume = config.volume;
        let playback_mode = config.playback_mode;

        let mut state = Self {
            config,
            logged_in,
            connection: Connection::default(),
            // 由 `App::new` 探测终端能力后填入
            picker: None,
            tab: Tab::default(),
            focus: Focus::default(),
            last_qr_key_at: None,
            context_menu: None,
            user_info: None,
            user_info_load: LoadState::default(),
            avatar: Avatar::default(),
            resume: None,
            sidebar_visible: true,
            help: HelpPane::default(),
            show_lyric_panel: true,
            lyric_visible: false,
            should_quit: false,
            force_quit: false,
            search: SearchPane::default(),
            playlists: PlaylistPane::default(),
            artists: ArtistPane::default(),
            ranks: RankPane::default(),
            cloud: CloudPane::default(),
            queue: crate::app::queue::PlayQueue::new(playback_mode),
            queue_cursor: ListState::default(),
            sources_cursor: ListState::default(),
            current: None,
            current_is_trial: false,
            playback: PlaybackState::Stopped,
            position_ms: 0,
            hover: None,
            levels: Vec::new(),
            spectrum: Vec::new(),
            smooth_spectrum: Vec::new(),
            peak_spectrum: Vec::new(),
            duration_ms: 0,
            volume,
            output_device: String::new(),
            audio_devices: Vec::new(),
            volume_before_mute: None,
            lyric: LyricPane::default(),
            cover: CoverArt::default(),
            settings_cursor: 0,
            sync_target: None,
            status: "按 / 搜索，或按 2-5 浏览歌单/歌手/排行榜/云端".to_string(),
            status_level: StatusLevel::Info,
            busy: None,
            download_progress: None,
            cache_bytes: 0,
            ticks: 0,
            hit_zones: Vec::new(),
            last_click: None,
            last_click_at: std::time::Instant::now(),
            sort_descending: true,
            pending_confirm: None,
            login: None,
            login_picker: None,
            quality_picker: None,
            prompt: None,
            vip_info: None,
            vip_claimed_day: None,
            vip_claiming: false,
        };

        state.apply_empty_hints();
        state
    }

    /// 供 ui 层在每帧开始时调用：清掉上一帧的命中区。
    pub fn begin_frame(&mut self) {
        self.hit_zones.clear();
        // 同 `hit_zones`：本帧的歌词行映射只对「这一帧画出来的东西」有效，
        // 留着上一帧的会让点击跳到已经滚走的那一句。
        self.lyric.display_line_index.clear();
        // 每帧先当作「没画歌词」，由 `render_lyric` 在真的画出内容时置位。
        // 不复位的话，切走标签页之后还会一直按 30fps 重绘。
        self.lyric_visible = false;
    }

    /// ui 层登记一个可命中区域。
    pub fn add_hit_zone(&mut self, rect: Rect, target: HitTarget, first_index: usize, len: usize) {
        self.hit_zones.push(HitZone {
            rect,
            target,
            first_index,
            len,
        });
    }

    /// 找出包含指定屏幕坐标的命中区（后登记的优先，这样小区域能覆盖大区域）。
    pub fn hit_test(&self, column: u16, row: u16) -> Option<HitZone> {
        self.hit_zones
            .iter()
            .rev()
            .find(|zone| zone.contains(column, row))
            .copied()
    }

    /// 上一次鼠标点击命中的（区域, 数据下标），用于识别双击。
    pub fn set_last_click(&mut self, target: HitTarget, index: Option<usize>) {
        self.last_click = Some((target, index));
        self.last_click_at = std::time::Instant::now();
    }

    /// 判断本次点击是否是双击（同一行、500ms 内）。
    pub fn is_double_click(&self, target: HitTarget, index: Option<usize>) -> bool {
        match self.last_click {
            Some((last_target, last_index)) => {
                last_target == target
                    && last_index == index
                    && self.last_click_at.elapsed() < std::time::Duration::from_millis(500)
            }
            None => false,
        }
    }

    /// 给各标签页的歌曲列表设置空态提示语。
    ///
    /// 启动时停在搜索页且结果为空——此时按上下键本来就无事可做，如果界面只写
    /// 「暂无数据」，用户既看不出「按键无效」还是「没有内容」，也不知道下一步
    /// 该按什么。把下一步动作直接写在空态里，是成本最低的可用性修复。
    fn apply_empty_hints(&mut self) {
        self.search
            .results
            .set_empty_hint("按 / 输入关键词，Enter 开始搜索");
        self.playlists
            .songs
            .set_empty_hint("选中歌单后按 Enter 载入歌曲");
        self.artists
            .songs
            .set_empty_hint("选中歌手后按 Enter 载入歌曲");
        self.ranks
            .songs
            .set_empty_hint("选中榜单后按 Enter 载入歌曲");
        self.cloud
            .songs
            .set_empty_hint("选中云端歌单后按 Enter 载入歌曲");
    }

    /// 选中「条目列表」（歌单/歌手/榜单）的第 `index` 项。
    pub fn select_entry_index(&mut self, index: usize) {
        match self.tab {
            Tab::Playlists => self.playlists.list.select(index),
            Tab::Artists => self.artists.list.select(index),
            Tab::Ranks => self.ranks.list.select(index),
            Tab::Cloud => self.cloud.list.select(index),
            // 音源页的「条目」就是音源，点哪行选哪个
            Tab::Sources => {
                let len = self.config.sources.ordered().len();
                if index < len {
                    self.sources_cursor.select(Some(index));
                }
            }
            // 其余页面没有条目列表
            Tab::Search | Tab::Home | Tab::Queue | Tab::Visualizer | Tab::Settings => {}
        }
    }

    /// 当前标签页的歌曲列表。可视化页没有列表，返回 `None`。
    ///
    /// # 为什么要有这一层
    ///
    /// 「当前标签页该操作哪个歌曲列表」的分派原先散落在 6 处（`state.rs` 的
    /// `select_song_index` / `current_tab_songs`、`ui/mod.rs` 的 `songs_offset` /
    /// `songs_len`、`update.rs` 的 `move_selection` / `move_selection_edge`），
    /// 每处都是一份同样的 6 分支 match。新增标签页时漏改一处，表现就是「这个页里
    /// 方向键没反应」——可视化页就踩过一次。收敛到这里之后只剩这一份。
    pub fn songs(&self) -> Option<&SongList> {
        match self.tab {
            Tab::Search => Some(&self.search.results),
            Tab::Playlists => Some(&self.playlists.songs),
            Tab::Artists => Some(&self.artists.songs),
            Tab::Ranks => Some(&self.ranks.songs),
            Tab::Cloud => Some(&self.cloud.songs),
            Tab::Home | Tab::Queue | Tab::Visualizer | Tab::Sources | Tab::Settings => None,
        }
    }

    /// 同上，可变版本。
    pub fn songs_mut(&mut self) -> Option<&mut SongList> {
        match self.tab {
            Tab::Search => Some(&mut self.search.results),
            Tab::Playlists => Some(&mut self.playlists.songs),
            Tab::Artists => Some(&mut self.artists.songs),
            Tab::Ranks => Some(&mut self.ranks.songs),
            Tab::Cloud => Some(&mut self.cloud.songs),
            Tab::Home | Tab::Queue | Tab::Visualizer | Tab::Sources | Tab::Settings => None,
        }
    }

    /// 选中「歌曲列表」的第 `index` 项。
    pub fn select_song_index(&mut self, index: usize) {
        if let Some(list) = self.songs_mut() {
            list.select(index);
        }
    }

    /// 写入状态栏消息。
    pub fn notify(&mut self, level: StatusLevel, message: impl Into<String>) {
        self.status = message.into();
        self.status_level = level;
    }

    pub fn info(&mut self, message: impl Into<String>) {
        self.notify(StatusLevel::Info, message);
    }

    pub fn success(&mut self, message: impl Into<String>) {
        self.notify(StatusLevel::Success, message);
    }

    pub fn warn(&mut self, message: impl Into<String>) {
        self.notify(StatusLevel::Warning, message);
    }

    pub fn error(&mut self, message: impl Into<String>) {
        self.notify(StatusLevel::Error, message);
    }

    /// 当前是否处于「输入框吃字符」的状态。
    ///
    /// 两个输入点都要算进来：搜索框（`search.editing`）和新建歌单的
    /// 文本弹窗（`prompt`）。之前只算了前者，而 prompt 走的是 `KeyMode::Normal`
    /// ——于是它在 `resolve` 里拿不到 `Action::Char`，字母全被快捷键表吃走，
    /// 连 `←`/`→` 也变成了快进快退（prompt 分支不处理就被 `_ => {}` 吞掉，
    /// 光标根本移不动）。把 prompt 归入输入态之后，两者共用同一套
    /// `resolve_text_input` 键表（字符、退格、Delete、Home/End、光标、Enter/Esc）。
    pub fn is_editing(&self) -> bool {
        self.search.editing || self.prompt.is_some()
    }

    /// 播放进度，`0.0 ~ 1.0`。时长为 0 时返回 0，避免除零。
    pub fn progress_ratio(&self) -> f64 {
        if self.duration_ms == 0 {
            return 0.0;
        }
        (self.position_ms as f64 / self.duration_ms as f64).clamp(0.0, 1.0)
    }

    pub fn is_muted(&self) -> bool {
        self.volume <= f32::EPSILON
    }

    /// 状态栏右侧的忙碌指示文本，附带下载百分比。
    pub fn busy_label(&self) -> Option<String> {
        let busy = self.busy.as_ref()?;
        match self.download_progress {
            Some((received, Some(total))) if total > 0 => {
                let percent = (received as f64 / total as f64 * 100.0).min(100.0);
                Some(format!("{busy} {percent:.0}%"))
            }
            // 服务端没给 Content-Length，或者长度为 0：退回按字节数显示
            Some((received, _)) => Some(format!("{busy} {:.1} MiB", received as f64 / 1_048_576.0)),
            None => Some(busy.clone()),
        }
    }

    /// 当前焦点面板对应的歌曲列表（如果该面板确实在展示歌曲）。
    /// 当前「选中」的那首歌——给 `s`（收藏到云端）、`a`/`i`（加入队列）这类
    /// 作用于单曲的动作用。
    ///
    /// # 为什么不能只用 `focused_songs`
    ///
    /// `focused_songs()` 要求焦点落在**歌曲列表**那一栏，于是：
    ///
    /// * 焦点停在歌单 / 歌手 / 排行榜的**上半部分**时它返回 `None`；
    /// * 队列页压根不在它的匹配里（`Tab::Queue` 没有对应分支）。
    ///
    /// 两种情况下用户明明选中了一首歌，按 `s` 却报「当前没有选中的歌曲」。
    /// 所以这里按优先级依次找：
    ///
    /// 1. 焦点在队列面板 → 队列里选中的那首；
    /// 2. 当前标签页的歌曲列表（不管焦点在哪一栏）→ 它的选中项；
    /// 3. 队列页 → 队列当前这首；
    /// 4. 都没有（比如在首页、歌词页）→ 正在播放的这首。
    ///
    /// 最后这条兜底很重要：用户按 `s` 时心里想的通常是「把正在听的这首收了」，
    /// 而不是「这一页没有列表所以什么都不做」。
    pub fn selected_song(&self) -> Option<Song> {
        if self.focus == Focus::Queue {
            // 注意这里**不能**用 `?`：焦点在队列但队列里没选中任何一首时应该继续
            // 往下找（比如回退到正在播放的这首），而不是直接宣告「没有选中的歌曲」。
            if let Some(index) = self.queue_cursor.selected()
                && let Some(song) = self.queue.items().get(index)
            {
                return Some(song.clone());
            }
        }

        if let Some(song) = self.songs().and_then(|list| list.selected()) {
            return Some(song.clone());
        }

        if self.tab == Tab::Queue
            && let Some(song) = self.queue.current()
        {
            return Some(song.clone());
        }

        self.current.clone()
    }

    pub fn focused_songs(&self) -> Option<&SongList> {
        match (self.tab, self.focus) {
            (Tab::Search, Focus::Primary | Focus::Secondary) => Some(&self.search.results),
            (Tab::Playlists, Focus::Secondary) => Some(&self.playlists.songs),
            (Tab::Artists, Focus::Secondary) => Some(&self.artists.songs),
            (Tab::Ranks, Focus::Secondary) => Some(&self.ranks.songs),
            (Tab::Cloud, Focus::Secondary) => Some(&self.cloud.songs),
            _ => None,
        }
    }

    /// 在当前标签页里按 `Tab` 轮转焦点。
    ///
    /// 只把「本页确实可见」的面板纳入轮转，避免焦点跑到看不见的地方。
    pub fn cycle_focus(&mut self, forward: bool) {
        let mut candidates = vec![Focus::Sidebar];
        candidates.push(Focus::Primary);
        if self
            .current_tab_songs()
            .is_some_and(|songs| !songs.is_empty())
        {
            candidates.push(Focus::Secondary);
        }
        if !self.queue.is_empty() {
            candidates.push(Focus::Queue);
        }

        let current = candidates
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);

        let next = if forward {
            (current + 1) % candidates.len()
        } else {
            (current + candidates.len() - 1) % candidates.len()
        };

        self.focus = candidates[next];
        // 离开搜索框时必须退出编辑态，否则字母键会被继续吞掉
        if self.focus != Focus::Primary || self.tab != Tab::Search {
            self.search.editing = false;
        }
    }

    /// 当前标签页的歌曲列表。可视化页没有列表，返回 `None`。
    fn current_tab_songs(&self) -> Option<&SongList> {
        self.songs()
    }

    /// 推进可视化动画：把原始电平做「快起慢落」的缓动，并维护峰值刻度。
    ///
    /// 按**真实经过时间**做指数平滑，而不是按帧数。这样帧率变化（例如从省电的
    /// 5fps 切到 30fps）时，柱子的快慢观感保持一致，不会出现「帧率低就掉得慢」。
    pub fn advance_visualizer(&mut self, elapsed: std::time::Duration) {
        if self.spectrum.is_empty() {
            self.smooth_spectrum.clear();
            self.peak_spectrum.clear();
            return;
        }

        let count = self.spectrum.len();
        self.smooth_spectrum.resize(count, 0.0);
        self.peak_spectrum.resize(count, 0.0);

        // 夹一下：切标签页或卡顿后 elapsed 可能很大，避免一帧跳到底
        let seconds = elapsed.as_secs_f32().clamp(0.0, 0.5);
        // 起振/回落的时间常数。
        //
        // 这两个值要按**帧间隔**来取，不是按 60fps 的习惯：主循环默认 200ms
        // 一拍（5fps），若沿用 40ms 起振，每拍的系数是 1-exp(-0.2/0.04)≈0.99，
        // 等于电平每帧直接跳到当前真实值——八级块字符在 ▁ 与 █ 之间剧烈跳变，
        // 看起来就是侧边栏一直在闪（用户实测反馈）。
        // 时间常数必须**远大于帧间隔**才平滑：200ms 一帧时，0.45s 的起振每拍只走
        // 36%，鼓点的瞬时冲击被摊到好几帧里，块字符才是「涨落」而不是「跳变」。
        // （先前试过 0.15s，每拍仍走 74%，鼓点一响照样整条跳——用户实测反馈
        // 「鼓点强的地方闪得快」，说的就是这个。）
        let attack = 1.0 - (-seconds / 0.45).exp();
        let decay = 1.0 - (-seconds / 1.20).exp();
        // 峰值刻度每秒下落 50%（约 2 秒落到底），比柱子慢得多，才有频谱仪的余韵
        let peak_fall = seconds * 0.5;

        for index in 0..count {
            let target = self.spectrum[index].clamp(0.0, 1.0);
            let current = self.smooth_spectrum[index];
            let factor = if target > current { attack } else { decay };
            let next = current + (target - current) * factor;
            self.smooth_spectrum[index] = next;

            let peak = self.peak_spectrum[index];
            self.peak_spectrum[index] = (peak - peak_fall).max(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ================================================================
    // 歌词换行过渡
    // ================================================================

    use crate::api::model::{Lyric, LyricLine};
    use std::time::Duration;

    fn lyric_with(times: &[u64]) -> Lyric {
        Lyric {
            lines: times
                .iter()
                .map(|time_ms| LyricLine {
                    time_ms: *time_ms,
                    text: "词".to_string(),
                    ..Default::default()
                })
                .collect(),
        }
    }

    fn config(anim_ms: u64) -> Config {
        Config {
            lyric_anim_ms: anim_ms,
            ..Config::default()
        }
    }

    /// 一个已经载入歌词、当前行已定的面板。
    ///
    /// 用构造式而不是「先 `default()` 再逐字段赋值」：后者会撞 clippy 的
    /// `field_reassign_with_default`，而且字段一多就看不出到底设了什么。
    fn pane_with(times: &[u64], active: Option<usize>) -> LyricPane {
        LyricPane {
            lyric: lyric_with(times),
            active_line: active,
            ..LyricPane::default()
        }
    }

    /// 没换过行时必须是**稳态**——这条是既有渲染测试能一行不改的前提：
    /// 它们只设 `active_line`，不碰过渡状态，断言的是改造前的配色。
    #[test]
    fn progress_is_settled_before_any_line_change() {
        let pane = LyricPane::default();
        assert_eq!(pane.transition_progress(), 1.0);
    }

    /// 换行瞬间进度为 0，走完一个时长后到 1 并收掉旧锚点。
    #[test]
    fn transition_runs_from_zero_to_one_then_settles() {
        let mut pane = LyricPane::default();
        pane.retarget(Some(3), 200.0);
        assert_eq!(pane.transition_progress(), 0.0, "换行那一帧应当是 t = 0");

        pane.advance_transition(Duration::from_millis(100));
        let half = pane.transition_progress();
        assert!(half > 0.5, "ease-out 前半段就该走完大半：{half}");
        assert!(half < 1.0);

        pane.advance_transition(Duration::from_millis(120));
        assert_eq!(pane.transition_progress(), 1.0, "超时后必须正好收在 1.0");
        assert_eq!(pane.prev_line, None, "走完要清掉旧锚点，渲染层才走稳态路径");
    }

    /// 过渡走完之后再推进时钟不该有任何变化（稳态不能被越推越偏）。
    #[test]
    fn advancing_after_settle_changes_nothing() {
        let mut pane = LyricPane::default();
        pane.retarget(Some(1), 100.0);
        pane.advance_transition(Duration::from_millis(500));
        let settled = pane.transition_progress();
        pane.advance_transition(Duration::from_secs(10));
        assert_eq!(pane.transition_progress(), settled);
    }

    /// `total_ms = 0` 表示不做过渡，进度必须直接是 1.0——否则会卡在 0 上，
    /// 表现为「整块歌词一直是暗的」。
    #[test]
    fn zero_duration_means_no_transition() {
        let mut pane = LyricPane::default();
        pane.retarget(Some(2), 0.0);
        assert_eq!(pane.transition_progress(), 1.0);
    }

    /// 四道否决：用户关了 / `lite_mode` / 16 色 / 跨行太多。
    #[test]
    fn transition_is_skipped_when_it_should_be() {
        let pane = pane_with(&[0, 1_000, 2_000, 3_000, 4_000, 5_000], Some(0));

        assert_eq!(
            pane.transition_ms_for(&config(0), 60_000, Some(1)),
            0.0,
            "用户关了"
        );
        assert_eq!(
            pane.transition_ms_for(
                &Config {
                    lite_mode: true,
                    ..config(200)
                },
                60_000,
                Some(1)
            ),
            0.0,
            "简易模式要的就是少重绘"
        );
        assert_eq!(
            pane.transition_ms_for(
                &Config {
                    basic_color: true,
                    ..config(200)
                },
                60_000,
                Some(1)
            ),
            0.0,
            "16 色没有中间色阶，淡入会退化成整块硬翻"
        );
        assert_eq!(
            pane.transition_ms_for(&config(200), 60_000, None),
            0.0,
            "变成「没有当前行」时没有新行可点亮"
        );
        assert_eq!(
            pane.transition_ms_for(&config(200), 60_000, Some(5)),
            0.0,
            "跨 5 行是拖动进度条，逐行淡过去又慢又晕"
        );
        assert!(
            pane.transition_ms_for(&config(200), 60_000, Some(2)) > 0.0,
            "跨 2 行是正常换行，应当有过渡"
        );
    }

    /// 时长按行距自适应：短行压短，长行取用户配的上限。
    #[test]
    fn duration_adapts_to_the_line_span() {
        // 行距 300ms → 300 × 0.55 = 165ms，低于上限 200
        let short_pane = pane_with(&[0, 300, 600], Some(0));
        let short = short_pane.transition_ms_for(&config(200), 60_000, Some(1));
        assert!(
            (short - 165.0).abs() < 0.01,
            "短行应当压到 165ms，实际 {short}"
        );

        // 行距 10s → 5.5s，被用户配的 200ms 封顶
        let long_pane = pane_with(&[0, 10_000], Some(0));
        assert_eq!(
            long_pane.transition_ms_for(&config(200), 60_000, Some(1)),
            200.0
        );
    }

    /// 最后一行没有「下一行」，用歌曲总时长兜底——不能算出 0 而丢掉过渡。
    #[test]
    fn last_line_falls_back_to_the_song_duration() {
        let pane = pane_with(&[0, 1_000], Some(0));
        let total = pane.transition_ms_for(&config(200), 60_000, Some(1));
        assert_eq!(total, 200.0, "60s 的兜底远大于上限，应当取上限");

        // 总时长也很短时（比如最后一句接尾奏），仍要给出一个正数
        let total = pane.transition_ms_for(&config(200), 1_200, Some(1));
        assert!(total > 0.0 && total <= 200.0, "实际 {total}");
    }

    /// 切歌 / 清空歌词要把过渡状态一并复位，否则新歌第一句会从上一首淡过来。
    #[test]
    fn reset_clears_everything() {
        let mut pane = LyricPane::default();
        pane.retarget(Some(4), 200.0);
        pane.advance_transition(Duration::from_millis(50));
        pane.reset_transition();

        assert_eq!(pane.active_line, None);
        assert_eq!(pane.prev_line, None);
        assert_eq!(pane.transition_progress(), 1.0);
    }

    /// 换行时旧锚点要接住**换行前**的当前行，而不是换行后的。
    #[test]
    fn retarget_keeps_the_previous_anchor() {
        let mut pane = pane_with(&[0, 1_000, 2_000], Some(7));
        pane.retarget(Some(8), 200.0);
        assert_eq!(pane.active_line, Some(8));
        assert_eq!(pane.prev_line, Some(7), "旧锚点必须是 7，否则没有行会淡出");
    }

    /// 过渡没走完就再次换行：旧锚点改成「上一次的当前行」，不会闪烁。
    #[test]
    fn retarget_mid_flight_uses_the_latest_anchor() {
        let mut pane = LyricPane::default();
        pane.retarget(Some(1), 200.0);
        pane.advance_transition(Duration::from_millis(60));
        pane.retarget(Some(2), 200.0);
        assert_eq!(pane.prev_line, Some(1));
        assert_eq!(pane.transition_progress(), 0.0, "时钟归零，从头淡向新行");
    }

    /// 显示行 → 歌词行的映射：越界给 `None`，不能 panic。
    #[test]
    fn display_line_mapping_is_bounds_checked() {
        let pane = LyricPane {
            display_line_index: vec![0, 0, 1],
            ..LyricPane::default()
        };
        assert_eq!(pane.line_index_at_display(0), Some(0));
        assert_eq!(pane.line_index_at_display(1), Some(0), "译文行仍属于同一句");
        assert_eq!(pane.line_index_at_display(2), Some(1));
        assert_eq!(pane.line_index_at_display(3), None);
    }

    // ================================================================
    // LoadState：载入三态
    // ================================================================

    /// 失败必须同时收掉「载入中」并留下原因。
    ///
    /// 只收掉 loading 的话面板会退回「暂无数据」，用户以为这份数据本来就是空的；
    /// 只留原因不清 loading 的话，面板会永远转圈——这两条边都踩过。
    #[test]
    fn load_failure_clears_loading_and_keeps_the_reason() {
        let mut load = LoadState::default();
        assert!(!load.is_loading());
        assert_eq!(load.error(), None);

        load.begin();
        assert!(load.is_loading(), "begin 之后应处于载入中");
        assert_eq!(load.error(), None, "开始载入要清掉上一次的失败原因");

        load.fail("连不上");
        assert!(!load.is_loading(), "失败必须收掉载入中，否则永久转圈");
        assert_eq!(load.error(), Some("连不上"), "失败原因要留下来给面板显示");

        // 重新载入时失败原因要消失，否则成功之后还挂着上一次的错
        load.begin();
        assert_eq!(load.error(), None);
    }

    #[test]
    fn load_success_clears_both_flags() {
        let mut load = LoadState::default();
        load.begin();
        load.fail("超时");
        load.succeed();
        assert!(!load.is_loading());
        assert_eq!(load.error(), None, "成功之后不该还挂着旧的失败原因");
    }

    // ================================================================
    // HelpPane：帮助面板滚动
    // ================================================================

    #[test]
    fn help_scroll_is_clamped_to_the_last_page() {
        let mut help = HelpPane::default();
        help.open();
        // 先渲染一帧，登记视口高度（滚动钳位要用它）
        assert_eq!(help.visible_range(37, 27), 0..27);

        help.scroll_by(1, 37);
        assert_eq!(help.visible_range(37, 27), 1..28);

        // 一直往下：最后一页必须正好贴住底部，不能滚过头留出空白
        help.scroll_to_bottom(37);
        assert_eq!(help.visible_range(37, 27), 10..37);

        help.scroll_by(999, 37);
        assert_eq!(help.visible_range(37, 27), 10..37, "越界要被钳住");

        // 往上同理
        help.scroll_by(-999, 37);
        assert_eq!(help.visible_range(37, 27), 0..27);
    }

    /// 视口比内容长时只能贴顶显示，不能因为「total - viewport 下溢」而崩。
    #[test]
    fn help_scroll_handles_content_shorter_than_the_viewport() {
        let mut help = HelpPane::default();
        help.open();
        help.scroll_by(5, 3);
        assert_eq!(help.visible_range(3, 40), 0..3);
    }

    /// 打开要复位滚动位置，否则第二次打开会停在上次的地方。
    #[test]
    fn reopening_help_resets_the_scroll_position() {
        let mut help = HelpPane::default();
        help.open();
        help.visible_range(37, 10);
        help.scroll_by(20, 37);
        assert_ne!(help.visible_range(37, 10).start, 0);

        help.close();
        help.open();
        assert!(help.is_open());
        assert_eq!(help.visible_range(37, 10), 0..10, "重新打开应回到顶部");
    }

    #[test]
    fn help_page_scroll_moves_by_almost_a_full_viewport() {
        let mut help = HelpPane::default();
        help.open();
        help.visible_range(100, 20);
        help.scroll_page(1, 100);
        // 步长 = 视口 - 1，留一行重叠给视线当锚点
        assert_eq!(help.visible_range(100, 20).start, 19);
    }

    /// `SIDEBAR_ORDER` 必须是 `ALL` 的一个**排列**。
    ///
    /// 侧边栏按 `SIDEBAR_ORDER` 渲染、鼠标命中区也照它算行号。漏掉一个标签，
    /// 那页就再也点不到（也不会报错，只是静默消失）；多一个则会重复渲染。
    #[test]
    fn sidebar_order_is_a_permutation_of_all() {
        let mut all = Tab::ALL.to_vec();
        all.sort_by_key(|tab| tab.index());
        let mut sidebar = Tab::SIDEBAR_ORDER.to_vec();
        sidebar.sort_by_key(|tab| tab.index());
        assert_eq!(
            all, sidebar,
            "SIDEBAR_ORDER 必须与 ALL 包含完全相同的标签页"
        );
    }

    /// 分组内不能有交错：同一个分组的标签在 SIDEBAR_ORDER 里必须连续。
    ///
    /// 不连续的话侧边栏会为同一组插两次标题行。
    #[test]
    fn sidebar_groups_are_contiguous() {
        let mut seen: Vec<&'static str> = Vec::new();
        for tab in Tab::SIDEBAR_ORDER {
            let group = tab.group();
            if seen.last() != Some(&group) {
                assert!(
                    !seen.contains(&group),
                    "分组「{group}」在侧边栏里出现了不止一段"
                );
                seen.push(group);
            }
        }
    }

    /// 数字键落点没被显示重排影响，删掉歌词/封面两页后也**没有顺延**。
    ///
    /// 顺延的话 `0`（可视化）会被顶到 `8`、音源/设置也跟着前移，老用户已经形成
    /// 的肌肉记忆就全乱了。这条测试就是钉住「1-7 与 0 一个都没动」。
    #[test]
    fn number_keys_follow_all_order() {
        assert_eq!(Tab::ALL[0].number_key(), Some('1'));
        assert_eq!(Tab::ALL[6].number_key(), Some('7'));
        assert_eq!(Tab::ALL[8].number_key(), Some('9'));
        assert_eq!(Tab::ALL[9].number_key(), Some('0'));

        // 1-7 与 0 是删页之前就定下的，必须一个都没动
        assert_eq!(Tab::from_number(1), Some(Tab::Home));
        assert_eq!(Tab::from_number(2), Some(Tab::Search));
        assert_eq!(Tab::from_number(7), Some(Tab::Queue));
        assert_eq!(Tab::from_number(0), Some(Tab::Visualizer));

        // 空出来的 8、9 给了原先够不到数字键的音源与设置
        assert_eq!(Tab::from_number(8), Some(Tab::Sources));
        assert_eq!(Tab::from_number(9), Some(Tab::Settings));

        // 越界被挡住：11 是 1 基索引下的第 11 项，而标签页只有 10 个
        assert_eq!(Tab::from_number(11), None, "越界的数字键不该切到任何页");
    }

    /// 鼠标点击侧边栏必须能到**每一页**，且下标按**显示顺序**解释。
    ///
    /// 命中区登记的就是 `SIDEBAR_ORDER` 里的位置，所以这里必须拿它比对——
    /// 拿 `ALL` 比会「测试全绿、点哪都跳错页」。
    #[test]
    fn sidebar_click_reaches_every_tab() {
        for (index, expected) in Tab::SIDEBAR_ORDER.iter().enumerate() {
            assert_eq!(
                Tab::from_sidebar_index(index),
                Some(*expected),
                "侧边栏第 {index} 项应当切到 {expected:?}"
            );
        }
        // 第一项是首页——侧边栏置顶的那个，点它必须回到首页
        assert_eq!(Tab::from_sidebar_index(0), Some(Tab::Home));
        assert!(Tab::from_sidebar_index(Tab::SIDEBAR_ORDER.len()).is_none());
    }

    fn named(name: &str) -> Song {
        Song {
            name: name.to_string(),
            hash: name.to_string(),
            ..Song::default()
        }
    }

    #[test]
    fn songs_are_descending_by_default() {
        // 进入歌单后默认「从最后一首开始」：载入时按排列方向反转存储
        let mut list = SongList::default();
        list.set_songs_sorted("t", vec![named("a"), named("b"), named("c")], true);
        let names: Vec<&str> = list.songs.iter().map(|song| song.name.as_str()).collect();
        assert_eq!(names, vec!["c", "b", "a"]);
    }

    #[test]
    fn toggle_sort_mirrors_order_and_cursor() {
        let mut list = SongList::default();
        list.replace("t", vec![named("a"), named("b"), named("c")]);
        list.select(0); // 选中最上面那首 a

        list.toggle_sort();

        assert_eq!(list.songs[0].name, "c", "反转后最后一首排在最上面");
        assert_eq!(
            list.selected_index(),
            Some(2),
            "光标应镜像到另一端，仍指向同一首歌"
        );
    }

    #[test]
    fn text_input_handles_multibyte_backspace() {
        let mut input = TextInput::default();
        input.set("海阔天空");
        assert_eq!(input.cursor_byte_index(), 12, "光标应停在末尾");

        // 一次退格只删一个汉字，不能把 UTF-8 字节切一半
        input.backspace();
        assert_eq!(input.text(), "海阔天");
        assert_eq!(input.cursor_byte_index(), 9);
    }

    #[test]
    fn text_input_inserts_at_cursor_position() {
        let mut input = TextInput::default();
        input.set("ac");
        input.move_home();
        input.move_right();
        input.insert('b');
        assert_eq!(input.text(), "abc");
        assert_eq!(input.cursor_byte_index(), 2);
    }

    #[test]
    fn text_input_delete_removes_character_under_cursor() {
        let mut input = TextInput::default();
        input.set("abc");
        input.move_home();
        input.delete();
        assert_eq!(input.text(), "bc");
    }

    #[test]
    fn text_input_ignores_control_characters() {
        let mut input = TextInput::default();
        input.insert('\n');
        input.insert('\t');
        assert!(input.is_empty());
    }

    /// 新建歌单弹窗必须走输入框，而不只是一个裸 `String`。
    ///
    /// 理由：弹窗打开时 `resolve` 按 `KeyMode::TextInput` 解析，会产出
    /// `CursorLeft` / `Delete` / `CursorHome` 这类编辑动作。buffer 若是 `String`，
    /// 这些动作无处可接〔只能 `_ => {}` 吞掉〕——用户能打字却移不了光标。
    #[test]
    fn prompt_keeps_an_editable_buffer() {
        let mut prompt = PromptState::new("新建歌单", PromptAction::CreateCloudPlaylist);

        for c in "abc".chars() {
            prompt.buffer.insert(c);
        }
        assert_eq!(prompt.buffer.text(), "abc");

        // 光标回移一格再插入：验证插入点真的跟着光标走
        prompt.buffer.move_left();
        prompt.buffer.insert('X');
        assert_eq!(prompt.buffer.text(), "abXc", "插入应发生在光标处");

        // Delete 删光标右边那个字符
        prompt.buffer.delete();
        assert_eq!(prompt.buffer.text(), "abX");

        // 退格删光标左边那个字符
        prompt.buffer.backspace();
        assert_eq!(prompt.buffer.text(), "ab");

        prompt.buffer.move_home();
        prompt.buffer.insert('Z');
        assert_eq!(prompt.buffer.text(), "Zab");
    }

    #[test]
    fn text_input_cursor_never_exceeds_length() {
        let mut input = TextInput::default();
        input.set("ab");
        for _ in 0..10 {
            input.move_right();
        }
        assert_eq!(input.cursor_byte_index(), 2);
        input.insert('c');
        assert_eq!(input.text(), "abc");
    }

    #[test]
    fn move_selection_clamps_at_both_ends() {
        let mut state = ListState::default();
        move_selection(&mut state, 3, 1);
        assert_eq!(state.selected(), Some(0));
        move_selection(&mut state, 3, -5);
        assert_eq!(state.selected(), Some(0));
        move_selection(&mut state, 3, 99);
        assert_eq!(state.selected(), Some(2));
    }

    #[test]
    fn move_selection_on_empty_list_clears_selection() {
        let mut state = ListState::default();
        state.select(Some(5));
        move_selection(&mut state, 0, 1);
        assert_eq!(state.selected(), None);
    }

    /// 5fps（200ms 一拍）下，鼓点的瞬时冲击不能被一帧走完——否则块字符在
    /// ▁ 与 █ 之间跳变，看起来就是侧边栏一直在闪（用户实测反馈「鼓点强的地方
    /// 闪得快」，说的就是这个）。
    #[test]
    fn level_easing_smooths_spikes_at_low_fps() {
        let mut state = AppState::new(Config::default());
        let frame = std::time::Duration::from_millis(200);

        // 静音中突然来一记鼓点
        state.spectrum = vec![1.0; 8];
        state.smooth_spectrum = vec![0.0; 8];
        state.advance_visualizer(frame);
        let after_one = state.smooth_spectrum[0];
        assert!(
            after_one < 0.5,
            "单帧就跳到 {after_one:.2}，太快了，视觉上就是闪"
        );

        // 但持续几拍后要能爬到位，不能永远上不去
        for _ in 0..8 {
            state.advance_visualizer(frame);
        }
        assert!(
            state.smooth_spectrum[0] > 0.9,
            "8 拍后只到 {:.2}，太迟钝",
            state.smooth_spectrum[0]
        );
    }

    #[test]
    fn progress_ratio_avoids_division_by_zero() {
        let mut state = AppState::new(Config::default());
        assert_eq!(state.progress_ratio(), 0.0);
        state.duration_ms = 100;
        state.position_ms = 250;
        assert!((state.progress_ratio() - 1.0).abs() < f64::EPSILON);
    }

    fn sample_song() -> crate::api::model::Song {
        crate::api::model::Song {
            hash: "test-hash".to_string(),
            name: "测试歌曲".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn quality_picker_defaults_to_current_quality() {
        // 全局音质是 flac，框一打开就该停在 flac——多数人下载就是想要现在
        // 听的这个档，不该每次都从第一项开始挑。
        assert_eq!(
            QualityPicker::new(sample_song(), "flac").selected(),
            Some("flac")
        );
        assert_eq!(
            QualityPicker::new(sample_song(), "320").selected(),
            Some("320")
        );
    }

    #[test]
    fn quality_picker_handles_unknown_quality() {
        // 配置里写了不在列表里的值（手改过或上游改名）不能越界
        assert_eq!(
            QualityPicker::new(sample_song(), "不存在的音质").selected(),
            Some("128")
        );
    }

    #[test]
    fn quality_picker_move_stays_in_range() {
        let mut picker = QualityPicker::new(sample_song(), "128");
        // 已在第一项，再往上不跑负
        picker.move_by(-1);
        assert_eq!(picker.selected(), Some("128"));

        picker.move_by(1);
        assert_eq!(picker.selected(), Some("320"));

        // 一直往下到末尾也不能越界
        for _ in 0..20 {
            picker.move_by(1);
        }
        assert_eq!(
            picker.selected(),
            Some("viper_tape"),
            "应当停在最后一项而不是越界"
        );
    }

    #[test]
    fn quality_picker_covers_all_supported() {
        let picker = QualityPicker::new(sample_song(), "128");
        assert_eq!(
            picker.candidates.len(),
            crate::config::SUPPORTED_QUALITIES.len()
        );
        for quality in crate::config::SUPPORTED_QUALITIES {
            assert!(
                picker.candidates.iter().any(|c| c == quality),
                "缺少音质 {quality}"
            );
        }
    }
}
