//! 视图渲染。
//!
//! 每个函数只负责一个矩形区域的绘制，通过参数接收它需要的那部分状态，
//! 而不是整个 [`AppState`]——这样借用关系是显式的，编译器能帮我们发现
//! 「同一帧里既读又写同一个字段」这类问题。
//!
//! 视图函数**不修改**状态，除了把 [`ListState`] 交给 `render_stateful_widget`
//! 以便 ratatui 更新滚动偏移。

pub mod lists;
pub mod player;
pub mod settings;
pub mod sources;
pub mod visualizer;

pub use lists::SongView;
pub use lists::{
    render_artist_entries, render_cloud_entries, render_playlist_entries, render_rank_entries,
    render_search_input, render_song_list,
};
pub use player::{
    QueueView, prepare_cover, render_home, render_lyric_panel, render_player, render_queue,
};
pub use settings::render_settings;
pub use sources::{render_login_picker, render_sources};
pub use visualizer::render_visualizer;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Clear, Paragraph, Row, Table, Wrap};

use crate::app::state::{AppState, ConfirmAction, Focus, LoginState, PromptState, Tab};
use crate::keymap::{CHEATSHEET, display_keys_with_custom};
use crate::ui::theme::Theme;
use crate::ui::widgets::{display_width, human_bytes, panel, placeholder, truncate_to_width};

/// 侧边栏宽度：随终端宽度伸缩，但保持在一个可读区间内。
pub fn sidebar_width(total_width: u16) -> u16 {
    // 下限 24 而不是 20：panel 的左右内边距各占 1 列，加上边框 2 列，
    // 20 列只剩 16 列可用——「API 127.0.0.1:3001」这类行会被挤断成两行。
    // 24 列刚好放得下最长的那一行（20 字符）。
    (total_width / 5).clamp(24, 32)
}

/// 左侧导航栏：标签页切换 + 连接/播放/缓存概览。
/// 把播放电平画成一排竖条。
///
/// 用的是 `▁▂▃▄▅▆▇█` 这组八级块字符：不依赖真彩，在没有 256 色的终端上也能看；
/// 而且只占一行，比用 Gauge 省地方。数据是音频线程算出的真实峰值，静音会掉到底、
/// 鼓点会顶到头，不是随机动画。
fn level_line(levels: &[f32], width: u16, theme: &Theme) -> Line<'static> {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    let width = width as usize;
    if width == 0 {
        return Line::default();
    }

    if levels.is_empty() {
        return Line::from(Span::styled("  未播放", theme.dim()));
    }

    // 电平格子数（LEVEL_BUCKETS）和侧边栏能显示的列数不一定相等。若直接按格子数铺，
    // 超宽就会折行，把下面整个「连接」区顶下去——所以这里按可用宽度重采样。
    let text: String = (0..width)
        .map(|column| {
            let index = (column * levels.len() / width).min(levels.len() - 1);
            let level = levels[index].clamp(0.0, 1.0);
            let bar = (level * (BARS.len() - 1) as f32).round() as usize;
            BARS[bar.min(BARS.len() - 1)]
        })
        .collect();

    Line::from(Span::styled(text, theme.now_playing()))
}

pub fn render_sidebar(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let focused = state.focus == Focus::Sidebar;
    let block = panel("kugou-tui", focused, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let width = inner.width;

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("kugou-tui {}", state.tab.position_text()),
        theme.dim(),
    )));

    // 按分组渲染：10 个标签平铺会像一堵文字墙，分组后才扫得动。
    // 每个标签前标出数字键——显示顺序与 `Tab::ALL`（数字键落点）不同，
    // 不标出来的话用户按 1 却跳到别的页。
    let mut current_group: Option<&'static str> = None;
    for tab in Tab::SIDEBAR_ORDER {
        if Some(tab.group()) != current_group {
            current_group = Some(tab.group());
            lines.push(Line::from(Span::styled(
                format!("── {} ──", tab.group()),
                theme.dim(),
            )));
        }

        let selected = tab == state.tab;
        let pointer = if selected { ">" } else { " " };
        let style = if selected {
            theme.now_playing()
        } else {
            theme.body()
        };
        let key = match tab.number_key() {
            Some(key) => key.to_string(),
            None => " ".to_string(),
        };
        lines.push(Line::from(Span::styled(
            format!("{pointer} {key} {} {}", tab.icon(), tab.title()),
            style,
        )));
    }

    // 播放电平放在导航下方：它是「现在在放什么」最直观的反馈。
    // 传入可用宽度，避免条数超出后折行、把下面的区块顶下去。
    lines.push(level_line(&state.levels, width, theme));

    // ---- 状态块：按优先级排，装不下就**整块**略过并留一行说明 ----
    //
    // 34 行的终端里侧边栏只有 25 行可用，而全部内容要 34 行。早先的做法是直接
    // 画出去、让 ratatui 裁掉——「播放」「缓存」两块就这么无声无息地消失了，
    // 用户既不知道下面还有东西，也不知道去哪儿看。
    //
    // 优先级：连接 > 播放 > 缓存。连接块是排查用的（服务端认成了哪种会员、
    // 指纹有没有拿到），别处看不到；播放块的内容在播放条上都有（音量、循环模式），
    // 缓存块在设置页里有。
    let mut connect: Vec<Line> = vec![
        kv("API", api_host(&state.config.api_base), width, theme),
        kv(
            "音源",
            state.config.active_source_kind().label(),
            width,
            theme,
        ),
        kv(
            "登录",
            if state.logged_in { "是" } else { "否" },
            width,
            theme,
        ),
        kv(
            "指纹",
            if state.config.dfid.is_some() {
                "已获取"
            } else {
                "—"
            },
            width,
            theme,
        ),
    ];
    // 会员形态直接显示出来：排查「有会员却只能试听」时，一眼就能看出
    // 服务端到底认成了哪种会员、什么时候到期。
    //
    // 侧边栏宽度随终端变（24~32 列），所以挑**放得下的那个形态**：够宽就用完整
    // 形态，不够就退回短形态。不挑的话窄终端上会折行，而折出来的第二行没有缩进。
    if let Some(info) = state.vip_info.as_ref() {
        let room = (width as usize).saturating_sub(display_width("  会员 "));
        let full = info.label();
        let label = if display_width(&full) <= room {
            full
        } else {
            info.short_label()
        };
        connect.push(kv("会员", &label, width, theme));
    }

    let playback: Vec<Line> = vec![
        kv("模式", state.queue.mode().label(), width, theme),
        kv("队列", &format!("{} 首", state.queue.len()), width, theme),
        kv(
            "音量",
            &if state.is_muted() {
                "静音".to_string()
            } else {
                format!("{:.0}%", state.volume * 100.0)
            },
            width,
            theme,
        ),
    ];

    let limit = if state.config.cache_limit_mib == 0 {
        "不限".to_string()
    } else {
        format!("{} MiB", state.config.cache_limit_mib)
    };
    let cache: Vec<Line> = vec![
        kv("已用", &human_bytes(state.cache_bytes), width, theme),
        kv("上限", &limit, width, theme),
        // 缓存目录与清理按键挤在一行：侧边栏很窄，完整路径用 `--print-config` 看。
        // 这里只显示目录最后一段，指个方向就够。
        //
        // 键位用 `[C]` 的方括号写法，和状态栏的 `[?] 帮助 [q] 退出` 一致——
        // 写成 `C清理 kugou-tui` 的话键位和目录名粘成一个词，读不出来。
        Line::from(vec![
            Span::styled("  [C] 清理 ", theme.dim()),
            Span::styled(
                truncate_to_width(
                    &dir_basename(&state.config.cache_dir),
                    (width as usize).saturating_sub(display_width("  [C] 清理 ")),
                ),
                theme.body(),
            ),
        ]),
    ];

    let connect_title = format!("连接 · {}", state.connection.label());
    let blocks: [(&str, Vec<Line>); 3] = [
        (&connect_title, connect),
        ("播放", playback),
        ("缓存", cache),
    ];

    // 每块占「1 行空行 + 1 行标题 + 内容行数」
    let needed: usize = blocks.iter().map(|(_, body)| 2 + body.len()).sum();
    // 装不下时要留一行写「还有哪些没显示」，否则那一行自己也会被裁掉
    let budget = if lines.len() + needed > inner.height as usize {
        (inner.height as usize).saturating_sub(1)
    } else {
        inner.height as usize
    };

    // **能放几行放几行**，不要「一块放不下就整块丢掉」——那会在 30 行的终端里
    // 把「连接」整块吞掉，屏幕上一条状态都没有，比只显示前几行还糟。
    //
    // 只有**标题都没放下的**那些块才列进提示：标题出现了就说明用户知道这一块
    // 存在，提示再说一遍只会让人以为它完全没显示。
    let mut dropped: Vec<&str> = Vec::new();
    for (title, body) in &blocks {
        if lines.len() + 2 > budget {
            dropped.push(title);
            continue;
        }
        lines.push(Line::default());
        lines.push(section(title, theme));
        for line in body {
            if lines.len() >= budget {
                break;
            }
            lines.push(line.clone());
        }
    }
    if !dropped.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate_to_width(&format!("… 另有 {}", dropped.join(" / ")), width as usize),
            theme.dim(),
        )));
    }

    // 不套 `Wrap`：值在 `kv` 里已经按宽度截断，行数是算好的。留一个会折行的
    // 段落只会让「算好的行数」失效，把下面的块重新顶出去。
    frame.render_widget(Paragraph::new(lines), inner);
}

/// 取路径的最后一段（目录名），用于在窄侧边栏里指代缓存目录。
///
/// 取不到（比如路径以 `..` 结尾）就退回完整路径——宁可让它被侧边栏裁掉，
/// 也不要显示一个认不出来的空值。
fn dir_basename(path: &std::path::Path) -> String {
    // 用 to_string_lossy 而不是 OsStr::display()：这里要的是一个 String，
    // 而 display() 给的是 Display 适配器。
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

fn section(title: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(format!("── {title}"), theme.dim()))
}

/// 侧边栏的一行「键 值」。
///
/// 值按可用宽度**截断**而不是折行。侧边栏的行数是按优先级算好的（见
/// `render_sidebar`），多折出一行就会把后面的块顶出去——那正是「内容无声消失」
/// 的来源。
fn kv(key: &str, value: &str, width: u16, theme: &Theme) -> Line<'static> {
    let key_text = format!("  {key} ");
    let room = (width as usize).saturating_sub(display_width(&key_text));
    Line::from(vec![
        Span::styled(key_text, theme.dim()),
        Span::styled(truncate_to_width(value, room), theme.body()),
    ])
}

/// 从 API 地址里剥掉协议前缀，侧边栏窄，省几个字符。
fn api_host(api_base: &str) -> &str {
    api_base
        .trim_start_matches("https://")
        .trim_start_matches("http://")
}

/// 底部状态栏：左侧消息 + 右侧忙碌指示与快捷键提示。
pub fn render_status(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    busy: Option<String>,
    theme: &Theme,
) {
    if area.height == 0 {
        return;
    }

    let message = truncate_to_width(&state.status, area.width.saturating_sub(24) as usize);
    let mut spans = vec![Span::styled(
        format!(" {message}"),
        theme.status(state.status_level),
    )];

    if let Some(busy) = busy {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("[{busy}]"), theme.title()));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    // 右侧快捷键提示靠右对齐，单独画一层，避免和长消息互相挤压
    let hint = Line::from(vec![
        Span::styled("[?]", theme.key_hint()),
        Span::styled(" 帮助 ", theme.dim()),
        Span::styled("[q]", theme.key_hint()),
        Span::styled(" 退出 ", theme.dim()),
    ]);
    let hint_width = display_width("[?] 帮助 [q] 退出 ");
    if area.width as usize > hint_width + 20 {
        let hint_area = Rect {
            x: area.x + area.width - hint_width as u16,
            y: area.y,
            width: hint_width as u16,
            height: 1,
        };
        frame.render_widget(Paragraph::new(hint), hint_area);
    }
}

/// 文本输入弹窗（新建歌单等）。
pub fn render_prompt(frame: &mut Frame, prompt: &PromptState, theme: &Theme) {
    let popup = crate::ui::widgets::centered_rect(frame.area(), 52, 5);
    frame.render_widget(Clear, popup);

    let block = panel(prompt.title.clone(), true, theme);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height == 0 || inner.width < 4 {
        return;
    }

    let line = Line::from(vec![
        Span::styled("> ", theme.title()),
        Span::styled(
            truncate_to_width(prompt.buffer.text(), inner.width.saturating_sub(2) as usize),
            theme.body(),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(vec![
            line,
            Line::from(""),
            Line::from(Span::styled(
                "Enter 创建 · Esc 取消 · ←/→ 移光标",
                theme.dim(),
            )),
        ]),
        inner,
    );

    // 弹窗是模态的，只要它在就一定处于输入态：把真实终端光标放到插入点，
    // 否则用户看不到自己删到哪、光标在哪（←/→ 支持了却看不见位置）。
    let before_cursor = &prompt.buffer.text()[..prompt.buffer.cursor_byte_index()];
    let offset = display_width(before_cursor);
    let max_offset = inner.width.saturating_sub(3) as usize;
    let x = inner.x + 2 + offset.min(max_offset) as u16;
    frame.set_cursor_position((x, inner.y));
}

/// 扫码登录弹窗。
///
/// 二维码用 `█` 与空格渲染——终端显示不了接口返回的 PNG，只能自己编码。每个模块横向
/// 重复一次以修正终端字符的高宽比。
pub fn render_login(
    frame: &mut Frame,
    login: &LoginState,
    source: crate::source::SourceKind,
    aspect: f32,
    theme: &Theme,
) {
    // 按**终端实际尺寸**决定二维码怎么画。
    //
    // 二维码的行数由内容长度决定、不能压缩（压缩就扫不出来），所以窄终端上
    // 唯一正确的做法是承认画不下、改成给一句能照做的提示。汽水的扫码地址比
    // 酷狗长得多，正是它先在这个场景里撑满了整屏。
    //
    // 可用空间 = 画面减去弹窗边框（左右各 1 列 + 内边距）与提示文字占的行。
    let area = frame.area();
    let max_width = usize::from(area.width.saturating_sub(6));
    // 提示语 + 空行 + 「Esc 取消」各占一行，再留一行余量给边框
    let max_rows = usize::from(area.height.saturating_sub(9)).max(1);

    let fitted =
        crate::ui::widgets::qr_lines_fitted(&login.qr_content, aspect, max_width, max_rows)
            // 没有原始内容时（理论上不会）退回预先渲染好的那份
            .or_else(|| {
                if login.qr.len() <= max_rows {
                    Some(login.qr.clone())
                } else {
                    None
                }
            });

    let (qr_lines, too_small): (Vec<String>, Option<(usize, usize)>) = match fitted {
        Some(lines) => (lines, None),
        None => (
            Vec::new(),
            crate::ui::widgets::qr_needed_size(&login.qr_content, aspect),
        ),
    };

    let qr_width = qr_lines
        .first()
        .map(|line| line.chars().count())
        .unwrap_or(0) as u16;
    let popup_width = (qr_width + 6).max(30);
    let popup_height = (qr_lines.len() as u16 + 7).clamp(9, area.height);
    let popup = crate::ui::widgets::centered_rect(area, popup_width, popup_height);
    frame.render_widget(Clear, popup);

    let title = if login.succeeded {
        "登录成功"
    } else {
        "扫码登录"
    };
    let block = panel(title, true, theme);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let qr_style = theme.qr();
    let mut lines: Vec<Line> = Vec::new();
    for row in &qr_lines {
        lines.push(Line::from(Span::styled(row.clone(), qr_style)));
    }
    if let Some((need_width, need_rows)) = too_small {
        // 明确说清「差多少」，并给出两条出路（放大终端 / 手填 cookie）。
        lines.push(Line::from(Span::styled(
            "终端太小，二维码放不下（被裁掉的码扫不出来）",
            theme.title(),
        )));
        lines.push(Line::from(Span::styled(
            format!("需要约 {need_width} 列 × {need_rows} 行，当前窗口不够大"),
            theme.dim(),
        )));
        lines.push(Line::from(Span::styled(
            "放大终端窗口后按 Esc 再按 L 重试；或直接在配置里手填 cookie",
            theme.dim(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        login.message.clone(),
        theme.title(),
    )));

    // 未结束 + 不是网易云：上面那段说明对酷狗够用了；网易云用户得明确知道用哪个 App。
    if !login.finished && matches!(source, crate::source::SourceKind::Netease) {
        lines.push(Line::from(Span::styled(
            "用网易云 App 扫描（手机端登录入口）",
            theme.dim(),
        )));
    }

    // 已结束的弹窗里已经有结论了，「取消」字样跟原消息打架，改成「关闭」。
    lines.push(Line::from(Span::styled(
        if login.finished {
            "Esc 关闭"
        } else {
            "Esc 取消"
        },
        theme.dim(),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false }),
        inner,
    );
}

/// 二次确认对话框。
///
/// 用于清空队列这类破坏性操作。故意做成模态——任何按键都先被它接走，只有确认键才
/// 生效，这样误按一下不会把整个队列清掉。
pub fn render_confirm(frame: &mut Frame, action: ConfirmAction, theme: &Theme) {
    let popup = crate::ui::widgets::centered_rect(frame.area(), 52, 5);
    frame.render_widget(Clear, popup);

    let block = panel("请确认", true, theme);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let text = vec![
        Line::from(action.question()),
        Line::from(""),
        Line::from(Span::styled(action.hint(), theme.dim())),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        inner,
    );
}

/// 歌曲右键菜单。
///
/// 居中的小弹窗，每项右侧标出对应键位——菜单是**键盘动作的索引**而不是
/// 另一套交互，标出来用户下次就能直接按。
pub fn render_context_menu(
    frame: &mut Frame,
    menu: &crate::app::state::ContextMenu,
    theme: &Theme,
) {
    use crate::app::state::MenuAction;

    let height = (menu.items.len() as u16 + 4).min(frame.area().height);
    let popup = crate::ui::widgets::centered_rect(frame.area(), 34, height);
    frame.render_widget(Clear, popup);

    // 标题带歌名：菜单是「对哪首歌操作」，不写清楚容易点错
    let title = format!("《{}》", menu.song.name);
    let block = panel(title, true, theme);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for (index, action) in menu.items.iter().enumerate() {
        let selected = index == menu.cursor;
        let pointer = if selected { ">" } else { " " };
        let style = if selected {
            theme.selection().add_modifier(Modifier::BOLD)
        } else {
            theme.body()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{pointer} {}", action.label()), style),
            Span::styled(
                format!("{:>width$}", action.key_hint(), width = 10),
                if selected { style } else { theme.dim() },
            ),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑↓ 选择 · Enter 执行 · Esc 关闭",
        theme.dim(),
    )));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);

    let _ = MenuAction::Play; // 类型已用于 items，这里仅为可读性
}

/// 下载音质选择框：列出可选音质，默认落在当前全局音质上。
///
/// 与右键菜单同样的浮层写法（Clear + panel + 居中），但内容换成音质列表。
pub fn render_quality_picker(
    frame: &mut Frame,
    picker: &crate::app::state::QualityPicker,
    theme: &Theme,
) {
    let height = (picker.candidates.len() as u16 + 6).min(frame.area().height);
    // 宽度要放得下最长的那一行（「蝰蛇全景声  viper_atmos」），太窄会截断
    let popup = crate::ui::widgets::centered_rect(frame.area(), 46, height);
    frame.render_widget(Clear, popup);

    let block = panel("下载音质", true, theme);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height == 0 {
        return;
    }

    let selected_index = picker.cursor.selected().unwrap_or(0);
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled(
        truncate_to_width(&picker.song.name, inner.width as usize),
        theme.dim(),
    )));
    lines.push(Line::from(""));

    for (index, quality) in picker.candidates.iter().enumerate() {
        let selected = index == selected_index;
        let pointer = if selected { ">" } else { " " };
        let style = if selected {
            theme.selection().add_modifier(Modifier::BOLD)
        } else {
            theme.body()
        };
        let label = crate::app::settings::quality_label(quality);
        lines.push(Line::from(vec![
            Span::styled(
                truncate_to_width(
                    &format!("{pointer} {label}"),
                    inner.width.saturating_sub(8) as usize,
                ),
                style,
            ),
            Span::styled(
                format!("{:>width$}", quality, width = 7),
                if selected { style } else { theme.dim() },
            ),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑↓ 选择 · Enter 下载 · Esc 取消",
        theme.dim(),
    )));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// 帮助弹窗。
///
/// `CHEATSHEET` 有 39 条键位，34 行的终端只放得下 26 条，所以面板是**可滚动**的：
/// `j`/`k`、`↑`/`↓`、`PgUp`/`PgDn`、`g`/`G`。底部固定一行显示「当前是第几条 /
/// 共几条」——不给这个提示的话，用户看不出下面还有内容，滚动也就等于没有。
/// 内容一次放得下时不占这一行。
pub fn render_help(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let width = (area.width.saturating_sub(8)).min(76);
    // 高度取满可用空间：内容本来就超长，再留白只会让能看到的更少
    let height = (area.height.saturating_sub(4)).min(CHEATSHEET.len() as u16 + 8);
    let popup = crate::ui::widgets::centered_rect(area, width, height);

    frame.render_widget(Clear, popup);

    let block = panel(
        Line::from(Span::styled("快捷键 · 按 ? 或 Esc 关闭", theme.title())),
        true,
        theme,
    );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height == 0 {
        return;
    }

    let total = CHEATSHEET.len();
    // 表头占一行，其余才是可见条目数
    let rows_without_footer = inner.height.saturating_sub(1) as usize;
    // 只有真的溢出才占底部那一行——放得下就没必要为提示牺牲一行内容
    let overflow = total > rows_without_footer && inner.height >= 4;
    let rows_area = Rect {
        height: if overflow {
            inner.height - 1
        } else {
            inner.height
        },
        ..inner
    };

    let viewport = rows_area.height.saturating_sub(1) as usize;
    let visible = state.help.visible_range(total, viewport);

    // 列宽从**数据本身**算出来，不写死。
    //
    // 早先这里是 `Length(14) / Min(20) / Length(6)`，两个毛病：
    //
    // * 「功能」用 `Min(20)` 会吃掉所有剩余宽度，把「分类」推到弹窗最右边，
    //   中间空出二十几列——看着像表格裂成了两半；
    // * 「按键」写死 14，而最长的键名（`Tab / S-Tab`）只有 11 列，白占 3 列。
    //
    // 从数据算的话，加一条更长的说明或键名都不用回来改这里。
    // 按用户的自定义键位改写一次「按键」列，后续列宽计算与单元格渲染都用它。
    //
    // 只算一次（而不是在闭包里逐行调）：列宽和单元格必须基于**同一份**字符串，
    // 否则自定义键名比默认的长（如 `p` → `ctrl+p`）时列会被撑断。
    let displayed: Vec<(String, &str, &str)> = CHEATSHEET
        .iter()
        .map(|(key, description, group)| (display_keys_with_custom(key), *description, *group))
        .collect();

    // 注意是拿**整张表**算宽度，不是可见的那一段：按可见段算的话，一滚动列宽
    // 就会跟着变，表格左右横跳。
    let key_col = displayed
        .iter()
        .map(|(key, _, _)| display_width(key))
        .max()
        .unwrap_or(8);
    let description_col = displayed
        .iter()
        .map(|(_, description, _)| display_width(description))
        .max()
        .unwrap_or(20);
    let group_col = displayed
        .iter()
        .map(|(_, _, group)| display_width(group))
        .max()
        .unwrap_or(4);

    // 表格两处列间距各占 1 列
    const COLUMN_SPACING: usize = 2;

    // 窄终端里「分类」先让位：它是分组标签，不影响「这个键是干嘛的」。
    // 不丢的话三列会互相挤，最坏情况只剩「功能」一列可见（按键被挤成 0 宽）。
    let show_group = inner.width as usize >= key_col + description_col + group_col + COLUMN_SPACING;

    let rows = displayed[visible.clone()]
        .iter()
        .map(|(key, description, group)| {
            let mut cells = vec![
                Cell::from(key.clone()),
                Cell::from((*description).to_string()),
            ];
            if show_group {
                cells.push(Cell::from((*group).to_string()));
            }
            Row::new(cells)
        });

    let header = if show_group {
        Row::new(vec!["按键", "功能", "分类"])
    } else {
        Row::new(vec!["按键", "功能"])
    };
    let constraints = if show_group {
        vec![
            Constraint::Length(key_col as u16),
            Constraint::Length(description_col as u16),
            Constraint::Length(group_col as u16),
        ]
    } else {
        // 只剩两列时让「功能」吃掉剩下的宽度（没有第三列，也就不会留出空档）
        vec![Constraint::Length(key_col as u16), Constraint::Min(4)]
    };

    let table = Table::new(rows, constraints)
        .header(header.style(theme.title()))
        .column_spacing(1);

    frame.render_widget(table, rows_area);

    if overflow {
        let shown = format!(
            "第 {}-{} 条 / 共 {total} 条 · ↑↓ 或 PgUp/PgDn 滚动",
            visible.start + 1,
            visible.end
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(shown, theme.key_hint()))),
            Rect {
                y: inner.y + inner.height - 1,
                height: 1,
                ..inner
            },
        );
    }
}

/// 供视图复用的占位段落。
pub fn loading_placeholder(theme: &Theme) -> Paragraph<'static> {
    placeholder("载入中…", theme).alignment(Alignment::Center)
}

/// 供视图复用的空列表占位。
pub fn empty_placeholder(text: &str, theme: &Theme) -> Paragraph<'static> {
    placeholder(text, theme).alignment(Alignment::Center)
}

/// 载入失败的占位。
///
/// 比空态多两样东西：**原因**和**能照做的下一步**。只显示「暂无数据」是不够的
/// ——用户会以为这份列表本来就是空的，而不是没取到；而「载入中…」永远转下去
/// 更糟，那是没有任何出口的死状态。
///
/// `next_step` 为 `None` 时省掉那一行：有些地方没有对应的刷新键（比如歌词，
/// 只能等下一首歌），硬写一个按键会指向一个按了没用的键。
pub fn failed_placeholder(
    reason: &str,
    next_step: Option<&str>,
    theme: &Theme,
) -> Paragraph<'static> {
    let mut lines = vec![Line::from(Span::styled(
        format!("载入失败：{reason}"),
        theme.status(crate::app::state::StatusLevel::Error),
    ))];
    if let Some(step) = next_step {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(step.to_string(), theme.key_hint())));
    }
    Paragraph::new(lines)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ui::theme::ThemeName;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// 取屏幕文本并去掉所有空白。
    ///
    /// ratatui 会给宽字符后面补一个占位格，直接按原样比对会失败；而换行处也可能
    /// 插入空格，所以两边都先挤掉空白再比。
    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    fn draw_help(state: &mut AppState, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("建测试终端");
        let theme = Theme::for_config(ThemeName::Default, false);
        terminal
            .draw(|frame| render_help(frame, Rect::new(0, 0, width, height), state, &theme))
            .expect("渲染帮助面板");
        terminal
    }

    fn draw_sidebar(state: &AppState, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("建测试终端");
        let theme = Theme::for_config(ThemeName::Default, false);
        terminal
            .draw(|frame| render_sidebar(frame, Rect::new(0, 0, width, height), state, &theme))
            .expect("渲染侧边栏");
        terminal
    }

    /// 侧边栏装不下时要「**能放几行放几行**」，并在末尾说明还有什么没显示。
    ///
    /// 早先是「一块放不下就整块丢掉」：108×30 的终端里「连接」整块被吞掉，
    /// 屏幕上一条状态都没有——比只显示前几行还糟。而且丢掉时没有任何提示，
    /// 用户不知道下面还有东西。
    #[test]
    fn sidebar_fills_what_it_can_and_names_what_it_dropped() {
        let state = AppState::new(Config::default());

        // 108×30 是 README 用的参考尺寸，也是这个 bug 最容易露出来的地方
        let text = screen_text(&draw_sidebar(&state, 108, 30));
        assert!(
            text.contains("连接"),
            "「连接」的标题必须放得下——放不下就等于一条状态都不显示：{text}"
        );
        assert!(text.contains("API"), "连接块的前几行应当照常显示：{text}");
        assert!(
            text.contains("另有"),
            "有内容没显示时必须说明，不能静默裁掉：{text}"
        );
    }

    /// 「清理缓存」的键位不能和目录名粘成一个词。
    ///
    /// 早先写成 `C清理 kugou-tui`，键位和目录名连在一起读不出来。改成 `[C]` 的
    /// 方括号写法，和状态栏的「[?] 帮助 [q] 退出」一致。
    #[test]
    fn cache_clear_hint_is_separated_from_the_directory_name() {
        let state = AppState::new(Config::default());
        // 112×60：三块都放得下，缓存块才在
        let text = screen_text(&draw_sidebar(&state, 112, 60));
        assert!(text.contains("缓存"), "60 行终端应显示缓存块：{text}");
        assert!(
            text.contains("[C]清理"),
            "键位应写成 [C] 并与目录名分开：{text}"
        );
    }

    /// 高度够时不该出现「另有…」——那时内容已经全在屏幕上了。
    #[test]
    fn sidebar_has_no_dropped_marker_when_everything_fits() {
        let state = AppState::new(Config::default());

        let text = screen_text(&draw_sidebar(&state, 112, 60));
        assert!(
            !text.contains("另有"),
            "内容全放得下时不该提示有省略：{text}"
        );
        for block in ["连接", "播放", "缓存"] {
            assert!(text.contains(block), "60 行终端应显示完整的「{block}」块");
        }
    }

    /// 帮助面板必须能滚到最后一条。
    ///
    /// `CHEATSHEET` 有 30 多条，34 行的终端只放得下 20 多条。早先面板是模态的、只认
    /// 「关闭」，`j`/`k`/PgDn 全被吞掉——最后十几条（Space / n·p / +·- / m / r /
    /// l / [·] / W，也就是整块播放控制）在常见尺寸下永远看不到。
    ///
    /// 不写死条数：这里原先写「有 37 条、放得下 27 条、最后 10 条」，而实际是
    /// 38 / 26 / 12——加一条键位就过期一次，还看不出来。
    #[test]
    fn help_panel_can_reach_the_last_shortcut() {
        let mut state = AppState::new(Config::default());
        state.help.open();

        let last = CHEATSHEET.last().expect("帮助条目不应为空").1;
        let needle: String = last.chars().filter(|c| !c.is_whitespace()).collect();

        let terminal = draw_help(&mut state, 112, 34);
        let first_screen = screen_text(&terminal);
        assert!(
            !first_screen.contains(&needle),
            "首屏本来就不该显示最后一条，否则这个测试证明不了滚动有效"
        );

        state.help.scroll_to_bottom(CHEATSHEET.len());
        let terminal = draw_help(&mut state, 112, 34);
        assert!(
            screen_text(&terminal).contains(&needle),
            "滚到底之后必须能看到最后一条：{last}"
        );
    }

    /// 内容没显示完时要给出提示，否则用户根本不知道下面还有。
    #[test]
    fn help_panel_advertises_the_overflow() {
        let mut state = AppState::new(Config::default());
        state.help.open();

        let terminal = draw_help(&mut state, 112, 34);
        let text = screen_text(&terminal);
        let total = CHEATSHEET.len();
        assert!(
            text.contains(&format!("共{total}条")),
            "溢出时应显示「共 {total} 条」，实际：{text}"
        );
        assert!(text.contains("滚动"), "溢出时该提示怎么滚动");
    }

    /// 最长的「功能」说明不能被截断——列宽是照它算的。
    ///
    /// 早先「功能」列是 `Min(20)`，宽度够；改成从数据算之后，这条测试锁住
    /// 「算出来的宽度真的放得下最长那条」。
    #[test]
    fn help_panel_fits_the_longest_description() {
        let mut state = AppState::new(Config::default());
        state.help.open();

        let longest = CHEATSHEET
            .iter()
            .max_by_key(|(_, description, _)| display_width(description))
            .expect("帮助条目不应为空")
            .1;
        let needle: String = longest.chars().filter(|c| !c.is_whitespace()).collect();

        // 最长那条在表的靠后位置，先滚到底再断言
        state.help.scroll_to_bottom(CHEATSHEET.len());
        let terminal = draw_help(&mut state, 112, 34);
        assert!(
            screen_text(&terminal).contains(&needle),
            "最长的说明应完整显示，不该被截断：{longest}"
        );
    }

    /// 窄终端里先丢「分类」这一列，而不是把「按键」挤成 0 宽。
    ///
    /// 早先三列都是固定宽度（14 / Min(20) / 6），塞进 22 列的弹窗时 ratatui 会
    /// 把「按键」压没——屏幕上只剩「功能」，用户看不到是哪个键。
    #[test]
    fn help_panel_drops_the_group_column_when_narrow() {
        let mut state = AppState::new(Config::default());
        state.help.open();

        let terminal = draw_help(&mut state, 34, 24);
        let text = screen_text(&terminal);

        assert!(
            text.contains("按键"),
            "窄终端也必须看得到「按键」表头：{text}"
        );
        assert!(
            text.contains("功能"),
            "窄终端也必须看得到「功能」表头：{text}"
        );
        assert!(!text.contains("分类"), "放不下时「分类」应整列让位：{text}");
        // 第一条键位本身要看得到，光有表头没用
        let first_key = CHEATSHEET[0].0;
        let key: String = first_key.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(text.contains(&key), "第一条键位 {first_key} 应可见：{text}");
    }

    /// 宽度够时「分类」要回来。
    #[test]
    fn help_panel_keeps_the_group_column_when_wide_enough() {
        let mut state = AppState::new(Config::default());
        state.help.open();

        let terminal = draw_help(&mut state, 112, 34);
        assert!(
            screen_text(&terminal).contains("分类"),
            "112 列下三列都放得下，不该丢掉「分类」"
        );
    }

    /// 终端足够高时不该出现滚动提示——那时内容已经全在屏幕上了。
    #[test]
    fn help_panel_hides_the_hint_when_everything_fits() {
        let mut state = AppState::new(Config::default());
        state.help.open();

        let terminal = draw_help(&mut state, 112, 60);
        let text = screen_text(&terminal);
        assert!(
            !text.contains("滚动"),
            "内容全放得下时不该再提示滚动：{text}"
        );
        // 最后一条也应当直接可见
        let last = CHEATSHEET.last().expect("帮助条目不应为空").1;
        let needle: String = last.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(text.contains(&needle), "60 行终端应能一次显示全部条目");
    }
}
