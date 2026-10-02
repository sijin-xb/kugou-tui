//! 云端与账号：登录、音源切换、VIP、云端歌单。
//!
//! 从 `update.rs` 里切出来的第五块（见 `docs/MAINTENANCE.md` §1.8）。判据还是那
//! 两条：**调用方在哪、结果谁处理**——这一块的方法要么由 `update.rs` 的分派层
//! 触发，要么由 `handle_loaded` 里的登录 / 会员结果触发；请求与结果处理配套，
//! 所以放在同一个文件里读起来最省上下文。
//!
//! 分四类，改之前先认准自己在哪一类：
//!
//! | 类别 | 入口 | 出口 |
//! |---|---|---|
//! | 登录 | `L` 键、`ConfirmAction::Relogin` | `Loaded::LoginQr` / `LoginSucceeded` / `LoginFailed` |
//! | 音源 | 音源页的 Enter / `E` / `K`·`J` | 直接改配置并落盘（没有异步结果） |
//! | VIP | `V` 键、启动与登录后的自动同步 | `Loaded::VipClaimed` / `VipStatus` |
//! | 云端歌单 | `s` / `d` / `S` / `N` 与确认弹窗 | `Loaded::CloudNotice` / `CloudPlaylistChanged` |
//!
//! # 三条容易踩的坑
//!
//! * **凭据按音源分开**。`switch_source_to` 必须先把当前身份存回档案
//!   （`sync_active_source()`）再切，并且切完要按**新音源**重算 `logged_in`
//!   ——沿用上一个音源的状态会把「未登录」误判成「已登录」，登录流程就进不去了。
//! * **登录 ≠ 凭据有效**。`logged_in` 只看 cookie 里有没有 `token=`，判断不出
//!   token 是否已被服务端作废，所以 `begin_login_for` 在「已登录」时不是直接
//!   返回，而是走一次确认（见那里的注释）。
//! * **写操作都要「先提示、再刷新」**。服务端歌单同步有延迟（实测约 5 秒），
//!   所以收藏 / 移除之后要等一小会儿再发 `CloudPlaylistChanged`，否则列表里的
//!   曲数纹丝不动。

use std::time::Instant;

use crate::api::cloud::QrStatus;
use crate::app::App;
use crate::app::state::{ConfirmAction, LoginPicker, LoginState};
use crate::app::update::describe_song;
use crate::config::Config;
use crate::error::AppError;
use crate::event::{Loaded, LoadingTarget, VipClaimOutcome};
use crate::logger::tlog;
use crate::source::SourceKind;

/// 领取当日概念版 VIP：**先领一天，再升级成畅听 VIP**。
///
/// 两步是连着的——上游要求先领一天才能升级，中间隔 500ms 让服务端状态落库。
/// MoeKoeMusic 的 `getVip()` 也是这两步加同一个间隔。
async fn claim_and_upgrade(api: &crate::api::ApiClient, day: &str) -> crate::error::Result<()> {
    api.claim_day_vip(day).await?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    api.upgrade_day_vip().await?;
    Ok(())
}

/// 从错误里取出**该给用户看**的那句话。
///
/// `AppError::Api` 的 Display 是「接口 /youth/day/vip 返回错误：code=30201 今日已领取」。
/// 路径那半句对用户是纯噪音（他只有一个可能的操作），而且会把状态栏挤爆、
/// 把真正有用的原因截掉——实测过，112 列的终端上「今日已领取」正好被切没。
fn readable_reason(error: &AppError) -> String {
    match error {
        AppError::Api { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

impl App {
    // ==================================================================
    // 扫码登录
    // ==================================================================

    /// `L`：在应用内开始扫码登录。
    ///
    /// 流程是 `/login/qr/key` → `/login/qr/create` → 轮询 `/login/qr/check`。
    /// 二维码直接渲染在界面上，不用切终端，也不用额外依赖 `qrencode` 之类的命令行工具。
    pub(super) fn start_login(&mut self) {
        // 先决定「给哪个音源登录」：登录态按音源分开存，登错了地方等于没登。
        self.open_login_picker();
    }

    /// 弹出音源选择器。只有一个候选时直接跳过，不多一次交互。
    fn open_login_picker(&mut self) {
        let candidates: Vec<SourceKind> = SourceKind::ALL
            .iter()
            .copied()
            .filter(|kind| kind.capability().login)
            .collect();

        match candidates.len() {
            0 => self.state.warn("当前没有任何音源支持登录"),
            1 => self.begin_login_for(candidates[0]),
            _ => {
                let mut picker = LoginPicker {
                    candidates,
                    ..Default::default()
                };
                // 默认停在当前音源上：多数情况下用户就是想登这个
                let current = self.state.config.active_source_kind();
                if let Some(index) = picker.candidates.iter().position(|kind| *kind == current) {
                    picker.cursor.select(Some(index));
                } else {
                    picker.cursor.select(Some(0));
                }
                self.state.login_picker = Some(picker);
            }
        }
    }

    /// 选定了音源：必要时先切过去（会重建 HTTP 客户端），再走扫码。
    pub(super) fn confirm_login_source(&mut self) {
        let Some(picker) = self.state.login_picker.take() else {
            return;
        };
        let Some(kind) = picker.selected() else {
            return;
        };
        self.begin_login_for(kind);
    }

    /// 对指定音源开始扫码登录。
    fn begin_login_for(&mut self, kind: SourceKind) {
        // 音源不同就先切过去：登录请求要发到那个服务上，凭据也要存进它的档案。
        if kind != self.state.config.active_source_kind() {
            self.switch_source_to(kind);
        }

        // 已登录时**不能**就此挡住。
        //
        // `logged_in` 只看 cookie 里有没有 `token=` 字段，判断不出 token 是否已经
        // 被服务端作废（实测 `/user/playlist` 会返回 20017）。若在这里直接返回，
        // 用户就会陷入死局：云端功能全部报登录失效，可按 `L` 只回一句「已登录」，
        // 没有任何途径重新扫码。
        //
        // 但仍要确认一次：凭据有效时误按 `L` 会把它冲掉。
        if self.state.logged_in {
            self.state.pending_confirm = Some(ConfirmAction::Relogin);
            return;
        }

        self.begin_login();
    }

    /// 确认后重新登录：清掉旧凭据（保留 dfid，它是设备指纹不是登录态）再扫码。
    pub(super) fn relogin(&mut self) {
        self.state.config.cookie = None;
        self.state.logged_in = false;
        // cookie 清空后 cookie_header() 只会剩下 dfid，正是想要的效果
        self.api.set_cookie(self.state.config.cookie_header());
        self.begin_login();
    }

    /// 真正发起扫码请求。与「要不要扫」的判断分开，两条入口共用。
    fn begin_login(&mut self) {
        if let Some(login) = self.state.login.as_ref()
            && !login.finished
        {
            self.state.info("登录已在进行中，扫码或按 Esc 取消");
            return;
        }

        // 节流：每次取 key 都是向服务端申请一个新的登录会话，短时间内反复申请
        // 会被判「登录频繁」，手机端就扫不了了（实测用户就是这么被限的）。
        // 宁可让用户多等几秒，也别把账号搞限流。
        const QR_KEY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);
        if let Some(last) = self.state.last_qr_key_at {
            let waited = last.elapsed();
            if waited < QR_KEY_COOLDOWN {
                let left = (QR_KEY_COOLDOWN - waited).as_secs() + 1;
                self.state.warn(format!(
                    "取二维码太频繁了，请 {left} 秒后再按 L（频繁申请会被服务端限流）"
                ));
                return;
            }
        }
        self.state.last_qr_key_at = Some(Instant::now());

        self.state.login = Some(LoginState {
            message: "正在获取二维码…".to_string(),
            ..Default::default()
        });

        let api = self.api.clone();
        let bus = self.bus.clone();
        let active_source = self.state.config.active_source_kind();

        self.runtime.spawn(async move {
            let key = match active_source.login_qr_key(&api).await {
                Ok(key) => key,
                Err(error) => {
                    bus.fail("获取登录二维码失败", error);
                    return;
                }
            };

            match active_source.login_qr_create(&api, &key).await {
                Ok(content) => bus.emit(Loaded::LoginQr { key, content }),
                Err(error) => bus.fail("生成登录二维码失败", error),
            }
        });
    }

    /// 轮询扫码状态。由 tick 每约 2 秒调用一次。
    pub(super) fn poll_login(&mut self) {
        let Some(login) = self.state.login.as_ref() else {
            return;
        };
        if login.finished || login.key.is_empty() {
            return;
        }

        let key = login.key.clone();
        let api = self.api.clone();
        let bus = self.bus.clone();
        let active_source = self.state.config.active_source_kind();

        self.runtime.spawn(async move {
            let check = match active_source.login_qr_check(&api, &key).await {
                Ok(check) => check,
                Err(error) => {
                    bus.fail("查询扫码状态失败", error);
                    return;
                }
            };

            match check.status {
                QrStatus::Expired => bus.emit(Loaded::LoginFailed {
                    message: "二维码已过期，请按 Esc 后重新按 L".to_string(),
                }),
                QrStatus::Waiting => bus.emit(Loaded::LoginStatus {
                    message: "等待扫码…".to_string(),
                }),
                QrStatus::Pending => bus.emit(Loaded::LoginStatus {
                    message: "已扫码，请在手机上确认".to_string(),
                }),
                QrStatus::Success => {
                    // 登录态由服务端持有的音源（如网易云）拿不到 token，
                    // 成功就是成功，不该报「未拿到 token」。
                    if !active_source.capability().client_token {
                        bus.emit(Loaded::LoginSucceeded {
                            token: None,
                            userid: None,
                            cookie: check.cookie.clone(),
                        })
                    } else {
                        match (check.token, check.userid) {
                            (Some(token), Some(userid)) => bus.emit(Loaded::LoginSucceeded {
                                token: Some(token),
                                userid: Some(userid),
                                cookie: None,
                            }),
                            _ => bus.emit(Loaded::LoginFailed {
                                message: "扫码已授权，但未拿到 token".to_string(),
                            }),
                        }
                    }
                }
            }
        });
    }

    /// 结束登录（成功或失败）。
    pub(super) fn finish_login(&mut self, succeeded: bool, message: String) {
        self.state.login = Some(LoginState {
            finished: true,
            succeeded,
            message,
            ..Default::default()
        });
    }

    /// 兜底：服务端下发的 cookie 为空时（理论上不会，但万一）的收尾路径。
    ///
    /// 正常路径走 `apply_server_cookie`——把含 `MUSIC_U` 的 cookie 写进
    /// 配置，热更新 ApiClient 并存盘。这里**只**在服务端 cookie 为空字符串
    /// 时被兜住，只更新界面状态，没有任何凭据可存。
    pub(super) fn finish_server_side_login(&mut self) {
        let kind = self.state.config.active_source_kind();
        self.state.logged_in = true;
        self.finish_login(
            true,
            format!(
                "「{}」已在服务端完成登录（凭据由 {} 保管，未写入本地配置）",
                kind.label(),
                kind.service_name()
            ),
        );
    }

    /// 写入登录凭据并热更新 ApiClient 的 cookie。
    pub(super) fn apply_login(&mut self, token: String, userid: String) {
        let cookie = format!("token={token}; userid={userid}");

        self.state.config.cookie = Some(cookie);
        // 必须走 `cookie_header()`：它会把 dfid 拼进去。直接用裸 cookie 会把
        // 已有的 dfid 冲掉，本次会话取播放直链就会报「本次请求需要验证」。
        self.api.set_cookie(self.state.config.cookie_header());

        let config_path = Config::path();
        match self.state.config.save() {
            Ok(()) => {
                self.state.logged_in = true;
                // 刻意**不回显 cookie**：它等价于账号密码，显示在界面上会被旁观者看到。
                // 只告诉用户写到了哪里。
                self.finish_login(
                    true,
                    format!(
                        "登录成功（userid={userid}），凭据已写入 {}",
                        config_path.display()
                    ),
                );
                self.state.success("登录成功，按 Esc 关闭");
                // 顺带取一次会员信息，界面上能直接看到服务端认定的会员形态
                self.fetch_vip_status();
                self.fetch_user_info();
                // 概念版账号刚登录就把当天的 VIP 领了——这就是它的机制，
                // 「登录就是 VIP」。今天已经领过的话内部会直接返回。
                self.maybe_claim_daily_vip();
            }
            Err(error) => {
                self.finish_login(false, format!("保存登录凭据失败：{error}"));
            }
        }
    }

    /// 登录凭证由**服务端下发**时的收尾（网易云走这条路）。
    ///
    /// 网易云的登录态**就是** `login/qr/check` 响应里那个 cookie（含 `MUSIC_U`）。
    /// 不接住并写进配置，之后的请求不带任何身份：界面上写着「登录成功」，可
    /// `/user/playlist` 拿不到 uid、云端歌单照旧报「尚未登录」——成功只是个谎言。
    ///
    /// 与 [`Self::apply_login`] 的差别只是凭据从哪来：那边自己拼 `token=; userid=`，
    /// 这边直接用服务端给的一整串。
    pub(super) fn apply_server_cookie(&mut self, cookie: String) {
        self.state.config.cookie = Some(cookie);
        self.api.set_cookie(self.state.config.cookie_header());

        let config_path = Config::path();
        match self.state.config.save() {
            Ok(()) => {
                self.state.logged_in = true;
                // 同样**不回显** cookie：它等价于账号密码。
                self.finish_login(
                    true,
                    format!(
                        "「{}」登录成功，凭据已写入 {}",
                        self.state.config.active_source_kind().label(),
                        config_path.display()
                    ),
                );
                self.state.success("登录成功，按 Esc 关闭");
                self.fetch_vip_status();
                self.fetch_user_info();
                self.maybe_claim_daily_vip();
            }
            Err(error) => {
                self.finish_login(false, format!("保存登录凭据失败：{error}"));
            }
        }
    }

    // ==================================================================
    // 音源管理：启用 / 设为默认 / 调优先级
    // ==================================================================

    /// 当前在音源页选中的是哪个音源。
    fn selected_source(&self) -> Option<SourceKind> {
        let kinds = self.state.config.sources.ordered();
        let index = self.state.sources_cursor.selected().unwrap_or(0);
        kinds.get(index).copied()
    }

    /// 启用 / 禁用选中的音源。
    pub(super) fn toggle_source_enabled(&mut self) {
        let Some(kind) = self.selected_source() else {
            return;
        };
        let profile = self.state.config.sources.profile_mut(kind);
        profile.enabled = !profile.enabled;
        let enabled = profile.enabled;

        // 不能把当前正在用的音源关掉：那样界面会处于「有音源但没选中」的状态。
        if !enabled && self.state.config.active_source_kind() == kind {
            if let Some(fallback) = self.state.config.sources.enabled().first().copied() {
                self.state.config.sync_active_source();
                self.state.config.switch_source(fallback);
                self.state.warn(format!(
                    "已禁用「{}」，当前音源切到「{}」",
                    kind.label(),
                    fallback.label()
                ));
            } else {
                self.state.config.sources.profile_mut(kind).enabled = true;
                self.state.error("至少要保留一个启用的音源");
                return;
            }
        } else {
            self.state.success(format!(
                "「{}」已{}",
                kind.label(),
                if enabled { "启用" } else { "禁用" }
            ));
        }

        self.persist_source_config();
    }

    /// 把选中的音源设为默认（即当前音源）。
    pub(super) fn set_default_source(&mut self) {
        let Some(kind) = self.selected_source() else {
            return;
        };
        if self.state.config.active_source_kind() == kind {
            self.state
                .info(format!("「{}」已经是当前音源", kind.label()));
            return;
        }
        if !self.state.config.sources.profile(kind).enabled {
            self.state
                .warn(format!("「{}」已禁用，先按 Enter 启用", kind.label()));
            return;
        }
        self.switch_source_to(kind);
    }

    /// 调整选中音源的优先级（`raise = true` 表示往前排）。
    ///
    /// 直接交换相邻两项的 priority：比「整体重排」改动小，也更符合直觉。
    pub(super) fn shift_source_priority(&mut self, raise: bool) {
        let Some(kind) = self.selected_source() else {
            return;
        };
        let mut kinds = self.state.config.sources.ordered();
        let Some(position) = kinds.iter().position(|candidate| *candidate == kind) else {
            return;
        };
        let target = if raise {
            position.checked_sub(1)
        } else {
            (position + 1 < kinds.len()).then_some(position + 1)
        };
        let Some(target) = target else {
            self.state.info(if raise {
                "已经是第一个"
            } else {
                "已经是最后一个"
            });
            return;
        };
        kinds.swap(position, target);

        // 按新顺序重排 priority，间隔 10 方便以后往中间插
        for (index, kind) in kinds.iter().enumerate() {
            self.state.config.sources.profile_mut(*kind).priority = (index as u32 + 1) * 10;
        }
        self.state.success(format!(
            "「{}」优先级已{}（第 {} 位）",
            kind.label(),
            if raise { "上调" } else { "下调" },
            target + 1
        ));
        self.persist_source_config();
    }

    /// 音源配置改动后落盘。失败只提示，不阻断操作。
    fn persist_source_config(&mut self) {
        if let Err(error) = self.state.config.save() {
            self.state.error(format!("保存音源配置失败：{error}"));
        }
    }

    ///
    /// 只换「去哪儿请求 + 带什么身份」，**不碰播放队列、不打断当前曲目**——
    /// 正在放的音频已经在本地缓存里，换音源没有理由把它停掉。
    /// 切换到指定音源，并重建 HTTP 客户端。
    pub fn switch_source_to(&mut self, kind: SourceKind) {
        let previous = self.state.config.active_source_kind();
        if previous == kind {
            return;
        }

        // 先把当前身份存回档案，否则切走再切回来时登录态和 dfid 就丢了
        self.state.config.sync_active_source();
        self.state.config.switch_source(kind);
        // 登录态是按音源分开的：切过去之后要按**新音源**的凭据重新判断，
        // 否则会沿用上一个音源的 logged_in，把「未登录」误判成「已登录」，
        // 于是登录流程被「是否覆盖已有凭据」的确认挡住，进不了扫码。
        self.state.logged_in = self.state.config.is_logged_in();

        if let Err(error) = self.state.config.save() {
            self.state
                .warn(format!("音源已切换，但保存配置失败：{error}"));
        }

        // 目标音源的服务可能还没起（默认只为当前音源拉起一个）。
        //
        // 这里只 spawn、不下载安装：界面已经画在屏幕上了，一次 `npm install` 要几十
        // 秒，冻住界面不可接受。而依赖目录是各平台共用的，装过一次就够，正常情况只是
        // 起一个进程、一到两秒。真没装过时下面会给一句「先跑 --api-start」的提示。
        if let Err(error) = crate::bootstrap::ensure_running(
            kind,
            &self.state.config.api_base,
            self.state.config.api_dir.as_deref(),
        ) {
            self.state.warn(format!("{error}"));
        }

        match crate::api::ApiClient::new(
            &self.state.config.api_base,
            self.state.config.cookie_header(),
            self.state.config.proxy.as_deref(),
        ) {
            Ok(client) => {
                self.api = client;
                self.state.vip_info = None;
                self.fetch_vip_status();
                self.fetch_user_info();
                self.ensure_device_fingerprint();
                // 切到概念版就把当天 VIP 领了。放在这里而不只在启动时做：
                // 常用标准版的人切换过来时，启动那次早就过去了，否则永远等不到
                // 自动领取。今天已经领过的话内部会直接返回，不会重复打接口。
                self.maybe_claim_daily_vip();
                let capability = kind.capability();
                if !capability.catalog {
                    self.state.warn(format!(
                        "已切换到「{}」，该音源仅支持搜索与播放（歌单/榜单/云端歌单不可用）",
                        kind.label()
                    ));
                } else if self.state.config.cookie.is_none() {
                    self.state.warn(format!(
                        "已切换到「{}」，但该音源还没登录——按 L 重新扫码（两个平台账号不通用）",
                        kind.label()
                    ));
                } else {
                    self.state.success(format!(
                        "已切换到「{}」音源，当前播放不受影响",
                        kind.label()
                    ));
                }
            }
            Err(error) => {
                self.state.error(format!(
                    "切换到「{}」失败：{error}（需要 {} 服务运行在 {}）",
                    kind.label(),
                    kind.service_name(),
                    self.state.config.api_base
                ));
            }
        }
    }

    // ==================================================================
    // 账号资料与 VIP
    // ==================================================================

    /// 取当前登录用户的资料（昵称 / 头像 / 等级 / 听歌时长）。
    ///
    /// 跟会员信息一样，取不到不影响听歌，静默降级。
    pub fn fetch_user_info(&mut self) {
        if !self.state.logged_in {
            self.state.user_info = None;
            self.state.user_info_load.succeed();
            return;
        }

        let api = self.api.clone();
        let bus = self.bus.clone();
        // 走音源分派：网易云的资料在 `/user/detail?uid=` 里，而且得先问出 uid；
        // 酷狗的是同一个端点但不带参数。写死哪一个都会让另一边报错。
        let source = self.state.config.active_source_kind();

        self.state.user_info_load.begin();
        self.runtime.spawn(async move {
            match source.user_detail(&api).await {
                Ok(info) => bus.emit(Loaded::UserInfo(Box::new(info))),
                // 早先这里只写日志，于是接口挂掉时首页永远显示「加载中…」——
                // 用户分不清是失败还是慢。失败也得走事件，面板才有出口。
                Err(error) => bus.fail_loading(LoadingTarget::UserInfo, "获取用户资料失败", error),
            }
        });
    }

    /// 下载并解码头像。
    ///
    /// 走和封面同一条管线，但**不复用** \`state.cover\`——那是当前歌曲的专辑图，
    /// 会被切歌换掉；头像得单独存一份。
    pub(super) fn load_avatar(&mut self, url: String) {
        if self.state.config.lite_mode {
            return;
        }
        let downloader = self.downloader.clone();
        let bus = self.bus.clone();

        self.runtime.spawn(async move {
            let bytes = match downloader.fetch_bytes(&url).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    tlog!(crate::logger::LEVEL_WARN, "下载头像失败 {url}：{error}");
                    return;
                }
            };
            match image::load_from_memory(&bytes) {
                Ok(image) => bus.emit(Loaded::AvatarReady { image }),
                Err(error) => tlog!(crate::logger::LEVEL_WARN, "解码头像失败 {url}：{error}"),
            }
        });
    }

    /// 拉一次会员信息，把摘要显示在侧边栏。
    ///
    /// 这一步是排查「明明有会员却只能试听」的关键：界面上能直接看到服务端认定的
    /// 会员形态与到期时间，不用去翻日志或 curl。
    pub fn fetch_vip_status(&mut self) {
        // 会员接口只有酷狗有。网易云没有对应端点（`/user/vip/detail` 是 404），
        // 去请求只会白打一次接口，所以先问能力再决定。
        if !self.state.logged_in || !self.state.config.active_source_kind().capability().vip {
            self.state.vip_info = None;
            return;
        }

        let api = self.api.clone();
        let bus = self.bus.clone();

        self.runtime.spawn(async move {
            match api.user_vip_detail().await {
                Ok(info) => bus.emit(Loaded::VipStatus(Box::new(info))),
                // 取不到会员信息不影响听歌，静默降级即可
                Err(error) => tlog!(crate::logger::LEVEL_WARN, "获取会员信息失败：{error}"),
            }
        });
    }

    /// 启动 / 登录 / 切到概念版时**自动**同步当天的概念版 VIP。
    ///
    /// 与手动触发（[`Self::claim_daily_vip`]）的唯一区别：本地已经记着今天领过时
    /// **直接返回，不打网络**。上游文档写着「尽量别频繁调用」，这接口还带风控。
    pub fn maybe_claim_daily_vip(&mut self) {
        if !self.state.logged_in
            || self.state.config.active_source_kind() != SourceKind::KugouConcept
        {
            return;
        }
        let Some(day) = crate::util::today_local() else {
            // 取不到本地日期就不自动领；手动按键时会给出明确提示
            return;
        };
        if self.state.vip_claimed_day.as_deref() == Some(day.as_str()) {
            return;
        }
        self.start_vip_sync(day, false);
    }

    /// 手动同步当天的概念版 VIP（快捷键 `V`，或点「我的资料」里那一行）。
    ///
    /// 手动时不看本地日期，一律问服务端——本地那个日期只记「**这台机器**领过」，
    /// 你在手机上领过它是不知道的。
    pub fn claim_daily_vip(&mut self) {
        if self.state.vip_claiming {
            return;
        }
        if !self.state.logged_in {
            self.state.warn("领取 VIP 需要先登录（按 L 扫码）");
            return;
        }
        if self.state.config.active_source_kind() != SourceKind::KugouConcept {
            self.state
                .warn("领取 VIP 是概念版专属功能，先按 v 切到「酷狗概念版」");
            return;
        }
        let Some(day) = crate::util::today_local() else {
            // 宁可放弃也不猜：猜错就是白领一天已经过去的 VIP
            self.state
                .warn("取不到本地日期，无法领取 VIP（可到手机端领取）");
            return;
        };
        self.start_vip_sync(day, true);
    }

    /// 同步当日 VIP 的实际动作：**先问服务端今天领过没，没领过才去领**。
    ///
    /// # 为什么必须先查记录
    ///
    /// 领取接口对「今天已经领过」**只回一个 `error_code`、不给描述**（实测如此），
    /// 界面只能显示「服务端未提供错误描述」，看着像程序坏了；更糟的是原来的文案
    /// 会补一句「反复失败请到手机端领取」，而事实恰恰相反——手机上领过了才是原因。
    ///
    /// 只读的 `/youth/month/vip/record` 能直接回答这个问题，而且比本地记的日期可靠：
    /// 领取可能发生在手机或另一台机器上。查到已经领过就不打写请求了，既省一次调用
    /// （接口带风控），也不会报一个与事实相反的错。
    fn start_vip_sync(&mut self, day: String, manual: bool) {
        let api = match self.client_for(SourceKind::KugouConcept) {
            Ok(client) => client,
            Err(error) => {
                self.state.error(format!("无法连接概念版接口：{error}"));
                return;
            }
        };
        let bus = self.bus.clone();

        self.state.vip_claiming = true;
        self.state.info("正在同步今日 VIP…");

        self.runtime.spawn(async move {
            // 查不到记录就当没领过，照常尝试——不能因为一个只读接口失败就放弃领取
            let already = match api.claimed_vip_days().await {
                Ok(days) => days.iter().any(|claimed| claimed == &day),
                Err(error) => {
                    tlog!(crate::logger::LEVEL_WARN, "查询 VIP 领取记录失败：{error}");
                    false
                }
            };

            let outcome = if already {
                VipClaimOutcome::AlreadyClaimed
            } else {
                match claim_and_upgrade(&api, &day).await {
                    Ok(()) => VipClaimOutcome::Claimed,
                    Err(error) => {
                        tlog!(crate::logger::LEVEL_WARN, "领取 VIP 失败：{error}");
                        VipClaimOutcome::Failed(readable_reason(&error))
                    }
                }
            };

            bus.emit(Loaded::VipClaimed {
                day,
                outcome,
                manual,
            });
        });
    }

    // ==================================================================
    // 云端歌单：收藏 / 移除 / 新建 / 删除 / 同步
    // ==================================================================

    /// `d`：把选中歌曲从当前云端歌单移除。
    ///
    /// 用的是歌单条目的 `fileid` 而不是 hash —— 歌单里同一首歌的 fileid 才是它在歌单里的位置标识。
    pub fn remove_focused_song_from_cloud(&mut self) {
        // 取 owned 而不是借用：成功后要把它 move 进异步任务里去发刷新事件，
        // 借用既不能 move 进 `'static` 闭包，也会和下面的 `self.state.warn` 抢借用。
        let Some(target) = self.state.sync_target.clone() else {
            self.state.warn("请先在「云端」标签页选中一个歌单");
            return;
        };

        let Some(list_id) = target.list_id else {
            self.state.error("该歌单不可写（缺少 listid）");
            return;
        };

        if !self.state.logged_in {
            self.state.warn("需要登录才能修改云端歌单，按 L 扫码登录");
            return;
        }

        let Some(song) = self.state.selected_song() else {
            self.state.warn("当前没有选中的歌曲");
            return;
        };

        // 酷狗靠**歌单条目的 fileid** 定位，网易云靠**歌曲 id**（`Song::hash`），
        // 所以只有前者需要 fileid——用一个统一的检查把网易云的歌也拦掉是错的。
        let source = self.state.config.active_source_kind();
        if !matches!(source, SourceKind::Netease) && song.file_id.is_none() {
            // 只有歌单接口才给 fileid；搜索结果没有，无法定位酷狗歌单内的条目。
            self.state
                .warn("这首歌不在歌单里（缺少 fileid），无法从歌单移除");
            return;
        }

        let label = describe_song(&song);
        let name = target.name.clone();
        let api = self.api.clone();
        let bus = self.bus.clone();

        self.state.busy = Some(format!("从《{name}》移除歌曲"));

        self.runtime.spawn(async move {
            match api
                .remove_tracks_from_playlist(source, list_id, std::slice::from_ref(&song))
                .await
            {
                Ok(count) => {
                    bus.emit(Loaded::CloudNotice(format!(
                        "已从《{name}》移除 {count} 首歌"
                    )));
                    // 同上
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    bus.emit(Loaded::CloudPlaylistChanged {
                        playlist: Box::new(target.clone()),
                    });
                }
                Err(error) => bus.fail(format!("从《{}》移除《{}》失败", name, label), error),
            }
        });
    }

    /// 执行「删除云端歌单」。
    pub(super) fn delete_cloud_playlist(&mut self) {
        let Some(target) = self.state.sync_target.take() else {
            self.state.warn("没有选中的云端歌单");
            return;
        };

        let Some(list_id) = target.list_id else {
            self.state.error("该歌单不可写（缺少 listid）");
            return;
        };

        if !self.state.logged_in {
            self.state.warn("需要登录才能删除云端歌单，按 L 扫码登录");
            return;
        }

        let name = target.name.clone();
        let source = self.state.config.active_source_kind();
        let api = self.api.clone();
        let bus = self.bus.clone();

        self.state.busy = Some(format!("删除歌单《{name}》"));

        self.runtime.spawn(async move {
            match api.delete_playlist(source, list_id).await {
                Ok(()) => bus.emit(Loaded::CloudNotice(format!("已删除歌单《{}》", name))),
                Err(error) => bus.fail(format!("删除歌单《{}》失败", name), error),
            }
        });
    }

    /// 用输入的名字新建云端歌单。
    pub(super) fn create_cloud_playlist(&mut self, name: String) {
        if name.trim().is_empty() {
            self.state.warn("歌单名称不能为空");
            return;
        }

        if !self.state.logged_in {
            self.state.warn("需要登录才能新建云端歌单，按 L 扫码登录");
            return;
        }

        let source = self.state.config.active_source_kind();
        let api = self.api.clone();
        let bus = self.bus.clone();
        self.state.busy = Some("新建云端歌单".to_string());

        self.runtime.spawn(async move {
            match api.create_playlist(source, &name).await {
                Ok(list_id) => bus.emit(Loaded::CloudNotice(match list_id {
                    Some(id) => format!("已新建歌单《{name}》（listid={id}）"),
                    None => format!("已新建歌单《{name}》"),
                })),
                Err(error) => bus.fail(format!("新建歌单《{}》失败", name), error),
            }
        });
    }

    pub(super) fn add_focused_song_to_cloud(&mut self) {
        if !self.state.logged_in {
            self.state.warn("云端歌单需要登录，请配置 cookie");
            return;
        }
        // 优先用**当前打开的**歌单：用户眼前就是这个歌单，加到这里才符合直觉。
        // 原来只用 sync_target（上次选的那个），它未必等于眼前这个，于是出现
        // 「在《我喜欢》里按 s，歌加到别的歌单去了，眼前这个不刷新」。
        let Some(target) = self
            .state
            .cloud
            .open_playlist
            .clone()
            .filter(|playlist| playlist.is_writable())
            .or_else(|| self.state.sync_target.clone())
        else {
            self.state.warn("请先在「云端」标签页选中一个歌单作为目标");
            return;
        };
        let Some(list_id) = target.list_id else {
            self.state.error("该歌单不可写（缺少 listid）");
            return;
        };
        let Some(song) = self.state.selected_song() else {
            self.state.warn("当前没有选中的歌曲");
            return;
        };

        let label = describe_song(&song);
        let playlist_name = target.name.clone();
        let source = self.state.config.active_source_kind();
        let api = self.api.clone();
        let bus = self.bus.clone();

        self.state.busy = Some(format!("收藏《{label}》"));

        self.runtime.spawn(async move {
            match api
                .add_tracks_to_playlist(source, list_id, std::slice::from_ref(&song))
                .await
            {
                Ok(_) => {
                    bus.emit(Loaded::CloudNotice(format!(
                        "已把《{label}》收藏到《{playlist_name}》，正在刷新列表"
                    )));
                    // 先给提示，稍等一下再拉列表。真正让「刷新看不到新歌」的是服务端
                    // 2 分钟缓存（已由 fresh=true 绕开），这里只留一点余量给写操作落定。
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    bus.emit(Loaded::CloudPlaylistChanged {
                        playlist: Box::new(target.clone()),
                    });
                }
                Err(error) => bus.fail(format!("收藏《{label}》失败"), error),
            }
        });
    }

    pub(super) fn sync_queue_to_cloud(&mut self) {
        if !self.state.logged_in {
            self.state.warn("云端歌单需要登录，请配置 cookie");
            return;
        }
        let Some(target) = self.state.sync_target.clone() else {
            self.state
                .warn("请先在「云端」标签页选中一个歌单作为同步目标");
            return;
        };
        let Some(list_id) = target.list_id else {
            self.state.error("该歌单不可写（缺少 listid）");
            return;
        };
        if self.state.queue.is_empty() {
            self.state.warn("播放队列为空，没有可同步的内容");
            return;
        }

        let songs = self.state.queue.items().to_vec();
        let count = songs.len();
        let playlist_name = target.name.clone();
        let source = self.state.config.active_source_kind();
        let api = self.api.clone();
        let bus = self.bus.clone();

        self.state.busy = Some(format!("同步 {count} 首到《{playlist_name}》"));

        self.runtime.spawn(async move {
            match api.add_tracks_to_playlist(source, list_id, &songs).await {
                Ok(written) => bus.emit(Loaded::CloudNotice(format!(
                    "已把 {written} 首歌同步到《{playlist_name}》"
                ))),
                Err(error) => bus.fail(format!("同步到《{playlist_name}》失败"), error),
            }
        });
    }
}
