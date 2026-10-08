//! 设备指纹：`guid` / `mid` / `dev`。
//!
//! 来源：KuGouMusicApi v1.6.0 `util/util.js` 的 `randomString` / `getGuid` /
//! `calculateMid`，以及 `server.js:53`、`:60`、`:257-266` 的组装方式。
//!
//! # 上游是怎么用的
//!
//! ```text
//! server.js:53   const guid = cryptoMd5(getGuid());
//! server.js:60   const serverDev = randomString(10).toUpperCase();
//! server.js:257  const env_guid = isUUIDv4(process.env.KUGOU_API_GUID)
//!                  ? cryptoMd5(process.env.KUGOU_API_GUID)
//!                  : process.env.KUGOU_API_GUID;
//! server.js:260  const mid = calculateMid(env_guid ?? guid);
//! ```
//!
//! 三者通过 `Set-Cookie`（`KUGOU_API_MID` / `KUGOU_API_GUID` / `KUGOU_API_DEV`）
//! 下发给客户端，**只在 Node 进程启动时算一次**，进程内保持不变。
//!
//! 关键一点：`mid` 的输入是**已经 MD5 过的 guid**（32 位 hex），不是裸 UUID。
//! 写成 `calculate_mid(&get_guid(rng))` 会得到另一个值，而且两边都没有报错。
//!
//! # 为什么 native 必须自己持久化
//!
//! kugou-tui 的 `reqwest` 客户端**没有开 `cookie_store`**，上游那些 `Set-Cookie`
//! 全被丢弃。今天能正常工作，靠的是「同一个 Node 进程内 `mid` 稳定」——只要进程
//! 不重启，每次请求带的 `mid` 都一样。native 没有那个进程，所以必须自己生成一次
//! 并落盘，否则每次启动都是新设备，风控会当成异常。
//!
//! 落盘位置是配置目录下的 `device.toml`（跟随 `KUGOU_TUI_CONFIG_DIR`），
//! 权限 0600。
//!
//! # 随机输入可注入
//!
//! 上游这三个函数都只依赖 `Math.random()`。这里把随机源做成 `FnMut() -> f64`
//! 参数，生产路径传 [`crate::util::random_f64`]，测试传固定序列——这样
//! known-answer 测试才能和上游的注入脚本逐字符对齐。

use std::path::{Path, PathBuf};

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

use super::crypto::md5_hex;

/// `randomString` / `randomNumber` 用的字符池（`util/util.js`）。
const KEY_STRING: &[u8] = b"1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// 设备标识落盘的文件名。
const DEVICE_FILE: &str = "device.toml";

/// 一个随机数源：返回 `[0, 1)` 内的值，语义同 `Math.random()`。
pub type Rng<'a> = &'a mut dyn FnMut() -> f64;

/// 上游 `randomString(len)`。
///
/// 注意索引算法是 `Math.ceil((36 - 1) * Math.random())`——**取的是 `ceil` 而不是
/// `floor`，且乘的是 35 而不是 36**。写成 `floor(r * 36)` 会让字符分布完全不同，
/// 但两者都「看起来像随机串」，不会有任何报错。
pub fn random_string(len: usize, rng: Rng<'_>) -> String {
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        let index = ((KEY_STRING.len() - 1) as f64 * rng()).ceil() as usize;
        // 上游没有做越界保护：`Math.random()` 理论上可以返回 0 而 ceil(0) = 0，
        // 索引 0 是合法的。这里把越界兜住只是为了不 panic，正常输入不会走到。
        let index = index.min(KEY_STRING.len() - 1);
        out.push(KEY_STRING[index] as char);
    }
    out
}

/// 上游 `getGuid()`：UUID v4 **形状**的随机串。
///
/// 每段来自 `((65536 * (1 + random())) | 0).toString(16).substring(1)`：
///
/// * `| 0` 是 ToInt32，对 `[65536, 131072)` 区间就是截断；
/// * `.toString(16)` 得到 5 位小写 hex（因为值必落在 `0x10000..0x20000`）；
/// * `.substring(1)` 砍掉最高位，剩下 4 位。
///
/// **上游并没有强制设置 version / variant 位**，尽管 `isUUIDv4` 的正则要求第 13
/// 位是 `4`、第 17 位是 `[89ab]`。所以 `getGuid()` 的输出**大多数时候不是合法
/// UUID v4**——这只影响 `server.js:257` 那条 `KUGOU_API_GUID` 环境变量分支，
/// 对默认路径没有影响。
pub fn get_guid(rng: Rng<'_>) -> String {
    let segment = |rng: &mut dyn FnMut() -> f64| -> String {
        let value = (65536.0 * (1.0 + rng())) as i64;
        // 与 JS 的 `toString(16)` 一致：小写、无前导零。
        let hex = format!("{value:x}");
        // 与 `substring(1)` 一致：砍掉首字符。
        hex.chars().skip(1).collect()
    };

    let a = segment(rng);
    let b = segment(rng);
    let c = segment(rng);
    let d = segment(rng);
    let e = segment(rng);
    let f = segment(rng);
    let g = segment(rng);
    let h = segment(rng);
    // 上游是 `${e()}${e()}-${e()}-${e()}-${e()}-${e()}${e()}${e()}`——
    // **八个**段，不是七个。数错会让最后一段凭空消失，而 guid 依然是
    // 32 位 hex 形状、依然能算出 mid，不报任何错。
    format!("{a}{b}-{c}-{d}-{e}-{f}{g}{h}")
}

/// 上游 `calculateMid(str)`：把 `MD5(str)` 的 hex 当作 128 位大整数转十进制。
pub fn calculate_mid(value: &str) -> String {
    let digest = md5_hex(value.as_bytes());
    BigUint::parse_bytes(digest.as_bytes(), 16)
        .unwrap_or_default()
        .to_str_radix(10)
}

/// 一套设备标识。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// `MD5(getGuid())`，32 位小写 hex。对应上游 `KUGOU_API_GUID`。
    pub guid: String,
    /// `calculateMid(guid)`，十进制大整数。对应上游 `KUGOU_API_MID`。
    pub mid: String,
    /// `randomString(10).toUpperCase()`。对应上游 `KUGOU_API_DEV`。
    pub dev: String,
}

impl Device {
    /// 用给定的随机源生成一套新标识。
    pub fn generate(rng: Rng<'_>) -> Self {
        // 顺序与上游一致：先 guid（吃 8 个随机数），再 dev（吃 10 个）。
        // 顺序错了结果就对不上——上游 `server.js` 里 `guid` 在 `:53`、
        // `serverDev` 在 `:60`，也是这个先后。
        let guid = md5_hex(get_guid(rng).as_bytes());
        let mid = calculate_mid(&guid);
        let dev = random_string(10, rng).to_uppercase();
        Self { guid, mid, dev }
    }

    /// 用生产随机源生成。
    pub fn generate_random() -> Self {
        Self::generate(&mut crate::util::random_f64)
    }

    /// 落盘路径：配置目录下的 `device.toml`。
    ///
    /// 走 [`crate::config::config_root`]，所以 `KUGOU_TUI_CONFIG_DIR` 一样能覆盖
    /// 它——测试和便携安装都靠这个隔离。
    pub fn path() -> PathBuf {
        crate::config::config_root().join(DEVICE_FILE)
    }

    /// 字段是否都像模像样。
    ///
    /// 只做形状检查：`guid` 必须是 32 位 hex、`mid` 必须是十进制、`dev` 非空。
    /// 文件被手改坏时宁可重新生成，也不要带着一个半截的 guid 去请求。
    pub fn is_valid(&self) -> bool {
        self.guid.len() == 32
            && self.guid.bytes().all(|b| b.is_ascii_hexdigit())
            && !self.mid.is_empty()
            && self.mid.bytes().all(|b| b.is_ascii_digit())
            && !self.dev.is_empty()
    }

    /// 从 `path` 读；不存在、读不动、或内容不合法时重新生成并写回。
    ///
    /// 与上游一样：**生成一次就固定下来**，之后每次启动都复用。否则每重启一次
    /// 就是一个新设备，风控会当成异常。
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(device) = toml::from_str::<Self>(&text)
        {
            if device.is_valid() {
                return Ok(device);
            }
            crate::logger::tlog!(
                crate::logger::LEVEL_WARN,
                "设备标识文件内容不合法，重新生成：{}",
                path.display()
            );
        }

        let device = Self::generate_random();
        device.save(path)?;
        Ok(device)
    }

    /// 写回 `path`，目录 0700、文件 0600。
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| AppError::io_at(parent.display().to_string(), error))?;
            crate::config::restrict_permissions(parent, 0o700)?;
        }
        let text = toml::to_string(self)
            .map_err(|error| AppError::Config(format!("序列化设备标识失败：{error}")))?;
        std::fs::write(path, text)
            .map_err(|error| AppError::io_at(path.display().to_string(), error))?;
        // 设备标识虽然不是账号凭据，但它是可被拿去冒用的身份，权限按凭据收紧。
        crate::config::restrict_permissions(path, 0o600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/tmp/kat_device.js` 的固定随机序列。
    const SEQ_A: [f64; 10] = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9];
    const SEQ_B: [f64; 7] = [
        0.0,
        0.999_999_999_999_999_9,
        0.5,
        0.25,
        0.75,
        0.123_456_789,
        0.987_654_321,
    ];

    /// 把固定序列包成随机源。序列循环，与上游注入脚本一致。
    fn sequence(values: &[f64]) -> impl FnMut() -> f64 + '_ {
        let mut index = 0usize;
        move || {
            let value = values[index % values.len()];
            index += 1;
            value
        }
    }

    #[test]
    fn random_string_matches_upstream() {
        let mut a = sequence(&SEQ_A);
        assert_eq!(random_string(16, &mut a), "158BEILPSW158BEI");

        let mut a = sequence(&SEQ_A);
        assert_eq!(random_string(24, &mut a), "158BEILPSW158BEILPSW158B");

        let mut b = sequence(&SEQ_B);
        assert_eq!(random_string(10, &mut b), "1ZI0R6Z1ZI");
    }

    /// `0.9999999999999999` 是小于 1 的最大双精度数。`1 + 它` 在双精度下
    /// **进位成 2.0**（间距只有半个 ULP），于是这一段的 hex 是 `20000`、
    /// 砍掉首位后是 `0000`。算错的话会得到 `1ffff` → `ffff`。
    #[test]
    fn get_guid_matches_upstream() {
        let mut a = sequence(&SEQ_A);
        assert_eq!(
            get_guid(&mut a),
            "00001999-3333-4ccc-6666-80009999b333"
        );

        let mut b = sequence(&SEQ_B);
        assert_eq!(
            get_guid(&mut b),
            "00000000-8000-4000-c000-1f9afcd60000"
        );
    }

    #[test]
    fn calculate_mid_matches_upstream() {
        // 上游 kat_gen.js：calculateMid('5f2b1c3d4e5f60718293a4b5c6d7e8f9')
        assert_eq!(
            calculate_mid("5f2b1c3d4e5f60718293a4b5c6d7e8f9"),
            "231699103997194646178265604655475531917"
        );
        assert_eq!(
            md5_hex(b"5f2b1c3d4e5f60718293a4b5c6d7e8f9"),
            "ae4f9fceb0a32165f8d1b6865997208d"
        );
    }

    /// `server.js` 的完整链路：`mid = calculateMid(MD5(getGuid()))`。
    ///
    /// **不是** `calculateMid(getGuid())`——那个值同样能在上游跑出来
    /// （`276005198214608486238597840788595108259`），但服务端用的不是它。
    /// 两个值都列出来，免得将来有人「修正」成错的那个。
    #[test]
    fn mid_comes_from_the_hashed_guid() {
        let mut a = sequence(&SEQ_A);
        let raw_guid = get_guid(&mut a);
        assert_eq!(raw_guid, "00001999-3333-4ccc-6666-80009999b333");

        let hashed = md5_hex(raw_guid.as_bytes());
        assert_eq!(hashed, "cfa4aae82603ab029745e499af2b69a3");
        assert_eq!(
            calculate_mid(&hashed),
            "256847382746584159527093497586030968585",
            "server.js 链路：calculateMid(MD5(getGuid()))"
        );
        assert_eq!(
            calculate_mid(&raw_guid),
            "276005198214608486238597840788595108259",
            "这是 calculateMid(getGuid())，上游 kat_device.js 测的那个，服务端不用"
        );
    }

    #[test]
    fn generate_consumes_the_rng_in_upstream_order() {
        let mut a = sequence(&SEQ_A);
        let device = Device::generate(&mut a);
        assert_eq!(device.guid, "cfa4aae82603ab029745e499af2b69a3");
        assert_eq!(device.mid, "256847382746584159527093497586030968585");
        // guid 吃 8 个随机数，dev 从第 9 个开始：SEQ_A 的第 9、10 个是 0.8、0.9，
        // 之后回绕到 0.0……
        assert_eq!(device.dev.len(), 10);
        assert!(device.is_valid());
    }

    #[test]
    fn rejects_a_malformed_device() {
        let good = Device {
            guid: "a".repeat(32),
            mid: "123".to_string(),
            dev: "ABCDEFGHIJ".to_string(),
        };
        assert!(good.is_valid());

        assert!(
            !Device {
                guid: "short".to_string(),
                ..good.clone()
            }
            .is_valid()
        );
        assert!(
            !Device {
                guid: "z".repeat(32),
                ..good.clone()
            }
            .is_valid()
        );
        assert!(
            !Device {
                mid: "12a".to_string(),
                ..good.clone()
            }
            .is_valid()
        );
        assert!(
            !Device {
                dev: String::new(),
                ..good
            }
            .is_valid()
        );
    }

    /// 落盘后能读回同一套标识，且权限是 0600。
    #[test]
    fn persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!(
            "kugou-tui-device-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join(DEVICE_FILE);

        let first = Device::load_or_create(&path).expect("首次生成");
        assert!(first.is_valid());

        let second = Device::load_or_create(&path).expect("读回");
        assert_eq!(first, second, "重启后设备标识必须不变");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("stat")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "设备标识文件权限应为 0600");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件被改坏时重新生成，而不是带着半截 guid 去请求。
    #[test]
    fn regenerates_a_corrupt_file() {
        let dir = std::env::temp_dir().join(format!(
            "kugou-tui-device-bad-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join(DEVICE_FILE);

        std::fs::write(&path, "guid = \"oops\"\nmid = \"x\"\ndev = \"\"\n").expect("写入坏文件");
        let device = Device::load_or_create(&path).expect("应重新生成");
        assert!(device.is_valid());
        assert_ne!(device.guid, "oops");

        // 重新生成的结果已经写回文件，下一次读到的是同一套。
        let again = Device::load_or_create(&path).expect("读回");
        assert_eq!(device, again);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
