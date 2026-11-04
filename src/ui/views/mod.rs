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
    render_artist_entries, render_artist_entries_titled, render_cloud_entries,
    render_playlist_entries, render_playlist_entries_titled, render_rank_entries,
    render_search_input, render_song_list,
};
pub use player::{
    QueueView, prepare_cover, render_home, render_lyric_panel, render_player, render_queue,
};
pub use settings::render_settings;
pub use sources::{render_login_picker, render_sources};
pub use visualizer::{render_cava_panel, render_visualizer};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Clear, Paragraph, Row, Table, Wrap};

use crate::app::state::{
    AppState, ConfirmAction, Connection, Focus, LoginState, PromptState, StatusLevel, Tab,
};
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

/// 侧边栏导航区的一行：分组标题，或一个标签项。
///
/// 渲染（[`render_sidebar`]）与鼠标命中区（`ui::render`）**共用**同一份行计划，
/// 行号因此不会各算各的。历史上命中区是照着渲染函数手动重放行号的，改一行内容
/// 就可能让点「搜索」跳到别的页，而且没有任何提示。
#[derive(Debug, Clone, Copy)]
pub enum SidebarRow {
    /// 分组标题行，不可点击。
    Group(&'static str),
    /// 一个标签项；`index` 是它在 [`Tab::SIDEBAR_ORDER`] 里的位置。
    Tab { index: usize, tab: Tab },
}

/// 按 [`Tab::SIDEBAR_ORDER`] 生成导航区的逐行计划（每个新分组前插一行标题）。
pub fn sidebar_nav_rows() -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    let mut current: Option<&'static str> = None;
    for (index, tab) in Tab::SIDEBAR_ORDER.iter().enumerate() {
        if Some(tab.group()) != current {
            current = Some(tab.group());
            rows.push(SidebarRow::Group(tab.group()));
        }
        rows.push(SidebarRow::Tab { index, tab: *tab });
    }
    rows
}

/// 左侧导航栏：标签页切换 + 连接/缓存概览。
///
/// 返回内容区矩形（已扣掉边框），供调用方按**同一套行号**登记鼠标命中区。
pub fn render_sidebar(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) -> Rect {
    let focused = state.focus == Focus::Sidebar;
    let block = panel("kugou-tui", focused, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return inner;
    }
    let width = inner.width;

    let mut lines: Vec<Line> = Vec::new();
    // 导航：分组标题 + 标签项。按分组渲染——10 个标签平铺会像一堵文字墙，分组后
    // 才扫得动。标签项格式统一为「指针 数字键 标题」，不再放逐项图标：图标挤掉
    // 了标题的位置，而且同一排图标里没有哪一个是「一眼认得出」的。
    //
    // 数字键必须留着——显示顺序与 `Tab::ALL`（数字键落点）不同，不标出来的话
    // 用户按 1 却跳到别的页。
    for (offset, row) in sidebar_nav_rows().into_iter().enumerate() {
        match row {
            SidebarRow::Group(name) => lines.push(section(name, theme)),
            SidebarRow::Tab { tab, .. } => {
                let selected = tab == state.tab;
                let pointer = if selected { ">" } else { " " };
                // 悬停行与选中行的区别只在「有没有指针」：悬停加粗、选中上色，
                // 同时成立时按选中画——不然鼠标一挪过就把当前位置盖掉了。
                let row_rect = Rect::new(inner.x, inner.y + offset as u16, inner.width, 1);
                let hovered = !selected && state.hit_map.is_over(row_rect);
                let style = if selected {
                    theme.now_playing()
                } else if hovered {
                    theme.body().add_modifier(Modifier::BOLD)
                } else {
                    theme.body()
                };
                let key = match tab.number_key() {
                    Some(key) => key.to_string(),
                    None => " ".to_string(),
                };
                lines.push(Line::from(Span::styled(
                    format!("{pointer} {key} {}", tab.title()),
                    style,
                )));
            }
        }
    }

    // ---- 状态块：按优先级排，整块放不下就压成一行小结 ----
    //
    // 这里只剩两块。「播放」块（模式、队列、音量）在播放条与队列标题里都有，是
    // 纯粹的重复，删掉后腾出的行正好让「连接」「缓存」在常见尺寸下都放得下。
    // 剩下两块各自都是别处看不到的：连接是排查用的（服务端认成了哪种会员、
    // 指纹有没有拿到、接口是不是内嵌），缓存是已用 / 上限 / 清理入口。
    let mut connect: Vec<Line> = vec![
        kv("API", &api_endpoint_label(state), width, theme),
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

    let limit = if state.config.cache_limit_mib == 0 {
        "不限".to_string()
    } else {
        format!("{} MiB", state.config.cache_limit_mib)
    };
    let mut cache: Vec<Line> = vec![
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

    // native 下没有「连接」这回事：接口就在进程里，不存在连不通。标题照旧写
    // 「已连通」会让人以为在跟某个服务说话，出问题时去查端口。
    let connect_title = if is_native(state) {
        "接口 · 内嵌".to_string()
    } else {
        format!("连接 · {}", state.connection.label())
    };

    // 矮终端下的**一行小结**。整块（标题 + 四五行）放不下时，宁可给它这一行能读
    // 的摘要，也不要整块丢掉、只剩一句「… 另有 …」让人猜里面到底是什么。
    //
    // 连接那行把「通不通」和「当前音源」并在一起——这两项是排查时最先要看的。
    let source_label = state.config.active_source_kind().label();
    let connect_compact: Line = if is_native(state) {
        Line::from(Span::styled(
            format!("● 接口内嵌 · {source_label}"),
            theme.dim(),
        ))
    } else {
        let (word, style) = match state.connection {
            Connection::Connected => ("已连通", theme.status(StatusLevel::Success)),
            Connection::Unreachable => ("未连通", theme.status(StatusLevel::Error)),
            Connection::Unknown => ("未验证", theme.dim()),
        };
        Line::from(vec![
            Span::styled(format!("● {word}"), style),
            Span::styled(format!(" · {source_label}"), theme.dim()),
        ])
    };
    // 缓存那行给出「已用 / 上限」，清理入口留给键盘 C（状态栏另有提示）。
    let cache_compact: Line = Line::from(vec![
        Span::styled("缓存 ", theme.dim()),
        Span::styled(
            truncate_to_width(
                &format!("{} / {limit}", human_bytes(state.cache_bytes)),
                (width as usize).saturating_sub(display_width("缓存 ")),
            ),
            theme.body(),
        ),
    ]);

    let connect_len = connect.len();
    let cache_len = cache.len();
    let mut block_lines: Vec<Line> = Vec::new();
    // 导航已经占掉一部分，剩多少行可以给状态块。
    let mut room = (inner.height as usize).saturating_sub(lines.len());

    // 连接优先：放得下整块（空行 + 标题 + 内容）就放整块，否则压成一行。
    if room >= connect_len + 2 {
        block_lines.push(Line::default());
        block_lines.push(section(&connect_title, theme));
        block_lines.append(&mut connect);
        room -= connect_len + 2;
    } else if room >= 1 {
        block_lines.push(connect_compact);
        room -= 1;
    }

    // 缓存次之，同样的取舍。
    if room >= cache_len + 2 {
        block_lines.push(Line::default());
        block_lines.push(section("缓存", theme));
        block_lines.append(&mut cache);
    } else if room >= 1 {
        block_lines.push(cache_compact);
    }

    lines.extend(block_lines);

    // 不套 `Wrap`：值在 `kv` 里已经按宽度截断，行数是算好的。留一个会折行的
    // 段落只会让「算好的行数」失效，把下面的块重新顶出去。
    frame.render_widget(Paragraph::new(lines), inner);
    inner
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

/// 当前配置下接口是不是内嵌在进程里的。
fn is_native(state: &AppState) -> bool {
    state
        .config
        .api_backend
        .effective_for(state.config.active_source_kind())
        == crate::api::ApiBackend::Native
}

/// 侧边栏「API」那一行显示什么。
///
/// native 后端下接口在进程内，`config.api_base` 里那个 `127.0.0.1:3001` 只是
/// 历史遗留的配置值，**没有任何进程在监听它**。照着配置显示会让用户以为背后
/// 还有个服务（甚至以为 Node 还在跑），排查时往错的方向找。
fn api_endpoint_label(state: &AppState) -> String {
    if is_native(state) {
        "内嵌".to_string()
    } else {
        api_host(&state.config.api_base).to_string()
    }
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
    let max_width = usize::from(area.width.saturating_sub(4));
    // 提示语 + 空行 + 「Esc 取消」各占一行，再留给边框
    let max_rows = usize::from(area.height.saturating_sub(7)).max(1);

    // `login.qr` 在收到二维码时就按当前 `aspect` 渲染好了（见 `Loaded::LoginQr`），
    // 与这里要画的是同一份：只判断装不装得下，装得下直接复用，**不再重新编码**。
    let (qr_lines, too_small): (Vec<String>, Option<(usize, usize)>) =
        match crate::ui::widgets::qr_lines_fitted(&login.qr, max_width, max_rows) {
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
    // 边距收到最小：二维码本来就大，多一圈留白就把「装不下」的临界点往前推
    let popup_width = (qr_width + 4).max(30);
    // `clamp(7, area.height)` 在终端比弹窗还矮时会 panic（clamp 要求 min <= max，
    // 高度不足 7 行时 7 反而大于 max）。换成 `max(7).min(area.height)`：够高时与
    // clamp 等价，矮到装不下 7 行时退成整个画面高度，照样把「太小」的提示画出来。
    let popup_height = (qr_lines.len() as u16 + 5).max(7).min(area.height);
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
pub fn render_confirm(
    frame: &mut Frame,
    action: ConfirmAction,
    hits: &crate::ui::hit::HitMap,
    theme: &Theme,
) -> crate::ui::hit::HitMap {
    let popup = crate::ui::widgets::centered_rect(frame.area(), 52, 7);
    frame.render_widget(Clear, popup);

    let block = panel("请确认", true, theme);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height == 0 {
        return crate::ui::hit::HitMap::default();
    }

    frame.render_widget(
        Paragraph::new(vec![Line::from(action.question())])
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        Rect { height: 1, ..inner },
    );

    // 两个按钮并排居中。终端里没有「按钮控件」，所以画成带方括号的文字块，
    // 靠 hover 反色表达可点——和列表行的反馈保持同一套语言。
    let mut zones = crate::ui::hit::HitMap::default();
    let button_row = inner.y + inner.height.saturating_sub(1);
    let labels = [("  确认  ", true), ("  取消  ", false)];
    let total: u16 = labels
        .iter()
        .map(|(label, _)| label.chars().count() as u16)
        .sum();
    let gap = 2u16;
    let mut x = inner.x + inner.width.saturating_sub(total + gap) / 2;
    for (label, yes) in labels {
        let width = label.chars().count() as u16;
        let rect = Rect::new(x, button_row, width, 1);
        let hovered = hits.is_over(rect);
        let style = if yes {
            if hovered {
                theme.selection()
            } else {
                theme.now_playing()
            }
        } else if hovered {
            theme.body().add_modifier(Modifier::BOLD)
        } else {
            theme.dim()
        };
        frame.render_widget(Paragraph::new(Line::from(Span::styled(label, style))), rect);
        zones.insert(
            rect,
            if yes {
                crate::app::state::HitTarget::ConfirmYes
            } else {
                crate::app::state::HitTarget::ConfirmNo
            },
            0,
            1,
        );
        x += width + gap;
    }
    zones
}

/// 歌曲右键菜单。
///
/// 居中的小弹窗，每项右侧标出对应键位——菜单是**键盘动作的索引**而不是
/// 另一套交互，标出来用户下次就能直接按。
pub fn render_context_menu(
    frame: &mut Frame,
    menu: &crate::app::state::ContextMenu,
    hits: &crate::ui::hit::HitMap,
    theme: &Theme,
) -> crate::ui::hit::HitMap {
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
        return crate::ui::hit::HitMap::default();
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut zones = crate::ui::hit::HitMap::default();
    // 标题 1 行 + 边框 1 行，菜单项从第 2 行开始
    let first_item_row = popup.y + 1 + 1;
    for (index, action) in menu.items.iter().enumerate() {
        let selected = index == menu.cursor;
        let pointer = if selected { ">" } else { " " };
        let rect = Rect::new(
            popup.x + 1,
            first_item_row + index as u16,
            popup.width.saturating_sub(2),
            1,
        );
        let hovered = !selected && hits.is_over(rect);
        let style = if selected {
            theme.selection().add_modifier(Modifier::BOLD)
        } else if hovered {
            theme.body().add_modifier(Modifier::BOLD)
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
        zones.insert(
            rect,
            crate::app::state::HitTarget::MenuItem,
            index,
            menu.items.len(),
        );
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑↓ 选择 · Enter 执行 · Esc 关闭",
        theme.dim(),
    )));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);

    let _ = MenuAction::Play; // 类型已用于 items，这里仅为可读性
    zones
}

/// 下载音质选择框：列出可选音质，默认落在当前全局音质上。
///
/// 与右键菜单同样的浮层写法（Clear + panel + 居中），但内容换成音质列表。
pub fn render_quality_picker(
    frame: &mut Frame,
    picker: &crate::app::state::QualityPicker,
    hits: &crate::ui::hit::HitMap,
    theme: &Theme,
) -> crate::ui::hit::HitMap {
    let height = (picker.candidates.len() as u16 + 6).min(frame.area().height);
    // 宽度要放得下最长的那一行（「蝰蛇全景声  viper_atmos」），太窄会截断
    let popup = crate::ui::widgets::centered_rect(frame.area(), 46, height);
    frame.render_widget(Clear, popup);

    let block = panel("下载音质", true, theme);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height == 0 {
        return crate::ui::hit::HitMap::default();
    }

    let selected_index = picker.cursor.selected().unwrap_or(0);
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled(
        truncate_to_width(&picker.song.name, inner.width as usize),
        theme.dim(),
    )));
    lines.push(Line::from(""));

    let mut zones = crate::ui::hit::HitMap::default();
    // 片名 1 行 + 空行 1 行 + 边框 1 行 = 第 3 行起是候选
    let first_item_row = popup.y + 3;
    for (index, quality) in picker.candidates.iter().enumerate() {
        let selected = index == selected_index;
        let pointer = if selected { ">" } else { " " };
        let rect = Rect::new(
            popup.x + 1,
            first_item_row + index as u16,
            popup.width.saturating_sub(2),
            1,
        );
        let hovered = !selected && hits.is_over(rect);
        let style = if selected {
            theme.selection().add_modifier(Modifier::BOLD)
        } else if hovered {
            theme.body().add_modifier(Modifier::BOLD)
        } else {
            theme.body()
        };
        zones.insert(
            rect,
            crate::app::state::HitTarget::QualityItem,
            index,
            picker.candidates.len(),
        );
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
    zones
}

/// 帮助弹窗。
///
/// `CHEATSHEET` 有 40 条键位，34 行的终端只放得下 26 条，所以面板是**可滚动**的：
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
            .draw(|frame| {
                render_sidebar(frame, Rect::new(0, 0, width, height), state, &theme);
            })
            .expect("渲染侧边栏");
        terminal
    }

    /// 登录弹窗在矮终端下不能 panic。
    ///
    /// 弹窗高度原先是 `(行数 + 5).clamp(7, area.height)`，而 `clamp` 要求
    /// min <= max：终端高度不足 7 行时（缩窗口、分屏、嵌在小面板里都会出现）
    /// 直接 panic，界面没了、终端还留在 raw 模式。这里逐档钉住临界高度。
    #[test]
    fn login_modal_survives_a_short_terminal() {
        let login = LoginState {
            qr: vec!["██".to_string()],
            qr_content: "https://example.com/login?key=abcdef".to_string(),
            key: "abcdef".to_string(),
            message: "等待扫码…".to_string(),
            finished: false,
            succeeded: false,
        };
        let theme = Theme::for_config(ThemeName::Default, false);
        let aspect = Config::default().qr_aspect;

        // 7 是原 `clamp` 的下界，取它上下各几档把边界夹住。
        for height in [1, 3, 5, 6, 7, 8] {
            let mut terminal = Terminal::new(TestBackend::new(80, height)).expect("建测试终端");
            terminal
                .draw(|frame| {
                    render_login(
                        frame,
                        &login,
                        crate::source::SourceKind::Kugou,
                        aspect,
                        &theme,
                    )
                })
                .unwrap_or_else(|error| panic!("高度 {height} 渲染登录弹窗失败：{error}"));
        }
    }

    /// 登录弹窗每帧的渲染代价。
    ///
    /// 原先 `render_login` 每帧都调 `qr_lines_fitted` 把二维码**重编码**一遍
    /// （`qr_lines` 实测约 0.9ms/次，见 widgets 的 `bench_qr_encoding_cost`）。
    /// 改成复用 `login.qr` 后热路径上只剩「克隆已渲染行 + 排版」，这里把每帧代价
    /// 钉在远低于 300µs，防止有人再把它改回重编码。带 `#[ignore]`，手动跑：
    /// `cargo test --release -- --ignored --nocapture bench_login_render_cost`。
    #[test]
    #[ignore = "基准测试，手动运行"]
    fn bench_login_render_cost() {
        // 约 80 字符，与线上登录链接同一量级，二维码尺寸一致。
        const URL: &str = "https://login-user.kugou.com/v2/qrcode?appid=1005&clientver=20000&qrcode=abcdef0123456789";
        let qr = crate::ui::widgets::qr_lines(URL, 2.0).expect("二维码能编码");
        let login = LoginState {
            qr,
            qr_content: URL.to_string(),
            key: "abcdef".to_string(),
            message: "等待扫码…".to_string(),
            finished: false,
            succeeded: false,
        };
        let theme = Theme::for_config(ThemeName::Default, false);
        let aspect = Config::default().qr_aspect;
        // 终端取「刚好装得下二维码」的大小：既保证走「复用 login.qr」的快路径，
        // 又不让 TestBackend 的整屏缓冲区 diff 喧宾夺主（它随单元格数线性增长）。
        let mut terminal = Terminal::new(TestBackend::new(60, 30)).expect("建测试终端");
        let draw = |terminal: &mut Terminal<TestBackend>| {
            terminal
                .draw(|frame| {
                    render_login(
                        frame,
                        &login,
                        crate::source::SourceKind::Kugou,
                        aspect,
                        &theme,
                    )
                })
                .expect("渲染登录弹窗");
        };

        draw(&mut terminal); // 热身，避免把首次分配算进去

        const ROUNDS: u32 = 200;
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            draw(&mut terminal);
        }
        let per_frame = start.elapsed() / ROUNDS;
        println!("登录弹窗渲染每帧：{per_frame:?}");
    }

    // ---- 登录弹窗的尺寸回归（第 6 项把「装不装得下」改成同时看宽高，这里锁死行为） ----

    /// 三种规模的二维码地址：小、中、大（大的比常见 80 列终端还宽，专门覆盖降级分支）。
    fn login_urls() -> Vec<String> {
        let mut large = String::from(
            "https://bff-pc.qishui.com/ucenter_web/app/sdk-next?aid=386088&uc_sdk=scan-auth",
        );
        for index in 0..20 {
            large.push_str(&format!("&p{index}=0123456789abcdef"));
        }
        vec![
            "https://m.kugou.com/qr?key=abc123def456".to_string(),
            "https://bff-pc.qishui.com/ucenter_web/app/sdk-next?aid=386088&token=0123456789abcdef0123456789abcdef&uc_sdk=scan-auth".to_string(),
            large,
        ]
    }

    fn login_state(content: &str, aspect: f32) -> LoginState {
        LoginState {
            qr: crate::ui::widgets::qr_lines(content, aspect).expect("二维码能编码"),
            qr_content: content.to_string(),
            key: "abcdef".to_string(),
            message: "等待扫码…".to_string(),
            finished: false,
            succeeded: false,
        }
    }

    /// 复刻 `render_login` 的几何，返回弹窗矩形与这一尺寸下实际会画出的二维码行。
    ///
    /// 故意把公式抄一遍：它一变、这里就得跟着变，正好逼着改动者回来核对
    /// 「装得下 / 装不下」的边界；断言本身只看渲染结果，不看过程。
    fn login_layout(area: Rect, login: &LoginState) -> (Rect, Vec<String>) {
        let max_width = usize::from(area.width.saturating_sub(4));
        let max_rows = usize::from(area.height.saturating_sub(7)).max(1);
        let qr_lines =
            crate::ui::widgets::qr_lines_fitted(&login.qr, max_width, max_rows).unwrap_or_default();
        let qr_width = qr_lines
            .first()
            .map(|line| line.chars().count())
            .unwrap_or(0) as u16;
        let popup_width = (qr_width + 4).max(30);
        let popup_height = (qr_lines.len() as u16 + 5).max(7).min(area.height);
        let popup = crate::ui::widgets::centered_rect(area, popup_width, popup_height);
        (popup, qr_lines)
    }

    fn is_qr_block(symbol: &str) -> bool {
        matches!(symbol, "█" | "▀" | "▄")
    }

    fn render_login_buffer(
        login: &LoginState,
        aspect: f32,
        theme: &Theme,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("建测试终端");
        terminal
            .draw(|frame| {
                render_login(
                    frame,
                    login,
                    crate::source::SourceKind::Kugou,
                    aspect,
                    theme,
                )
            })
            .expect("渲染登录弹窗");
        terminal.backend().buffer().clone()
    }

    fn count_qr_blocks(buffer: &ratatui::buffer::Buffer) -> usize {
        let area = buffer.area();
        (area.y..area.y + area.height)
            .flat_map(|y| (area.x..area.x + area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| is_qr_block(buffer[(x, y)].symbol()))
            .count()
    }

    fn blocks_in(lines: &[String]) -> usize {
        lines
            .iter()
            .map(|line| {
                line.chars()
                    .filter(|c| matches!(c, '█' | '▀' | '▄'))
                    .count()
            })
            .sum()
    }

    /// 把整个缓冲区拼成一段文字，用于在提示文案里找关键词。
    ///
    /// 必须丢掉空白：宽字符（中文）占两格，后一格是 `Cell::EMPTY`（符号为空格），
    /// 不丢的话「终端太小」会被拆成「终 端 太 小」而匹配不上。
    fn compact_text(buffer: &ratatui::buffer::Buffer) -> String {
        let area = buffer.area();
        (area.y..area.y + area.height)
            .flat_map(|y| (area.x..area.x + area.width).map(move |x| (x, y)))
            .map(|(x, y)| buffer[(x, y)].symbol())
            .filter(|symbol| !symbol.trim().is_empty())
            .collect()
    }

    /// 三种规模的二维码确实依次变大，且「大」这一档比常见 80 列终端还宽，否则测不到降级分支。
    #[test]
    fn login_url_scales_span_small_to_bigger_than_a_common_terminal() {
        let aspect = Config::default().qr_aspect;
        let widths: Vec<usize> = login_urls()
            .iter()
            .map(|url| {
                crate::ui::widgets::qr_needed_size(url, aspect)
                    .expect("能编码")
                    .0
            })
            .collect();
        assert!(
            widths[0] < widths[1] && widths[1] < widths[2],
            "三档规模应依次变大：{widths:?}"
        );
        assert!(
            widths[2] > 76,
            "最大的二维码应比 80 列终端（可用 76 列）还宽：{widths:?}"
        );
    }

    /// 任意终端尺寸、任意二维码规模下都不得 panic，也不得把二维码画到弹窗外。
    #[test]
    fn login_modal_never_panics_or_overflows() {
        let theme = Theme::for_config(ThemeName::Default, false);
        let aspect = Config::default().qr_aspect;
        let widths = [
            1u16, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 15, 20, 25, 30, 31, 32, 33, 34, 35, 36, 37, 40,
            45, 50, 51, 52, 53, 54, 55, 56, 57, 60, 70, 80, 90, 100, 120,
        ];
        let heights = [
            1u16, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 15, 20, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33,
            40, 50, 60,
        ];

        for url in login_urls() {
            let login = login_state(&url, aspect);
            for &width in &widths {
                for &height in &heights {
                    let (popup, _qr_lines) = login_layout(Rect::new(0, 0, width, height), &login);
                    let buffer = render_login_buffer(&login, aspect, &theme, width, height);
                    for y in 0..height {
                        for x in 0..width {
                            if is_qr_block(buffer[(x, y)].symbol()) {
                                assert!(
                                    x >= popup.x
                                        && x < popup.x + popup.width
                                        && y >= popup.y
                                        && y < popup.y + popup.height,
                                    "{url} 在 {width}x{height} 把二维码画到了弹窗外 ({x},{y})"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// 装得下就必须完整画出；差一格就转降级提示，且降级提示本身不越界（无块字符）。
    #[test]
    fn login_qr_appears_exactly_when_it_fits() {
        let theme = Theme::for_config(ThemeName::Default, false);
        let aspect = Config::default().qr_aspect;

        for url in login_urls() {
            let login = login_state(&url, aspect);
            let (needed_width, needed_rows) =
                crate::ui::widgets::qr_needed_size(&url, aspect).expect("能算出所需尺寸");
            // 临界尺寸：宽 = 二维码宽 + 4（弹窗左右各 2 列），高 = 行数 + 7（正文 5 行 + 上下边框）。
            let fit_width = needed_width as u16 + 4;
            let fit_height = needed_rows as u16 + 7;

            let (_, qr_lines) = login_layout(Rect::new(0, 0, fit_width, fit_height), &login);
            assert_eq!(qr_lines.len(), needed_rows, "{url}: 临界高度应判为装得下");
            let full = render_login_buffer(&login, aspect, &theme, fit_width, fit_height);
            assert_eq!(
                count_qr_blocks(&full),
                blocks_in(&qr_lines),
                "{url} 在 {fit_width}x{fit_height} 没把二维码画全"
            );

            let narrow = render_login_buffer(&login, aspect, &theme, fit_width - 1, fit_height);
            assert_eq!(
                count_qr_blocks(&narrow),
                0,
                "{url} 宽度差一格时不该再画二维码"
            );
            assert!(
                compact_text(&narrow).contains("终端太小"),
                "{url} 宽度差一格时缺降级提示"
            );

            let short = render_login_buffer(&login, aspect, &theme, fit_width, fit_height - 1);
            assert_eq!(
                count_qr_blocks(&short),
                0,
                "{url} 高度差一格时不该再画二维码"
            );
            assert!(
                compact_text(&short).contains("终端太小"),
                "{url} 高度差一格时缺降级提示"
            );
        }
    }

    /// 同一个终端上「缩小 → 放大 → 再缩小」，每一步只由当前尺寸决定，不残留上一步状态。
    #[test]
    fn login_modal_leaves_no_state_across_resizes() {
        let theme = Theme::for_config(ThemeName::Default, false);
        let aspect = Config::default().qr_aspect;
        let login = login_state(&login_urls()[1], aspect);

        let mut terminal = Terminal::new(TestBackend::new(120, 60)).expect("建测试终端");
        let draw = |terminal: &mut Terminal<TestBackend>| {
            terminal
                .draw(|frame| {
                    render_login(
                        frame,
                        &login,
                        crate::source::SourceKind::Kugou,
                        aspect,
                        &theme,
                    )
                })
                .expect("渲染登录弹窗");
        };

        draw(&mut terminal);
        let big_first = terminal.backend().buffer().clone();
        assert!(count_qr_blocks(&big_first) > 0, "大终端下二维码没画出来");

        terminal.backend_mut().resize(20, 8);
        draw(&mut terminal);
        assert_eq!(
            count_qr_blocks(terminal.backend().buffer()),
            0,
            "缩小后仍残留二维码"
        );

        terminal.backend_mut().resize(120, 60);
        draw(&mut terminal);
        assert_eq!(
            &big_first,
            terminal.backend().buffer(),
            "放大后结果与首次不一致（残留了上一步状态）"
        );
    }

    /// 侧边栏装不下时，连接状态要**压成一行摘要**保留，而不是丢进「… 另有 …」。
    ///
    /// 早先是「整块放不下就丢掉、末尾留一句『… 另有 连接 / 缓存』」：矮终端里
    /// 用户只看到一句含糊的提示，连「接口通没通、当前是什么音源」都看不到。
    /// 现在把连接压成一行 `● …`，信息仍然留在屏幕上。
    ///
    /// 取 108×18：导航占 14 行，剩下的行连连接的整块（6 行）都放不下，正好走
    /// 压缩分支。
    #[test]
    fn sidebar_keeps_a_compact_connection_line_when_short() {
        let state = AppState::new(Config::default());

        let text = screen_text(&draw_sidebar(&state, 108, 18));
        assert!(
            text.contains('●'),
            "矮终端要保留一行连接状态，而不是只剩一句含糊提示：{text}"
        );
        // 默认后端是 native：这行是「● 接口内嵌 · 酷狗」
        assert!(text.contains("接口"), "应说清接口形态：{text}");
        assert!(text.contains("酷狗"), "应带上当前音源：{text}");
        assert!(
            !text.contains("另有"),
            "给了具体摘要就不该再有「另有」这种让人猜的提示：{text}"
        );
    }

    /// 中等高度：连接放得下整块就放整块，缓存放不下就压成一行——两块都给出具体
    /// 内容，不该退回「另有」。
    #[test]
    fn sidebar_mixes_a_full_block_with_a_compact_one() {
        let state = AppState::new(Config::default());

        let text = screen_text(&draw_sidebar(&state, 108, 24));
        assert!(text.contains("API"), "连接块放得下时应显示完整内容：{text}");
        assert!(
            text.contains("缓存"),
            "缓存放不下整块时也要有一行摘要：{text}"
        );
        assert!(!text.contains("另有"), "不该再出现含糊提示：{text}");
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
    ///
    /// 「另有」这种含糊提示现在已经被具体的一行摘要取代；这条留作回归守卫，
    /// 防止以后又在信息齐全时画出一句多余的省略提示。
    #[test]
    fn sidebar_has_no_dropped_marker_when_everything_fits() {
        let state = AppState::new(Config::default());

        let text = screen_text(&draw_sidebar(&state, 112, 60));
        assert!(
            !text.contains("另有"),
            "内容全放得下时不该提示有省略：{text}"
        );
        // 第一块的标题随后端变（native 是「接口 · 内嵌」，node 是「连接 · …」），
        // 这里断言的是「两块都画出来了」，所以用各自稳定的字眼。
        for block in ["接口", "缓存"] {
            assert!(text.contains(block), "60 行终端应显示完整的「{block}」块");
        }
    }

    /// native 后端下「API」那一行不能显示 `127.0.0.1:3001`。
    ///
    /// 那个地址是历史配置值，native 时**没有进程在监听它**。照着配置显示会让用户
    /// 以为背后还有个服务（甚至以为 Node 还在跑），排查时往错的方向找。
    #[test]
    fn native_backend_does_not_advertise_a_service_address() {
        let mut state = AppState::new(Config::default());
        state.config.api_backend = crate::api::ApiBackend::Native;
        state.config.sources.active = crate::source::SourceKind::KugouConcept;
        state.config.api_base = "http://127.0.0.1:3001".to_string();

        let text = screen_text(&draw_sidebar(&state, 112, 60));
        assert!(
            !text.contains("127.0.0.1:3001"),
            "native 下不该显示服务地址：{text}"
        );
        assert!(text.contains("内嵌"), "应当说明接口是内嵌的：{text}");
        assert!(
            !text.contains("已连通"),
            "native 下没有「连接」这回事，不该报连通状态：{text}"
        );
    }

    /// node 后端照旧显示主机端口，方便排查连的是哪个实例。
    #[test]
    fn node_backend_still_shows_the_host_and_port() {
        let mut state = AppState::new(Config::default());
        state.config.api_backend = crate::api::ApiBackend::Node;
        state.config.api_base = "http://127.0.0.1:3001".to_string();

        let text = screen_text(&draw_sidebar(&state, 112, 60));
        assert!(text.contains("127.0.0.1:3001"), "实际：{text}");
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
