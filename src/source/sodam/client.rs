//! 汽水的配置装配：把本项目的配置翻译成 libresoda 的运行时设置。
//!
//! # 为什么不再自己实现数据面
//!
//! 这个音源的数据面**直接用 libresoda**——汽水官方客户端 SodaM 用的同一套库
//! （<https://github.com/sodahub-org/sodam>）。这里早先有一套自写实现（登录参数、
//! MP4/CENC 解密、响应解析，约 4300 行），能用但缺了不少能力：歌单、歌手、专辑、
//! 会员状态全都没接。它们和 VIP 整曲卡在**同一道门槛**上——应用级签名；
//! 而那道门槛 libresoda 已经解决（内置签名服务客户端 + 内置 CDP 签名页），
//! 自己重写只是把这些能力继续挡在门外。
//!
//! # libresoda 的运行时模型
//!
//! 它自带一个**进程级单例**（[`libresoda::soda()`]），cookie、设备指纹、
//! 签名服务、签名页全部挂在那个实例上。这与本项目「按音源造客户端」的既有结构
//! 正好契合：**每次取用前重新灌一遍配置**，于是改了配置立刻生效，不需要为
//! 「配置变了要重建实例」再写一套失效逻辑。灌配置只是几次 `Mutex` 写，很便宜。
//!
//! # 阻塞
//!
//! libresoda 内部是**阻塞式** `ureq`。所有调用都要包在 `spawn_blocking` 里，
//! 否则会占住 tokio 的工作线程。`Soda` 本身是 `Send + Sync`（内部全是 `Mutex`），
//! 可以安全跨线程共享。

use serde::{Deserialize, Serialize};

use crate::api::ApiClient;

/// 上游内置的公共签名服务地址。
///
/// 取自 SodaM 的 `crates/sodam-core/src/config.rs`（`DEFAULT_SIGNER_URL` /
/// `DEFAULT_SIGNER_TOKEN`），上游的说明是「项目自建的签名服务：开箱即用」。
///
/// # 为什么默认用它
///
/// 不配签名时汽水只能放试听片段与免费曲目；有签名才能拿到整曲、无损、
/// 云端歌单、歌手/专辑浏览。上游把它当默认值，这里保持一致，用户不必先折腾
/// 一台签名服务器才能用。
///
/// # 注意
///
/// 这是一台**共享的第三方服务**：可能限流、可能不可用、token 可能轮换。
/// 配置里显式填地址就用自己的（自建 libmssdk 或局域网那台）；填 `none`
/// 可显式关掉签名。详见 `docs/USER_GUIDE.md` 的汽水章节。
pub const DEFAULT_SIGNER_URL: &str = "http://222.186.10.201:8921/sign";
/// 上面那台服务的 token（上游随代码公开）。
pub const DEFAULT_SIGNER_TOKEN: &str = "05f8089b8c5f60c63f2a6dcfe1028d28ee2725a504f3e59b";

/// 显式关闭签名的哨兵值。
const SIGNER_DISABLED: &str = "none";

/// 汽水的额外配置。对应配置文件的 `[sources.sodam_app]`。
///
/// 字段名与 libresoda / SodaM 对齐，便于两边共用一份配置与文档。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppCredentials {
    /// 设备指纹 `device_id`。**可以留空**——签名服务侧有自己的设备身份。
    #[serde(alias = "deviceId", alias = "DEVICE_ID")]
    pub device_id: String,
    /// install id `iid`。
    #[serde(alias = "install_id", alias = "installId", alias = "IID")]
    pub iid: String,
    /// `fp`，一般等于 `device_id`。
    pub fp: String,
    /// 静态签名头 `x-helios`（**旧路径**：手工抓包时用）。
    ///
    /// 配了签名服务就不需要它——那时签名由服务逐请求生成。
    #[serde(alias = "xHelios", alias = "X-Helios", alias = "helios")]
    pub x_helios: String,
    /// 静态签名头 `x-medusa`（同 `x_helios`）。
    #[serde(alias = "xMedusa", alias = "X-Medusa", alias = "medusa")]
    pub x_medusa: String,
    /// 抓包时的客户端 UA，留空用 libresoda 的默认值。
    #[serde(alias = "userAgent", alias = "ua")]
    pub user_agent: String,
    /// 应用签名服务地址（libmssdk 的 `/sign`，例如
    /// `http://127.0.0.1:8899/sign`）。
    ///
    /// **留空用上游的公共默认值**（[`DEFAULT_SIGNER_URL`]）；填 `none` 显式关掉。
    #[serde(default, alias = "signerUrl", alias = "signer")]
    pub signer_url: String,
    /// 签名服务的 Bearer Token。留空则在用默认地址时配默认 token。
    #[serde(default, alias = "signerToken")]
    pub signer_token: String,
}

impl AppCredentials {
    /// 只要设备指纹。
    pub fn has_device_fingerprint(&self) -> bool {
        !self.device_id.trim().is_empty()
    }

    /// `fp` 缺省时回落到 `device_id`（上游客户端两者通常一致）。
    pub fn fp_or_device_id(&self) -> String {
        let fp = self.fp.trim();
        if fp.is_empty() {
            self.device_id.trim().to_string()
        } else {
            fp.to_string()
        }
    }

    /// 生效的签名服务地址；`None` = 不用签名服务。
    ///
    /// 优先级：配置 > 环境变量 `QISHUI_SIGNER_URL` > 上游公共默认值。
    /// 与 SodaM 的 `merged_with_env` 同一套语义（空字段才让环境变量补），
    /// 只是这里再多一层默认值，让「什么都不配」也能拿到整曲。
    pub fn signer_base(&self) -> Option<String> {
        let configured = self.signer_url.trim();
        if configured.eq_ignore_ascii_case(SIGNER_DISABLED) {
            return None;
        }
        if !configured.is_empty() {
            return Some(configured.to_string());
        }

        if let Ok(from_env) = std::env::var("QISHUI_SIGNER_URL") {
            let from_env = from_env.trim();
            if from_env.eq_ignore_ascii_case(SIGNER_DISABLED) {
                return None;
            }
            if !from_env.is_empty() {
                return Some(from_env.to_string());
            }
        }

        Some(DEFAULT_SIGNER_URL.to_string())
    }

    /// 生效的签名服务 Token。
    ///
    /// 地址与 token 必须**成对**决定：用默认地址就得用默认 token——拿自己的
    /// token 去打公共服务的鉴权一定 401；反过来把公共 token 发给自建服务则是
    /// 把凭据泄漏到别人机器上。
    pub fn signer_token_for(&self, signer_base: &str) -> String {
        let using_default = signer_base == DEFAULT_SIGNER_URL;
        if !using_default {
            let configured = self.signer_token.trim();
            if !configured.is_empty() {
                return configured.to_string();
            }
            if let Ok(from_env) = std::env::var("QISHUI_SIGNER_TOKEN") {
                let from_env = from_env.trim();
                if !from_env.is_empty() {
                    return from_env.to_string();
                }
            }
            // 自建服务多半没开鉴权，别硬塞 token
            return String::new();
        }
        DEFAULT_SIGNER_TOKEN.to_string()
    }
}

/// 把配置灌进 libresoda 的进程级单例并返回它。
///
/// # 每次都灌
///
/// `set_*` 只是几次加锁赋值，比「记住上次灌了什么、变了再重建」简单得多，
/// 也不会出现「改了配置没生效」的陈旧状态——音源切换、登录成功、改设置都自动生效。
///
/// # cookie 归属
///
/// 汽水的登录态存在**它自己档案**里（`[sources.sodam].cookie`），由调用方经
/// `ApiClient` 带进来，与另三个音源一致。
pub fn configure(api: &ApiClient) -> &'static libresoda::Soda {
    let credentials = super::active_credentials();
    let soda = libresoda::soda();

    // 没配 cookie 就清空，避免上一个账号的登录态残留（匿名请求是合法的）
    soda.set_cookie(api.cookie().unwrap_or_default());

    // 设备指纹留空也合法，所以无条件灌——留空即清掉，语义清楚。
    soda.set_app_credentials(libresoda::soda::signature::AppCredentials {
        device_id: credentials.device_id.trim().to_string(),
        iid: credentials.iid.trim().to_string(),
        fp: credentials.fp_or_device_id(),
        x_helios: credentials.x_helios.trim().to_string(),
        x_medusa: credentials.x_medusa.trim().to_string(),
        // 抓包时用的 UA。留空即用 libresoda 的默认值——手工抓包才需要填。
        user_agent: credentials.user_agent.trim().to_string(),
    });

    // 签名服务：逐请求签名，决定整曲/无损/歌单能不能拿到。
    match credentials.signer_base() {
        Some(base) => {
            let token = credentials.signer_token_for(&base);
            soda.set_signature_provider(std::sync::Arc::new(
                libresoda::soda::signature::HttpSignature::new(base).with_token(token),
            ));
        }
        // 显式关掉时装一个 noop，免得残留上一个 provider
        None => soda.set_signature_provider(std::sync::Arc::new(
            libresoda::soda::signature::NoopSignature,
        )),
    }

    // 扫码登录的签名页：libresoda 内置的 Rust CDP 实现，直控本机 Chromium，
    // **不需要 Node**。没装浏览器只在真正发起登录时报错，不影响播放。
    soda.enable_cdp_signer();

    soda
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 什么都不配时落到上游公共默认值——「开箱即用」的关键。
    #[test]
    fn unconfigured_signer_falls_back_to_the_public_default() {
        if std::env::var("QISHUI_SIGNER_URL").is_ok() {
            // 环境变量会干扰，跳过（CI 上通常没有）
            return;
        }
        let credentials = AppCredentials::default();
        assert_eq!(
            credentials.signer_base().as_deref(),
            Some(DEFAULT_SIGNER_URL)
        );
        // 用默认地址就得配默认 token，否则打公共服务必然 401
        assert_eq!(
            credentials.signer_token_for(DEFAULT_SIGNER_URL),
            DEFAULT_SIGNER_TOKEN
        );
    }

    /// 配置里填了地址就用它。
    #[test]
    fn configured_signer_wins_over_the_default() {
        let credentials = AppCredentials {
            signer_url: "http://127.0.0.1:8899/sign".to_string(),
            signer_token: "my-token".to_string(),
            ..Default::default()
        };
        assert_eq!(
            credentials.signer_base().as_deref(),
            Some("http://127.0.0.1:8899/sign")
        );
        assert_eq!(
            credentials.signer_token_for("http://127.0.0.1:8899/sign"),
            "my-token"
        );
    }

    /// 自建服务没开鉴权时**不能**塞公共 token 进去——那是把别人的凭据
    /// 发到第三方，也必然鉴权失败。
    #[test]
    fn self_hosted_signer_without_token_gets_no_default_token() {
        let credentials = AppCredentials {
            signer_url: "http://127.0.0.1:8899/sign".to_string(),
            ..Default::default()
        };
        let token = credentials.signer_token_for("http://127.0.0.1:8899/sign");
        assert_ne!(token, DEFAULT_SIGNER_TOKEN);
        assert!(token.is_empty());
    }

    /// 显式 `none` = 不要签名服务（只要试听/免费曲目时有用）。
    #[test]
    fn signer_can_be_explicitly_disabled() {
        let credentials = AppCredentials {
            signer_url: "none".to_string(),
            ..Default::default()
        };
        assert!(credentials.signer_base().is_none());
        // 大小写不敏感
        let upper = AppCredentials {
            signer_url: "NONE".to_string(),
            ..Default::default()
        };
        assert!(upper.signer_base().is_none());
    }

    #[test]
    fn fp_falls_back_to_device_id() {
        let mut credentials = AppCredentials {
            device_id: "dev-1".to_string(),
            ..Default::default()
        };
        assert_eq!(credentials.fp_or_device_id(), "dev-1");
        credentials.fp = "fp-2".to_string();
        assert_eq!(credentials.fp_or_device_id(), "fp-2");
    }

    /// 老配置（没有 signer 字段）必须能读进来。
    #[test]
    fn old_config_without_signer_fields_still_parses() {
        let text = r#"
            device_id = "dev-1"
            iid = "iid-1"
            x_helios = "h"
            x_medusa = "m"
        "#;
        let parsed: AppCredentials = toml::from_str(text).expect("老配置应能解析");
        assert_eq!(parsed.device_id, "dev-1");
        assert!(parsed.signer_url.is_empty(), "缺失字段走 Default");
        assert_eq!(parsed.x_helios, "h");
    }
}
