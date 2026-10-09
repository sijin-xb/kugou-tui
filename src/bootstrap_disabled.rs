//! `node-bootstrap` feature 关闭时的替身。
//!
//! 关掉那个 feature 意味着这个二进制**完全不带 Node 接口服务的引导能力**：不探测
//! 3000/3001、不下载 KuGouMusicApi、不跑 `npm install`、不 spawn `node app.js`。
//! 产出的二进制在文件系统上不会留下 `~/.local/share/kugou-tui/api`，进程表里也不
//! 可能出现 Node。
//!
//! 函数签名与 `bootstrap.rs` 逐一对齐，这样调用点（`main.rs`、`app/cloud.rs`）
//! 不需要各自再包一层 `#[cfg]`——少一处 cfg 就少一处两边行为漂移的机会。
//!
//! 唯一的取舍：`--api node` 在这个构建里必然失败。这本来就是它的语义（要 Node
//! 却不带引导能力），失败时说清楚「是构建时关掉的」而不是含糊的「连不上」。

use std::path::Path;

use crate::api::ApiBackend;
use crate::config::Config;
use crate::error::{AppError, Result};
use crate::source::SourceKind;

/// 这个构建关掉了 `node-bootstrap`，没有任何需要善后的子进程。
pub fn shutdown() {}

/// 同上：没有实例可脱离。
pub fn detach() {}

/// 说明这个构建不带 Node 引导。
const DISABLED: &str = "这个二进制在构建时关闭了 node-bootstrap feature，不带 Node 接口服务的\
     引导能力。请用 `--api native`，或改用带该 feature 的构建。";

/// native 后端下与真实模块一样是空操作；只有真要 Node 服务时才报错。
pub fn prepare(config: &Config) -> Result<&'static str> {
    if config
        .api_backend
        .effective_for(config.active_source_kind())
        == ApiBackend::Native
    {
        return Ok("后端为 native，接口在进程内，无需本机服务");
    }
    Err(AppError::Service(DISABLED.to_string()))
}

/// 与 [`prepare`] 同一判据：native 放行，其余报错。
pub fn ensure_running(
    backend: ApiBackend,
    kind: SourceKind,
    _api_base: &str,
    _api_dir: Option<&Path>,
) -> Result<()> {
    if backend.effective_for(kind) == ApiBackend::Native {
        return Ok(());
    }
    Err(AppError::Service(DISABLED.to_string()))
}

/// 没有引导能力，也就没有实例可停。返回 0 让 `--api-stop` 打印「没有正在运行的
/// 接口服务实例」，与真实模块在无实例时的输出一致。
pub fn stop_recorded() -> Result<usize> {
    Ok(0)
}

/// 恒为假：这个构建在任何音源上都不会去管本机服务。
#[cfg(test)]
pub fn manages_service(_kind: SourceKind) -> bool {
    false
}
