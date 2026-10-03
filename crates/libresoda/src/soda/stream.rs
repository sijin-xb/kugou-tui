//! 整曲取流诊断：一次调用说清「拿到的是整曲还是试听、为什么、缺什么」。
//!
//! 汽水的播放流分两层：
//!
//! | 层 | 端点 | 特点 |
//! | --- | --- | --- |
//! | Web / 分享页 | `/luna/h5/seo_track`、`/luna/pc/track_v2`（`device_platform=web`） | 只回**试听片段**（30s / 60s），不需要签名 |
//! | App | `POST /luna/pc/track_v2`（带 `x-helios` / `x-medusa`） | 回整曲，未带签名头时 **HTTP 200 + 空 body** |
//!
//! 于是"VIP 曲目只能拿到 30 秒"的根因不是 VIP 判定，而是缺少应用级签名头。
//! [`crate::Soda::check_stream_access`] 把这条链路的所有结果摊开：
//! 每个来源的候选流、是否试听、以及 App 端点的原始失败原因。
//!
//! 抓取应用签名凭证的步骤见 `docs/FULL-QUALITY-STREAM.md`。

use super::quality::{better_stream_candidate, is_lossless, is_preview, track_duration_seconds};
use super::track::{
    best_from_video_model, fetch_pc_track_v2, fetch_player_info, fetch_web_track_v2,
};
use super::types::DownloadInfo;
use super::Soda;
use crate::error::{Result, SodaError};
use serde::{Deserialize, Serialize};

/// 取流诊断结果。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamAccessReport {
    pub track_id: String,
    /// 曲目元数据里的整曲时长（秒）。
    pub track_duration_seconds: i64,
    /// 选中的流来自哪个端点：`pc`（App 端点）或 `web`（分享页 / 网页端点）。
    pub source: String,
    /// 选中的流；一个候选都没有时为 `None`。
    pub best: Option<DownloadInfo>,
    /// 选中流是否只是试听片段。
    pub is_preview: bool,
    /// 选中流是否无损。
    pub is_lossless: bool,
    /// 该曲目在平台侧是否属于"仅会员"曲目。
    pub requires_vip: bool,
    pub has_cookie: bool,
    /// 是否已配置设备指纹（`device_id`）——App 端点请求必须与签名器同设备。
    pub has_app_credentials: bool,
    /// 是否配置了实时签名器（`x-helios` / `x-medusa` 每次现算）。
    pub has_signature_provider: bool,
    /// App 端点不可用时的原始原因（未配置 / 空 body / 已过期）。
    pub app_error: String,
    /// 人话结论，可直接展示给终端用户。
    pub hint: String,
}

impl StreamAccessReport {
    /// 是否拿到了整曲（非试听）流。
    pub fn is_full_track(&self) -> bool {
        self.best.is_some() && !self.is_preview
    }

    /// 设备指纹与签名来源是否都就位（App 端点可用）。
    pub fn has_signing_chain(&self) -> bool {
        self.has_app_credentials && self.has_signature_provider
    }

    /// 选中流的 URL（含 `#auth=`）。
    pub fn url(&self) -> String {
        self.best
            .as_ref()
            .map(DownloadInfo::full_url)
            .unwrap_or_default()
    }
}

fn candidate_is_better(
    candidate: &DownloadInfo,
    current: &DownloadInfo,
    full_duration: i64,
) -> bool {
    // 先比完整性（试听片段一律垫底），再比音质档位。
    let candidate_preview = is_preview(candidate, full_duration);
    let current_preview = is_preview(current, full_duration);
    if candidate_preview != current_preview {
        return !candidate_preview;
    }
    better_stream_candidate(
        candidate.duration,
        &candidate.quality,
        &candidate.format,
        candidate.bitrate,
        candidate.size,
        current.duration,
        &current.quality,
        &current.format,
        current.bitrate,
        current.size,
    )
}

fn push_candidates(
    list: &mut Vec<(String, DownloadInfo)>,
    source: &str,
    soda: &Soda,
    response: &super::types::TrackV2Response,
) {
    if let Some(info) = response
        .track_player
        .video_model
        .as_ref()
        .and_then(best_from_video_model)
    {
        list.push((source.to_string(), info));
    }
    if !response.track_player.url_player_info.trim().is_empty() {
        if let Ok(info) = fetch_player_info(soda, &response.track_player.url_player_info) {
            list.push((source.to_string(), info));
        }
    }
}

/// 诊断某首曲目的取流能力（`Soda::check_stream_access` 的实现）。
pub fn check_stream_access(soda: &Soda, track_id: &str) -> Result<StreamAccessReport> {
    let track_id = track_id.trim();
    if track_id.is_empty() {
        return Err(SodaError::invalid_input("track id is empty"));
    }

    let web = fetch_web_track_v2(soda, track_id)?;
    let track = web.primary_track();
    let full_duration = track_duration_seconds(track.duration);
    let requires_vip = track.label_info.is_vip();

    let mut candidates: Vec<(String, DownloadInfo)> = Vec::new();
    push_candidates(&mut candidates, "web", soda, &web);

    let credentials = soda.app_credentials();
    let has_device_fingerprint = credentials
        .as_ref()
        .map(|value| value.has_device_fingerprint())
        .unwrap_or(false);
    let has_signature_provider = soda.signature_provider().is_some();
    // 两条路都能让 App 端点返回整曲：抓包得到的静态签名头，或实时签名器。
    let can_sign = has_device_fingerprint
        && (has_signature_provider
            || credentials
                .as_ref()
                .map(|value| value.is_complete())
                .unwrap_or(false));
    let mut app_error = String::new();
    if can_sign {
        match fetch_pc_track_v2(soda, track_id) {
            Ok(pc) => push_candidates(&mut candidates, "pc", soda, &pc),
            Err(err) => app_error = err.to_string(),
        }
    } else if credentials.is_some() && !has_device_fingerprint {
        app_error = "应用签名凭证缺 device_id（必须与签名器同设备）".to_string();
    } else if has_device_fingerprint {
        app_error = "只有设备指纹、没有签名来源：请 set_signature_provider()（实时签名器）或提供抓包的 x-helios / x-medusa".to_string();
    } else {
        app_error =
            "未配置应用签名：App 端点只会返回空 body（见 docs/SIGNER-SERVICE.md）".to_string();
    }

    let mut selected: Option<(String, DownloadInfo)> = None;
    for (source, info) in candidates {
        match &selected {
            None => selected = Some((source, info)),
            Some((_, current)) => {
                if candidate_is_better(&info, current, full_duration) {
                    selected = Some((source, info));
                }
            }
        }
    }

    let mut report = StreamAccessReport {
        track_id: track_id.to_string(),
        track_duration_seconds: full_duration,
        requires_vip,
        has_cookie: soda.has_cookie(),
        has_app_credentials: has_device_fingerprint,
        has_signature_provider,
        app_error,
        ..Default::default()
    };

    if let Some((source, info)) = selected {
        let preview = is_preview(&info, full_duration);
        report.source = source;
        report.is_preview = preview;
        report.is_lossless = is_lossless(&info);
        report.best = Some(DownloadInfo {
            is_preview: preview,
            note: if preview {
                "试听片段".to_string()
            } else {
                String::new()
            },
            ..info
        });
    }

    report.hint = build_hint(&report);
    Ok(report)
}

fn build_hint(report: &StreamAccessReport) -> String {
    if report.best.is_none() {
        return "没有解析到任何播放流，请检查曲目 ID 与网络".to_string();
    }
    if !report.is_preview {
        return if report.is_lossless {
            "已拿到整曲无损流".to_string()
        } else {
            "已拿到整曲流".to_string()
        };
    }
    if report.requires_vip && !report.has_signing_chain() {
        return "该曲目整曲仅限会员播放：分享页/网页端点只会下发试听片段，需要「设备指纹 + 签名器」后走 App 端点；见 docs/SIGNER-SERVICE.md".to_string();
    }
    if report.requires_vip && report.has_app_credentials {
        return format!(
            "签名链路已启用但仍只拿到试听片段，通常是签名器/设备指纹不匹配或账号对该曲目没有整曲权益；App 端点返回：{}",
            if report.app_error.is_empty() {
                "（无错误信息）"
            } else {
                report.app_error.as_str()
            }
        );
    }
    format!(
        "只拿到试听片段（{:.0}s / 整曲 {}s）{}",
        report
            .best
            .as_ref()
            .map(|info| info.duration)
            .unwrap_or(0.0),
        report.track_duration_seconds,
        if report.app_error.is_empty() {
            String::new()
        } else {
            format!("；App 端点返回：{}", report.app_error)
        }
    )
}

impl Soda {
    /// 诊断整曲取流：拿到的是什么流、来自哪个端点、缺什么。
    ///
    /// ```no_run
    /// use libresoda::Soda;
    ///
    /// let soda = Soda::new("<cookie>");
    /// soda.load_app_credentials("/etc/libresoda/qishui-credentials.json")?;
    /// let report = soda.check_stream_access("7501674235158431760")?;
    /// println!("整曲={} 试听={} 提示={}", report.is_full_track(), report.is_preview, report.hint);
    /// # Ok::<(), libresoda::SodaError>(())
    /// ```
    pub fn check_stream_access(&self, track_id: &str) -> Result<StreamAccessReport> {
        check_stream_access(self, track_id)
    }
}
