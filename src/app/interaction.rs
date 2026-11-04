//! 鼠标交互状态机：悬停、按下、拖拽。
//!
//! 终端里的「拖拽」和浏览器不一样：crossterm 只在按住期间持续上报
//! `MouseEventKind::Drag(button)`，没有"进入/离开"这种语义。所以状态机只能
//! 自己维护三个事实：
//!
//! * **hover** —— 指针落在哪个可交互元素上（每帧由渲染层回填，见 `HitMap`）；
//! * **press** —— 哪个元素被按下、按的是哪个按钮、按下时的坐标；
//! * **drag** —— 按下后指针移动了多少格。
//!
//! 三者分开存的原因是结算时机不同：hover 每帧重算，press 在 `Up` 时结算，
//! drag 在 `Drag` 事件里即时响应。混成一个「当前选中」变量就没法表达
//! 「按住进度条拖到一半又拖回来」这种状态。

use ratatui::crossterm::event::MouseButton;

use crate::app::state::HitTarget;

/// 按下但还没松开的那个目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Press {
    pub target: HitTarget,
    /// 按下时命中的数据下标（列表行、菜单项）。按钮类为 `None`。
    pub index: Option<usize>,
    pub button: MouseButton,
    /// 按下时的屏幕坐标，拖拽判定要靠它算位移。
    pub origin: (u16, u16),
    /// 指针是否已经离开过原点足够远（超过 `DRAG_THRESHOLD`）。
    ///
    /// 单纯按住不动再松开是「点击」，按住移动过才是「拖拽」——两者的结算
    /// 语义不同：点进度条是跳转到那一格，拖进度条是连续跟随。没有这个标志
    /// 的话，手抖一格也会被当成拖拽，点击就失效了。
    pub dragging: bool,
}

/// 移动超过这个格数才算「真的在拖」。
///
/// 取 2 而不是 1：鼠标在终端里一格就是一整行/一整列，手抖很容易跨过 1 格。
/// 阈值 1 会让「想点一下」被识别成拖拽，进度条就会跳到一个用户没想去的点。
/// 2 格容差足以滤掉抖动，同时真正拖起来时第一格就能感觉到跟手。
pub const DRAG_THRESHOLD: u16 = 2;

/// 交互状态。
#[derive(Debug, Default)]
pub struct Interaction {
    /// 指针当前位置。`None` 表示终端不上报移动，或指针在窗口外。
    pub hover: Option<(u16, u16)>,
    /// 当前的 press（按下未松开）。
    pub press: Option<Press>,
}

impl Interaction {
    pub fn set_hover(&mut self, pointer: Option<(u16, u16)>) {
        self.hover = pointer;
    }

    /// 开始一次按下。同一个目标重复按下（终端偶尔会重复上报）不覆盖原点，
    /// 否则拖拽位移永远从最后一格算起，拖不动。
    pub fn begin_press(
        &mut self,
        target: HitTarget,
        index: Option<usize>,
        button: MouseButton,
        origin: (u16, u16),
    ) {
        if let Some(existing) = self.press
            && existing.target == target
            && existing.index == index
            && existing.button == button
        {
            return;
        }
        self.press = Some(Press {
            target,
            index,
            button,
            origin,
            dragging: false,
        });
    }

    /// 报到一次拖拽，返回**是否刚刚跨过阈值**（首次变成拖拽时返回 true）。
    ///
    /// 调用方拿它决定「要不要执行一次初始动作」：进度条第一次跨过阈值时
    /// 应当立刻跳到指针位置，之后的每次移动只做增量跟随。
    pub fn drag_to(&mut self, pointer: (u16, u16)) -> bool {
        let Some(press) = self.press.as_mut() else {
            return false;
        };
        if press.dragging {
            return false;
        }
        let dx = pointer.0.abs_diff(press.origin.0);
        let dy = pointer.1.abs_diff(press.origin.1);
        if dx >= DRAG_THRESHOLD || dy >= DRAG_THRESHOLD {
            press.dragging = true;
            return true;
        }
        false
    }

    /// 结束按下，返回被释放的那个 press。
    pub fn end_press(&mut self) -> Option<Press> {
        self.press.take()
    }

    /// 当前按住的区域是不是 `target`（不论有没有在拖）。
    ///
    /// 「按着就生效」的控件（音量条这类）用它做按下反馈。
    #[allow(dead_code)]
    pub fn pressed(&self, target: HitTarget) -> bool {
        self.press.is_some_and(|press| press.target == target)
    }

    /// 当前按住的区域是不是 `target` 且确实在拖。
    #[allow(dead_code)]
    pub fn dragging(&self, target: HitTarget) -> bool {
        self.press
            .is_some_and(|press| press.target == target && press.dragging)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_press_starts_undragged_and_crosses_the_threshold_once() {
        let mut interaction = Interaction::default();
        interaction.begin_press(HitTarget::Progress, None, MouseButton::Left, (10, 5));

        assert!(!interaction.drag_to((11, 5)), "同一格不该算拖拽");
        assert!(interaction.drag_to((12, 5)), "跨过阈值应当返回 true");
        assert!(
            !interaction.drag_to((20, 5)),
            "已经在拖了，不该重复返回 true"
        );
        assert!(interaction.dragging(HitTarget::Progress));
    }

    #[test]
    fn a_repeat_press_on_the_same_target_keeps_the_original_origin() {
        let mut interaction = Interaction::default();
        interaction.begin_press(HitTarget::Progress, None, MouseButton::Left, (10, 5));
        // 终端重复上报同一格的按下：不能把原点改成当前位置
        interaction.begin_press(HitTarget::Progress, None, MouseButton::Left, (14, 5));

        assert_eq!(interaction.press.unwrap().origin, (10, 5));
    }

    #[test]
    fn pressing_a_different_target_replaces_the_press() {
        let mut interaction = Interaction::default();
        interaction.begin_press(HitTarget::Songs, Some(3), MouseButton::Left, (10, 5));
        interaction.begin_press(HitTarget::Progress, None, MouseButton::Left, (20, 5));

        let press = interaction.press.unwrap();
        assert_eq!(press.target, HitTarget::Progress);
        assert_eq!(press.index, None);
    }

    #[test]
    fn releasing_clears_the_press() {
        let mut interaction = Interaction::default();
        interaction.begin_press(HitTarget::Queue, Some(2), MouseButton::Right, (4, 4));

        let press = interaction.end_press().expect("应当拿到 press");
        assert_eq!(press.target, HitTarget::Queue);
        assert_eq!(press.index, Some(2));
        assert_eq!(press.button, MouseButton::Right);
        assert!(!interaction.dragging(HitTarget::Queue));
        assert!(interaction.end_press().is_none(), "第二次释放应当为空");
    }

    #[test]
    fn drag_without_a_press_is_ignored() {
        let mut interaction = Interaction::default();
        assert!(!interaction.drag_to((50, 50)));
    }
}
