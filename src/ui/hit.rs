//! 声明式命中区登记。
//!
//! 早先每个视图直接调 `state.add_hit_zone(rect, target, first, len)`，问题有三：
//!
//! * 每个可点元素都要手写一次下标换算（列表的第几行 → 数据的第几条）；
//! * `first/len` 与渲染时用的 `ListState` 偏移必须**两边各算一遍**，改一行就可能错位；
//! * hover 判定只能由视图自己再算一次鼠标坐标落在哪一行上（`lists.rs` 里那段
//!   `pointer → hover_row` 就是这样），五个视图各写一份。
//!
//! 这里把三件事收进 [`HitMap`]：登记时给出**几何**，查询时统一换算。
//! 视图只负责「这块矩形可点、它对应哪个语义目标」，不再关心坐标数学。
//!
//! # 与 `HitZone` 的关系
//!
//! [`HitZone`](crate::app::state::HitZone) 仍是存储结构（app 层要按它派发事件），
//! `HitMap` 只是渲染侧的写入入口。两者字段一一对应，不引入第二套语义。

use ratatui::layout::Rect;

use crate::app::state::{HitTarget, HitZone};

/// 一帧的命中区集合。
///
/// 由 `AppState` 持有，`begin_frame` 时清空，渲染过程中由视图填充，
/// 事件到达时由 [`HitMap::at`] 查询。
#[derive(Debug, Default)]
pub struct HitMap {
    zones: Vec<HitZone>,
    /// 本帧鼠标所在的行，由渲染层统一算出（见 [`HitMap::set_pointer`]）。
    ///
    /// 视图读它来决定要不要画 hover 态，不必各自重算坐标。
    pointer: Option<(u16, u16)>,
}

impl HitMap {
    pub fn clear(&mut self) {
        self.zones.clear();
    }

    /// 记录本帧鼠标位置。`None` 表示鼠标不在窗口内（终端不上报移动时恒为 `None`）。
    pub fn set_pointer(&mut self, pointer: Option<(u16, u16)>) {
        self.pointer = pointer;
    }

    /// 登记一块可点区域。
    ///
    /// 后登记的优先（见 [`HitMap::at`]），所以「小区域盖大区域」只要按顺序登记即可：
    /// 先整块列表，再列表里的按钮。
    pub fn insert(&mut self, rect: Rect, target: HitTarget, first_index: usize, len: usize) {
        self.zones.push(HitZone {
            rect,
            target,
            first_index,
            len,
        });
    }

    /// 登记一块**没有行概念**的区域（进度条、按钮、单行动作）。
    pub fn insert_flat(&mut self, rect: Rect, target: HitTarget) {
        self.insert(rect, target, 0, 1);
    }

    /// 找出包含指定坐标的命中区（后登记的优先）。
    pub fn at(&self, column: u16, row: u16) -> Option<HitZone> {
        self.zones
            .iter()
            .rev()
            .find(|zone| zone.contains(column, row))
            .copied()
    }

    /// 坐标落在 `rect` 内时返回它相对 `rect` 的行号（0 起）。
    ///
    /// 视图用它判断 hover 落在列表的哪一行：`rect` 传内容区，返回值加滚动偏移
    /// 就是数据下标。
    pub fn row_in(&self, rect: Rect) -> Option<usize> {
        let (column, row) = self.pointer?;
        if !rect.contains(ratatui::layout::Position::new(column, row)) {
            return None;
        }
        Some(row.saturating_sub(rect.y) as usize)
    }

    /// 指针是否落在 `rect` 内。按钮、面板标题这类不需要行号的地方用它。
    pub fn is_over(&self, rect: Rect) -> bool {
        self.row_in(rect).is_some()
    }

    /// 指针落在 `rect` 内的哪一行（**数据下标**，已加滚动偏移）；不在则 `None`。
    pub fn data_index_under(&self, rect: Rect, first_index: usize, len: usize) -> Option<usize> {
        let row = self.row_in(rect)?;
        let index = first_index + row;
        (index < len).then_some(index)
    }

    /// 并入另一份命中区。弹窗渲染函数产出的区域经这里回到主表。
    pub fn extend(&mut self, other: HitMap) {
        self.zones.extend(other.zones);
    }

    /// 本帧登记了多少个命中区。内存追踪用（见 `App::tick`）。
    pub fn len(&self) -> usize {
        self.zones.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_with_pointer(column: u16, row: u16) -> HitMap {
        let mut map = HitMap::default();
        map.set_pointer(Some((column, row)));
        map
    }

    #[test]
    fn the_last_registered_zone_wins() {
        let mut map = HitMap::default();
        map.insert(Rect::new(0, 0, 20, 10), HitTarget::Songs, 0, 10);
        // 后登记的小区域应当盖住大区域
        map.insert(Rect::new(5, 5, 3, 1), HitTarget::Progress, 0, 1);

        assert_eq!(
            map.at(6, 5).map(|zone| zone.target),
            Some(HitTarget::Progress)
        );
        assert_eq!(map.at(1, 1).map(|zone| zone.target), Some(HitTarget::Songs));
    }

    #[test]
    fn hover_helpers_report_the_row_under_the_pointer() {
        let map = map_with_pointer(3, 12);
        let rect = Rect::new(0, 10, 20, 5);

        assert_eq!(map.row_in(rect), Some(2));
        assert!(map.is_over(rect));
        assert!(!map.is_over(Rect::new(0, 20, 20, 5)));

        assert_eq!(map.data_index_under(rect, 4, 100), Some(6));
        // 数据只有 5 条时，第 2 行已经越界
        assert_eq!(map.data_index_under(rect, 4, 5), None);
    }

    #[test]
    fn no_pointer_means_no_hover() {
        let map = HitMap::default();
        assert!(!map.is_over(Rect::new(0, 0, 20, 10)));
        assert_eq!(map.row_in(Rect::new(0, 0, 20, 10)), None);
    }

    #[test]
    fn insert_flat_is_a_single_row_target() {
        let mut map = HitMap::default();
        map.insert_flat(Rect::new(2, 3, 10, 1), HitTarget::Progress);

        let zone = map.at(5, 3).expect("应当命中");
        assert_eq!(zone.target, HitTarget::Progress);
        assert_eq!(zone.index_at(3), Some(0));
        assert_eq!(map.len(), 1);
    }
}
