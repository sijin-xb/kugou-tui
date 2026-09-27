//! 搜索：提交关键词、分页追加。
//!
//! 从 `update.rs` 里切出来的第一块。边界最干净：只有「提交」和「加载更多」两个入口，
//! 不碰播放、不碰队列，出去的东西只有一个 `Loaded::Search`。
//! **这一块不依赖任何兄弟模块**（它只改 `state` 与 `spawn` 任务），
//! 改搜索行为不用读别的地方。
//!
//! `--search` 的启动入口 `startup_search` **不在这里**：它要「切到搜索页 + 填词 +
//! 提交」，横跨 navigation 与 search，所以住在 `update.rs` 的分派层。曾经放在这里，
//! 结果是 search 反过来依赖 navigation——叶子互依，边界就假了。
//!
//! 两条容易踩的坑都在这里，改之前先看：
//!
//! * **分页是刻意的**。酷狗搜索只有第 1 页是精确匹配，深页塞的是兜底内容
//!   （实测搜「黑色幽默」第 2 页起变成有声书）。所以不一次取全，让用户按 `M` 一页页要。
//! * **结果回来要自证身份**。判定留在 `update.rs` 的 `Loaded::Search` 分支里
//!   （`accepts_search_result`）——用户可能已经改搜了别的词，这条迟到的结果必须丢掉。
//!   `loading_more` 这个标志同理：它挡的是连按 `M` 导致的重复页与乱序。

use crate::app::App;
use crate::event::{Loaded, LoadingTarget};

/// 搜索接口的最大页数。
///
/// 实测 `page` 超过 16 会返回 `error_code: 149`（Out Page Range）。分页是服务端
/// 硬限，不是约定，所以提在这里并注明：改这个数之前要重新打一次接口确认。
const SEARCH_MAX_PAGES: u32 = 16;

impl App {
    pub fn run_search(&mut self) {
        let keyword = self.state.search.input.text().trim().to_string();
        if keyword.is_empty() {
            self.state.warn("请输入搜索关键词");
            return;
        }

        self.state.search.submitted = keyword.clone();
        self.state.search.editing = false;
        // 新的一次搜索取代一切：上一次「加载更多」无论在不在飞，都作废
        self.state.search.loading_more = false;
        self.state.search.results.load.begin();
        self.state.search.results.title = format!("搜索「{keyword}」");
        self.state.busy = Some(format!("搜索 {keyword}"));

        let api = self.api.clone();
        let active_source = self.state.config.active_source_kind();
        let bus = self.bus.clone();
        let page_size = self.state.config.page_size;

        self.runtime.spawn(async move {
            // 刻意**只取第一页**，不做全量翻页。
            //
            // 实测：酷狗搜索只有第 1 页是精确匹配，深页塞的是兜底内容——
            // 搜「黑色幽默」翻到第 2 页往后全是「卖花的惹不起」这类有声书，
            // 全量合并会把相关结果淹没在垃圾里。MoeKoeMusic 也是分页浏览
            // （`searchResults.value = response.data.lists` 只放当前页）。
            // 想看更多按 `M` 一页页追加，顺序保持服务端的相关性。
            match active_source
                .search_songs(&api, &keyword, 1, page_size)
                .await
            {
                Ok(songs) => bus.emit(Loaded::Search {
                    keyword,
                    songs,
                    append: false,
                }),
                Err(error) => bus.fail_loading(
                    LoadingTarget::SearchResults,
                    format!("搜索「{keyword}」失败"),
                    error,
                ),
            }
        });
    }
    /// 搜索结果「加载更多」：追加下一页。
    ///
    /// 之所以分页而不是一次取全：酷狗搜索只有第 1 页是精确匹配，
    /// 深页塞的是兜底内容（实测搜「黑色幽默」第 2 页起变成有声书）。
    /// 全量合并会把相关结果淹没，所以让用户主动一页页要看。
    pub(super) fn load_more_search(&mut self) {
        let keyword = self.state.search.submitted.clone();
        if keyword.is_empty() {
            self.state.warn("请先搜索");
            return;
        }
        if self.state.search.results.songs.is_empty() {
            self.state.warn("当前没有搜索结果");
            return;
        }
        if self.state.search.loading_more {
            // 连按 M 会基于同一个 page 各发一次请求，两条结果都追加进去就是重复的
            // 一页，晚到的那条还会让顺序倒过来。等这一页回来再按。
            self.state.info("上一页还在加载，稍候");
            return;
        }

        let next_page = self.state.search.page + 1;
        // 实测上限 16 页，再往后会返回 code=149 Out Page Range
        if next_page > SEARCH_MAX_PAGES {
            self.state.info("已经到最后一页了");
            return;
        }

        let api = self.api.clone();
        let active_source = self.state.config.active_source_kind();
        let bus = self.bus.clone();
        let page_size = self.state.config.page_size;
        self.state.busy = Some(format!("加载「{keyword}」第 {next_page} 页"));

        self.state.search.page = next_page;
        self.state.search.loading_more = true;

        self.runtime.spawn(async move {
            match active_source
                .search_songs(&api, &keyword, next_page, page_size)
                .await
            {
                Ok(songs) => bus.emit(Loaded::Search {
                    keyword,
                    songs,
                    append: true,
                }),
                Err(error) => {
                    // 越界就是没有更多了，不是故障
                    if error.is_page_out_of_range() {
                        bus.emit(Loaded::Search {
                            keyword,
                            songs: Vec::new(),
                            append: true,
                        });
                    } else {
                        bus.fail_loading(
                            LoadingTarget::SearchResults,
                            format!("加载「{keyword}」更多结果失败"),
                            error,
                        );
                    }
                }
            }
        });
    }
}
