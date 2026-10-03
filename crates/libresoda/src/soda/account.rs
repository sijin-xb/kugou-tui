//! 账号态探测（对应上游 `soda/account.go`）。

use super::quality::is_preview;
use super::types::{VIP_PROBE_TRACK_ID, VIP_PROBE_TRACK_URL};
use super::Soda;
use crate::error::Result;
use crate::model::{Song, SOURCE_SODA};

/// 等价 `IsVipAccount`：用一首已知的 VIP 曲目探测当前 Cookie 是否能拿到完整流。
pub fn is_vip_account(soda: &Soda) -> Result<bool> {
    if let Some(cached) = soda.cached_vip() {
        return Ok(cached);
    }

    if !soda.has_cookie() {
        soda.set_cached_vip(false);
        return Ok(false);
    }

    // 首选：官方 `/luna/pc/me` 的 `my_info.is_vip`。
    // 这是账号自身的会员标记，不受"某首歌是否可播/是否需要签名"影响；
    // 早先按"探测曲目能否拿到完整流"判断会把 SVIP 误判为非会员（实测 is_vip=true 却返回 false）。
    if let Ok(me) = super::user_playlist::fetch_pc_me(soda) {
        let is_vip = me.my_info.is_vip
            || matches!(
                me.my_info.vip_stage.trim().to_lowercase().as_str(),
                "vip" | "svip"
            );
        soda.set_cached_vip(is_vip);
        return Ok(is_vip);
    }

    let probe = Song {
        id: VIP_PROBE_TRACK_ID.to_string(),
        source: SOURCE_SODA.to_string(),
        link: VIP_PROBE_TRACK_URL.to_string(),
        extra: crate::util::extra_from_pairs([("track_id", VIP_PROBE_TRACK_ID)]),
        ..Default::default()
    };

    match super::download::get_download_info(soda, &probe) {
        Ok(info) => {
            let is_vip = !info.url.is_empty() && !is_preview(&info, 180);
            soda.set_cached_vip(is_vip);
            Ok(is_vip)
        }
        Err(err) => {
            if err.is_missing_entitlement() {
                soda.set_cached_vip(false);
                return Ok(false);
            }
            Err(err)
        }
    }
}

impl Soda {
    /// 等价 `(*Soda).IsVipAccount`。
    pub fn is_vip_account(&self) -> Result<bool> {
        is_vip_account(self)
    }
}
