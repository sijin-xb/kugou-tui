//! 纯 Rust 后端。底层（签名 / 设备指纹 / KRC 解密）已实现并有 known-answer
//! 测试对照上游；网络接口按阶段逐个接入，每个都先有 KAT 再有实现。
//!
//! 实现顺序：底层先行，KAT 全绿之后才接网络——签名错一个字节就全部失败，
//! 而且不会有清楚的报错，混在网络调试里查不动。
//!
//! 平台参数由 [`SourceKind`] 携带：标准版与概念版（lite）的盐值、`appid`、
//! `clientver` 都不同，两个都要能跑。**native 知道自己在哪个平台**，这是它相对
//! `NodeApi` 的一个优势：`module/song_url.js` 里那些 `isLite` 分支依赖
//! `process.env.platform`，客户端根本看不到，而这里可以直接按平台取对的那一支。

// 底层纯函数模块。`sign`/`device`/`krc`/`crypto` 都已被网络层用上，不需要
// 模块级 allow。
pub mod crypto;
pub mod device;
pub mod krc;
pub mod sign;
pub mod transport;

use crate::api::catalog::{
    StreamSource, StreamUrl, collect_all_pages, collect_artists, collect_playlists,
    resolve_stream_url,
};
use crate::api::cloud::{QrCheck, QrStatus, UserInfo, VipInfo, VipKind};
use crate::api::model::{
    Artist, Lyric, Playlist, RankBoard, Song, extract_songs, pick_i64, pick_string,
    rank_board_from_json,
};
use crate::api::native::device::random_string;
use crate::api::native::transport::{
    EncryptType, Endpoint, GATEWAY_BASE, LOGIN_BASE, LYRICS_BASE, OPENAPI_BASE, Transport,
    USER_SERVICE_BASE, VIP_BASE,
};
use crate::api::traits::MusicApi;
use crate::api::{data_of, extract_list};
use crate::error::{AppError, Result};
use crate::source::SourceKind;
use crate::util::random_f64;
use serde_json::Value;

/// 内嵌的纯 Rust 后端。
#[derive(Debug, Clone)]
pub struct NativeApi {
    transport: Transport,
    /// 供界面显示。native 没有服务地址，用平台名代替——标准版与概念版是两套
    /// `appid`/盐值，出问题时「现在到底在用哪一套」是第一个要确认的事。
    label: String,
}

impl NativeApi {
    pub fn new(kind: SourceKind, cookie: Option<String>, proxy: Option<&str>) -> Result<Self> {
        Ok(Self {
            transport: Transport::new(kind, cookie, proxy)?,
            label: format!("native（{}，内嵌）", kind.label()),
        })
    }

    pub fn base(&self) -> &str {
        &self.label
    }

    pub fn cookie(&self) -> Option<&str> {
        self.transport.cookie()
    }

    pub fn set_cookie(&mut self, cookie: Option<String>) {
        self.transport.set_cookie(cookie);
    }

    fn kind(&self) -> SourceKind {
        self.transport.kind()
    }
}

/// 上游 `module/search.js` 的请求规格（路由 `/search`）。
///
/// `type` 白名单里只有 `song` 走 `/v3`；本项目只搜单曲，固定这一支。
pub(crate) fn search_endpoint(keywords: &str, page: u32, page_size: u32) -> Endpoint<'static> {
    Endpoint::get(GATEWAY_BASE, "/v3/search/song")
        .header("x-router", "complexsearch.kugou.com")
        .param("albumhide", "0")
        .param("iscorrection", "1")
        .param("keyword", keywords)
        .param("nocollect", "0")
        .param("page", page.to_string())
        .param("pagesize", page_size.to_string())
        .param("platform", "AndroidFilter")
}

/// 上游 `module/privilege_lite.js` 的请求规格（路由 `/privilege/lite`）。
///
/// `NodeApi` 那条路是**把 `hash` 交给 Node 服务、由服务去建 `resource` 数组**；
/// native 直连网关，得自己建。`resource` 数组进的是 POST body，而 android 签名
/// 覆盖 body 字符串，所以它的**字节形态**必须与上游一致。
pub(crate) fn privilege_lite_endpoint(kind: SourceKind, song: &Song) -> Result<Endpoint<'static>> {
    // 逗号分隔可一次问多个 hash：主 hash 在前，audio_info 里那些在后。
    let mut seen = std::collections::HashSet::new();
    let mut hashes = vec![song.hash.clone()];
    seen.insert(song.hash.clone());
    for hash in song.extra_hashes.values() {
        if seen.insert(hash.clone()) {
            hashes.push(hash.clone());
        }
    }

    // `resource[l]['album_id'] = s`，其中 `s` 来自
    // `(params?.album_id || '').split(',')`。我们（和 `NodeApi` 一样）从不传
    // `album_id`，于是 `''.split(',') === ['']`——**只有第 0 个元素**被改写成
    // 空字符串，其余保持初始的数字 0。这个 `""` 与 `0` 的区别会直接改变 body
    // 字节，进而改变签名。
    let resource: Vec<Value> = hashes
        .iter()
        .enumerate()
        .map(|(index, hash)| {
            let album_id = if index == 0 {
                Value::String(String::new())
            } else {
                Value::from(0)
            };
            serde_json::json!({
                "type": "audio",
                "page_id": 0,
                "hash": hash,
                "album_id": album_id,
            })
        })
        .collect();

    // `appid` / `clientver` 在上游取自 `require('../util')`，即平台默认值
    // （标准 1005/20489，概念 3116/11440）。
    let body = serde_json::json!({
        "appid": sign::appid(kind),
        "area_code": 1,
        "behavior": "play",
        "clientver": sign::clientver(kind),
        "need_hash_offset": 1,
        "relate": 1,
        "support_verify": 1,
        "resource": resource,
        "qualities": [
            "128", "320", "flac", "high", "viper_atmos", "viper_tape",
            "viper_clear", "super", "multitrack",
        ],
    });
    let body = serde_json::to_string(&body)
        .map_err(|error| AppError::Other(format!("序列化 /privilege/lite 请求体失败：{error}")))?;

    Ok(Endpoint::post(GATEWAY_BASE, "/v2/get_res_privilege/lite")
        .header("x-router", "media.store.kugou.com")
        .header("Content-Type", "application/json")
        .body(body))
}

/// 上游 `module/song_url.js` 的请求规格（路由 `/song/url`）。
///
/// `dfid` 由调用方给出：上游那一行是
/// `cookie: Object.assign({}, {dfid: randomString(24)}, params?.cookie)`，
/// 即**没登录时是每次调用新生成的 24 字符随机串**，不是 `-` 也不是空。
pub(crate) fn song_url_endpoint(
    kind: SourceKind,
    dfid: String,
    hash: &str,
    quality: &str,
    free_part: bool,
) -> Endpoint<'static> {
    let (page_id, pid, ppage_id) = match kind {
        SourceKind::KugouConcept => ("967177915", "411", "356753938"),
        _ => ("151369488", "2", "463467626,350369493,788954147"),
    };

    Endpoint::get(GATEWAY_BASE, "/v5/url")
        .header("x-router", "trackercdn.kugou.com")
        .dfid(dfid)
        .param("album_id", "0")
        .param("area_code", "1")
        // 上游 `(params?.hash || '').toLowerCase()`。
        .param("hash", hash.to_lowercase())
        .param("ssa_flag", "is_fromtrack")
        .param("version", "11430")
        .param("page_id", page_id)
        // 上游 `quality: quality || 128`。
        .param("quality", if quality.is_empty() { "128" } else { quality })
        .param("album_audio_id", "0")
        .param("behavior", "play")
        .param("pid", pid)
        .param("cmd", "26")
        .param("pidversion", "3001")
        .param("IsFreePart", if free_part { "1" } else { "0" })
        .param("ppage_id", ppage_id)
        .param("cdnBackup", "1")
        .param("module", "")
        // dataMap 里的 `clientver: 11430` 会覆盖平台默认值（见
        // `transport::merge_params`），标准版与概念版都是 11430。
        .param("clientver", "11430")
        .encrypt_key()
}

/// 上游 `module/search_lyric.js` 的请求规格（路由 `/search/lyric`）。
///
/// 这个模块带 `clearDefaultParams: true`，所以 `dfid`/`mid`/`uuid`/`appid`/
/// `clientver`/`clienttime` 全部不进参数（也不进签名）——`appid`/`clientver`
/// 由模块自己按平台写进 `dataMap`。注意 `module/search_lyric.js` 里的
/// `notSign: true` 是**死参数**（`util/request.js:126` 读的是 `notSignature`），
/// 所以照常带 android 签名。
pub(crate) fn search_lyric_endpoint(kind: SourceKind, song: &Song) -> Endpoint<'static> {
    Endpoint::get(LYRICS_BASE, "/v1/search")
        .clear_defaults()
        .param("album_audio_id", "0")
        .param("appid", sign::appid(kind).to_string())
        .param("clientver", sign::clientver(kind).to_string())
        .param("duration", song.duration_ms.to_string())
        .param("hash", song.hash.clone())
        .param("keyword", format!("{} - {}", song.singer_text(), song.name))
        .param("lrctxt", "1")
        .param("man", "yes")
}

/// 上游 `module/lyric.js` 的请求规格（路由 `/lyric`）。
///
/// 与 `/search/lyric` 不同，这个模块**没有** `clearDefaultParams`，所以走的是
/// 标准默认参数（含登录态的 `token`/`userid`），头里也带 `clienttime`。
pub(crate) fn lyric_endpoint(lyric_id: &str, access_key: &str) -> Endpoint<'static> {
    Endpoint::get(LYRICS_BASE, "/download")
        .param("ver", "1")
        .param("client", "android")
        .param("id", lyric_id)
        .param("accesskey", access_key)
        // 必须是 krc：译文与逐字时间戳只在 KRC 里有。
        .param("fmt", "krc")
        .param("charset", "utf8")
}

/// 把 `/lyric` 响应里的 `content` 就地解成 `decodeContent`。
///
/// 等价于上游 `module/lyric.js` 的 `decode` 分支：
/// ```js
/// res.body['decodeContent'] = params?.fmt == 'lrc' || Number(res.body?.contenttype) !== 0
///   ? Buffer.from(content, 'base64').toString()
///   : decodeLyrics(content);
/// ```
/// **`contenttype !== 0` 时是 base64 而不是 KRC**——少了这个分支，那种响应会被
/// 当成 KRC 解，得到一片乱码而不是报错。
///
/// 解密失败时**写空串**而不是返回 `Err`：上游 `decodeLyrics` 解不开就
/// `return ''`，空文本解析出空歌词、候选被跳过，最终是「未找到歌词」而不是
/// 一个错误。这里对齐同一语义，两端的用户可见行为才一致。
///
/// `decode=false`（本项目不用）时上游会把 `content` 原样留下，这里同理什么都不做。
pub(crate) fn inject_decoded_lyric(root: &mut Value) {
    // 与 `lyric::extract_lyric_text` 的 `data_of` 取同一层：有 `data` 对象就写进
    // `data`，否则写进根。
    let target = if root.get("data").is_some_and(Value::is_object) {
        root.get_mut("data")
    } else {
        Some(root)
    };
    let Some(object) = target.and_then(Value::as_object_mut) else {
        return;
    };
    let Some(content) = object.get("content").and_then(Value::as_str) else {
        return;
    };
    let content = content.to_string();

    // `Number(undefined) !== 0` 为真 → 缺 contenttype 时走 base64 分支。
    let contenttype = object
        .get("contenttype")
        .and_then(crate::api::model::value_to_i64)
        .unwrap_or(-1);
    let decoded = if contenttype != 0 {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(content.trim())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_default()
    } else {
        // 解码失败只退化成空歌词，但必须留一条 WARN：静默吞掉会让「歌词怎么
        // 没了」变成一个查不出来的问题。
        crate::api::native::krc::decode(&content).unwrap_or_else(|error| {
            crate::logger::tlog!(
                crate::logger::LEVEL_WARN,
                "KRC 歌词解码失败，退回空歌词：{error}"
            );
            String::new()
        })
    };

    object.insert("decodeContent".to_string(), Value::String(decoded));
}

/// 上游 `module/register_dev.js` 的设备信息表（31 键，插入序即 JSON 键序）。
///
/// `imei` 与 `uuid` 都取 `cookie.KUGOU_API_GUID`，上游对两者都用 `||`——GUID 缺失时
/// 它们会变成 `undefined` 并被 `JSON.stringify` **整个丢掉**（31 键变 29 键，AES
/// 明文随之改变，而且不会报错）。native 的 `Transport::cookie_map` 保证
/// `KUGOU_API_GUID` 始终存在（缺时用本地设备标识补），所以这里不会走到那一支。
pub(crate) fn register_dev_data_map(guid: &str) -> Value {
    serde_json::json!({
        "availableRamSize": 4983533568u64,
        "availableRomSize": 48114719u64,
        "availableSDSize": 48114717u64,
        "basebandVer": "",
        "batteryLevel": 100,
        "batteryStatus": 3,
        "brand": "Redmi",
        "buildSerial": "unknown",
        "device": "marble",
        "imei": guid,
        "imsi": "",
        "manufacturer": "Xiaomi",
        "uuid": guid,
        "accelerometer": false,
        "accelerometerValue": "",
        "gravity": false,
        "gravityValue": "",
        "gyroscope": false,
        "gyroscopeValue": "",
        "light": false,
        "lightValue": "",
        "magnetic": false,
        "magneticValue": "",
        "orientation": false,
        "orientationValue": "",
        "pressure": false,
        "pressureValue": "",
        "step_counter": false,
        "step_counterValue": "",
        "temperature": false,
        "temperatureValue": "",
    })
}

/// 上游 `module/register_dev.js` 的请求规格（路由 `/register/dev`）。
///
/// 随机量与时间**全部由调用方注入**，所以能用固定输入对着上游实跑基准逐字节断言：
/// `aes_key` 对应 `playlistAesEncrypt` 内部那次 `randomString(6).toLowerCase()`，
/// `rsa_fill` 对应 `rsaEncrypt2` 里 `forge.random.getBytes` 的填充源。
///
/// 请求体是 AES 密文的 base64，**同时参与 android 签名**（上游
/// `util/request.js` 把 `options.data` 原样拼进签名输入）；`p` 是
/// `{"aes":…,"uid":…,"token":…}` 的 PKCS#1 v1.5 密文。两者都用平台公钥/盐值，
/// 所以标准版与概念版各有一套基准。
pub(crate) fn register_dev_endpoint(
    kind: SourceKind,
    guid: &str,
    userid: Option<&str>,
    token: &str,
    aes_key: &str,
    rsa_fill: &[u8],
) -> Result<Endpoint<'static>> {
    let plain = serde_json::to_string(&register_dev_data_map(guid))
        .map_err(|error| AppError::Other(format!("序列化设备信息失败：{error}")))?;
    let (encrypt_key, iv) = crate::api::native::crypto::playlist_key_material(aes_key);
    let body = crate::api::native::crypto::aes_cbc_encrypt(&encrypt_key, &iv, plain.as_bytes())?;

    // 上游 `params?.userid || params?.cookie?.userid || 0`：缺省是**数字** 0，
    // 有值时是 cookie 里的字符串。这个区别会原样进 JSON、进而进 RSA 明文，
    // 写错不报错，只是服务端不认。
    let uid = match userid {
        Some(value) if !value.is_empty() => Value::String(value.to_string()),
        _ => Value::from(0),
    };
    let rsa_input = serde_json::to_string(&serde_json::json!({
        "aes": aes_key,
        "uid": uid,
        "token": token,
    }))
    .map_err(|error| AppError::Other(format!("序列化 RSA 明文失败：{error}")))?;
    let p = crate::api::native::crypto::pkcs1_v15_encrypt(kind, rsa_input.as_bytes(), rsa_fill)?;

    Ok(Endpoint::post(USER_SERVICE_BASE, "/risk/v2/r_register_dev")
        .param("part", "1")
        .param("platid", "1")
        .param("p", p)
        .body(body))
}

/// 解开 `/register/dev` 的 `arraybuffer` 响应，等价于上游
/// `playlistAesDecrypt({ str: res.body.toString('base64'), key })`。
///
/// `toString('base64')` 之后再 `Base64.parse` 是**恒等变换**，所以直接拿原始字节
/// 当密文。解出的文本按上游那样先试 `JSON.parse`，失败则原样返回字符串——
/// 响应被网关换成 HTML 时不会报错，只是后面取不到 `dfid`。
pub(crate) fn parse_register_dev_response(aes_key: &str, body: &[u8]) -> Result<Value> {
    let (encrypt_key, iv) = crate::api::native::crypto::playlist_key_material(aes_key);
    let plain = crate::api::native::crypto::aes_cbc_decrypt(&encrypt_key, &iv, body)?;
    let text = String::from_utf8_lossy(&plain).into_owned();
    Ok(serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)))
}

/// 二维码 key 的请求规格（上游 `module/login_qr_key.js`，路由 `/login/qr/key`）。
///
/// 三个 `appid` 各不相同，照抄时最容易串：查询串里的 `appid` 固定 **1001**
/// （上游 `params?.type === 'web' ? 1014 : 1001`，本项目不传 `type` 故取 1001），
/// `srcappid` 固定 2919，只有 `qrcode_txt` 里嵌的那个才是**平台** `appid`
/// （标准版 1005 / 概念版 3116）。串了不会报错，只是扫码后拿不到登录态。
pub(crate) fn login_qr_key_endpoint(kind: SourceKind) -> Endpoint<'static> {
    Endpoint::get(LOGIN_BASE, "/v2/qrcode")
        .encrypt_type(EncryptType::Web)
        .param("appid", "1001")
        .param("type", "1")
        .param("plat", "4")
        .param(
            "qrcode_txt",
            format!(
                "https://h5.kugou.com/apps/loginQRCode/html/index.html?appid={}&",
                crate::api::native::sign::appid(kind)
            ),
        )
        .param("srcappid", crate::api::native::sign::SRCAPPID.to_string())
}

/// 二维码状态轮询的请求规格（上游 `module/login_qr_check.js`，路由 `/login/qr/check`）。
///
/// 这里的 `appid` 是**平台** `appid`（与上面那个 1001 不是一回事）。
pub(crate) fn login_qr_check_endpoint(kind: SourceKind, key: &str) -> Endpoint<'static> {
    Endpoint::get(LOGIN_BASE, "/v2/get_userinfo_qrcode")
        .encrypt_type(EncryptType::Web)
        .param("plat", "4")
        .param("appid", crate::api::native::sign::appid(kind).to_string())
        .param("srcappid", crate::api::native::sign::SRCAPPID.to_string())
        .param("qrcode", key.to_string())
}

/// 用户资料的请求规格（上游 `module/user_detail.js`，路由 `/user/detail`）。
///
/// 该模块**没有** `baseURL`，所以走默认网关。`p` 是裸 RSA（无填充）加密
/// `{"token":…,"clienttime":…}` 后转**大写** hex，注意上游 `cryptoRSAEncrypt`
/// 返回小写，是模块自己 `.toUpperCase()` 的。
///
/// `clienttime` 由调用方注入：它同时进 body 与签名，不能取当前时间，否则
/// 对照基准无法复现。
pub(crate) fn user_detail_endpoint(
    kind: SourceKind,
    token: &str,
    userid: Option<&str>,
    clienttime: &str,
) -> Result<Endpoint<'static>> {
    let seconds: i64 = clienttime.parse().map_err(|error| {
        AppError::Other(format!("clienttime 不是合法整数（{clienttime}）：{error}"))
    })?;
    // 上游 `Number(params?.userid || params?.cookie?.userid || '0')`：非数字得到 NaN，
    // 序列化成 null；这里退化成 0，比发出 `null` 更接近服务端预期。
    let userid_value = match userid.and_then(|value| value.parse::<i64>().ok()) {
        Some(value) => Value::from(value),
        None => Value::from(0),
    };
    let rsa_input = serde_json::to_string(&serde_json::json!({
        "token": token,
        "clienttime": seconds,
    }))
    .map_err(|error| AppError::Other(format!("序列化 RSA 明文失败：{error}")))?;
    let p = crate::api::native::crypto::raw_rsa_encrypt(kind, rsa_input.as_bytes())?.to_uppercase();
    let body = serde_json::to_string(&serde_json::json!({
        "visit_time": seconds,
        "usertype": 1,
        "p": p,
        "userid": userid_value,
    }))
    .map_err(|error| AppError::Other(format!("序列化用户资料请求体失败：{error}")))?;
    Ok(Endpoint::post(GATEWAY_BASE, "/v3/get_my_info")
        .header("x-router", "usercenter.kugou.com")
        .param("plat", "1")
        .body(body))
}

/// 会员信息的请求规格（上游 `module/user_vip_detail.js`，路由 `/user/vip/detail`）。
pub(crate) fn user_vip_detail_endpoint() -> Endpoint<'static> {
    Endpoint::get(VIP_BASE, "/v1/get_union_vip").param("busi_type", "concept")
}

// ----------------------------------------------------------------------
// 目录类接口（上游 module/{top_playlist,playlist_track_all,
// playlist_track_all_new,user_playlist,artist_lists,artist_audios,
// rank_list,rank_audio,youth_month_vip_record}.js）
// ----------------------------------------------------------------------

/// 广场歌单的请求规格（上游 `module/top_playlist.js`，路由 `/top/playlist`）。
///
/// 这个模块**没有 `params`**：入参全在 body，`key` 也在 body 里。`key` 用的是
/// `signParamsKey(dateTime.toString())`，与 `encryptKey` 那条路（`signKey`）是
/// 两套算法，别混。`clienttime` 在 body 里是**字符串**（`.toFixed(0)`），
/// 而默认参数里的 `clienttime` 是数字，两者在这里都取同一个秒值。
pub(crate) fn plaza_playlists_endpoint(
    kind: SourceKind,
    category_id: i64,
    page: u32,
    page_size: u32,
    clienttime: &str,
    mid: &str,
    userid: Option<&str>,
) -> Result<Endpoint<'static>> {
    // `special_recommend` 的键序即上游对象字面量的插入序，它会进 android 签名的
    // body 字符串，顺序错了签名就不对。
    let special_recommend = serde_json::json!({
        "withtag": "1",
        "withsong": "0",
        "sort": 1,
        "ugc": 1,
        "is_selected": 0,
        "withrecommend": 1,
        "area_code": 1,
        "categoryid": category_id.to_string(),
    });

    let body = serde_json::json!({
        "appid": sign::appid(kind),
        "mid": mid,
        "clientver": sign::clientver(kind),
        "platform": "android",
        "clienttime": clienttime,
        "userid": userid.unwrap_or("0"),
        "module_id": 1,
        "page": page.to_string(),
        "pagesize": page_size.to_string(),
        "key": sign::sign_params_key(kind, clienttime, None, None),
        "special_recommend": special_recommend,
        "req_multi": 1,
        "retrun_min": 5,
        "return_special_falg": 1,
    });

    Ok(Endpoint::post(GATEWAY_BASE, "/v2/special_recommend")
        .header("x-router", "specialrec.service.kugou.com")
        .header("Content-Type", "application/json")
        .body(serialize_body(body, "广场歌单")?)
        .cache_identity(format!(
            "category_id={category_id}&page={page}&pagesize={page_size}"
        )))
}

/// 公开歌单一页的请求规格（上游 `module/playlist_track_all.js`，路由 `/playlist/track/all`）。
///
/// `begin_idx` 是**数字**（`(Number(page) - 1) * pagesize`），`pagesize` 是**字符串**——
/// 上游从 query 拿到的一律是字符串，`Number()` 只用在 `page` 上。这个区别会进签名。
pub(crate) fn playlist_tracks_endpoint(
    global_id: &str,
    page: u32,
    page_size: u32,
) -> Endpoint<'static> {
    Endpoint::get(GATEWAY_BASE, "/pubsongs/v2/get_other_list_file_nofilt")
        .param("area_code", "1")
        .param(
            "begin_idx",
            (page.saturating_sub(1) as u64 * u64::from(page_size)).to_string(),
        )
        .param("plat", "1")
        .param("type", "1")
        .param("mode", "1")
        .param("personal_switch", "1")
        .param("extend_fields", "abtags,hot_cmt,popularization")
        .param("pagesize", page_size.to_string())
        .param("global_collection_id", global_id)
}

/// 当前用户歌单的请求规格（上游 `module/user_playlist.js`，路由 `/user/playlist`）。
///
/// `userid` 在 `params` 里是 `Number(userid)`，在 body 里是原样的字符串；
/// `total_ver: 979` 与 `type: 2` 是上游写死的。
pub(crate) fn user_playlists_endpoint(
    page: u32,
    page_size: u32,
    userid: Option<&str>,
    token: Option<&str>,
) -> Result<Endpoint<'static>> {
    let userid = userid.unwrap_or("0");
    let token = token.unwrap_or("");

    let body = serde_json::json!({
        "userid": userid,
        "token": token,
        "total_ver": 979,
        "type": 2,
        "page": page.to_string(),
        "pagesize": page_size.to_string(),
    });

    Ok(Endpoint::post(GATEWAY_BASE, "/v7/get_all_list")
        .header("x-router", "cloudlist.service.kugou.com")
        .header("Content-Type", "application/json")
        .param("plat", "1")
        .param("userid", userid)
        .param("token", token)
        .body(serialize_body(body, "用户歌单")?)
        .cache_identity(format!("page={page}&pagesize={page_size}")))
}

/// 用户歌单一页的请求规格（上游 `module/playlist_track_all_new.js`，路由 `/playlist/track/all/new`）。
///
/// 这个模块**没有 `params`**，入参全在 body。`token` 缺失时上游写 `'0'`
/// （不是空串），`userid` 同理——这个差异会进签名。
pub(crate) fn user_playlist_tracks_endpoint(
    list_id: i64,
    page: u32,
    page_size: u32,
    userid: Option<&str>,
    token: Option<&str>,
) -> Result<Endpoint<'static>> {
    let body = serde_json::json!({
        "listid": list_id.to_string(),
        "userid": userid.unwrap_or("0"),
        "area_code": 1,
        "show_relate_goods": 0,
        "pagesize": page_size.to_string(),
        "allplatform": 1,
        "show_cover": 1,
        "type": 0,
        "token": token.unwrap_or("0"),
        "page": page.to_string(),
    });

    Ok(Endpoint::post(GATEWAY_BASE, "/v4/get_list_all_file")
        .header("x-router", "cloudlist.service.kugou.com")
        .header("Content-Type", "application/json")
        .body(serialize_body(body, "用户歌单歌曲")?)
        .cache_identity(format!("listid={list_id}&page={page}&pagesize={page_size}")))
}

/// 歌手列表的请求规格（上游 `module/artist_lists.js`，路由 `/artist/lists`）。
///
/// `musician` 与 `hotsize` 走 `Number()` 是数字，`sextype` 与 `type` 直接取
/// query 里那串**字符串**——`"0"` 是 truthy，所以 `params?.type || 0` 不会
/// 退化成数字 `0`。写错不报错，只是签名不对。
pub(crate) fn artist_list_endpoint(kind: i64, hot_size: u32) -> Endpoint<'static> {
    Endpoint::get(GATEWAY_BASE, "/ocean/v6/singer/list")
        .param("musician", "0")
        .param("sextype", "0")
        .param("showtype", "2")
        .param("type", kind.to_string())
        .param("hotsize", hot_size.to_string())
}

/// 歌手单曲的请求规格（上游 `module/artist_audios.js`，路由 `/artist/audios`）。
///
/// 入参全在 body，`clienttime` 在这里是**数字**（`Math.floor(new Date().getTime()/1000)`），
/// 与广场那个字符串不同。`sort` 只有 `'hot'` 是 1，其余一律 2。
pub(crate) fn artist_tracks_endpoint(
    kind: SourceKind,
    artist_id: i64,
    sort: &str,
    page: u32,
    page_size: u32,
    clienttime: &str,
    mid: &str,
) -> Result<Endpoint<'static>> {
    // `signParamsKey(clienttime)`：上游传的是数字，模板串里会转成十进制文本。
    let key = sign::sign_params_key(kind, clienttime, None, None);
    let clienttime_number = clienttime.parse::<i64>().map_err(|error| {
        AppError::Other(format!("clienttime 不是合法整数（{clienttime}）：{error}"))
    })?;

    let body = serde_json::json!({
        "appid": sign::appid(kind),
        "clientver": sign::clientver(kind),
        "mid": mid,
        "clienttime": clienttime_number,
        "key": key,
        "author_id": artist_id.to_string(),
        "pagesize": page_size.to_string(),
        "page": page.to_string(),
        "sort": if sort == "hot" { 1 } else { 2 },
        "area_code": "all",
    });

    Ok(Endpoint::post(OPENAPI_BASE, "/kmr/v1/audio_group/author")
        .header("x-router", "openapi.kugou.com")
        .header("kg-tid", "220")
        .header("Content-Type", "application/json")
        .body(serialize_body(body, "歌手单曲")?)
        .cache_identity(format!(
            "id={artist_id}&sort={sort}&page={page}&pagesize={page_size}"
        )))
}

/// 排行榜列表的请求规格（上游 `module/rank_list.js`，路由 `/rank/list`）。
pub(crate) fn rank_boards_endpoint() -> Endpoint<'static> {
    Endpoint::get(GATEWAY_BASE, "/ocean/v6/rank/list")
        .param("plat", "2")
        .param("withsong", "0")
        .param("parentid", "0")
}

/// 排行榜歌曲的请求规格（上游 `module/rank_audio.js`，路由 `/rank/audio`）。
///
/// 入参全在 body，`rank_id` 是**字符串**，`rank_cid` 是数字 `0`。
pub(crate) fn rank_tracks_endpoint(
    rank_id: i64,
    page: u32,
    page_size: u32,
) -> Result<Endpoint<'static>> {
    let body = serde_json::json!({
        "show_portrait_mv": 1,
        "show_type_total": 1,
        "filter_original_remarks": 1,
        "area_code": 1,
        "pagesize": page_size.to_string(),
        "rank_cid": 0,
        "type": 1,
        "page": page.to_string(),
        "rank_id": rank_id.to_string(),
    });

    Ok(Endpoint::post(GATEWAY_BASE, "/openapi/kmr/v2/rank/audio")
        .header("kg-tid", "369")
        .header("Content-Type", "application/json")
        .body(serialize_body(body, "排行榜歌曲")?)
        .cache_identity(format!("rankid={rank_id}&page={page}&pagesize={page_size}")))
}

/// 本月已领取的会员天数（上游 `module/youth_month_vip_record.js`，
/// 路由 `/youth/month/vip/record`）。
pub(crate) fn claimed_vip_days_endpoint() -> Endpoint<'static> {
    Endpoint::get(GATEWAY_BASE, "/youth/v1/activity/get_month_vip_record")
        .param("latest_limit", "100")
}

/// 领取某一天会员的请求规格（上游 `module/youth_day_vip.js`，
/// 路由 `/youth/day/vip`）。
///
/// 这个模块**没有 body**，`source_id` 是写死的 `90139`（只有概念版账号能领），
/// `receive_day` 是要领取的那一天（`2026-09-23`），不是「今天」。它带一个
/// `content-type: application/x-www-form-urlencoded` 头，但请求体是空的——
/// 上游 `request.js` 无 `data` 时传空串，签名用的就是那个空串。
pub(crate) fn claim_day_vip_endpoint(receive_day: &str) -> Endpoint<'static> {
    Endpoint::post(GATEWAY_BASE, "/youth/v1/recharge/receive_vip_listen_song")
        .header("content-type", "application/x-www-form-urlencoded")
        .param("source_id", "90139")
        .param("receive_day", receive_day.to_string())
}

/// 升级当天会员的请求规格（上游 `module/youth_day_vip_upgrade.js`，
/// 路由 `/youth/day/vip/upgrade`）。
///
/// 同样没有 body，也没有自定义头。`kugouid` 取上游
/// `Number(params?.userid || params?.cookie?.userid || 0)`——非数字得到 `NaN`、
/// 序列化成 `null`，这里退化成 `0`（与 [`user_detail_endpoint`] 同一处理）。
pub(crate) fn upgrade_day_vip_endpoint(userid: Option<&str>) -> Endpoint<'static> {
    let kugouid = userid
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_default();
    Endpoint::post(GATEWAY_BASE, "/youth/v1/listen_song/upgrade_vip_reward")
        .param("kugouid", kugouid.to_string())
        .param("ad_type", "1")
}

// ----------------------------------------------------------------------
// 云歌单写接口（上游 module/{playlist_add,playlist_del,
// playlist_tracks_add,playlist_tracks_del}.js）
// ----------------------------------------------------------------------

/// 单次提交的歌数上限，与 `NodeApi` 一致：服务端按逗号分隔多首，
/// 一次提交太多会被截断。
const WRITE_BATCH_SIZE: usize = 20;

/// 上游 `params?.userid || params?.cookie?.userid || 0` 的取值形态。
///
/// 注意 JS 里字符串 `"0"` 是 **truthy**，所以只有空串/缺省才落到数字 `0`；
/// 这个 `"0"` 与 `0` 的差别会原样进 JSON、进而进签名与 RSA 明文，写错不报错。
fn userid_value(userid: Option<&str>) -> Value {
    match userid {
        Some(value) if !value.is_empty() => Value::String(value.to_string()),
        _ => Value::from(0),
    }
}

/// 新建歌单的请求规格（上游 `module/playlist_add.js`，路由 `/playlist/add`）。
///
/// `url` 自带 `cloudlist.service` 前缀，**没有 `x-router`**。`type` 取 query 里
/// 那串字符串 `"0"`（Express 给的全是字符串），因此上游
/// `if (params.type === 0)` 这个严格比较**不成立**，`is_pri` 保持字面量 `0`；
/// 同理 `params.type === 0 ? {...} : {}` 让这里**没有额外 params**。
/// 若把 `type` 写成数字 `0`，会同时多出一组 params 并改变签名。
pub(crate) fn playlist_add_endpoint(
    name: &str,
    userid: Option<&str>,
    token: Option<&str>,
) -> Result<Endpoint<'static>> {
    // 未传的 `list_create_userid` / `list_create_listid` 在 JS 里是 `undefined`，
    // `JSON.stringify` 会把它们整个丢掉——所以这里**不出现**这两个键。
    let body = serde_json::json!({
        "userid": userid_value(userid),
        "token": token.unwrap_or(""),
        "total_ver": 0,
        "name": name,
        "type": "0",
        "source": 1,
        "is_pri": 0,
        "list_create_gid": "",
        "from_shupinmv": 0,
    });

    Ok(
        Endpoint::post(GATEWAY_BASE, "/cloudlist.service/v5/add_list")
            .header("Content-Type", "application/json")
            .body(serialize_body(body, "新建歌单")?),
    )
}

/// 从歌单移除歌曲的请求规格（上游 `module/playlist_tracks_del.js`，
/// 路由 `/playlist/tracks/del`）。
///
/// 入参全在 body，**没有额外 params**。`listid` 是字符串，`fileid` 是数字。
pub(crate) fn playlist_tracks_del_endpoint(
    list_id: i64,
    file_ids: &[i64],
    userid: Option<&str>,
    token: Option<&str>,
) -> Result<Endpoint<'static>> {
    let entries: Vec<Value> = file_ids
        .iter()
        .map(|file_id| serde_json::json!({ "fileid": file_id }))
        .collect();

    let body = serde_json::json!({
        "listid": list_id.to_string(),
        "userid": userid_value(userid),
        "data": entries,
        "type": 0,
        "token": token.unwrap_or(""),
        "list_ver": 0,
    });

    Ok(Endpoint::post(GATEWAY_BASE, "/v4/delete_songs")
        .header("x-router", "cloudlist.service.kugou.com")
        .header("Content-Type", "application/json")
        .body(serialize_body(body, "歌单移除歌曲")?))
}

/// 上游 `module/playlist_tracks_add.js` 里 `data` 的单个条目。
///
/// 上游拿到的是 `歌名|hash|专辑id|album_audio_id` 拼成的串，再 `split('|')`
/// 还原成对象；`album_id` / `mixsongid` 走 `Number(d[i] || 0)`。空串走 `|| 0`
/// 得数字 `0`——写成字符串会改变 body 与签名。
fn track_resource_entry(entry: &str) -> Value {
    let fields: Vec<&str> = entry.split('|').collect();
    let numeric = |index: usize| -> i64 {
        fields
            .get(index)
            .filter(|value| !value.is_empty())
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0)
    };

    serde_json::json!({
        "number": 1,
        "name": fields.first().copied().unwrap_or(""),
        "hash": fields.get(1).copied().unwrap_or(""),
        "size": 0,
        "sort": 0,
        "timelen": 0,
        "bitrate": 0,
        "album_id": numeric(2),
        "mixsongid": numeric(3),
    })
}

/// 往歌单加歌的请求规格（上游 `module/playlist_tracks_add.js`，
/// 路由 `/playlist/tracks/add`）。
///
/// `url` 自带 `cloudlist.service` 前缀，**没有 `x-router`**。`params` 只有
/// `last_time` 与 `last_area`。条目串沿用 `NodeApi` 的 `encode_track_entry`
/// 生成，再按上游规则拆回对象，保证两端提交的内容完全一致。
pub(crate) fn playlist_tracks_add_endpoint(
    list_id: i64,
    payload: &str,
    userid: Option<&str>,
    token: Option<&str>,
    clienttime: &str,
) -> Result<Endpoint<'static>> {
    // 上游 `(params.data || '').split(',')`：空串会得到 `['']` 一个条目，
    // 这里保持一致（调用方已在空列表时提前返回）。
    let entries: Vec<Value> = payload.split(',').map(track_resource_entry).collect();

    let body = serde_json::json!({
        "userid": userid_value(userid),
        "token": token.unwrap_or(""),
        "listid": list_id.to_string(),
        "list_ver": 0,
        "type": 0,
        "slow_upload": 1,
        "scene": "false;null",
        "data": entries,
    });

    Ok(
        Endpoint::post(GATEWAY_BASE, "/cloudlist.service/v6/add_song")
            .header("Content-Type", "application/json")
            .param("last_time", clienttime)
            .param("last_area", "gztx")
            .body(serialize_body(body, "歌单添加歌曲")?),
    )
}

/// 删除歌单的请求规格（上游 `module/playlist_del.js`，路由 `/playlist/del`）。
///
/// 与其余三个写接口不同：body 是 AES-128-CBC 密文的 base64，`p` 是 PKCS#1 v1.5
/// 加密后的大写 hex，`params` 还带一个 `signParamsKey(clienttime)` 的 `key`。
/// 密钥与 RSA 填充由调用方注入，便于用固定输入做 KAT。
pub(crate) fn playlist_del_endpoint(
    kind: SourceKind,
    list_id: i64,
    userid: Option<&str>,
    token: Option<&str>,
    clienttime: &str,
    aes_key: &str,
    rsa_fill: &[u8],
) -> Result<Endpoint<'static>> {
    // `dataMap = {listid: Number(params.listid), total_ver: 0, type: 1}`：
    // `listid` 是**数字**，与其余接口的字符串形态不同。
    let plain = serde_json::to_string(&serde_json::json!({
        "listid": list_id,
        "total_ver": 0,
        "type": 1,
    }))
    .map_err(|error| AppError::Other(format!("序列化待删歌单失败：{error}")))?;
    let (encrypt_key, iv) = crate::api::native::crypto::playlist_key_material(aes_key);
    let body = crate::api::native::crypto::aes_cbc_encrypt(&encrypt_key, &iv, plain.as_bytes())?;

    let rsa_input = serde_json::to_string(&serde_json::json!({
        "aes": aes_key,
        "uid": userid_value(userid),
        "token": token.unwrap_or(""),
    }))
    .map_err(|error| AppError::Other(format!("序列化 RSA 明文失败：{error}")))?;
    let p = crate::api::native::crypto::pkcs1_v15_encrypt(kind, rsa_input.as_bytes(), rsa_fill)?
        .to_uppercase();

    Ok(Endpoint::post(GATEWAY_BASE, "/v2/delete_list")
        .header("x-router", "cloudlist.service.kugou.com")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .param("key", sign::sign_params_key(kind, clienttime, None, None))
        .param("last_area", "gztx")
        .param("last_time", clienttime)
        .param("p", p)
        .body(body))
}

/// `serde_json::to_string` 的错误信息要带上接口名——签名覆盖 body，序列化一旦
/// 变了就全线 403，报错里能看出是哪个接口才有得查。
fn serialize_body(body: Value, what: &str) -> Result<String> {
    serde_json::to_string(&body)
        .map_err(|error| AppError::Other(format!("序列化{what}请求体失败：{error}")))
}

/// 二维码内容（上游 `module/login_qr_create.js`，路由 `/login/qr/create`）。
///
/// 上游这个模块**不联网**：它只是把 key 拼进 H5 登录页地址，再用 `qrcode` 包
/// 渲染成 data URL。终端里由 `src/ui/widgets.rs` 自己编码，所以 `base64` 那半
/// 不用管，只返回待编码的字符串。
pub(crate) fn login_qr_content(key: &str) -> String {
    format!("https://h5.kugou.com/apps/loginQRCode/html/index.html?qrcode={key}")
}

/// 把 `/login/qr/check` 的响应翻成 [`QrCheck`]。
///
/// 上游注释：`0` 过期、`1` 等待扫码、`2` 待确认、`4` 授权成功（此时才有 token）。
/// 上游 `4` 时还会往 Set-Cookie 里塞裸 `token=…`/`userid=…`，本项目按客户端既有
/// 约定直接读 body 字段（`src/app/cloud.rs` 的 `apply_login` 会自己拼 cookie）。
pub(crate) fn parse_qr_check(root: &Value) -> QrCheck {
    let data = data_of(root);
    let status = match pick_i64(data, &["status", "code"]) {
        Some(0) => QrStatus::Expired,
        Some(2) => QrStatus::Pending,
        Some(4) => QrStatus::Success,
        _ => QrStatus::Waiting,
    };
    QrCheck {
        status,
        token: pick_string(data, &["token"]),
        userid: pick_string(data, &["userid"]),
        cookie: None,
    }
}

/// 把 `/user/detail` 的响应翻成 [`UserInfo`]（与 `NodeApi` 同字段、同兜底）。
pub(crate) fn parse_user_detail(root: &Value) -> UserInfo {
    let data = data_of(root);
    UserInfo {
        nickname: pick_string(data, &["nickname"]).unwrap_or_default(),
        pic: pick_string(data, &["pic"]).filter(|url| !url.trim().is_empty()),
        grade: pick_i64(data, &["p_grade"]).and_then(|value| u32::try_from(value).ok()),
        duration_min: pick_i64(data, &["duration"]).and_then(|value| u64::try_from(value).ok()),
    }
}

/// 把 `/user/vip/detail` 的响应翻成 [`VipInfo`]（与 `NodeApi` 同逻辑）。
///
/// 顶层 `is_vip` 只反映标准版豪华 VIP；概念版等形态在 `busi_vip[]` 里。
pub(crate) fn parse_user_vip_detail(root: &Value) -> VipInfo {
    let data = data_of(root);
    let mut info = VipInfo::default();
    if pick_i64(data, &["is_vip", "vip_type"]) == Some(1) {
        info.kind = VipKind::Standard;
        info.product = "VIP".to_string();
        info.end_time = pick_string(data, &["vip_end_time"]).unwrap_or_default();
        return info;
    }
    if let Some(entries) = data.get("busi_vip").and_then(Value::as_array) {
        for entry in entries {
            if pick_i64(entry, &["is_vip"]) != Some(1) {
                continue;
            }
            let busi_type = pick_string(entry, &["busi_type"]).unwrap_or_default();
            info.kind = if busi_type == "concept" {
                VipKind::Concept
            } else {
                VipKind::Other(busi_type)
            };
            info.product = pick_string(entry, &["product_type"]).unwrap_or_default();
            info.end_time = pick_string(entry, &["vip_end_time"]).unwrap_or_default();
            return info;
        }
    }
    info
}

/// `NativeApi` 的歌手与榜单单页实现。
///
/// 这两个方法**不在 `MusicApi` 里**：它们只被 `*_all` 的翻页闭包调用，与
/// `NodeApi` 的 `artist_tracks` / `rank_tracks` 一样属于分页实现细节。
impl NativeApi {
    /// 歌手单曲一页。`sort`: `hot` 热门 / `new` 最新。
    pub(crate) async fn artist_tracks(
        &self,
        artist_id: i64,
        sort: &str,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<Song>> {
        let cookies = self.transport.cookie_map();
        let clienttime = (crate::util::now_unix_millis() / 1000).to_string();
        let endpoint = artist_tracks_endpoint(
            self.kind(),
            artist_id,
            sort,
            page,
            page_size,
            &clienttime,
            cookies
                .get("KUGOU_API_MID")
                .map(String::as_str)
                .unwrap_or_default(),
        )?;
        let root = self.transport.get_json(&endpoint, true).await?;
        Ok(extract_songs(data_of(&root)))
    }

    /// 排行榜歌曲一页。
    pub(crate) async fn rank_tracks(
        &self,
        rank_id: i64,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<Song>> {
        let endpoint = rank_tracks_endpoint(rank_id, page, page_size)?;
        let root = self.transport.get_json(&endpoint, true).await?;
        Ok(extract_songs(data_of(&root)))
    }
}

/// `NativeApi` 的歌词实现。
///
/// 算法在 [`crate::api::lyric::fetch_lyric_via`]，与 `NodeApi` **共用同一份**；
/// 这里只负责把两个请求按上游规则拼出来，并在下载那步本地解密 KRC
/// （`NodeApi` 是把 `decode=true` 交给本机 Node 服务去做）。
impl crate::api::lyric::LyricSource for NativeApi {
    async fn search_lyric(&self, song: &Song) -> Result<Value> {
        let endpoint = search_lyric_endpoint(self.kind(), song);
        // `NodeApi` 这条路走的是 `get_json`（带缓存、不加 timestamp），
        // 所以这里也传 `cached = true`，两端的请求条数才一致。
        self.transport.get_json(&endpoint, true).await
    }

    async fn lyric_body(&self, lyric_id: &str, access_key: &str) -> Result<String> {
        let endpoint = lyric_endpoint(lyric_id, access_key);
        let body = self.transport.get_text(&endpoint, true).await?;

        // `/lyric` 在 `decode=true` 下返回 JSON，偶尔直接吐纯文本；两种都接住，
        // 与 `fetch_lyric_via` 的解释方式保持一致。
        match serde_json::from_str::<Value>(&body) {
            Ok(mut root) => {
                inject_decoded_lyric(&mut root);
                serde_json::to_string(&root)
                    .map_err(|error| AppError::Other(format!("序列化歌词响应失败：{error}")))
            }
            Err(_) => Ok(body),
        }
    }
}

/// `NativeApi` 的取流实现。
///
/// 算法在 [`resolve_stream_url`]，与 `NodeApi` **共用同一份**；这里只负责把
/// 「问权限」「取直链」两个请求按上游 `module/*.js` 的规则拼出来。
impl StreamSource for NativeApi {
    async fn privilege_lite(&self, song: &Song) -> Result<Value> {
        let endpoint = privilege_lite_endpoint(self.kind(), song)?;
        self.transport.get_json(&endpoint, false).await
    }

    async fn song_url(&self, hash: &str, quality: &str, free_part: bool) -> Result<Value> {
        // 登录态带 `dfid` 就用它；否则按上游生成一次性随机串。
        let dfid = self
            .transport
            .cookie_map()
            .get("dfid")
            .filter(|value| !value.is_empty() && *value != "-")
            .cloned()
            .unwrap_or_else(|| random_string(24, &mut random_f64));
        let endpoint = song_url_endpoint(self.kind(), dfid, hash, quality, free_part);
        self.transport.get_json(&endpoint, false).await
    }
}

impl MusicApi for NativeApi {
    async fn search_songs(&self, keywords: &str, page: u32, page_size: u32) -> Result<Vec<Song>> {
        let endpoint = search_endpoint(keywords, page, page_size);
        let root = self.transport.get_json(&endpoint, false).await?;
        Ok(extract_songs(data_of(&root)))
    }

    async fn song_stream_url(&self, song: &Song, quality: &str) -> Result<StreamUrl> {
        resolve_stream_url(self, song, quality).await
    }

    async fn plaza_playlists(
        &self,
        category_id: i64,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<Playlist>> {
        let cookies = self.transport.cookie_map();
        let clienttime = (crate::util::now_unix_millis() / 1000).to_string();
        let endpoint = plaza_playlists_endpoint(
            self.kind(),
            category_id,
            page,
            page_size,
            &clienttime,
            cookies
                .get("KUGOU_API_MID")
                .map(String::as_str)
                .unwrap_or_default(),
            cookies.get("userid").map(String::as_str),
        )?;
        let root = self.transport.get_json(&endpoint, true).await?;
        Ok(collect_playlists(&root, false))
    }

    async fn playlist_tracks(
        &self,
        global_id: &str,
        page: u32,
        page_size: u32,
        fresh: bool,
    ) -> Result<Vec<Song>> {
        let endpoint = playlist_tracks_endpoint(global_id, page, page_size);
        let root = self.transport.get_json(&endpoint, !fresh).await?;
        Ok(extract_songs(data_of(&root)))
    }

    async fn user_playlists(&self) -> Result<Vec<Playlist>> {
        let cookies = self.transport.cookie_map();
        let endpoint = user_playlists_endpoint(
            1,
            100,
            cookies.get("userid").map(String::as_str),
            cookies.get("token").map(String::as_str),
        )?;
        // `NodeApi` 走 `get_json_uncached`，这里对齐。
        let root = self.transport.get_json(&endpoint, false).await?;
        Ok(collect_playlists(&root, true))
    }

    async fn user_playlist_tracks(
        &self,
        list_id: i64,
        page: u32,
        page_size: u32,
        fresh: bool,
    ) -> Result<Vec<Song>> {
        let cookies = self.transport.cookie_map();
        let endpoint = user_playlist_tracks_endpoint(
            list_id,
            page,
            page_size,
            cookies.get("userid").map(String::as_str),
            cookies.get("token").map(String::as_str),
        )?;
        let root = self.transport.get_json(&endpoint, !fresh).await?;
        Ok(extract_songs(data_of(&root)))
    }

    async fn artist_list(&self, kind: i64, hot_size: u32) -> Result<Vec<Artist>> {
        let endpoint = artist_list_endpoint(kind, hot_size);
        let root = self.transport.get_json(&endpoint, true).await?;
        Ok(collect_artists(&root))
    }

    async fn rank_boards(&self) -> Result<Vec<RankBoard>> {
        let endpoint = rank_boards_endpoint();
        let root = self.transport.get_json(&endpoint, true).await?;
        Ok(extract_list(
            &root,
            &["info", "list", "rank_list"],
            rank_board_from_json,
        ))
    }

    async fn playlist_tracks_all(&self, global_id: &str, fresh: bool) -> Result<Vec<Song>> {
        let client = self.clone();
        let global_id = global_id.to_string();
        collect_all_pages(client, move |client, page| {
            let global_id = global_id.clone();
            async move {
                client
                    .playlist_tracks(&global_id, page, crate::api::catalog::PAGE_LIMIT, fresh)
                    .await
            }
        })
        .await
    }

    async fn user_playlist_tracks_all(&self, list_id: i64, fresh: bool) -> Result<Vec<Song>> {
        let client = self.clone();
        collect_all_pages(client, move |client, page| async move {
            client
                .user_playlist_tracks(list_id, page, crate::api::catalog::PAGE_LIMIT, fresh)
                .await
        })
        .await
    }

    async fn artist_tracks_all(&self, artist_id: i64, sort: &str) -> Result<Vec<Song>> {
        let client = self.clone();
        let sort = sort.to_string();
        collect_all_pages(client, move |client, page| {
            let sort = sort.clone();
            async move {
                client
                    .artist_tracks(artist_id, &sort, page, crate::api::catalog::PAGE_LIMIT)
                    .await
            }
        })
        .await
    }

    async fn rank_tracks_all(&self, rank_id: i64) -> Result<Vec<Song>> {
        let client = self.clone();
        collect_all_pages(client, move |client, page| async move {
            client
                .rank_tracks(rank_id, page, crate::api::catalog::PAGE_LIMIT)
                .await
        })
        .await
    }

    async fn fetch_lyric(&self, song: &Song) -> Result<Lyric> {
        crate::api::lyric::fetch_lyric_via(self, song).await
    }

    async fn login_qr_key(&self) -> Result<String> {
        let endpoint = login_qr_key_endpoint(self.kind());
        let root = self.transport.get_json(&endpoint, false).await?;
        pick_string(data_of(&root), &["qrcode", "key"])
            .ok_or_else(|| AppError::NotFound("`/login/qr/key` 未返回 key".to_string()))
    }

    async fn login_qr_create(&self, key: &str) -> Result<String> {
        Ok(login_qr_content(key))
    }

    async fn login_qr_check(&self, key: &str) -> Result<QrCheck> {
        let endpoint = login_qr_check_endpoint(self.kind(), key);
        let root = self.transport.get_json(&endpoint, false).await?;
        Ok(parse_qr_check(&root))
    }

    async fn user_detail(&self) -> Result<UserInfo> {
        let cookies = self.transport.cookie_map();
        let token = cookies.get("token").cloned().unwrap_or_default();
        let userid = cookies.get("userid").cloned();
        let clienttime = (crate::util::now_unix_millis() / 1000).to_string();
        let endpoint = user_detail_endpoint(self.kind(), &token, userid.as_deref(), &clienttime)?;
        let root = self.transport.get_json(&endpoint, false).await?;
        Ok(parse_user_detail(&root))
    }

    async fn user_vip_detail(&self) -> Result<VipInfo> {
        let endpoint = user_vip_detail_endpoint();
        let root = self.transport.get_json(&endpoint, false).await?;
        Ok(parse_user_vip_detail(&root))
    }

    /// 上游 `module/youth_day_vip.js`（路由 `/youth/day/vip`）。
    ///
    /// 写接口，**不重试**：重发可能重复领取。返回原始响应而不替调用方断言成功——
    /// 「今天已经领过」「账号被风控」不一定给非零 `error_code`。
    async fn claim_day_vip(&self, receive_day: &str) -> Result<Value> {
        let endpoint = claim_day_vip_endpoint(receive_day);
        self.transport.get_json_mutating(&endpoint, false).await
    }

    /// 上游 `module/youth_day_vip_upgrade.js`（路由 `/youth/day/vip/upgrade`）。
    ///
    /// 写接口，不重试。`kugouid` 从 cookie 的 `userid` 取，与上游
    /// `Number(params?.userid || params?.cookie?.userid || 0)` 一致。
    async fn upgrade_day_vip(&self) -> Result<Value> {
        let cookies = self.transport.cookie_map();
        let userid = cookies.get("userid").map(String::as_str);
        let endpoint = upgrade_day_vip_endpoint(userid);
        self.transport.get_json_mutating(&endpoint, false).await
    }

    async fn claimed_vip_days(&self) -> Result<Vec<String>> {
        let endpoint = claimed_vip_days_endpoint();
        // `NodeApi` 这条路走 `get_json_uncached`，这里对齐。
        let root = self.transport.get_json(&endpoint, false).await?;
        let list = data_of(&root)
            .get("list")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();

        Ok(list
            .iter()
            .filter(|entry| pick_i64(entry, &["receive_vip"]) == Some(1))
            .filter_map(|entry| pick_string(entry, &["day"]))
            .collect())
    }

    /// 上游 `module/register_dev.js`（路由 `/register/dev`）。
    ///
    /// 请求体的 AES 密钥由本地随机生成，响应再用同一把密钥解开——上游把
    /// `dfid` 放在解密后的 `data.dfid` 里，同时以 `Set-Cookie` 下发；native
    /// 直接把值返回给调用方（`Loaded::DeviceFingerprint` 会写进配置）。
    async fn fetch_device_fingerprint(&self) -> Result<String> {
        let cookies = self.transport.cookie_map();
        // `cookie_map` 保证 KUGOU_API_GUID 存在（缺时用本地设备标识补），
        // 它同时充当上游的 imei 与 uuid。
        let guid = cookies.get("KUGOU_API_GUID").cloned().unwrap_or_default();
        let token = cookies.get("token").cloned().unwrap_or_default();
        let userid = cookies.get("userid").cloned();

        // 上游 `randomString(6).toLowerCase()` 与 `forge.random.getBytes`：
        // 保持可注入的分层——这里只在生产路径上消费随机数。
        let aes_key = random_string(6, &mut random_f64).to_lowercase();
        let rsa_fill = crate::api::native::crypto::random_fill();

        let endpoint = register_dev_endpoint(
            self.kind(),
            &guid,
            userid.as_deref(),
            &token,
            &aes_key,
            &rsa_fill,
        )?;
        let (_status, body) = self.transport.post_bytes(&endpoint).await?;
        let root = parse_register_dev_response(&aes_key, &body)?;

        let data = data_of(&root);
        crate::api::model::pick_string(data, &["dfid", "DFID"])
            .or_else(|| crate::api::model::pick_string(&root, &["dfid"]))
            .ok_or_else(|| AppError::NotFound("`/register/dev` 未返回 dfid".to_string()))
    }

    /// 上游 `module/playlist_tracks_add.js`（路由 `/playlist/tracks/add`）。
    ///
    /// 分批大小与 `NodeApi` 一致（服务端按逗号分隔多首，单次太多会被截断），
    /// 逐批提交、逐批校验，任一批失败即返回错误（写接口不重试）。
    async fn add_tracks_to_playlist(
        &self,
        _source: SourceKind,
        list_id: i64,
        songs: &[Song],
    ) -> Result<usize> {
        if songs.is_empty() {
            return Ok(0);
        }

        let cookies = self.transport.cookie_map();
        let userid = cookies.get("userid").map(String::as_str);
        let token = cookies.get("token").map(String::as_str);
        let mut written = 0usize;

        for chunk in songs.chunks(WRITE_BATCH_SIZE) {
            let payload = chunk
                .iter()
                .map(crate::api::cloud::encode_track_entry)
                .collect::<Vec<_>>()
                .join(",");
            let clienttime = (crate::util::now_unix_millis() / 1000).to_string();

            let endpoint =
                playlist_tracks_add_endpoint(list_id, &payload, userid, token, &clienttime)?;
            let root = self.transport.get_json_mutating(&endpoint, false).await?;
            crate::api::node::NodeApi::check_write_result("/playlist/tracks/add", &root)?;

            written += chunk.len();
        }

        Ok(written)
    }

    /// 上游 `module/playlist_tracks_del.js`（路由 `/playlist/tracks/del`）。
    ///
    /// 只处理有 `file_id` 的歌——`fileid` 是歌单条目的标识，搜索结果里的歌没有它，
    /// 传 hash 会静默删不掉。与 `NodeApi` 一致：没有可删的条目时返回 `0`。
    async fn remove_tracks_from_playlist(
        &self,
        _source: SourceKind,
        list_id: i64,
        songs: &[Song],
    ) -> Result<usize> {
        let file_ids: Vec<i64> = songs.iter().filter_map(|song| song.file_id).collect();
        if file_ids.is_empty() {
            return Ok(0);
        }

        let cookies = self.transport.cookie_map();
        let endpoint = playlist_tracks_del_endpoint(
            list_id,
            &file_ids,
            cookies.get("userid").map(String::as_str),
            cookies.get("token").map(String::as_str),
        )?;
        let root = self.transport.get_json_mutating(&endpoint, false).await?;
        crate::api::node::NodeApi::check_write_result("/playlist/tracks/del", &root)?;

        Ok(file_ids.len())
    }

    /// 上游 `module/playlist_del.js`（路由 `/playlist/del`）。
    ///
    /// 与 `NodeApi` 一致：**不校验 `error_code`**（这个接口成功时根本不返回它），
    /// 但解密失败必须报错——否则「删掉了」会是假的。
    async fn delete_playlist(&self, _source: SourceKind, list_id: i64) -> Result<()> {
        let cookies = self.transport.cookie_map();
        let clienttime = (crate::util::now_unix_millis() / 1000).to_string();
        let aes_key = random_string(6, &mut random_f64).to_lowercase();
        let rsa_fill = crate::api::native::crypto::random_fill();

        let endpoint = playlist_del_endpoint(
            self.kind(),
            list_id,
            cookies.get("userid").map(String::as_str),
            cookies.get("token").map(String::as_str),
            &clienttime,
            &aes_key,
            &rsa_fill,
        )?;
        let (_status, body) = self.transport.post_bytes(&endpoint).await?;

        // 上游把响应体当 arraybuffer 收，再 `toString('base64')` 后走
        // `playlistAesDecrypt`；这里拿到的已是原始字节，直接解密即可。
        //
        // 这个接口**成功时也不返回 `error_code`**，所以「解密成功」就是唯一的成功
        // 判据；把明文落一行 DEBUG，是为了删错东西时能看出服务端到底说了什么。
        let (encrypt_key, iv) = crate::api::native::crypto::playlist_key_material(&aes_key);
        let plain = crate::api::native::crypto::aes_cbc_decrypt(&encrypt_key, &iv, &body)?;
        crate::logger::tlog!(
            crate::logger::LEVEL_DEBUG,
            "playlist/del 响应 {} 字节，解密后 {} 字节：{}",
            body.len(),
            plain.len(),
            String::from_utf8_lossy(&plain)
        );
        Ok(())
    }

    /// 上游 `module/playlist_add.js`（路由 `/playlist/add`）。
    async fn create_playlist(&self, _source: SourceKind, name: &str) -> Result<Option<i64>> {
        let cookies = self.transport.cookie_map();
        let endpoint = playlist_add_endpoint(
            name,
            cookies.get("userid").map(String::as_str),
            cookies.get("token").map(String::as_str),
        )?;
        let root = self.transport.get_json_mutating(&endpoint, false).await?;
        crate::api::node::NodeApi::check_write_result("/playlist/add", &root)?;

        Ok(crate::api::cloud::created_listid(&root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::native::transport::{Method, Prepared};
    use std::collections::BTreeMap;

    /// 与 `transport` 的 KAT 用同一组固定输入，方便两边互相对照。
    const KAT_CLIENTTIME: &str = "1700000000";
    const KAT_MID: &str = "231699103997194646178265604655475531917";
    const KAT_DFID: &str = "1234567890abcdef12345678";

    fn kat_cookie(dfid: &str) -> BTreeMap<String, String> {
        [
            ("dfid", dfid),
            ("KUGOU_API_MID", KAT_MID),
            ("token", "TOKENFIXTURE"),
            ("userid", "10001"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
    }

    fn prepared_url(kind: SourceKind, endpoint: &Endpoint<'_>) -> String {
        prepared(kind, endpoint).url
    }

    fn prepared(kind: SourceKind, endpoint: &Endpoint<'_>) -> Prepared {
        crate::api::native::transport::build_prepared(
            kind,
            &kat_cookie(KAT_DFID),
            KAT_CLIENTTIME,
            endpoint,
        )
    }

    fn prepared_body(kind: SourceKind, endpoint: &Endpoint<'_>) -> Option<String> {
        prepared(kind, endpoint).body
    }

    /// 头集合，按名字排序。
    ///
    /// 头的顺序不参与签名、也不影响请求语义（同名头之外 HTTP 不规定顺序），
    /// 上游 KAT 记的本来就是个字典，所以这里排序后比对集合本身。
    fn prepared_headers(kind: SourceKind, endpoint: &Endpoint<'_>) -> Vec<(String, String)> {
        let mut headers = prepared(kind, endpoint).headers;
        headers.sort();
        headers
    }

    /// URL 里参数的**出现顺序**。上游签名先把参数按 key 排序再拼串，但 URL 本身
    /// 的顺序来自 `module/*.js` 的对象插入序，两端不同说明默认参数与模块参数的
    /// 合并（`Object.assign` 的「改值不改位」）走偏了。
    fn prepared_param_order(kind: SourceKind, endpoint: &Endpoint<'_>) -> Vec<String> {
        let url = prepared_url(kind, endpoint);
        let query = url.split_once('?').map(|(_, query)| query).unwrap_or("");
        query
            .split('&')
            .filter_map(|pair| pair.split_once('=').map(|(name, _)| name.to_string()))
            .collect()
    }

    /// 两套平台的盐值/appid 不同，构造时就必须带上平台。
    #[test]
    fn keeps_its_platform() {
        let standard = NativeApi::new(SourceKind::Kugou, None, None).unwrap();
        let lite = NativeApi::new(SourceKind::KugouConcept, None, None).unwrap();
        assert!(standard.base().contains("酷狗"));
        assert!(lite.base().contains("概念版"));
    }

    /// 构造 native 客户端**不碰文件系统、不发网络请求**——启动路径与测试都会构造它。
    #[test]
    fn construction_touches_nothing() {
        let api = NativeApi::new(SourceKind::Kugou, Some("dfid=x".to_string()), None).unwrap();
        assert_eq!(api.cookie(), Some("dfid=x"));
        assert_eq!(api.kind(), SourceKind::Kugou);
    }

    /// `set_cookie` 要能改到传输层，否则登录后仍是匿名身份。
    #[test]
    fn set_cookie_reaches_the_transport() {
        let mut api = NativeApi::new(SourceKind::Kugou, None, None).unwrap();
        assert_eq!(api.cookie(), None);
        api.set_cookie(Some("token=T; userid=1".to_string()));
        assert_eq!(api.cookie(), Some("token=T; userid=1"));
    }

    /// 阶段 3 出口：native 自己拼的搜索 URL 与上游逐字节一致。
    #[test]
    fn search_endpoint_matches_kat() {
        assert_eq!(
            prepared_url(SourceKind::Kugou, &search_endpoint("周杰伦", 1, 30)),
            "https://gateway.kugou.com/v3/search/song?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&albumhide=0&iscorrection=1&keyword=%E5%91%A8%E6%9D%B0%E4%BC%A6&nocollect=0&page=1&pagesize=30&platform=AndroidFilter&signature=9bb2d7192e4ec15c72add9daaf9728a6"
        );
    }

    /// 概念版只有 `appid`/`clientver` 与签名变。
    #[test]
    fn lite_search_endpoint_matches_kat() {
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &search_endpoint("周杰伦", 1, 30)),
            "https://gateway.kugou.com/v3/search/song?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&albumhide=0&iscorrection=1&keyword=%E5%91%A8%E6%9D%B0%E4%BC%A6&nocollect=0&page=1&pagesize=30&platform=AndroidFilter&signature=bca333395e727512b680fba6df0e4b82"
        );
    }

    /// 阶段 3 出口：取链 URL 与上游逐字节一致（含 `encryptKey` 的 `key`）。
    #[test]
    fn song_url_endpoint_matches_kat() {
        assert_eq!(
            prepared_url(
                SourceKind::Kugou,
                &song_url_endpoint(
                    SourceKind::Kugou,
                    KAT_DFID.to_string(),
                    "6af00fbd4d444a82c005843eef9dc2d4",
                    "128",
                    false
                ),
            ),
            "https://gateway.kugou.com/v5/url?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=11430&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&album_id=0&area_code=1&hash=6af00fbd4d444a82c005843eef9dc2d4&ssa_flag=is_fromtrack&version=11430&page_id=151369488&quality=128&album_audio_id=0&behavior=play&pid=2&cmd=26&pidversion=3001&IsFreePart=0&ppage_id=463467626,350369493,788954147&cdnBackup=1&module=&key=1e5533fbcad17c9aa8935349a8b7c1d3&signature=8128fd5afa89324ac65a4c7b77b270e9"
        );
    }

    /// 概念版的 `page_id`/`pid`/`ppage_id` 三个值都不同。
    #[test]
    fn lite_song_url_endpoint_matches_kat() {
        assert_eq!(
            prepared_url(
                SourceKind::KugouConcept,
                &song_url_endpoint(
                    SourceKind::KugouConcept,
                    KAT_DFID.to_string(),
                    "6af00fbd4d444a82c005843eef9dc2d4",
                    "128",
                    false
                ),
            ),
            "https://gateway.kugou.com/v5/url?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11430&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&album_id=0&area_code=1&hash=6af00fbd4d444a82c005843eef9dc2d4&ssa_flag=is_fromtrack&version=11430&page_id=967177915&quality=128&album_audio_id=0&behavior=play&pid=411&cmd=26&pidversion=3001&IsFreePart=0&ppage_id=356753938&cdnBackup=1&module=&key=7d03ca1bba0d0fa1f5c8d55cdd957b8d&signature=64a711097ebb42883b47892ecec00214"
        );
    }

    /// `free_part` 只改 `IsFreePart`（`1`/`0`），签名随之变化。
    #[test]
    fn free_part_flips_is_free_part() {
        let full = prepared_url(
            SourceKind::Kugou,
            &song_url_endpoint(
                SourceKind::Kugou,
                KAT_DFID.to_string(),
                "6af00fbd4d444a82c005843eef9dc2d4",
                "128",
                false,
            ),
        );
        let trial = prepared_url(
            SourceKind::Kugou,
            &song_url_endpoint(
                SourceKind::Kugou,
                KAT_DFID.to_string(),
                "6af00fbd4d444a82c005843eef9dc2d4",
                "128",
                true,
            ),
        );
        assert!(full.contains("&IsFreePart=0&"), "{full}");
        assert!(trial.contains("&IsFreePart=1&"), "{trial}");
        assert_ne!(full, trial);
    }

    /// 空 `quality` 落到上游的 `quality || 128`。
    #[test]
    fn empty_quality_falls_back_to_128() {
        let url = prepared_url(
            SourceKind::Kugou,
            &song_url_endpoint(
                SourceKind::Kugou,
                KAT_DFID.to_string(),
                "6AF00FBD4D444A82C005843EEF9DC2D4",
                "",
                false,
            ),
        );
        assert!(url.contains("&quality=128&"), "{url}");
        // 上游 `(params?.hash || '').toLowerCase()`：大写 hash 必须被压成小写，
        // 否则服务端查不到文件。
        assert!(
            url.contains("&hash=6af00fbd4d444a82c005843eef9dc2d4&"),
            "{url}"
        );
    }

    /// 阶段 3 出口：`/privilege/lite` 的 POST body 与上游逐字节一致。
    ///
    /// 这里锁的是 `resource[0].album_id` 为**空字符串**、`resource[1].album_id`
    /// 为**数字 0**——它们进 android 签名的输入，写错不会报错，只会 403。
    #[test]
    fn privilege_lite_body_matches_kat() {
        let song = Song {
            name: "测试".to_string(),
            hash: "6af00fbd4d444a82c005843eef9dc2d4".to_string(),
            extra_hashes: [(
                "hash_320".to_string(),
                "11111111111111111111111111111111".to_string(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let endpoint = privilege_lite_endpoint(SourceKind::Kugou, &song).unwrap();
        assert_eq!(
            endpoint.data.as_deref(),
            Some(
                r#"{"appid":1005,"area_code":1,"behavior":"play","clientver":20489,"need_hash_offset":1,"relate":1,"support_verify":1,"resource":[{"type":"audio","page_id":0,"hash":"6af00fbd4d444a82c005843eef9dc2d4","album_id":""},{"type":"audio","page_id":0,"hash":"11111111111111111111111111111111","album_id":0}],"qualities":["128","320","flac","high","viper_atmos","viper_tape","viper_clear","super","multitrack"]}"#
            )
        );
        assert_eq!(endpoint.method, crate::api::native::transport::Method::Post);
        assert_eq!(
            prepared_url(SourceKind::Kugou, &endpoint),
            "https://gateway.kugou.com/v2/get_res_privilege/lite?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=d21eec343b1f4dce059f1127625bbf5e"
        );
    }

    /// 概念版的 `/privilege/lite` body 里 `appid`/`clientver` 换成概念版的值。
    #[test]
    fn lite_privilege_lite_body_matches_kat() {
        let song = Song {
            name: "测试".to_string(),
            hash: "6af00fbd4d444a82c005843eef9dc2d4".to_string(),
            ..Default::default()
        };
        let endpoint = privilege_lite_endpoint(SourceKind::KugouConcept, &song).unwrap();
        let body = endpoint.data.as_deref().unwrap();
        assert!(body.contains(r#""appid":3116"#), "{body}");
        assert!(body.contains(r#""clientver":11440"#), "{body}");
        // 只有一个 hash 时 `resource` 只有一项，且 `album_id` 是空字符串。
        assert_eq!(body.matches("\"type\":\"audio\"").count(), 1, "{body}");
        assert!(body.contains(r#""album_id":"""#), "{body}");
    }

    /// `extra_hashes` 里的重复项要去重：上游 `Object.assign` 与我们的
    /// `HashSet` 都保证同一个 hash 不会进两次。
    #[test]
    fn duplicate_hashes_are_asked_once() {
        let song = Song {
            name: "测试".to_string(),
            hash: "6af00fbd4d444a82c005843eef9dc2d4".to_string(),
            extra_hashes: [
                (
                    "hash_320".to_string(),
                    "6af00fbd4d444a82c005843eef9dc2d4".to_string(),
                ),
                (
                    "hash_128".to_string(),
                    "22222222222222222222222222222222".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let endpoint = privilege_lite_endpoint(SourceKind::Kugou, &song).unwrap();
        let body = endpoint.data.as_deref().unwrap();
        assert_eq!(body.matches("\"type\":\"audio\"").count(), 2, "{body}");
        assert!(!body.contains("11111111111111111111111111111111"));
    }

    /// 未登录时 `song_url` 用的 dfid 是 24 字符随机串，不是 `-`。
    #[test]
    fn anonymous_dfid_is_a_fresh_24_char_string() {
        let api = NativeApi::new(SourceKind::Kugou, None, None).unwrap();
        assert!(!api.transport.cookie_map().contains_key("dfid"));
        let first = random_string(24, &mut random_f64);
        let second = random_string(24, &mut random_f64);
        assert_eq!(first.len(), 24);
        assert_eq!(second.len(), 24);
        assert_ne!(first, second, "两次调用不该拿到同一个 dfid");
    }

    /// 阶段 4 出口：歌词搜索 URL 与上游逐字节一致。
    ///
    /// 这个端点带 `clearDefaultParams`，所以 URL 里**没有** `dfid`/`mid`/`uuid`/
    /// `clienttime`/`token`/`userid`——`appid`/`clientver` 是模块自己写进 dataMap
    /// 的。少了这个 `clear_defaults()` 会多出六个参数，签名随之全错。
    #[test]
    fn search_lyric_endpoint_matches_kat() {
        assert_eq!(
            prepared_url(
                SourceKind::Kugou,
                &search_lyric_endpoint(SourceKind::Kugou, &kat_song())
            ),
            "https://lyrics.kugou.com/v1/search?album_audio_id=0&appid=1005&clientver=20489&duration=243722&hash=6af00fbd4d444a82c005843eef9dc2d4&keyword=Letter+-+arkady+sevidov&lrctxt=1&man=yes&signature=b90333d489a1aae225eb18ac61718d35"
        );
    }

    /// 概念版歌词搜索：只有 `appid`/`clientver` 与签名变。
    #[test]
    fn lite_search_lyric_endpoint_matches_kat() {
        assert_eq!(
            prepared_url(
                SourceKind::KugouConcept,
                &search_lyric_endpoint(SourceKind::KugouConcept, &kat_song())
            ),
            "https://lyrics.kugou.com/v1/search?album_audio_id=0&appid=3116&clientver=11440&duration=243722&hash=6af00fbd4d444a82c005843eef9dc2d4&keyword=Letter+-+arkady+sevidov&lrctxt=1&man=yes&signature=9f42c6d69256fa6e952cf44c547d0cbf"
        );
    }

    /// 阶段 4 出口：歌词下载 URL 与上游逐字节一致（含登录态的 `token`/`userid`）。
    #[test]
    fn lyric_endpoint_matches_kat() {
        let endpoint = lyric_endpoint("19525574", "0123456789ABCDEF0123456789ABCDEF");
        assert_eq!(
            prepared_url(SourceKind::Kugou, &endpoint),
            "https://lyrics.kugou.com/download?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&ver=1&client=android&id=19525574&accesskey=0123456789ABCDEF0123456789ABCDEF&fmt=krc&charset=utf8&signature=baf87af98752ef7562e9e4756f56cf32"
        );
    }

    /// 概念版歌词下载：`appid`/`clientver`/签名变，参数序不变。
    #[test]
    fn lite_lyric_endpoint_matches_kat() {
        let endpoint = lyric_endpoint("19525574", "0123456789ABCDEF0123456789ABCDEF");
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &endpoint),
            "https://lyrics.kugou.com/download?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&ver=1&client=android&id=19525574&accesskey=0123456789ABCDEF0123456789ABCDEF&fmt=krc&charset=utf8&signature=d3eeb438cbbe61bfd53bcf71999376c5"
        );
    }

    /// `contenttype == 0` 走 KRC 解密，解出的明文与上游 `decodeLyrics` 逐字相同。
    ///
    /// 样本来自真实 `/lyric` 响应（`tools/kat/krc_probe.json`），已脱敏为
    /// 「纯音乐，请欣赏」这一句。
    #[test]
    fn injects_krc_decoded_content_when_contenttype_is_zero() {
        let probe: Value = serde_json::from_str(include_str!("../../../tools/kat/krc_probe.json"))
            .expect("krc_probe.json 应当能解析");
        let mut root = probe.clone();
        inject_decoded_lyric(&mut root);

        let decoded = root["decodeContent"].as_str().unwrap();
        assert_eq!(decoded, probe["decodeContent"].as_str().unwrap());
        assert!(
            decoded.contains("<0,354,0>纯"),
            "逐字时间戳要保留：{decoded}"
        );
    }

    /// `contenttype != 0` 走 base64 而不是 KRC——少了这个分支会解出乱码。
    #[test]
    fn injects_base64_content_when_contenttype_is_not_zero() {
        let mut root = serde_json::json!({
            "status": 200,
            "contenttype": 1,
            "content": "aGVsbG8gd29ybGQ=",
        });
        inject_decoded_lyric(&mut root);
        assert_eq!(
            root["decodeContent"],
            Value::String("hello world".to_string())
        );
    }

    /// 解不开的 KRC 折成**空串**而不是报错：上游 `decodeLyrics` 也是
    /// `catch { return '' }`，两端最终都走到「歌词为空」。
    #[test]
    fn undecodable_krc_becomes_an_empty_string() {
        let mut root = serde_json::json!({
            "status": 200,
            "contenttype": 0,
            "content": "bm90LWEta3JjLXBheWxvYWQ=",
        });
        inject_decoded_lyric(&mut root);
        assert_eq!(root["decodeContent"], Value::String(String::new()));
    }

    /// 非 JSON 响应（纯文本歌词）不做任何注入——`lyric_body` 会原样透传。
    #[test]
    fn plain_text_body_is_passed_through() {
        // `inject_decoded_lyric` 只处理对象；字符串根节点不该 panic。
        let mut root = Value::String("[00:01.00]hi".to_string());
        inject_decoded_lyric(&mut root);
        assert_eq!(root, Value::String("[00:01.00]hi".to_string()));
    }

    /// 真实整首歌的 KAT：native 解出的明文必须与 Node 版 `decode=true` 的
    /// `decodeContent` **逐字节相同**，解析出的逐字时间戳也必须一致。
    ///
    /// 样本是真实 `/lyric` 响应（`tools/kat/krc_real_qt.json`，周杰伦《晴天》，
    /// 68 行、其中 63 行带时间标签）。`decodeContent` 那一栏是**上游 Node 版自己
    /// 解出来的**，不是本地推导值——所以这条断言同时锁住了「解密算法一致」和
    /// 「逐字时间戳一致」两件事。
    ///
    /// 解密正确但偏移错、或丢了一个字，都会让下面 `故事的小黄花` 的时间戳对不上。
    #[test]
    fn real_song_decode_matches_node_byte_for_byte() {
        let probe: Value =
            serde_json::from_str(include_str!("../../../tools/kat/krc_real_qt.json"))
                .expect("krc_real_qt.json 应当能解析");
        let expected = probe["decodeContent"].as_str().unwrap();

        let mut root = probe.clone();
        inject_decoded_lyric(&mut root);
        let decoded = root["decodeContent"].as_str().unwrap();
        assert_eq!(decoded, expected, "native 解密结果与 Node 版不一致");

        let lyric = crate::api::lyric::parse_lrc(decoded);
        assert_eq!(lyric.lines.len(), 63, "定时行数");

        let line = lyric
            .lines
            .iter()
            .find(|l| l.text == "故事的小黄花")
            .expect("应当有「故事的小黄花」这一行");
        assert_eq!(line.time_ms, 29264);
        assert_eq!(line.words.len(), 6, "六个字各有一个逐字区间");
        let starts: Vec<u64> = line.words.iter().map(|w| w.start_ms).collect();
        assert_eq!(starts, vec![29264, 29654, 30046, 30494, 31416, 31790]);
        let ends: Vec<u64> = line.words.iter().map(|w| w.end_ms).collect();
        assert_eq!(ends, vec![29654, 30046, 30494, 31416, 31790, 32294]);

        // 首行是 `[0,2250]<0,160,0>晴天…`，19 个字全部带逐字区间。
        let first = &lyric.lines[0];
        assert_eq!(first.time_ms, 0);
        assert_eq!(first.text, "晴天 - 周杰伦 (Jay Chou)");
        assert_eq!(first.words.len(), 19);
        assert_eq!(first.words[0].start_ms, 0);
        assert_eq!(first.words[0].end_ms, 160);
    }

    /// 与上游 KAT 同一首歌：`singer_text() - name` 要拼出 `Letter - arkady sevidov`。
    fn kat_song() -> Song {
        Song {
            name: "arkady sevidov".to_string(),
            hash: "6af00fbd4d444a82c005843eef9dc2d4".to_string(),
            duration_ms: 243722,
            singers: vec![crate::api::model::Singer {
                name: "Letter".to_string(),
                id: 0,
            }],
            ..Default::default()
        }
    }

    /// 阶段 5a：`/register/dev` 的固定输入，全部取自 `tools/kat/kat_aes.js`
    /// 的上游实跑基准（`/tmp/kat_aes_out.json` 的 `registerDev` 段）。
    const KAT_GUID: &str = "5f2b1c3d4e5f60718293a4b5c6d7e8f9";
    const KAT_AES_KEY: &str = "15iw0r";
    const KAT_RSA_FILL: [u8; 16] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        0x10,
    ];
    const KAT_REGISTER_P_STANDARD: &str = "02c6bdaa766f54607c2fdf52fb9292b948f849361a00c7bb88cde84bdfe25028d8daba3dde2f7c1eae3691158d89d54c00649bf8efe397a5d4e87f4a45221ea49b61e9f71ea27da879e200ba36b187f47f5fe5b9305e4a9c33cd60194fa0c14042921bfeaa96c87bf662152226f95ea7a7269d1114251a30aa0783a9458763a3";
    const KAT_REGISTER_P_LITE: &str = "6e0ec9a555b6555ca3c461c1ab2e00944bb773cc185bfecc3e136eb1094fbd8bb16269f5ad04ab3efd42357031ae49ef536480ab8366d2d23a1e9aed9933caa377a14ca4688a422e7c7fef9bc76fb9d94239e5f5618c8339eba1d2378c6fc8c4cf2661917e6dbaff854cc26f4dae16eda505eceada5dd6b93bc64c8ba6e89fa1";
    /// 上游 31 键明文，插入序即 JSON 键序。
    const KAT_REGISTER_PLAIN: &str = r#"{"availableRamSize":4983533568,"availableRomSize":48114719,"availableSDSize":48114717,"basebandVer":"","batteryLevel":100,"batteryStatus":3,"brand":"Redmi","buildSerial":"unknown","device":"marble","imei":"5f2b1c3d4e5f60718293a4b5c6d7e8f9","imsi":"","manufacturer":"Xiaomi","uuid":"5f2b1c3d4e5f60718293a4b5c6d7e8f9","accelerometer":false,"accelerometerValue":"","gravity":false,"gravityValue":"","gyroscope":false,"gyroscopeValue":"","light":false,"lightValue":"","magnetic":false,"magneticValue":"","orientation":false,"orientationValue":"","pressure":false,"pressureValue":"","step_counter":false,"step_counterValue":"","temperature":false,"temperatureValue":""}"#;

    /// `register_dev_data_map` 的键序与值必须与上游 `dataMap` 逐字节一致。
    ///
    /// 这条是「31 键一个不多一个不少、顺序不错」的护栏：`JSON.stringify` 的
    /// 键序就是插入序，而这段 JSON 是要被 AES 加密并签名的。
    #[test]
    fn register_dev_data_map_matches_upstream() {
        assert_eq!(
            register_dev_data_map(KAT_GUID).to_string(),
            KAT_REGISTER_PLAIN
        );
        assert_eq!(
            register_dev_data_map(KAT_GUID).as_object().unwrap().len(),
            31
        );
    }

    /// 阶段 5a 出口：`/register/dev` 的出站 URL 与上游逐字节一致（标准版）。
    #[test]
    fn register_dev_endpoint_matches_kat() {
        let endpoint = register_dev_endpoint(
            SourceKind::Kugou,
            KAT_GUID,
            Some("10001"),
            "TOKENFIXTURE",
            KAT_AES_KEY,
            &KAT_RSA_FILL,
        )
        .unwrap();
        assert_eq!(
            prepared_url(SourceKind::Kugou, &endpoint),
            format!(
                "https://userservice.kugou.com/risk/v2/r_register_dev?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&part=1&platid=1&p={KAT_REGISTER_P_STANDARD}&signature=9560aafc5263cea5b6c4133dd5180016"
            )
        );
    }

    /// 概念版换 `appid`/`clientver`/公钥/盐值，`p` 与签名随之全变。
    #[test]
    fn lite_register_dev_endpoint_matches_kat() {
        let endpoint = register_dev_endpoint(
            SourceKind::KugouConcept,
            KAT_GUID,
            Some("10001"),
            "TOKENFIXTURE",
            KAT_AES_KEY,
            &KAT_RSA_FILL,
        )
        .unwrap();
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &endpoint),
            format!(
                "https://userservice.kugou.com/risk/v2/r_register_dev?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&part=1&platid=1&p={KAT_REGISTER_P_LITE}&signature=20acedab6f254572eff1b5151f4cd0a1"
            )
        );
    }

    /// 请求体是 AES 密文，两平台相同（AES 不依赖平台），长度固定 896 字符。
    ///
    /// 它同时参与 android 签名，所以长度错一块（PKCS#7 少补一整块）会让
    /// 上面的签名断言一起失败——这里单独锁一次，失败时能一眼看出是哪一步。
    #[test]
    fn register_dev_body_is_the_kat_ciphertext() {
        let endpoint = register_dev_endpoint(
            SourceKind::Kugou,
            KAT_GUID,
            Some("10001"),
            "TOKENFIXTURE",
            KAT_AES_KEY,
            &KAT_RSA_FILL,
        )
        .unwrap();
        let body = endpoint.data.clone().unwrap();
        assert_eq!(body.len(), 896);
        // 密文的 md5 也写死，避免「长度对但内容错」这种情况漏过。
        assert_eq!(
            crate::api::native::crypto::md5_hex(body.as_bytes()),
            "21504e4c49cc44d354e71c036640784e"
        );
    }

    /// `uid` 缺省时是**数字** 0，不是字符串 `"0"`——两者进 RSA 明文后不同，
    /// 服务端不认也不报错。上游 `params?.userid || params?.cookie?.userid || 0`。
    #[test]
    fn register_dev_uid_defaults_to_a_number() {
        let with_user = register_dev_endpoint(
            SourceKind::Kugou,
            KAT_GUID,
            Some("10001"),
            "TOKENFIXTURE",
            KAT_AES_KEY,
            &KAT_RSA_FILL,
        )
        .unwrap();
        let without = register_dev_endpoint(
            SourceKind::Kugou,
            KAT_GUID,
            None,
            "TOKENFIXTURE",
            KAT_AES_KEY,
            &KAT_RSA_FILL,
        )
        .unwrap();
        // 未登录时 `p` 与登录时不同（RSA 明文里的 uid 变了）。
        assert_ne!(with_user.params[2].1, without.params[2].1);
        assert_eq!(with_user.params[0], ("part".to_string(), "1".to_string()));
        assert_eq!(with_user.params[1], ("platid".to_string(), "1".to_string()));
    }

    /// `arraybuffer` 响应解密：上游基准里 `key="1jx5zx"` 的密文解出
    /// `{"status":1,"data":{"dfid":"DFIDFIXTURE0123456789ab"}}`。
    #[test]
    fn register_dev_response_decrypts_matches_kat() {
        let cipher = crate::api::native::crypto::base64_decode(
            "02H1lHOQIzwMrj05HOAgLvLiPkqf9yl7uV+kZlcfSDuaw7BimABc+k0W8KH/NWgYcBAQu8TiWtQYtmsE6ZXj7g==",
        )
        .unwrap();
        let root = parse_register_dev_response("1jx5zx", &cipher).unwrap();
        assert_eq!(root["status"], Value::from(1));
        assert_eq!(root["data"]["dfid"], Value::from("DFIDFIXTURE0123456789ab"));
        assert_eq!(
            crate::api::model::pick_string(data_of(&root), &["dfid"]).as_deref(),
            Some("DFIDFIXTURE0123456789ab")
        );
    }

    /// 解不开的密文不能 panic，只能报错（上游 `playlistAesDecrypt` 会给出垃圾）。
    #[test]
    fn register_dev_response_rejects_unaligned_ciphertext() {
        assert!(parse_register_dev_response("15iw0r", b"not-a-block").is_err());
    }

    // ---- 阶段 5b：登录与用户信息 ----

    const KAT_QR_KEY: &str = "QRKEYFIXTURE0123456789abcdef";

    /// `/login/qr/key`：查询串里的 `appid` 固定 1001，平台 appid 只出现在
    /// `qrcode_txt` 里。
    #[test]
    fn login_qr_key_endpoint_matches_kat() {
        let standard = prepared_url(SourceKind::Kugou, &login_qr_key_endpoint(SourceKind::Kugou));
        assert!(
            standard.starts_with("https://login-user.kugou.com/v2/qrcode?"),
            "{standard}"
        );
        assert!(standard.contains("appid=1001&"), "{standard}");
        assert!(standard.contains("srcappid=2919&"), "{standard}");
        assert!(
            standard.contains("qrcode_txt=https:%2F%2Fh5.kugou.com%2Fapps%2FloginQRCode%2Fhtml%2Findex.html%3Fappid%3D1005%26"),
            "{standard}"
        );
        assert!(
            standard.ends_with("signature=809add981f2890a0ba0f768e5ad3dc2e"),
            "{standard}"
        );

        let lite = prepared_url(
            SourceKind::KugouConcept,
            &login_qr_key_endpoint(SourceKind::KugouConcept),
        );
        assert!(lite.contains("appid=1001&"), "{lite}");
        assert!(lite.contains("%3Fappid%3D3116%26"), "{lite}");
        assert!(
            lite.ends_with("signature=30d693c55315a86f337c9d1b21fb0384"),
            "{lite}"
        );
    }

    /// `/login/qr/check`：这里才是平台 appid（与上一个接口的 1001 不同）。
    #[test]
    fn login_qr_check_endpoint_matches_kat() {
        let standard = prepared_url(
            SourceKind::Kugou,
            &login_qr_check_endpoint(SourceKind::Kugou, KAT_QR_KEY),
        );
        assert!(
            standard.starts_with("https://login-user.kugou.com/v2/get_userinfo_qrcode?"),
            "{standard}"
        );
        assert!(standard.contains("plat=4&"), "{standard}");
        assert!(standard.contains("appid=1005&"), "{standard}");
        assert!(standard.contains("srcappid=2919&"), "{standard}");
        assert!(
            standard.contains(&format!("qrcode={KAT_QR_KEY}&")),
            "{standard}"
        );
        assert!(
            standard.ends_with("signature=e22b586d8f1406c8d2f8db8cd1fffb99"),
            "{standard}"
        );

        let lite = prepared_url(
            SourceKind::KugouConcept,
            &login_qr_check_endpoint(SourceKind::KugouConcept, KAT_QR_KEY),
        );
        assert!(lite.contains("appid=3116&"), "{lite}");
        assert!(
            lite.ends_with("signature=485f49b591d41e2a85660bfd66e10b4e"),
            "{lite}"
        );
    }

    /// 参数顺序也进签名，必须逐位对齐上游。
    #[test]
    fn login_endpoints_keep_kat_param_order() {
        let qr_key = prepared_url(SourceKind::Kugou, &login_qr_key_endpoint(SourceKind::Kugou));
        let query = qr_key.split_once('?').unwrap().1;
        let keys: Vec<&str> = query
            .split('&')
            .map(|pair| pair.split('=').next().unwrap())
            .collect();
        assert_eq!(
            keys,
            vec![
                "dfid",
                "mid",
                "uuid",
                "appid",
                "clientver",
                "clienttime",
                "token",
                "userid",
                "type",
                "plat",
                "qrcode_txt",
                "srcappid",
                "signature"
            ]
        );

        let qr_check = prepared_url(
            SourceKind::Kugou,
            &login_qr_check_endpoint(SourceKind::Kugou, KAT_QR_KEY),
        );
        let query = qr_check.split_once('?').unwrap().1;
        let keys: Vec<&str> = query
            .split('&')
            .map(|pair| pair.split('=').next().unwrap())
            .collect();
        assert_eq!(
            keys,
            vec![
                "dfid",
                "mid",
                "uuid",
                "appid",
                "clientver",
                "clienttime",
                "token",
                "userid",
                "plat",
                "srcappid",
                "qrcode",
                "signature"
            ]
        );
    }

    /// `/user/detail`：POST 到默认网关，`p` 是裸 RSA 后**大写**的 hex。
    #[test]
    fn user_detail_endpoint_matches_kat() {
        let standard = user_detail_endpoint(
            SourceKind::Kugou,
            "TOKENFIXTURE",
            Some("10001"),
            KAT_CLIENTTIME,
        )
        .unwrap();
        let url = prepared_url(SourceKind::Kugou, &standard);
        assert!(
            url.starts_with("https://gateway.kugou.com/v3/get_my_info?"),
            "{url}"
        );
        assert!(url.contains("plat=1&"), "{url}");
        assert!(
            url.ends_with("signature=fabdd1361171f08041b8d14d1534762d"),
            "{url}"
        );
        assert_eq!(standard.method, Method::Post);
        assert_eq!(standard.headers[0], ("x-router", "usercenter.kugou.com"));

        let body = standard.data.clone().unwrap();
        assert!(
            body.starts_with(r#"{"visit_time":1700000000,"usertype":1,"p":"#),
            "{body}"
        );
        assert!(body.ends_with(r#","userid":10001}"#), "{body}");
        assert!(
            body.contains(
                "872BB0033583FBE8528E9C4B6BE4D7833E779E612D041DC920F224100D968A1565F8F60BE0B953031A8AF9FF8F78682EA1FDE18A8DB23C28F4B948A962C637ACAA4A2BED6517855AC6406323FDB6954143E74C94901FB112354769DCB437E9BFAD2115B658BE512C80708ABCDE43AC7B29C9DD84EB34E98E76A9AF4B0A02196F"
            ),
            "{body}"
        );

        let lite = user_detail_endpoint(
            SourceKind::KugouConcept,
            "TOKENFIXTURE",
            Some("10001"),
            KAT_CLIENTTIME,
        )
        .unwrap();
        let url = prepared_url(SourceKind::KugouConcept, &lite);
        assert!(
            url.ends_with("signature=6129a60675eca869e470623d32657131"),
            "{url}"
        );
    }

    /// `p` 必须是 256 字符大写 hex——上游 `.toUpperCase()` 很容易漏。
    #[test]
    fn user_detail_p_is_uppercase_hex() {
        let endpoint = user_detail_endpoint(
            SourceKind::Kugou,
            "TOKENFIXTURE",
            Some("10001"),
            KAT_CLIENTTIME,
        )
        .unwrap();
        let body: Value = serde_json::from_str(endpoint.data.as_deref().unwrap()).unwrap();
        let p = body["p"].as_str().unwrap();
        assert_eq!(p.len(), 256);
        assert!(p.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(p, p.to_uppercase());
    }

    /// `clienttime` 同时进 body 与签名，解析失败不能静默变 0——那样只会换来一个
    /// 看不懂的 403。
    #[test]
    fn endpoints_reject_a_non_numeric_clienttime() {
        assert!(
            user_detail_endpoint(
                SourceKind::Kugou,
                "TOKENFIXTURE",
                Some("10001"),
                "not-a-number"
            )
            .is_err()
        );
        assert!(
            artist_tracks_endpoint(
                SourceKind::Kugou,
                1,
                "hot",
                1,
                20,
                "not-a-number",
                "mid-filler",
            )
            .is_err()
        );
    }

    /// `/user/vip/detail`：带 `busi_type=concept`。
    #[test]
    fn user_vip_detail_endpoint_matches_kat() {
        let standard = prepared_url(SourceKind::Kugou, &user_vip_detail_endpoint());
        assert!(
            standard.starts_with("https://kugouvip.kugou.com/v1/get_union_vip?"),
            "{standard}"
        );
        assert!(standard.contains("busi_type=concept&"), "{standard}");
        assert!(
            standard.ends_with("signature=96cf2266d36f85b67a59246d5f0424db"),
            "{standard}"
        );

        let lite = prepared_url(SourceKind::KugouConcept, &user_vip_detail_endpoint());
        assert!(
            lite.ends_with("signature=9db667847a40506f67f9834e90f75e58"),
            "{lite}"
        );
    }

    /// `login_qr_create` 不联网：只把 key 拼进 H5 地址。
    #[test]
    fn login_qr_content_is_built_locally() {
        assert_eq!(
            login_qr_content(KAT_QR_KEY),
            "https://h5.kugou.com/apps/loginQRCode/html/index.html?qrcode=QRKEYFIXTURE0123456789abcdef"
        );
    }

    /// 上游注释：0 过期、1 等待、2 待确认、4 成功。未知值一律当等待。
    #[test]
    fn qr_check_status_mapping_matches_upstream() {
        for (raw, expected) in [
            (0, QrStatus::Expired),
            (1, QrStatus::Waiting),
            (2, QrStatus::Pending),
            (4, QrStatus::Success),
            (99, QrStatus::Waiting),
        ] {
            let root = serde_json::json!({ "data": { "status": raw } });
            assert_eq!(parse_qr_check(&root).status, expected, "status={raw}");
        }
        // 上游 `pick_i64(data, ["status","code"])` 的兜底键。
        let root = serde_json::json!({ "data": { "code": 4, "token": "T", "userid": "7" } });
        let check = parse_qr_check(&root);
        assert_eq!(check.status, QrStatus::Success);
        assert_eq!(check.token.as_deref(), Some("T"));
        assert_eq!(check.userid.as_deref(), Some("7"));
        assert!(check.cookie.is_none());
    }

    /// `/user/detail` 的字段映射（`p_grade` 是 `u32`、`duration` 是分钟）。
    #[test]
    fn user_detail_parses_kat_body() {
        let root = serde_json::json!({
            "status": 1,
            "error_code": 0,
            "data": {
                "nickname": "NICKFIXTURE",
                "pic": "https://example.invalid/pic.jpg",
                "p_grade": 12,
                "duration": 79239,
            }
        });
        let info = parse_user_detail(&root);
        assert_eq!(info.nickname, "NICKFIXTURE");
        assert_eq!(info.pic.as_deref(), Some("https://example.invalid/pic.jpg"));
        assert_eq!(info.grade, Some(12));
        assert_eq!(info.duration_min, Some(79239));
    }

    /// 空白 `pic` 视作没有头像。
    #[test]
    fn user_detail_treats_blank_pic_as_none() {
        let root = serde_json::json!({ "data": { "nickname": "N", "pic": "   " } });
        let info = parse_user_detail(&root);
        assert!(info.pic.is_none());
        assert_eq!(info.grade, None);
        assert_eq!(info.duration_min, None);
    }

    /// 顶层 `is_vip` 命中即标准版 VIP。
    #[test]
    fn vip_detail_reads_top_level_flag() {
        let root = serde_json::json!({
            "data": { "is_vip": 1, "vip_end_time": "2027-01-01 00:00:00" }
        });
        let info = parse_user_vip_detail(&root);
        assert_eq!(info.kind, VipKind::Standard);
        assert_eq!(info.product, "VIP");
        assert_eq!(info.end_time, "2027-01-01 00:00:00");
    }

    /// 顶层 `is_vip: 0` 但 `busi_vip` 里有概念版 SVIP 时，必须认出会员。
    #[test]
    fn vip_detail_falls_back_to_busi_vip() {
        let root = serde_json::json!({
            "data": {
                "is_vip": 0,
                "busi_vip": [
                    { "is_vip": 0, "busi_type": "tvip" },
                    {
                        "is_vip": 1,
                        "busi_type": "concept",
                        "product_type": "svip",
                        "vip_end_time": "2028-02-02 00:00:00",
                    }
                ]
            }
        });
        let info = parse_user_vip_detail(&root);
        assert_eq!(info.kind, VipKind::Concept);
        assert_eq!(info.product, "svip");
        assert_eq!(info.end_time, "2028-02-02 00:00:00");
    }

    /// 非 `concept` 的形态归到 `Other`，原样保留 `busi_type`。
    #[test]
    fn vip_detail_keeps_unknown_busi_type() {
        let root = serde_json::json!({
            "data": { "busi_vip": [{ "is_vip": 1, "busi_type": "tvip", "product_type": "tvip" }] }
        });
        assert_eq!(
            parse_user_vip_detail(&root).kind,
            VipKind::Other("tvip".to_string())
        );
    }

    /// 什么都没命中时是「无会员」，不能是 `Standard`。
    #[test]
    fn vip_detail_defaults_to_none() {
        let root = serde_json::json!({ "data": { "is_vip": 0, "busi_vip": [] } });
        assert_eq!(parse_user_vip_detail(&root).kind, VipKind::None);
    }

    /// 出站 URL、头与请求体逐字节对齐上游 `plazaPlaylists` 基准。
    ///
    /// 签名覆盖参数序 + 头 + body，任一处不同服务端都只回 403 或空列表，
    /// 不会指出是哪里错了，所以这里必须锁死。KAT 的假 axios 记录的是**传入
    /// config 的头**，真实 axios 会在有对象 body 时补 `Content-Type`，所以带
    /// body 的接口期望里多这一条。
    #[test]
    fn plaza_playlists_endpoint_matches_kat() {
        let standard = plaza_playlists_endpoint(
            SourceKind::Kugou,
            0,
            1,
            30,
            KAT_CLIENTTIME,
            KAT_MID,
            Some("10001"),
        )
        .unwrap();
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://gateway.kugou.com/v2/special_recommend?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=860fd964c1370164484c07ae6964e0ba"#
        );
        assert_eq!(
            prepared_body(SourceKind::Kugou, &standard).as_deref(),
            Some(
                r#"{"appid":1005,"mid":"231699103997194646178265604655475531917","clientver":20489,"platform":"android","clienttime":"1700000000","userid":"10001","module_id":1,"page":"1","pagesize":"30","key":"a1f65b6a8fe7e191521406ce8661ae02","special_recommend":{"withtag":"1","withsong":"0","sort":1,"ugc":1,"is_selected":0,"withrecommend":1,"area_code":1,"categoryid":"0"},"req_multi":1,"retrun_min":5,"return_special_falg":1}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            vec![
                (
                    r#"Content-Type"#.to_string(),
                    r#"application/json"#.to_string()
                ),
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                ),
                (
                    r#"x-router"#.to_string(),
                    r#"specialrec.service.kugou.com"#.to_string()
                )
            ]
        );

        let lite = plaza_playlists_endpoint(
            SourceKind::KugouConcept,
            0,
            1,
            30,
            KAT_CLIENTTIME,
            KAT_MID,
            Some("10001"),
        )
        .unwrap();
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://gateway.kugou.com/v2/special_recommend?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=0e68cc8bf544dd74d5c0e9489e60f071"#
        );
        assert_eq!(
            prepared_body(SourceKind::KugouConcept, &lite).as_deref(),
            Some(
                r#"{"appid":3116,"mid":"231699103997194646178265604655475531917","clientver":11440,"platform":"android","clienttime":"1700000000","userid":"10001","module_id":1,"page":"1","pagesize":"30","key":"bad0ee207bf429bd91402b77fc8f7a5b","special_recommend":{"withtag":"1","withsong":"0","sort":1,"ugc":1,"is_selected":0,"withrecommend":1,"area_code":1,"categoryid":"0"},"req_multi":1,"retrun_min":5,"return_special_falg":1}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::KugouConcept, &lite),
            vec![
                (
                    r#"Content-Type"#.to_string(),
                    r#"application/json"#.to_string()
                ),
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                ),
                (
                    r#"x-router"#.to_string(),
                    r#"specialrec.service.kugou.com"#.to_string()
                )
            ]
        );
    }

    /// 出站 URL、头与请求体逐字节对齐上游 `playlistTracks` 基准。
    ///
    /// 签名覆盖参数序 + 头 + body，任一处不同服务端都只回 403 或空列表，
    /// 不会指出是哪里错了，所以这里必须锁死。KAT 的假 axios 记录的是**传入
    /// config 的头**，真实 axios 会在有对象 body 时补 `Content-Type`，所以带
    /// body 的接口期望里多这一条。
    #[test]
    fn playlist_tracks_endpoint_matches_kat() {
        let standard = playlist_tracks_endpoint("GLOBALCOLLECTIONIDFIXTURE", 1, 30);
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://gateway.kugou.com/pubsongs/v2/get_other_list_file_nofilt?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&area_code=1&begin_idx=0&plat=1&type=1&mode=1&personal_switch=1&extend_fields=abtags,hot_cmt,popularization&pagesize=30&global_collection_id=GLOBALCOLLECTIONIDFIXTURE&signature=e47c5e6f6b9f3d50b603c4a9d6275e5e"#
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            vec![
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                )
            ]
        );

        let lite = playlist_tracks_endpoint("GLOBALCOLLECTIONIDFIXTURE", 1, 30);
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://gateway.kugou.com/pubsongs/v2/get_other_list_file_nofilt?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&area_code=1&begin_idx=0&plat=1&type=1&mode=1&personal_switch=1&extend_fields=abtags,hot_cmt,popularization&pagesize=30&global_collection_id=GLOBALCOLLECTIONIDFIXTURE&signature=2fa3f9928e4bb06605367d2ebb9ee6db"#
        );
        assert_eq!(
            prepared_headers(SourceKind::KugouConcept, &lite),
            vec![
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                )
            ]
        );
    }

    /// 出站 URL、头与请求体逐字节对齐上游 `userPlaylists` 基准。
    ///
    /// 签名覆盖参数序 + 头 + body，任一处不同服务端都只回 403 或空列表，
    /// 不会指出是哪里错了，所以这里必须锁死。KAT 的假 axios 记录的是**传入
    /// config 的头**，真实 axios 会在有对象 body 时补 `Content-Type`，所以带
    /// body 的接口期望里多这一条。
    #[test]
    fn user_playlists_endpoint_matches_kat() {
        let standard =
            user_playlists_endpoint(1, 100, Some("10001"), Some("TOKENFIXTURE")).unwrap();
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://gateway.kugou.com/v7/get_all_list?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&plat=1&signature=05ec6eed672d0f2c76a67978be96819d"#
        );
        assert_eq!(
            prepared_body(SourceKind::Kugou, &standard).as_deref(),
            Some(
                r#"{"userid":"10001","token":"TOKENFIXTURE","total_ver":979,"type":2,"page":"1","pagesize":"100"}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            vec![
                (
                    r#"Content-Type"#.to_string(),
                    r#"application/json"#.to_string()
                ),
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                ),
                (
                    r#"x-router"#.to_string(),
                    r#"cloudlist.service.kugou.com"#.to_string()
                )
            ]
        );

        let lite = user_playlists_endpoint(1, 100, Some("10001"), Some("TOKENFIXTURE")).unwrap();
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://gateway.kugou.com/v7/get_all_list?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&plat=1&signature=5514fb47f5d9b892c1608eac498852ff"#
        );
        assert_eq!(
            prepared_body(SourceKind::KugouConcept, &lite).as_deref(),
            Some(
                r#"{"userid":"10001","token":"TOKENFIXTURE","total_ver":979,"type":2,"page":"1","pagesize":"100"}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::KugouConcept, &lite),
            vec![
                (
                    r#"Content-Type"#.to_string(),
                    r#"application/json"#.to_string()
                ),
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                ),
                (
                    r#"x-router"#.to_string(),
                    r#"cloudlist.service.kugou.com"#.to_string()
                )
            ]
        );
    }

    /// 出站 URL、头与请求体逐字节对齐上游 `userPlaylistTracks` 基准。
    ///
    /// 签名覆盖参数序 + 头 + body，任一处不同服务端都只回 403 或空列表，
    /// 不会指出是哪里错了，所以这里必须锁死。KAT 的假 axios 记录的是**传入
    /// config 的头**，真实 axios 会在有对象 body 时补 `Content-Type`，所以带
    /// body 的接口期望里多这一条。
    #[test]
    fn user_playlist_tracks_endpoint_matches_kat() {
        let standard =
            user_playlist_tracks_endpoint(1234567890, 1, 30, Some("10001"), Some("TOKENFIXTURE"))
                .unwrap();
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://gateway.kugou.com/v4/get_list_all_file?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=ee8b01f6d910704b0f579537bec90e2b"#
        );
        assert_eq!(
            prepared_body(SourceKind::Kugou, &standard).as_deref(),
            Some(
                r#"{"listid":"1234567890","userid":"10001","area_code":1,"show_relate_goods":0,"pagesize":"30","allplatform":1,"show_cover":1,"type":0,"token":"TOKENFIXTURE","page":"1"}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            vec![
                (
                    r#"Content-Type"#.to_string(),
                    r#"application/json"#.to_string()
                ),
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                ),
                (
                    r#"x-router"#.to_string(),
                    r#"cloudlist.service.kugou.com"#.to_string()
                )
            ]
        );

        let lite =
            user_playlist_tracks_endpoint(1234567890, 1, 30, Some("10001"), Some("TOKENFIXTURE"))
                .unwrap();
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://gateway.kugou.com/v4/get_list_all_file?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=a868a3147e7efdb5288e3349deb6073a"#
        );
        assert_eq!(
            prepared_body(SourceKind::KugouConcept, &lite).as_deref(),
            Some(
                r#"{"listid":"1234567890","userid":"10001","area_code":1,"show_relate_goods":0,"pagesize":"30","allplatform":1,"show_cover":1,"type":0,"token":"TOKENFIXTURE","page":"1"}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::KugouConcept, &lite),
            vec![
                (
                    r#"Content-Type"#.to_string(),
                    r#"application/json"#.to_string()
                ),
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                ),
                (
                    r#"x-router"#.to_string(),
                    r#"cloudlist.service.kugou.com"#.to_string()
                )
            ]
        );
    }

    /// 出站 URL、头与请求体逐字节对齐上游 `artistLists` 基准。
    ///
    /// 签名覆盖参数序 + 头 + body，任一处不同服务端都只回 403 或空列表，
    /// 不会指出是哪里错了，所以这里必须锁死。KAT 的假 axios 记录的是**传入
    /// config 的头**，真实 axios 会在有对象 body 时补 `Content-Type`，所以带
    /// body 的接口期望里多这一条。
    #[test]
    fn artist_list_endpoint_matches_kat() {
        let standard = artist_list_endpoint(0, 30);
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://gateway.kugou.com/ocean/v6/singer/list?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&musician=0&sextype=0&showtype=2&type=0&hotsize=30&signature=49ee6a9cda61f133bc2441438751b15d"#
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            vec![
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                )
            ]
        );

        let lite = artist_list_endpoint(0, 30);
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://gateway.kugou.com/ocean/v6/singer/list?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&musician=0&sextype=0&showtype=2&type=0&hotsize=30&signature=9ef5266682408856f9e50e9d9b0d0fa3"#
        );
        assert_eq!(
            prepared_headers(SourceKind::KugouConcept, &lite),
            vec![
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                )
            ]
        );
    }

    /// 出站 URL、头与请求体逐字节对齐上游 `artistAudios` 基准。
    ///
    /// 签名覆盖参数序 + 头 + body，任一处不同服务端都只回 403 或空列表，
    /// 不会指出是哪里错了，所以这里必须锁死。KAT 的假 axios 记录的是**传入
    /// config 的头**，真实 axios 会在有对象 body 时补 `Content-Type`，所以带
    /// body 的接口期望里多这一条。
    #[test]
    fn artist_tracks_hot_endpoint_matches_kat() {
        let standard = artist_tracks_endpoint(
            SourceKind::Kugou,
            12345,
            "hot",
            1,
            30,
            KAT_CLIENTTIME,
            KAT_MID,
        )
        .unwrap();
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://openapi.kugou.com/kmr/v1/audio_group/author?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=2064074c7c2a6fa6b62946b7f1d4f763"#
        );
        assert_eq!(
            prepared_body(SourceKind::Kugou, &standard).as_deref(),
            Some(
                r#"{"appid":1005,"clientver":20489,"mid":"231699103997194646178265604655475531917","clienttime":1700000000,"key":"a1f65b6a8fe7e191521406ce8661ae02","author_id":"12345","pagesize":"30","page":"1","sort":1,"area_code":"all"}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            vec![
                (
                    r#"Content-Type"#.to_string(),
                    r#"application/json"#.to_string()
                ),
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (r#"kg-tid"#.to_string(), r#"220"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                ),
                (
                    r#"x-router"#.to_string(),
                    r#"openapi.kugou.com"#.to_string()
                )
            ]
        );

        let lite = artist_tracks_endpoint(
            SourceKind::KugouConcept,
            12345,
            "hot",
            1,
            30,
            KAT_CLIENTTIME,
            KAT_MID,
        )
        .unwrap();
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://openapi.kugou.com/kmr/v1/audio_group/author?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=e0fabb79f8b70de52feee585c8e4c0e2"#
        );
        assert_eq!(
            prepared_body(SourceKind::KugouConcept, &lite).as_deref(),
            Some(
                r#"{"appid":3116,"clientver":11440,"mid":"231699103997194646178265604655475531917","clienttime":1700000000,"key":"bad0ee207bf429bd91402b77fc8f7a5b","author_id":"12345","pagesize":"30","page":"1","sort":1,"area_code":"all"}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::KugouConcept, &lite),
            vec![
                (
                    r#"Content-Type"#.to_string(),
                    r#"application/json"#.to_string()
                ),
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (r#"kg-tid"#.to_string(), r#"220"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                ),
                (
                    r#"x-router"#.to_string(),
                    r#"openapi.kugou.com"#.to_string()
                )
            ]
        );
    }

    /// 出站 URL、头与请求体逐字节对齐上游 `artistAudiosNew` 基准。
    ///
    /// 签名覆盖参数序 + 头 + body，任一处不同服务端都只回 403 或空列表，
    /// 不会指出是哪里错了，所以这里必须锁死。KAT 的假 axios 记录的是**传入
    /// config 的头**，真实 axios 会在有对象 body 时补 `Content-Type`，所以带
    /// body 的接口期望里多这一条。
    #[test]
    fn artist_tracks_new_endpoint_matches_kat() {
        let standard = artist_tracks_endpoint(
            SourceKind::Kugou,
            12345,
            "new",
            1,
            30,
            KAT_CLIENTTIME,
            KAT_MID,
        )
        .unwrap();
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://openapi.kugou.com/kmr/v1/audio_group/author?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=d9492e376d233efe66f29dc943287346"#
        );
        assert_eq!(
            prepared_body(SourceKind::Kugou, &standard).as_deref(),
            Some(
                r#"{"appid":1005,"clientver":20489,"mid":"231699103997194646178265604655475531917","clienttime":1700000000,"key":"a1f65b6a8fe7e191521406ce8661ae02","author_id":"12345","pagesize":"30","page":"1","sort":2,"area_code":"all"}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            vec![
                (
                    r#"Content-Type"#.to_string(),
                    r#"application/json"#.to_string()
                ),
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (r#"kg-tid"#.to_string(), r#"220"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                ),
                (
                    r#"x-router"#.to_string(),
                    r#"openapi.kugou.com"#.to_string()
                )
            ]
        );

        let lite = artist_tracks_endpoint(
            SourceKind::KugouConcept,
            12345,
            "new",
            1,
            30,
            KAT_CLIENTTIME,
            KAT_MID,
        )
        .unwrap();
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://openapi.kugou.com/kmr/v1/audio_group/author?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=d586eb6a704382777e808ac7be79c206"#
        );
        assert_eq!(
            prepared_body(SourceKind::KugouConcept, &lite).as_deref(),
            Some(
                r#"{"appid":3116,"clientver":11440,"mid":"231699103997194646178265604655475531917","clienttime":1700000000,"key":"bad0ee207bf429bd91402b77fc8f7a5b","author_id":"12345","pagesize":"30","page":"1","sort":2,"area_code":"all"}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::KugouConcept, &lite),
            vec![
                (
                    r#"Content-Type"#.to_string(),
                    r#"application/json"#.to_string()
                ),
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (r#"kg-tid"#.to_string(), r#"220"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                ),
                (
                    r#"x-router"#.to_string(),
                    r#"openapi.kugou.com"#.to_string()
                )
            ]
        );
    }

    /// 出站 URL、头与请求体逐字节对齐上游 `rankList` 基准。
    ///
    /// 签名覆盖参数序 + 头 + body，任一处不同服务端都只回 403 或空列表，
    /// 不会指出是哪里错了，所以这里必须锁死。KAT 的假 axios 记录的是**传入
    /// config 的头**，真实 axios 会在有对象 body 时补 `Content-Type`，所以带
    /// body 的接口期望里多这一条。
    #[test]
    fn rank_boards_endpoint_matches_kat() {
        let standard = rank_boards_endpoint();
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://gateway.kugou.com/ocean/v6/rank/list?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&plat=2&withsong=0&parentid=0&signature=05c71c49ddcafb388aa2d4ec0a2c0716"#
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            vec![
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                )
            ]
        );

        let lite = rank_boards_endpoint();
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://gateway.kugou.com/ocean/v6/rank/list?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&plat=2&withsong=0&parentid=0&signature=7b121b79176d37755580b9a5b69073b8"#
        );
        assert_eq!(
            prepared_headers(SourceKind::KugouConcept, &lite),
            vec![
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                )
            ]
        );
    }

    /// 出站 URL、头与请求体逐字节对齐上游 `rankAudio` 基准。
    ///
    /// 签名覆盖参数序 + 头 + body，任一处不同服务端都只回 403 或空列表，
    /// 不会指出是哪里错了，所以这里必须锁死。KAT 的假 axios 记录的是**传入
    /// config 的头**，真实 axios 会在有对象 body 时补 `Content-Type`，所以带
    /// body 的接口期望里多这一条。
    #[test]
    fn rank_tracks_endpoint_matches_kat() {
        let standard = rank_tracks_endpoint(8888, 1, 30).unwrap();
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://gateway.kugou.com/openapi/kmr/v2/rank/audio?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=a415b5e78eb7563835f316cb53563770"#
        );
        assert_eq!(
            prepared_body(SourceKind::Kugou, &standard).as_deref(),
            Some(
                r#"{"show_portrait_mv":1,"show_type_total":1,"filter_original_remarks":1,"area_code":1,"pagesize":"30","rank_cid":0,"type":1,"page":"1","rank_id":"8888"}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            vec![
                (
                    r#"Content-Type"#.to_string(),
                    r#"application/json"#.to_string()
                ),
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (r#"kg-tid"#.to_string(), r#"369"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                )
            ]
        );

        let lite = rank_tracks_endpoint(8888, 1, 30).unwrap();
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://gateway.kugou.com/openapi/kmr/v2/rank/audio?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=c0a5e015554824c08e305df9412cf210"#
        );
        assert_eq!(
            prepared_body(SourceKind::KugouConcept, &lite).as_deref(),
            Some(
                r#"{"show_portrait_mv":1,"show_type_total":1,"filter_original_remarks":1,"area_code":1,"pagesize":"30","rank_cid":0,"type":1,"page":"1","rank_id":"8888"}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::KugouConcept, &lite),
            vec![
                (
                    r#"Content-Type"#.to_string(),
                    r#"application/json"#.to_string()
                ),
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (r#"kg-tid"#.to_string(), r#"369"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                )
            ]
        );
    }

    /// 出站 URL、头与请求体逐字节对齐上游 `monthVipRecord` 基准。
    ///
    /// 签名覆盖参数序 + 头 + body，任一处不同服务端都只回 403 或空列表，
    /// 不会指出是哪里错了，所以这里必须锁死。KAT 的假 axios 记录的是**传入
    /// config 的头**，真实 axios 会在有对象 body 时补 `Content-Type`，所以带
    /// body 的接口期望里多这一条。
    #[test]
    fn claimed_vip_days_endpoint_matches_kat() {
        let standard = claimed_vip_days_endpoint();
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://gateway.kugou.com/youth/v1/activity/get_month_vip_record?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&latest_limit=100&signature=d071f1ad28132b119a0b7bb1683116cd"#
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            vec![
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                )
            ]
        );

        let lite = claimed_vip_days_endpoint();
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://gateway.kugou.com/youth/v1/activity/get_month_vip_record?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&latest_limit=100&signature=69088ac11eb2642419a9993b18f54576"#
        );
        assert_eq!(
            prepared_headers(SourceKind::KugouConcept, &lite),
            vec![
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                )
            ]
        );
    }

    /// 出站 URL、头与请求体逐字节对齐上游 `dayVip` 基准。
    ///
    /// 这个接口**没有 body**，签名里的 `data` 是空串；`content-type` 是模块自己
    /// 加的头（`request.js` 的 `Content-Type` 只在有对象 body 时才由 axios 补）。
    #[test]
    fn claim_day_vip_endpoint_matches_kat() {
        let standard = claim_day_vip_endpoint("2026-09-23");
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://gateway.kugou.com/youth/v1/recharge/receive_vip_listen_song?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&source_id=90139&receive_day=2026-09-23&signature=fd42ca24c3e83727ca8671b0629e0a96"#
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            vec![
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"content-type"#.to_string(),
                    r#"application/x-www-form-urlencoded"#.to_string()
                ),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                )
            ]
        );
        assert_eq!(prepared_body(SourceKind::Kugou, &standard), None);

        let lite = claim_day_vip_endpoint("2026-09-23");
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://gateway.kugou.com/youth/v1/recharge/receive_vip_listen_song?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&source_id=90139&receive_day=2026-09-23&signature=76e25350091fbea351263e6578b26fcf"#
        );
    }

    /// 出站 URL、头与请求体逐字节对齐上游 `dayVipUpgrade` 基准。
    ///
    /// `kugouid` 与默认参数里的 `userid` 是**两个不同的键、同一个值**：上游
    /// `paramsMap` 自己算了一份，`defaultParams` 又带了一份，两边都不能少。
    #[test]
    fn upgrade_day_vip_endpoint_matches_kat() {
        let standard = upgrade_day_vip_endpoint(Some("10001"));
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://gateway.kugou.com/youth/v1/listen_song/upgrade_vip_reward?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&kugouid=10001&ad_type=1&signature=add5a4a6c7728af06d8037f973165a4e"#
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            vec![
                (
                    r#"User-Agent"#.to_string(),
                    r#"Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi"#.to_string()
                ),
                (r#"clienttime"#.to_string(), r#"1700000000"#.to_string()),
                (
                    r#"dfid"#.to_string(),
                    r#"1234567890abcdef12345678"#.to_string()
                ),
                (r#"kg-rc"#.to_string(), r#"1"#.to_string()),
                (r#"kg-rec"#.to_string(), r#"1"#.to_string()),
                (
                    r#"kg-rf"#.to_string(),
                    r#"B9EDA08A64250DEFFBCADDEE00F8F25F"#.to_string()
                ),
                (r#"kg-thash"#.to_string(), r#"5d816a0"#.to_string()),
                (
                    r#"mid"#.to_string(),
                    r#"231699103997194646178265604655475531917"#.to_string()
                )
            ]
        );
        assert_eq!(prepared_body(SourceKind::Kugou, &standard), None);

        let lite = upgrade_day_vip_endpoint(Some("10001"));
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://gateway.kugou.com/youth/v1/listen_song/upgrade_vip_reward?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&kugouid=10001&ad_type=1&signature=46edb84063cb667d4ea05dfd4908eac6"#
        );
    }

    /// `kugouid` 走 `Number(...)`：cookie 里没有 userid 时上游得到 `0`（不是空串）。
    ///
    /// 写成空串会进签名与 URL，服务端只会回业务错误码，不会说是参数错了。
    /// 注意默认参数里的 `userid` 来自 cookie、与这里的 `kugouid` 是**两个键**：
    /// 上游 cookie 有 userid 时它照常出现，只有 `kugouid` 会退化成 0。
    #[test]
    fn upgrade_day_vip_userid_falls_back_to_zero() {
        let missing = upgrade_day_vip_endpoint(None);
        assert!(
            prepared_url(SourceKind::Kugou, &missing).contains("&kugouid=0&"),
            "缺 userid 时 kugouid 该是 0：{}",
            prepared_url(SourceKind::Kugou, &missing)
        );

        let non_numeric = upgrade_day_vip_endpoint(Some("abc"));
        assert!(
            prepared_url(SourceKind::Kugou, &non_numeric).contains("&kugouid=0&"),
            "非数字 userid 该退化成 0：{}",
            prepared_url(SourceKind::Kugou, &non_numeric)
        );

        let numeric = upgrade_day_vip_endpoint(Some("10001"));
        assert!(
            prepared_url(SourceKind::Kugou, &numeric).contains("&kugouid=10001&"),
            "数字 userid 原样带上：{}",
            prepared_url(SourceKind::Kugou, &numeric)
        );
    }

    /// 九个目录类接口的参数顺序必须与上游逐位一致。
    ///
    /// 上游签名先把参数按 key 排序再拼串，但**URL 本身的顺序**来自
    /// `module/*.js` 的对象插入序——两端 URL 不同就说明默认参数与模块参数的
    /// 合并方式（`Object.assign` 的「改值不改位」）走偏了，签名也会跟着错。
    #[test]
    fn cloud_endpoints_keep_kat_param_order() {
        let cases: Vec<(&str, Vec<&str>, Endpoint<'static>)> = vec![
            (
                "plaza_playlists",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "signature",
                ],
                plaza_playlists_endpoint(
                    SourceKind::KugouConcept,
                    0,
                    1,
                    30,
                    KAT_CLIENTTIME,
                    KAT_MID,
                    Some("10001"),
                )
                .unwrap(),
            ),
            (
                "playlist_tracks",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "area_code",
                    "begin_idx",
                    "plat",
                    "type",
                    "mode",
                    "personal_switch",
                    "extend_fields",
                    "pagesize",
                    "global_collection_id",
                    "signature",
                ],
                playlist_tracks_endpoint("GLOBALCOLLECTIONIDFIXTURE", 1, 30),
            ),
            (
                "user_playlists",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "plat",
                    "signature",
                ],
                user_playlists_endpoint(1, 100, Some("10001"), Some("TOKENFIXTURE")).unwrap(),
            ),
            (
                "user_playlist_tracks",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "signature",
                ],
                user_playlist_tracks_endpoint(
                    1234567890,
                    1,
                    30,
                    Some("10001"),
                    Some("TOKENFIXTURE"),
                )
                .unwrap(),
            ),
            (
                "artist_list",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "musician",
                    "sextype",
                    "showtype",
                    "type",
                    "hotsize",
                    "signature",
                ],
                artist_list_endpoint(0, 30),
            ),
            (
                "artist_tracks_hot",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "signature",
                ],
                artist_tracks_endpoint(
                    SourceKind::KugouConcept,
                    12345,
                    "hot",
                    1,
                    30,
                    KAT_CLIENTTIME,
                    KAT_MID,
                )
                .unwrap(),
            ),
            (
                "artist_tracks_new",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "signature",
                ],
                artist_tracks_endpoint(
                    SourceKind::KugouConcept,
                    12345,
                    "new",
                    1,
                    30,
                    KAT_CLIENTTIME,
                    KAT_MID,
                )
                .unwrap(),
            ),
            (
                "rank_boards",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "plat",
                    "withsong",
                    "parentid",
                    "signature",
                ],
                rank_boards_endpoint(),
            ),
            (
                "rank_tracks",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "signature",
                ],
                rank_tracks_endpoint(8888, 1, 30).unwrap(),
            ),
            (
                "claimed_vip_days",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "latest_limit",
                    "signature",
                ],
                claimed_vip_days_endpoint(),
            ),
            (
                "claim_day_vip",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "source_id",
                    "receive_day",
                    "signature",
                ],
                claim_day_vip_endpoint("2026-09-23"),
            ),
            (
                "upgrade_day_vip",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "kugouid",
                    "ad_type",
                    "signature",
                ],
                upgrade_day_vip_endpoint(Some("10001")),
            ),
        ];

        for (name, expected, endpoint) in cases {
            assert_eq!(
                prepared_param_order(SourceKind::KugouConcept, &endpoint),
                expected,
                "{name} 的参数顺序与上游不一致"
            );
        }
    }

    /// 写接口的公共头（不含 axios 自己补的 `accept`/`content-length`/`host` 等）。
    fn write_headers(content_type: &str, x_router: Option<&str>) -> Vec<(String, String)> {
        let mut headers = vec![
            ("Content-Type".to_string(), content_type.to_string()),
            (
                "User-Agent".to_string(),
                "Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi".to_string(),
            ),
            ("clienttime".to_string(), KAT_CLIENTTIME.to_string()),
            ("dfid".to_string(), KAT_DFID.to_string()),
            ("kg-rc".to_string(), "1".to_string()),
            ("kg-rec".to_string(), "1".to_string()),
            (
                "kg-rf".to_string(),
                "B9EDA08A64250DEFFBCADDEE00F8F25F".to_string(),
            ),
            ("kg-thash".to_string(), "5d816a0".to_string()),
            ("mid".to_string(), KAT_MID.to_string()),
        ];
        if let Some(router) = x_router {
            headers.push(("x-router".to_string(), router.to_string()));
        }
        headers.sort();
        headers
    }

    /// `/playlist/add`：`url` 自带服务名前缀、**没有 `x-router`**，`type` 是字符串
    /// `"0"` 所以 `params.type === 0` 不成立（`is_pri` 保持字面量 0、也没有额外 params）。
    #[test]
    fn playlist_add_endpoint_matches_kat() {
        let standard =
            playlist_add_endpoint("KATFIXTURE临时歌单", Some("10001"), Some("TOKENFIXTURE"))
                .unwrap();
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://gateway.kugou.com/cloudlist.service/v5/add_list?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=702cbc34f583b49433be804aa93e1ca1"#
        );
        assert_eq!(
            prepared_body(SourceKind::Kugou, &standard).as_deref(),
            Some(
                r#"{"userid":"10001","token":"TOKENFIXTURE","total_ver":0,"name":"KATFIXTURE临时歌单","type":"0","source":1,"is_pri":0,"list_create_gid":"","from_shupinmv":0}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            write_headers("application/json", None)
        );

        let lite = playlist_add_endpoint("KATFIXTURE临时歌单", Some("10001"), Some("TOKENFIXTURE"))
            .unwrap();
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://gateway.kugou.com/cloudlist.service/v5/add_list?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=97786700579016edc87b047e8882e41b"#
        );
    }

    /// `/playlist/tracks/add`：`params` 只有 `last_time`/`last_area`，条目由
    /// `encode_track_entry` 生成的串拆回对象（`album_id`/`mixsongid` 是数字）。
    #[test]
    fn playlist_tracks_add_endpoint_matches_kat() {
        let payload = "晴天|6af00fbd4d444a82c005843eef9dc2d4|1234567|8901234";
        let standard = playlist_tracks_add_endpoint(
            1234567890,
            payload,
            Some("10001"),
            Some("TOKENFIXTURE"),
            KAT_CLIENTTIME,
        )
        .unwrap();
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://gateway.kugou.com/cloudlist.service/v6/add_song?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&last_time=1700000000&last_area=gztx&signature=f1d71241e07abf1f5e0ab27a5f5cfbcf"#
        );
        assert_eq!(
            prepared_body(SourceKind::Kugou, &standard).as_deref(),
            Some(
                r#"{"userid":"10001","token":"TOKENFIXTURE","listid":"1234567890","list_ver":0,"type":0,"slow_upload":1,"scene":"false;null","data":[{"number":1,"name":"晴天","hash":"6af00fbd4d444a82c005843eef9dc2d4","size":0,"sort":0,"timelen":0,"bitrate":0,"album_id":1234567,"mixsongid":8901234}]}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            write_headers("application/json", None)
        );

        let lite = playlist_tracks_add_endpoint(
            1234567890,
            payload,
            Some("10001"),
            Some("TOKENFIXTURE"),
            KAT_CLIENTTIME,
        )
        .unwrap();
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://gateway.kugou.com/cloudlist.service/v6/add_song?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&last_time=1700000000&last_area=gztx&signature=98903612e61889b8467aeee46af8cc04"#
        );
    }

    /// `/playlist/tracks/del`：入参全在 body，`fileid` 是数字、`listid` 是字符串。
    #[test]
    fn playlist_tracks_del_endpoint_matches_kat() {
        let standard = playlist_tracks_del_endpoint(
            1234567890,
            &[111111, 222222],
            Some("10001"),
            Some("TOKENFIXTURE"),
        )
        .unwrap();
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            r#"https://gateway.kugou.com/v4/delete_songs?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=6f3595773802fb5f2d9a61efcf1b9528"#
        );
        assert_eq!(
            prepared_body(SourceKind::Kugou, &standard).as_deref(),
            Some(
                r#"{"listid":"1234567890","userid":"10001","data":[{"fileid":111111},{"fileid":222222}],"type":0,"token":"TOKENFIXTURE","list_ver":0}"#
            )
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            write_headers("application/json", Some("cloudlist.service.kugou.com"))
        );

        let lite = playlist_tracks_del_endpoint(
            1234567890,
            &[111111, 222222],
            Some("10001"),
            Some("TOKENFIXTURE"),
        )
        .unwrap();
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            r#"https://gateway.kugou.com/v4/delete_songs?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&signature=64c5f16b6fd4e2a471498e70e513abe1"#
        );
    }

    /// `/playlist/del`：body 是 AES-128-CBC 密文的 base64、`p` 是 PKCS#1 v1.5 的
    /// 大写 hex，`params` 里还有一个 `signParamsKey(clienttime)` 的 `key`。
    ///
    /// 这里的 `p` 与 `/register/dev` 的 `KAT_REGISTER_P_STANDARD` 逐字节相同——两次
    /// 独立录制的 RSA 明文与填充恰好一致，等于互证 `rsaEncrypt2` ≡ `pkcs1_v15_encrypt`；
    /// lite 因为公钥不同而不同，另录在 `KAT_REGISTER_P_LITE`。
    #[test]
    fn playlist_del_endpoint_matches_kat() {
        let standard = playlist_del_endpoint(
            SourceKind::Kugou,
            1234567890,
            Some("10001"),
            Some("TOKENFIXTURE"),
            KAT_CLIENTTIME,
            KAT_AES_KEY,
            &KAT_RSA_FILL,
        )
        .unwrap();
        assert_eq!(
            prepared_url(SourceKind::Kugou, &standard),
            format!(
                r#"https://gateway.kugou.com/v2/delete_list?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=1005&clientver=20489&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&key=a1f65b6a8fe7e191521406ce8661ae02&last_area=gztx&last_time=1700000000&p={}&signature=d8a8c134f34c4d5f90309e5b4e310bc4"#,
                KAT_REGISTER_P_STANDARD.to_uppercase()
            )
        );
        assert_eq!(
            prepared_body(SourceKind::Kugou, &standard).as_deref(),
            Some("q8ZMTjdbAJWDzYu965aeVTFm6q84iSfX6OIzVo+/XbBgAEBcIC7D/P/AicQ0GVlh")
        );
        assert_eq!(
            prepared_headers(SourceKind::Kugou, &standard),
            write_headers(
                "application/x-www-form-urlencoded",
                Some("cloudlist.service.kugou.com")
            )
        );

        let lite = playlist_del_endpoint(
            SourceKind::KugouConcept,
            1234567890,
            Some("10001"),
            Some("TOKENFIXTURE"),
            KAT_CLIENTTIME,
            KAT_AES_KEY,
            &KAT_RSA_FILL,
        )
        .unwrap();
        assert_eq!(
            prepared_url(SourceKind::KugouConcept, &lite),
            format!(
                r#"https://gateway.kugou.com/v2/delete_list?dfid=1234567890abcdef12345678&mid=231699103997194646178265604655475531917&uuid=-&appid=3116&clientver=11440&clienttime=1700000000&token=TOKENFIXTURE&userid=10001&key=bad0ee207bf429bd91402b77fc8f7a5b&last_area=gztx&last_time=1700000000&p={}&signature=734d3d812f3d931bba5de83ee7f8a232"#,
                KAT_REGISTER_P_LITE.to_uppercase()
            )
        );
        assert_eq!(
            prepared_body(SourceKind::KugouConcept, &lite).as_deref(),
            Some("q8ZMTjdbAJWDzYu965aeVTFm6q84iSfX6OIzVo+/XbBgAEBcIC7D/P/AicQ0GVlh")
        );
    }

    /// 写接口的参数顺序：`playlist_del` 与 `playlist_tracks_add` 在默认参数之后
    /// 追加模块参数，`playlist_add`/`playlist_tracks_del` 不加。
    #[test]
    fn write_endpoints_keep_kat_param_order() {
        let cases: Vec<(&str, Vec<&str>, Endpoint<'_>)> = vec![
            (
                "playlist_add",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "signature",
                ],
                playlist_add_endpoint("x", Some("10001"), Some("TOKENFIXTURE")).unwrap(),
            ),
            (
                "playlist_del",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "key",
                    "last_area",
                    "last_time",
                    "p",
                    "signature",
                ],
                playlist_del_endpoint(
                    SourceKind::KugouConcept,
                    1234567890,
                    Some("10001"),
                    Some("TOKENFIXTURE"),
                    KAT_CLIENTTIME,
                    KAT_AES_KEY,
                    &KAT_RSA_FILL,
                )
                .unwrap(),
            ),
            (
                "playlist_tracks_add",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "last_time",
                    "last_area",
                    "signature",
                ],
                playlist_tracks_add_endpoint(
                    1234567890,
                    "x",
                    Some("10001"),
                    Some("TOKENFIXTURE"),
                    KAT_CLIENTTIME,
                )
                .unwrap(),
            ),
            (
                "playlist_tracks_del",
                vec![
                    "dfid",
                    "mid",
                    "uuid",
                    "appid",
                    "clientver",
                    "clienttime",
                    "token",
                    "userid",
                    "signature",
                ],
                playlist_tracks_del_endpoint(1234567890, &[1], Some("10001"), Some("TOKENFIXTURE"))
                    .unwrap(),
            ),
        ];

        for (name, expected, endpoint) in cases {
            assert_eq!(
                prepared_param_order(SourceKind::KugouConcept, &endpoint),
                expected,
                "{name} 的参数顺序与上游不一致"
            );
        }
    }

    /// 上游 `params?.userid || params?.cookie?.userid || 0`：字符串 `"0"` 是 truthy，
    /// 只有空串/缺省才落到数字 `0`。这个差别会进 body 与签名，写错不报错。
    #[test]
    fn userid_value_matches_javascript_truthiness() {
        assert_eq!(userid_value(Some("10001")), Value::String("10001".into()));
        assert_eq!(userid_value(Some("0")), Value::String("0".into()));
        assert_eq!(userid_value(Some("")), Value::from(0));
        assert_eq!(userid_value(None), Value::from(0));
    }

    /// `/playlist/del` 的响应**没有 `error_code`**，`check_write_result` 会放过它；
    /// 但解密失败必须报错，否则「删掉了」是假的。这里锁住解密失败这条路径。
    #[test]
    fn delete_playlist_fails_when_response_cannot_be_decrypted() {
        // 用错误的密钥解同一段密文：CryptoJS 语义下不校验填充，但块数不足会报错。
        let body = crate::api::native::crypto::base64_decode(
            "q8ZMTjdbAJWDzYu965aeVTFm6q84iSfX6OIzVo+/XbBgAEBcIC7D/P/AicQ0GVlh",
        )
        .unwrap();
        let (key, iv) = crate::api::native::crypto::playlist_key_material("wrong1");
        let plain = crate::api::native::crypto::aes_cbc_decrypt(&key, &iv, &body).unwrap();
        // 填充不校验，所以这里不 panic，只是解出垃圾——与上游一致。
        assert_eq!(plain.len(), body.len());

        // 真正的失败路径：长度不是块大小整数倍。
        assert!(crate::api::native::crypto::aes_cbc_decrypt(&key, &iv, &body[..7]).is_err());
    }

    // ------------------------------------------------------------------
    // 云歌单写接口：真实账号端到端探针
    //
    // 写接口用 mock 只能证明「请求形状与签名对」，证明不了「服务端真的接受了」。
    // 这个探针在**隔离配置**下打真实服务端，只新建一个临时歌单、只删它自己，
    // 全程不碰任何既有歌单；任一断言失败立刻停止，并尝试只清理那一个临时歌单。
    //
    //   KUGOU_TUI_CONFIG_DIR=/tmp/kt-write KUGOU_TUI_DEBUG=1 \
    //   cargo test probe_real_cloud_playlist_write -- --ignored --nocapture
    //
    // 出站条数取自 `Transport::send`/`send_bytes` 的 DEBUG 行，所以必须开
    // `KUGOU_TUI_DEBUG=1`；输出只打印 listid、名称与数量。
    // ------------------------------------------------------------------

    /// 临时歌单名。带日期后缀，便于在官方客户端里认出并手动清理。
    const PROBE_PLAYLIST_NAME: &str = "kt-native-test-20261009";

    fn probe_mark(log: &std::path::Path) -> u64 {
        std::fs::metadata(log).map(|meta| meta.len()).unwrap_or(0)
    }

    fn probe_delta(log: &std::path::Path, mark: u64) -> String {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut file) = std::fs::File::open(log) else {
            return String::new();
        };
        if file.seek(SeekFrom::Start(mark)).is_err() {
            return String::new();
        }
        let mut text = String::new();
        let _ = file.read_to_string(&mut text);
        text
    }

    /// 四个写接口的路径。读接口不需要枚举——凡是出站行里不匹配这些的就算读。
    fn probe_is_write(url: &str) -> bool {
        [
            "/cloudlist.service/v5/add_list",
            "/cloudlist.service/v6/add_song",
            "/v4/delete_songs",
            "/v2/delete_list",
        ]
        .iter()
        .any(|path| url.contains(path))
    }

    /// 从日志增量里数出站请求：返回（读条数，写条数，写路径）。
    fn probe_count(text: &str) -> (usize, usize, Vec<String>) {
        let mut reads = 0usize;
        let mut writes = 0usize;
        let mut write_paths = Vec::new();
        for line in text.lines() {
            let Some(rest) = line.split("native 出站 ").nth(1) else {
                continue;
            };
            let mut parts = rest.split_whitespace();
            let _method = parts.next();
            let Some(url) = parts.next() else { continue };
            if probe_is_write(url) {
                writes += 1;
                let tail = url.split("://").nth(1).unwrap_or(url);
                let tail = tail.split_once('/').map(|(_, tail)| tail).unwrap_or(tail);
                write_paths.push(format!("/{}", tail.split('?').next().unwrap_or(tail)));
            } else {
                reads += 1;
            }
        }
        (reads, writes, write_paths)
    }

    /// 打印一步的出站条数，并锁住「写接口只发一次」。
    fn probe_step(
        log: &std::path::Path,
        mark: u64,
        label: &str,
        expect_writes: usize,
    ) -> std::result::Result<u64, String> {
        let text = probe_delta(log, mark);
        let (reads, writes, paths) = probe_count(&text);
        println!("  [{label}] 读 {reads} 条 / 写 {writes} 条");
        if writes != expect_writes {
            return Err(format!(
                "{label}：写请求 {writes} 条，期望 {expect_writes} 条（写接口不重试）"
            ));
        }
        if paths.len() > 1 {
            return Err(format!("{label}：单步出现多个写请求 {paths:?}"));
        }
        Ok(probe_mark(log))
    }

    async fn probe_playlists(api: &NativeApi) -> std::result::Result<Vec<Playlist>, String> {
        api.user_playlists()
            .await
            .map_err(|error| format!("user_playlists 失败：{error}"))
    }

    fn probe_ids(list: &[Playlist]) -> std::collections::BTreeSet<i64> {
        list.iter()
            .filter_map(|playlist| playlist.list_id)
            .collect()
    }

    /// b–g 全流程。`created` 是出参：一旦新建成功就立刻写进去，供失败时清理。
    async fn probe_run(
        api: &NativeApi,
        log: &std::path::Path,
        name: &str,
        before: &std::collections::BTreeSet<i64>,
        created: &mut Option<i64>,
    ) -> std::result::Result<(), String> {
        let source = SourceKind::KugouConcept;

        // b. 新建临时歌单
        let mut mark = probe_mark(log);
        let id = api
            .create_playlist(source, name)
            .await
            .map_err(|error| format!("create_playlist 失败：{error}"))?
            .ok_or_else(|| "create_playlist 没有返回 listid".to_string())?;
        *created = Some(id);
        mark = probe_step(log, mark, "b 新建", 1)?;
        if before.contains(&id) {
            return Err(format!("新建返回的 listid={id} 已在测试前的集合里"));
        }
        println!("  [b] listid={id} name={name}");

        // c. 集合必须恰好是 B ∪ {id}，且名称匹配
        let after_create = probe_playlists(api).await?;
        let names: std::collections::BTreeMap<i64, String> = after_create
            .iter()
            .filter_map(|playlist| playlist.list_id.map(|id| (id, playlist.name.clone())))
            .collect();
        let current: std::collections::BTreeSet<i64> = names.keys().copied().collect();
        let mut expected = before.clone();
        expected.insert(id);
        if current != expected {
            return Err(format!(
                "新建后集合不一致：当前 {} 个，期望 {} 个",
                current.len(),
                expected.len()
            ));
        }
        match names.get(&id) {
            Some(actual) if actual == name => {}
            Some(actual) => return Err(format!("listid={id} 的名称是「{actual}」，与预期不符")),
            None => return Err(format!("listid={id} 不在歌单列表里")),
        }
        mark = probe_step(log, mark, "c 复核列表", 0)?;

        // d. 搜一首歌，加进临时歌单，再读回来取 file_id
        let songs = api
            .search_songs("晴天", 1, 30)
            .await
            .map_err(|error| format!("search_songs 失败：{error}"))?;
        let song = songs
            .into_iter()
            .next()
            .ok_or_else(|| "搜索没有结果".to_string())?;
        mark = probe_step(log, mark, "d0 搜索", 0)?;

        let written = api
            .add_tracks_to_playlist(source, id, std::slice::from_ref(&song))
            .await
            .map_err(|error| format!("add_tracks_to_playlist 失败：{error}"))?;
        mark = probe_step(log, mark, "d 加歌", 1)?;
        if written != 1 {
            return Err(format!("add_tracks_to_playlist 返回 {written}，期望 1"));
        }
        // 云端列表有写入延迟，等一拍再读；这只是等待，不是重试写请求。
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        let tracks = api
            .user_playlist_tracks(id, 1, 100, true)
            .await
            .map_err(|error| format!("读歌单失败：{error}"))?;
        mark = probe_step(log, mark, "d 复核", 0)?;
        let entry = tracks
            .iter()
            .find(|track| track.hash == song.hash)
            .ok_or_else(|| format!("加歌后歌单里没有 hash={} 的歌", song.hash))?;
        let file_id = entry
            .file_id
            .ok_or_else(|| "歌单条目没有 file_id，无法执行移除".to_string())?;
        println!("  歌单曲目 {} 首，目标条目 file_id={file_id}", tracks.len());

        // e. 从临时歌单移除
        let mut target = entry.clone();
        target.file_id = Some(file_id);
        let removed = api
            .remove_tracks_from_playlist(source, id, std::slice::from_ref(&target))
            .await
            .map_err(|error| format!("remove_tracks_from_playlist 失败：{error}"))?;
        mark = probe_step(log, mark, "e 移除", 1)?;
        if removed != 1 {
            return Err(format!(
                "remove_tracks_from_playlist 返回 {removed}，期望 1"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        let tracks = api
            .user_playlist_tracks(id, 1, 100, true)
            .await
            .map_err(|error| format!("读歌单失败：{error}"))?;
        mark = probe_step(log, mark, "e 复核", 0)?;
        if tracks.iter().any(|track| track.hash == song.hash) {
            return Err("移除后歌单里仍有这首歌".to_string());
        }

        // f. 删除前再断言一次，然后只删这一个
        let before_delete = probe_playlists(api).await?;
        match before_delete
            .iter()
            .find(|playlist| playlist.list_id == Some(id))
        {
            Some(playlist) if playlist.name == name => {}
            Some(playlist) => {
                return Err(format!("待删 listid={id} 的名称是「{}」", playlist.name));
            }
            None => return Err(format!("待删 listid={id} 已不在歌单列表里")),
        }
        mark = probe_step(log, mark, "f 复核", 0)?;
        api.delete_playlist(source, id)
            .await
            .map_err(|error| format!("delete_playlist 失败：{error}"))?;
        mark = probe_step(log, mark, "f 删除", 1)?;

        // g. 以云端列表为准，不看 delete 的返回值
        let final_ids = probe_ids(&probe_playlists(api).await?);
        probe_step(log, mark, "g 复核", 0)?;
        if &final_ids != before {
            return Err(format!(
                "删除后集合没有回到测试前：当前 {} 个，测试前 {} 个",
                final_ids.len(),
                before.len()
            ));
        }
        Ok(())
    }

    /// 失败时的清理：确认这个 id 仍是那个临时歌单，只删它，再复核集合。
    async fn probe_cleanup(
        api: &NativeApi,
        log: &std::path::Path,
        name: &str,
        id: i64,
        before: &std::collections::BTreeSet<i64>,
    ) -> bool {
        println!("  清理：只删除 listid={id}");
        let current = match probe_playlists(api).await {
            Ok(list) => list,
            Err(error) => {
                println!("  清理失败：{error}");
                return false;
            }
        };
        match current.iter().find(|playlist| playlist.list_id == Some(id)) {
            Some(playlist) if playlist.name == name => {}
            Some(playlist) => {
                println!(
                    "  清理中止：listid={id} 的名称是「{}」，不是临时歌单",
                    playlist.name
                );
                return false;
            }
            None => println!("  清理：listid={id} 已不在列表里"),
        }

        let mark = probe_mark(log);
        if let Err(error) = api.delete_playlist(SourceKind::KugouConcept, id).await {
            println!("  清理失败：delete_playlist 报错：{error}");
            return false;
        }
        if let Err(error) = probe_step(log, mark, "清理 删除", 1) {
            println!("  {error}");
            return false;
        }

        match probe_playlists(api).await {
            Ok(list) => {
                if probe_ids(&list) == *before {
                    println!("  清理完成，集合已回到测试前");
                    true
                } else {
                    println!(
                        "  清理后集合仍不一致：当前 {} 个，测试前 {} 个",
                        list.len(),
                        before.len()
                    );
                    false
                }
            }
            Err(error) => {
                println!("  清理后复核失败：{error}");
                false
            }
        }
    }

    /// 云歌单写接口的真实端到端验证。只在隔离配置下跑。
    #[tokio::test]
    #[ignore = "真实写接口：需要隔离配置（KUGOU_TUI_CONFIG_DIR）与真实登录态"]
    async fn probe_real_cloud_playlist_write() {
        let Ok(config_dir) = std::env::var("KUGOU_TUI_CONFIG_DIR") else {
            eprintln!(
                "先设 KUGOU_TUI_CONFIG_DIR=<隔离配置目录>（内含 config.toml 与 device.toml）"
            );
            return;
        };
        if config_dir.trim().is_empty() {
            eprintln!("KUGOU_TUI_CONFIG_DIR 是空值，拒绝运行");
            return;
        }

        let mut config = crate::config::Config::load();
        config.switch_source(SourceKind::KugouConcept);
        if !config.is_logged_in() {
            eprintln!("隔离配置 {config_dir} 里没有酷狗登录态，拒绝运行");
            return;
        }

        let log = crate::config::Config::log_path();
        crate::logger::init(&log).expect("初始化日志");
        println!("配置目录={config_dir}");
        println!("日志={}", log.display());

        let api = NativeApi::new(
            SourceKind::KugouConcept,
            config.cookie_header(),
            config.proxy.as_deref(),
        )
        .expect("构造 NativeApi");

        // a. 测试前的歌单 id 集合
        let mark = probe_mark(&log);
        let before_list = probe_playlists(&api).await.expect("读测试前歌单失败");
        let before = probe_ids(&before_list);
        probe_step(&log, mark, "a 测试前", 0).expect("测试前不应有写请求");
        println!("  [a] 测试前歌单 {} 个", before.len());
        for playlist in &before_list {
            println!("      listid={:?} name={}", playlist.list_id, playlist.name);
        }

        let mut created: Option<i64> = None;
        match probe_run(&api, &log, PROBE_PLAYLIST_NAME, &before, &mut created).await {
            Ok(()) => println!("PROBE OK"),
            Err(error) => {
                println!("PROBE FAILED: {error}");
                match created {
                    Some(id) => {
                        if probe_cleanup(&api, &log, PROBE_PLAYLIST_NAME, id, &before).await {
                            println!("已清理临时歌单 listid={id}");
                        } else {
                            println!("!!! 请手动删除 listid={id}（名称 {PROBE_PLAYLIST_NAME}）!!!");
                        }
                    }
                    None => println!("没有创建任何歌单，无需清理"),
                }
                panic!("云歌单写接口探针失败：{error}");
            }
        }
    }

    /// 清理一个遗留的临时歌单：只删名字等于 [`PROBE_PLAYLIST_NAME`] 的那一个。
    ///
    /// 探针失败时若连 listid 都没拿到，云端可能已经留下了歌单；这个入口用来
    /// 手动收尾，名字不匹配就拒绝删除。
    ///
    ///   KUGOU_TUI_CONFIG_DIR=/tmp/kt-write KUGOU_TUI_DEBUG=1 \
    ///   KUGOU_TUI_PROBE_DELETE_LISTID=5 \
    ///   cargo test probe_delete_leftover_playlist -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "清理遗留的临时歌单：需要 KUGOU_TUI_CONFIG_DIR 与 KUGOU_TUI_PROBE_DELETE_LISTID"]
    async fn probe_delete_leftover_playlist() {
        let Ok(config_dir) = std::env::var("KUGOU_TUI_CONFIG_DIR") else {
            eprintln!("先设 KUGOU_TUI_CONFIG_DIR=<隔离配置目录>");
            return;
        };
        let Ok(raw_id) = std::env::var("KUGOU_TUI_PROBE_DELETE_LISTID") else {
            eprintln!("先设 KUGOU_TUI_PROBE_DELETE_LISTID=<要删的 listid>");
            return;
        };
        let Ok(id) = raw_id.trim().parse::<i64>() else {
            eprintln!("KUGOU_TUI_PROBE_DELETE_LISTID 不是整数：{raw_id}");
            return;
        };

        let mut config = crate::config::Config::load();
        config.switch_source(SourceKind::KugouConcept);
        if !config.is_logged_in() {
            eprintln!("隔离配置 {config_dir} 里没有酷狗登录态，拒绝运行");
            return;
        }
        let log = crate::config::Config::log_path();
        crate::logger::init(&log).expect("初始化日志");

        let api = NativeApi::new(
            SourceKind::KugouConcept,
            config.cookie_header(),
            config.proxy.as_deref(),
        )
        .expect("构造 NativeApi");

        let list = api.user_playlists().await.expect("读歌单失败");
        match list.iter().find(|playlist| playlist.list_id == Some(id)) {
            Some(playlist) if playlist.name == PROBE_PLAYLIST_NAME => {
                println!("确认 listid={id} 是临时歌单「{}」，开始删除", playlist.name);
            }
            Some(playlist) => {
                eprintln!(
                    "拒绝删除：listid={id} 的名称是「{}」，不是 {PROBE_PLAYLIST_NAME}",
                    playlist.name
                );
                return;
            }
            None => {
                eprintln!("listid={id} 不在歌单列表里，无需删除");
                return;
            }
        }

        let before = probe_ids(&list);
        api.delete_playlist(SourceKind::KugouConcept, id)
            .await
            .expect("delete_playlist 失败");
        match api.user_playlists().await {
            Ok(after) => {
                let now = probe_ids(&after);
                let mut expected = before.clone();
                expected.remove(&id);
                if now == expected {
                    println!(
                        "已删除 listid={id}，歌单数 {} → {}",
                        before.len(),
                        now.len()
                    );
                } else {
                    println!("!!! 删除后集合不一致，请手动检查 listid={id} !!!");
                }
            }
            Err(error) => println!("删除后复核失败：{error}"),
        }
    }
}
