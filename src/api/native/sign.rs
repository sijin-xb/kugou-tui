//! 上游 `util/helper.js` 的全部签名算法。
//!
//! 来源：KuGouMusicApi v1.6.0 `util/helper.js`
//! （commit a5a98013cce79fe0ae2ad65fc84b68176ebcfc1e，2026-08-14）。
//!
//! 六个函数全部照抄上游，逐个对应：
//!
//! | 本模块 | 上游 |
//! |---|---|
//! | [`signature_android_params`] | `signatureAndroidParams` |
//! | [`signature_web_params`] | `signatureWebParams` |
//! | [`signature_register_params`] | `signatureRegisterParams` |
//! | [`sign_params`] | `signParams` |
//! | [`sign_key`] | `signKey` |
//! | [`sign_params_key`] | `signParamsKey` |
//!
//! # 两个最容易写错的地方
//!
//! 1. **排序对象不同**。`signatureAndroidParams` 是 `.sort()` **再** `.map()`，
//!    排的是**渲染后的 `key=value` 串**；`signatureWebParams` 是 `.map()` **再**
//!    `.sort()`，排的也是渲染后的串。两者都是对「`key=value` 字符串」排序，
//!    但对 `signParams` 而言渲染格式是 `key` 与 `value` 直接相接（**无等号**）。
//!    把排序键写成「参数名」在这三处里有两处会得到错误结果。
//! 2. **平台分叉只在部分函数上**。`signatureAndroidParams` / `signKey` /
//!    `signParamsKey` 区分标准版与概念版；`signatureWebParams` /
//!    `signatureRegisterParams` / `signParams` 的盐值**与平台无关**。
//!
//! # `JSON.stringify` 的次序风险
//!
//! 上游对**对象类型**的参数值先 `JSON.stringify` 再参与拼接（`helper.js:61-80`）。
//! JS 的对象键序是**插入序**，所以本仓库必须开 `serde_json` 的 `preserve_order`
//! （`Value::Object` 用 `IndexMap`）才能得到同一份串；默认的 `BTreeMap` 会按
//! 字典序重排，签名直接算错——而且不会报错，只会 403。
//! `object_valued_params_keep_insertion_order` 锁定这一点。
//!
//! 注意 `preserve_order` 只保证「解析出来的键序 = JSON 文档里的键序」，
//! 而 `BTreeMap` 形态的参数表（[`signature_android_params`] 收的是
//! `BTreeMap`）在渲染 `key=value` 时本来就要先 `.sort()`，与上游一致，
//! 不受影响。

use std::collections::BTreeMap;

use serde_json::Value;

use crate::source::SourceKind;

use super::crypto::md5_hex;

/// `util/config.json` 的 `srcappid`。
///
/// 只有二维码登录的两个接口用它（`module/login_qr_key.js`、
/// `module/login_qr_check.js`），其余接口不带。
pub const SRCAPPID: u32 = 2919;

/// 标准版签名盐（`util/helper.js` 内联字面量）。
const ANDROID_SALT_STANDARD: &str = "OIlwieks28dk2k092lksi2UIkp";

/// 概念版（lite）签名盐。
const ANDROID_SALT_LITE: &str = "LnT6xpN3khm36zse0QzvmgTZ3waWdRSA";

/// Web 签名盐。**与平台无关**。
const WEB_SALT: &str = "NVPh5oo715z5DIWAeQlhMDsWXXQV4hwt";

/// `signParams` 盐。**与平台无关**。
#[allow(dead_code)] // 仅 [`sign_params`] 用，而迁移范围内没有接口走那条路
const SIGN_PARAMS_SALT: &str = "R6snCXJgbCaj9WFRJKefTMIFp0ey6Gza";

/// `signKey` 标准版盐。
const SIGN_KEY_SALT_STANDARD: &str = "57ae12eb6890223e355ccfcb74edf70d";

/// `signKey` 概念版盐。
const SIGN_KEY_SALT_LITE: &str = "185672dd44712f60bb1736df5a377e82";

/// `signatureRegisterParams` 的盐（就是个 `1014`）。
const REGISTER_SALT: &str = "1014";

/// 标准版 `appid`（`util/config.json` 的 `appid`）。
const APPID_STANDARD: u32 = 1005;

/// 概念版 `appid`（`util/config.json` 的 `liteAppid`）。
const APPID_LITE: u32 = 3116;

/// 标准版 `clientver`。
const CLIENTVER_STANDARD: u32 = 20489;

/// 概念版 `clientver`。
const CLIENTVER_LITE: u32 = 11440;

/// 请求参数的值。
///
/// 上游的参数值是 JS 的任意类型，`JSON.stringify` 只对 `object` 生效。
/// 这里只区分「已经渲染好的文本」与「需要序列化的 JSON 值」两种。
#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    /// 字符串、数字、布尔——上游直接拼进 `key=value`。
    Text(String),
    /// 对象——上游先 `JSON.stringify`。
    Json(Value),
}

impl ParamValue {
    /// 渲染成参与签名拼接的文本。
    fn render(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            // `JSON.stringify` 对对象产出紧凑 JSON，与 `to_string` 一致。
            Self::Json(value) => serde_json::to_string(value).unwrap_or_default(),
        }
    }
}

impl From<&str> for ParamValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<String> for ParamValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<u32> for ParamValue {
    fn from(value: u32) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<i64> for ParamValue {
    fn from(value: i64) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<u64> for ParamValue {
    fn from(value: u64) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<Value> for ParamValue {
    fn from(value: Value) -> Self {
        Self::Json(value)
    }
}

/// 该音源平台的 `appid`。
pub fn appid(kind: SourceKind) -> u32 {
    match kind {
        SourceKind::KugouConcept => APPID_LITE,
        _ => APPID_STANDARD,
    }
}

/// 该音源平台的 `clientver`。
pub fn clientver(kind: SourceKind) -> u32 {
    match kind {
        SourceKind::KugouConcept => CLIENTVER_LITE,
        _ => CLIENTVER_STANDARD,
    }
}

fn android_salt(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::KugouConcept => ANDROID_SALT_LITE,
        _ => ANDROID_SALT_STANDARD,
    }
}

/// Android 签名。对应上游 `signatureAndroidParams(params, data)`。
///
/// `data` 是请求体：上游对 `Buffer` 走流式 MD5（盐 / 参数串 / 原始字节 / 盐），
/// 对字符串走拼接。两种在哈希输入上完全等价，所以这里统一收字节切片。
pub fn signature_android_params(
    kind: SourceKind,
    params: &BTreeMap<String, ParamValue>,
    data: &[u8],
) -> String {
    let salt = android_salt(kind);
    // 上游：`.sort()` 在 `.map()` 之后，排的是渲染后的 `key=value` 串。
    let mut pairs: Vec<String> = params
        .iter()
        .map(|(key, value)| format!("{key}={}", value.render()))
        .collect();
    pairs.sort();

    let mut input = Vec::with_capacity(salt.len() * 2 + data.len() + 64);
    input.extend_from_slice(salt.as_bytes());
    for pair in &pairs {
        input.extend_from_slice(pair.as_bytes());
    }
    input.extend_from_slice(data);
    input.extend_from_slice(salt.as_bytes());
    md5_hex(&input)
}

/// Web 签名。对应上游 `signatureWebParams(params)`。**不区分平台。**
pub fn signature_web_params(params: &BTreeMap<String, ParamValue>) -> String {
    let mut pairs: Vec<String> = params
        .iter()
        .map(|(key, value)| format!("{key}={}", value.render()))
        .collect();
    pairs.sort();
    let joined: String = pairs.concat();
    md5_hex(format!("{WEB_SALT}{joined}{WEB_SALT}").as_bytes())
}

/// 设备注册签名。对应上游 `signatureRegisterParams(params)`。
///
/// **只取 value、丢掉 key**，值按字符串排序。**不区分平台。**
pub fn signature_register_params(params: &BTreeMap<String, ParamValue>) -> String {
    let mut values: Vec<String> = params.values().map(ParamValue::render).collect();
    values.sort();
    let joined: String = values.concat();
    md5_hex(format!("{REGISTER_SALT}{joined}{REGISTER_SALT}").as_bytes())
}

/// 通用 `sign` 签名。对应上游 `signParams(params, data)`。
///
/// 拼接格式是 `key` 与 `value` **直接相接、无等号**。
/// **不区分平台。**（迁移范围内的 26 个接口都没用它，这里一并移植并锁定，
/// 免得将来接 `user_cloud_url` 之类时再回头翻上游。）
#[allow(dead_code)]
pub fn sign_params(params: &BTreeMap<String, ParamValue>, data: &[u8]) -> String {
    let mut pairs: Vec<String> = params
        .iter()
        .map(|(key, value)| format!("{key}{}", value.render()))
        .collect();
    pairs.sort();

    let mut input = Vec::new();
    for pair in &pairs {
        input.extend_from_slice(pair.as_bytes());
    }
    input.extend_from_slice(data);
    input.extend_from_slice(SIGN_PARAMS_SALT.as_bytes());
    md5_hex(&input)
}

/// 请求密钥签名。对应上游 `signKey(hash, mid, userid, appid)`。
///
/// `userid` 与 `appid` 为 `None` 时分别落到 `0` 与当前平台的 `appid`
/// ——对应上游的 `userid || 0` 与 `appid || useAppid`。
pub fn sign_key(
    kind: SourceKind,
    hash: &str,
    mid: &str,
    userid: Option<i64>,
    appid: Option<u32>,
) -> String {
    let salt = match kind {
        SourceKind::KugouConcept => SIGN_KEY_SALT_LITE,
        _ => SIGN_KEY_SALT_STANDARD,
    };
    let appid = appid.unwrap_or_else(|| self::appid(kind));
    let userid = userid.unwrap_or(0);
    md5_hex(format!("{hash}{salt}{appid}{mid}{userid}").as_bytes())
}

/// 参数密钥签名。对应上游 `signParamsKey(data, appid, clientver)`。
///
/// `appid` / `clientver` 为 `None` 时落到当前平台的值。
///
/// 上游有两处用它：`module/top_playlist.js` 的 body `key`，以及
/// `module/artist_audios.js` 的 body `key`。它与 `encryptKey` 那条路的
/// [`sign_key`] **不是同一个算法**，别混。
pub fn sign_params_key(
    kind: SourceKind,
    data: &str,
    appid: Option<u32>,
    clientver: Option<u32>,
) -> String {
    let salt = android_salt(kind);
    let appid = appid.unwrap_or_else(|| self::appid(kind));
    let clientver = clientver.unwrap_or_else(|| self::clientver(kind));
    md5_hex(format!("{appid}{salt}{clientver}{data}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/tmp/kat_gen.js` 的固定输入。
    const FIXED_DFID: &str = "1234567890abcdef12345678";
    const FIXED_MID: &str = "12345678901234567890123456789012";
    const FIXED_UUID: &str = "-";
    const FIXED_CLIENTTIME: u32 = 1_700_000_000;
    const FIXED_HASH: &str = "6af00fbd4d444a82c005843eef9dc2d4";

    fn map<K: Into<String>>(entries: Vec<(K, ParamValue)>) -> BTreeMap<String, ParamValue> {
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect()
    }

    /// 上游 `defaultParams`。
    fn defaults(kind: SourceKind) -> Vec<(String, ParamValue)> {
        vec![
            ("dfid".to_string(), ParamValue::from(FIXED_DFID)),
            ("mid".to_string(), ParamValue::from(FIXED_MID)),
            ("uuid".to_string(), ParamValue::from(FIXED_UUID)),
            ("appid".to_string(), ParamValue::from(appid(kind))),
            ("clientver".to_string(), ParamValue::from(clientver(kind))),
            ("clienttime".to_string(), ParamValue::from(FIXED_CLIENTTIME)),
        ]
    }

    #[test]
    fn platform_constants_match_config_json() {
        // util/config.json
        assert_eq!(appid(SourceKind::Kugou), 1005);
        assert_eq!(clientver(SourceKind::Kugou), 20489);
        assert_eq!(appid(SourceKind::KugouConcept), 3116);
        assert_eq!(clientver(SourceKind::KugouConcept), 11440);
        assert_eq!(SRCAPPID, 2919);
    }

    #[test]
    fn android_search_signature_matches_upstream_standard() {
        let mut params = defaults(SourceKind::Kugou);
        params.extend([
            ("keyword".to_string(), ParamValue::from("周杰伦")),
            ("page".to_string(), ParamValue::from(1u32)),
            ("pagesize".to_string(), ParamValue::from(30u32)),
        ]);
        assert_eq!(
            signature_android_params(SourceKind::Kugou, &map(params), b""),
            "50368525446705f1a2c53d29a489ff9e"
        );
    }

    #[test]
    fn android_search_signature_matches_upstream_lite() {
        let mut params = defaults(SourceKind::KugouConcept);
        params.extend([
            ("keyword".to_string(), ParamValue::from("周杰伦")),
            ("page".to_string(), ParamValue::from(1u32)),
            ("pagesize".to_string(), ParamValue::from(30u32)),
        ]);
        assert_eq!(
            signature_android_params(SourceKind::KugouConcept, &map(params), b""),
            "2d8c5155be106ba6670a3cf14d2cadc8"
        );
    }

    #[test]
    fn song_url_key_and_signature_match_upstream() {
        // song_url 走 encryptKey：key 先算出来、再参与 android 签名。
        for (kind, expected_key, expected_sign) in [
            (
                SourceKind::Kugou,
                "b5e3f87dbe5ee68aed200497fe007c53",
                "dfe78bb1b3146f46ed5652e40bf26c1d",
            ),
            (
                SourceKind::KugouConcept,
                "f8c7ea0b8c92ea7c42c5af03f0d3afb4",
                "3e1762bc8d0138d6a23dc6d9e3c5c3bf",
            ),
        ] {
            let key = sign_key(kind, FIXED_HASH, FIXED_MID, None, None);
            assert_eq!(key, expected_key, "{kind:?} 的 key");

            let mut params = defaults(kind);
            params.extend([
                ("hash".to_string(), ParamValue::from(FIXED_HASH)),
                ("album_id".to_string(), ParamValue::from(0u32)),
                ("album_audio_id".to_string(), ParamValue::from(0u32)),
                ("quality".to_string(), ParamValue::from(128u32)),
                ("key".to_string(), ParamValue::from(key)),
            ]);
            assert_eq!(
                signature_android_params(kind, &map(params), b""),
                expected_sign,
                "{kind:?} 的签名"
            );
        }
    }

    /// `module/search_lyric.js` 设了 `clearDefaultParams: true`，
    /// 参数里**没有** `dfid`/`mid`/`uuid`/`clienttime`，且 `appid`/`clientver`
    /// 硬取标准版的 1005/20489（不随 lite 变）。这个测试同时锁定这两件事。
    #[test]
    fn search_lyric_signature_has_no_default_params() {
        let params = map(vec![
            ("album_audio_id", ParamValue::from(0u32)),
            ("appid", ParamValue::from(1005u32)),
            ("clientver", ParamValue::from(20489u32)),
            ("duration", ParamValue::from(243_722u32)),
            ("hash", ParamValue::from(FIXED_HASH)),
            ("keyword", ParamValue::from("Letter - arkady sevidov")),
            ("lrctxt", ParamValue::from(1u32)),
            ("man", ParamValue::from("yes")),
        ]);
        // 两平台相同：参数里的 appid/clientver 是硬编码的标准版值，
        // 而 signatureAndroidParams 的盐才随平台变——这里两个平台都要对上。
        assert_eq!(
            signature_android_params(SourceKind::Kugou, &params, b""),
            "b90333d489a1aae225eb18ac61718d35"
        );
        assert_eq!(
            signature_android_params(SourceKind::KugouConcept, &params, b""),
            "7fb230b0c4a899192b66f33abad7b0df"
        );
    }

    #[test]
    fn artist_audios_sign_params_key_matches_upstream() {
        for (kind, expected_key, expected_sign) in [
            (
                SourceKind::Kugou,
                "a1f65b6a8fe7e191521406ce8661ae02",
                "4ee22ca78a3d32cf1351e44ef8579347",
            ),
            (
                SourceKind::KugouConcept,
                "bad0ee207bf429bd91402b77fc8f7a5b",
                "5c3e304545addecb53e59b05ee17d9dd",
            ),
        ] {
            let sign = sign_params_key(kind, &FIXED_CLIENTTIME.to_string(), None, None);
            assert_eq!(sign, expected_key, "{kind:?} 的 signParamsKey");

            let mut params = defaults(kind);
            params.extend([
                ("artistid".to_string(), ParamValue::from(3520u32)),
                ("sort".to_string(), ParamValue::from(1u32)),
                ("page".to_string(), ParamValue::from(1u32)),
                ("pagesize".to_string(), ParamValue::from(30u32)),
                ("sign".to_string(), ParamValue::from(sign)),
            ]);
            assert_eq!(
                signature_android_params(kind, &map(params), b""),
                expected_sign,
                "{kind:?} 的签名"
            );
        }
    }

    #[test]
    fn web_signature_is_platform_independent() {
        let params = map(vec![
            ("appid", ParamValue::from(1005u32)),
            ("clientver", ParamValue::from(20489u32)),
            ("clienttime", ParamValue::from(FIXED_CLIENTTIME)),
            ("mid", ParamValue::from(FIXED_MID)),
            ("uuid", ParamValue::from(FIXED_UUID)),
        ]);
        assert_eq!(
            signature_web_params(&params),
            "8b567c2b89f5039062b987296f68bafe"
        );
    }

    #[test]
    fn register_signature_is_platform_independent() {
        let params = map(vec![
            ("part", ParamValue::from(1u32)),
            ("platid", ParamValue::from(1u32)),
            ("p", ParamValue::from("abcdef")),
        ]);
        assert_eq!(
            signature_register_params(&params),
            "2ad41280685b21c395c03edf35933ed6"
        );
    }

    #[test]
    fn sign_params_has_no_equals_sign() {
        let params = map(vec![
            ("a", ParamValue::from("1")),
            ("b", ParamValue::from("2")),
        ]);
        assert_eq!(
            sign_params(&params, b"body"),
            "2fa146b4dc8fc6a1b0a81316e0ac8026"
        );
    }

    /// `data` 参与 android 签名。用一个非空 data 确认它与空 data 不同，
    /// 且与上游 Buffer 分支的流式写法等价。
    #[test]
    fn android_signature_includes_the_body() {
        let params = map(defaults(SourceKind::Kugou));
        let empty = signature_android_params(SourceKind::Kugou, &params, b"");
        let with_body = signature_android_params(SourceKind::Kugou, &params, b"[]");
        assert_ne!(empty, with_body);

        // 手工按上游 Buffer 分支的拼接顺序复算一遍，确认等价。
        let mut input = Vec::new();
        input.extend_from_slice(ANDROID_SALT_STANDARD.as_bytes());
        input.extend_from_slice(b"appid=1005");
        input.extend_from_slice(b"clienttime=1700000000");
        input.extend_from_slice(b"clientver=20489");
        input.extend_from_slice(format!("dfid={FIXED_DFID}").as_bytes());
        input.extend_from_slice(format!("mid={FIXED_MID}").as_bytes());
        input.extend_from_slice(b"uuid=-");
        input.extend_from_slice(b"[]");
        input.extend_from_slice(ANDROID_SALT_STANDARD.as_bytes());
        assert_eq!(with_body, md5_hex(&input));
    }

    /// 对象值走 `JSON.stringify`。
    #[test]
    fn object_values_are_serialized_as_json() {
        let params = map(vec![(
            "data",
            ParamValue::Json(serde_json::json!({"fileid": 1})),
        )]);
        let mut expected_input = Vec::new();
        expected_input.extend_from_slice(ANDROID_SALT_STANDARD.as_bytes());
        expected_input.extend_from_slice(b"data={\"fileid\":1}");
        expected_input.extend_from_slice(ANDROID_SALT_STANDARD.as_bytes());
        assert_eq!(
            signature_android_params(SourceKind::Kugou, &params, b""),
            md5_hex(&expected_input)
        );
    }

    /// `signKey` 的 `userid || 0` 与 `appid || useAppid` 兜底。
    #[test]
    fn sign_key_falls_back_to_platform_appid() {
        let explicit = sign_key(
            SourceKind::Kugou,
            FIXED_HASH,
            FIXED_MID,
            Some(0),
            Some(1005),
        );
        let implicit = sign_key(SourceKind::Kugou, FIXED_HASH, FIXED_MID, None, None);
        assert_eq!(explicit, implicit);
    }

    // --- 真实请求的整份 dataMap ---
    //
    // 下面两组 KAT 不是「挑几个参数试试」，而是把 `module/song_url.js` 与
    // `module/search_lyric.js` 实际发出的**整份** `dataMap` 固化下来。
    // 基准由 `/tmp/kat_notsign.js` 产出：该脚本用假的 `useAxios` 截获模块真正
    // 传出的 options，再照抄 `util/request.js` 的变换（默认参数合并、`encryptKey`、
    // 签名分派）复算签名——即「上游会怎么签」的忠实复现，而不是我手写的期望值。

    /// `module/song_url.js` 的 dataMap（`/v5/url`）。
    ///
    /// 两个细节容易漏：
    ///
    /// * `clientver` 是 dataMap 里的 `11430`，**覆盖**了 `defaultParams` 的平台值
    ///   （20489 / 11440）；
    /// * `dfid` 来自该模块自己的 `cookie: {dfid: randomString(24), ...}` 兜底，
    ///   所以是 24 位随机串，不是配置里的 dfid。
    fn song_url_params(kind: SourceKind, dfid: &str, key: &str) -> BTreeMap<String, ParamValue> {
        let lite = kind == SourceKind::KugouConcept;
        map(vec![
            ("album_id", ParamValue::from(0u32)),
            ("area_code", ParamValue::from(1u32)),
            ("hash", ParamValue::from(FIXED_HASH)),
            ("ssa_flag", ParamValue::from("is_fromtrack")),
            ("version", ParamValue::from(11430u32)),
            (
                "page_id",
                ParamValue::from(if lite { 967_177_915u32 } else { 151_369_488u32 }),
            ),
            ("quality", ParamValue::from(128u32)),
            ("album_audio_id", ParamValue::from(0u32)),
            ("behavior", ParamValue::from("play")),
            ("pid", ParamValue::from(if lite { 411u32 } else { 2u32 })),
            ("cmd", ParamValue::from(26u32)),
            ("pidversion", ParamValue::from(3001u32)),
            ("IsFreePart", ParamValue::from(0u32)),
            (
                "ppage_id",
                ParamValue::from(if lite {
                    "356753938,823673182,967485191"
                } else {
                    "463467626,350369493,788954147"
                }),
            ),
            ("cdnBackup", ParamValue::from(1u32)),
            ("module", ParamValue::from("")),
            ("clientver", ParamValue::from(11430u32)),
            // defaultParams 里除 clientver 之外的部分
            ("dfid", ParamValue::from(dfid)),
            ("mid", ParamValue::from(FIXED_MID)),
            ("uuid", ParamValue::from(FIXED_UUID)),
            ("appid", ParamValue::from(appid(kind))),
            ("clienttime", ParamValue::from(FIXED_CLIENTTIME)),
            // encryptKey 在签名之前算出来，key 本身参与签名
            ("key", ParamValue::from(key)),
        ])
    }

    /// `module/song_url.js` 的 dataMap 签名。
    ///
    /// **这个测试同时是 `notSign` 死参数的证据**：该模块写了 `notSign: true`，
    /// 而 `util/request.js:126` 读的是 `options.notSignature`——全仓库没有任何
    /// 一处读 `notSign`。所以 `song_url` 照常带 android 签名。
    /// 若真按 `notSign` 的字面意思跳过签名，下面这两个值就都对不上。
    #[test]
    fn song_url_still_carries_an_android_signature() {
        // dfid 由 song_url 内部的 randomString(24) 生成；基准脚本注入了固定
        // Math.random，序列为 0,0.1,…,0.9 循环，得到下面这串。
        const RANDOM_DFID: &str = "158BEILPSW158BEILPSW158B";

        for (kind, expected_key, expected_sign) in [
            (
                SourceKind::Kugou,
                "b5e3f87dbe5ee68aed200497fe007c53",
                "ae50fda1d5edbaccb698df40f4949b95",
            ),
            (
                SourceKind::KugouConcept,
                "f8c7ea0b8c92ea7c42c5af03f0d3afb4",
                "7047881deecb6a33340b0d7d64d4a8e4",
            ),
        ] {
            let key = sign_key(kind, FIXED_HASH, FIXED_MID, None, None);
            assert_eq!(key, expected_key, "{kind:?} 的 key");
            assert_eq!(
                signature_android_params(kind, &song_url_params(kind, RANDOM_DFID, &key), b""),
                expected_sign,
                "{kind:?} 的 song_url 签名（notSign: true 无效，照常签名）"
            );
        }
    }

    /// `module/search_lyric.js` 的 dataMap（`https://lyrics.kugou.com/v1/search`）。
    ///
    /// 同样带无效的 `notSign: true`，同样照常签名。
    /// `clearDefaultParams: true` 使它**没有** `dfid`/`mid`/`uuid`/`clienttime`。
    fn search_lyric_params() -> BTreeMap<String, ParamValue> {
        map(vec![
            ("album_audio_id", ParamValue::from(0u32)),
            ("appid", ParamValue::from(1005u32)),
            ("clientver", ParamValue::from(20489u32)),
            ("duration", ParamValue::from(243_722u32)),
            ("hash", ParamValue::from(FIXED_HASH)),
            ("keyword", ParamValue::from("Letter - arkady sevidov")),
            ("lrctxt", ParamValue::from(1u32)),
            ("man", ParamValue::from("yes")),
        ])
    }

    #[test]
    fn search_lyric_still_carries_an_android_signature() {
        // 两平台相同：参数里的 appid/clientver 硬取标准版，只有盐随平台变。
        assert_eq!(
            signature_android_params(SourceKind::Kugou, &search_lyric_params(), b""),
            "b90333d489a1aae225eb18ac61718d35"
        );
        assert_eq!(
            signature_android_params(SourceKind::KugouConcept, &search_lyric_params(), b""),
            "7fb230b0c4a899192b66f33abad7b0df"
        );
    }

    /// 签名键集合：`search_lyric` 走 `clearDefaultParams`，所以**没有**那四个
    /// 默认参数。上游 `util/request.js:107` 是二选一的赋值，不是合并。
    #[test]
    fn clear_default_params_drops_the_default_keys() {
        let lyric = search_lyric_params();
        let keys: Vec<&str> = lyric.keys().map(String::as_str).collect();
        for dropped in ["dfid", "mid", "uuid", "clienttime"] {
            assert!(
                !keys.contains(&dropped),
                "clearDefaultParams 不该带 {dropped}，实际键：{keys:?}"
            );
        }
        // 对照：不 clear 的 song_url 是带的。
        let song_keys = song_url_params(SourceKind::Kugou, "x", "y");
        for kept in ["dfid", "mid", "uuid", "clienttime"] {
            assert!(song_keys.contains_key(kept), "song_url 应当带 {kept}");
        }
    }

    /// 对象型参数值必须按**插入序**序列化，与 JS 的 `JSON.stringify` 一致。
    ///
    /// 上游 `helper.js:61-80` 对 `typeof === 'object'` 的值先 `JSON.stringify`；
    /// JS 对象键序是插入序。这条测试的期望值来自
    /// `tools/kat/kat_object_order.js`，其中对象故意让插入序（`zeta,alpha,mid`）
    /// 与字典序（`alpha,mid,zeta`）不同——用默认的 `BTreeMap` 会得到另一份串。
    ///
    /// 依赖 `Cargo.toml` 里 `serde_json` 的 `preserve_order` 特性：关掉它这条会红。
    #[test]
    fn object_valued_params_keep_insertion_order() {
        // serde_json 的 Value 解析出来就是插入序（开了 preserve_order）。
        let nested: Value = serde_json::from_str(r#"{"zeta":1,"alpha":2,"mid":3}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&nested).unwrap(),
            r#"{"zeta":1,"alpha":2,"mid":3}"#,
            "preserve_order 没生效：Value::Object 又变回字典序了"
        );

        let params = map(vec![
            ("keyword", ParamValue::from("x")),
            ("nested", ParamValue::Json(nested)),
        ]);

        // tools/kat/kat_object_order.js 的 paramsString
        assert_eq!(
            signature_android_params(SourceKind::Kugou, &params, b""),
            "78c4cafe98a52eab308b1a6403812774"
        );
        assert_eq!(
            signature_android_params(SourceKind::KugouConcept, &params, b""),
            "5bcb30fb10df49df0e87d0beaaa9e590"
        );
    }
}
