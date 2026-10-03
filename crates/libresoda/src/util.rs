//! 与上游 `music-lib/utils` 及 `soda` 包内部工具函数对应的通用工具。

use std::collections::BTreeMap;

/// 有序查询参数集合，语义对齐 Go 的 `url.Values`。
///
/// * [`Params::encode`] 按 key 字典序输出（同 Go `url.Values.Encode`）。
/// * [`Params::encode_order`] 按给定顺序输出（用于对参数顺序敏感的口令接口）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Params {
    entries: Vec<(String, String)>,
}

impl Params {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut params = Params::new();
        for (key, value) in pairs {
            params.set(key, value);
        }
        params
    }

    /// 设置参数；同 key 覆盖旧值。
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        let value = value.into();
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
            return;
        }
        self.entries.push((key, value));
    }

    /// 追加参数；同 key 保留多个值（等价 Go `url.Values.Add`）。
    ///
    /// 客户端把数组型查询参数编码成**重复 key**（`item_types=a&item_types=b`），
    /// 解析端需要这个语义才能对齐。
    pub fn add(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.entries.push((key.into(), value.into()));
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn remove(&mut self, key: &str) {
        self.entries.retain(|(k, _)| k != key);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 按 key 字典序编码（等价 Go `url.Values.Encode`）。
    pub fn encode(&self) -> String {
        let mut sorted: Vec<&(String, String)> = self.entries.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        sorted
            .into_iter()
            .map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v)))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// 按给定顺序编码，`order` 中未出现的 key 追加在后面（按原插入序）。
    pub fn encode_order(&self, order: &[&str]) -> String {
        let mut parts: Vec<String> = Vec::new();
        for key in order {
            for (k, v) in &self.entries {
                if k == key {
                    parts.push(format!("{}={}", query_escape(k), query_escape(v)));
                }
            }
        }
        // 等价上游 `sodaEncodeOrderedForm`：未出现在 order 中的 key 按字典序追加。
        let mut rest: Vec<&(String, String)> = self
            .entries
            .iter()
            .filter(|(k, _)| !order.contains(&k.as_str()))
            .collect();
        rest.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, v) in rest {
            parts.push(format!("{}={}", query_escape(k), query_escape(v)));
        }
        parts.join("&")
    }
}

/// Go `url.QueryEscape` 语义：空格变 `+`，`A-Za-z0-9-_.~` 直通，其余 `%XX`。
pub fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Go `url.QueryUnescape` 语义：`+` 变空格，`%XX` 解码；非法转义返回 `None`。
pub fn query_unescape(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' => {
                if index + 2 >= bytes.len() {
                    return None;
                }
                let high = (bytes[index + 1] as char).to_digit(16)?;
                let low = (bytes[index + 2] as char).to_digit(16)?;
                out.push((high * 16 + low) as u8);
                index += 3;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// 从 JSON 对象里按候选 key 取字符串（等价上游 `sodaJSONString`）。
pub fn json_string(values: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = values.get(*key) {
            let text = any_string(value);
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

/// 从 JSON 对象里按候选 key 取字符串列表里的第一个非空值（等价 `sodaJSONFirstString`）。
pub fn json_first_string(
    values: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> String {
    for key in keys {
        let Some(serde_json::Value::Array(items)) = values.get(*key) else {
            continue;
        };
        for item in items {
            let text = any_string(item);
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

/// 把 JSON 值当字符串处理：字符串直接 trim，数组取第一个非空（等价 `sodaAnyString`）。
pub fn any_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.trim().to_string(),
        serde_json::Value::Array(items) => {
            for item in items {
                let text = any_string(item);
                if !text.is_empty() {
                    return text;
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

/// 数字/数字字符串取值（等价 `sodaJSONFloat`）。
pub fn json_float(values: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> f64 {
    for key in keys {
        let Some(value) = values.get(*key) else {
            continue;
        };
        match value {
            serde_json::Value::Number(number) => {
                if let Some(as_f64) = number.as_f64() {
                    return as_f64;
                }
            }
            serde_json::Value::String(text) => {
                if let Ok(parsed) = text.trim().parse::<f64>() {
                    return parsed;
                }
            }
            _ => {}
        }
    }
    0.0
}

/// 等价 `sodaJSONInt`（四舍五入到最近整数）。
pub fn json_int(values: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> i64 {
    (json_float(values, keys) + 0.5) as i64
}

/// 等价 `sodaFirstNonEmpty`。
pub fn first_non_empty(values: &[&str]) -> String {
    for value in values {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    String::new()
}

/// 等价 `sodaJoinArtists`：非空名字用 `" / "` 连接。
pub fn join_artists<'a, I>(names: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let mut parts: Vec<String> = Vec::new();
    for name in names {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    parts.join(" / ")
}

/// 去掉 `-`、`_`、空格并转小写（上游 `sodaQualityRank` / `sodaVideoModelQualityHint` 使用）。
pub fn normalize_token(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .chars()
        .filter(|ch| *ch != '-' && *ch != '_' && *ch != ' ')
        .collect()
}

/// 判断字符串是否为纯数字且非空（等价 `isSodaDigits`）。
pub fn is_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// 递归收集 JSON 对象里所有出现在 `field` 上的字符串（等价 `findSodaJSONString` 的深度优先版本）。
pub fn collect_strings(value: &serde_json::Value, field: &str, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key == field {
                    let text = any_string(child);
                    if !text.is_empty() {
                        out.push(text);
                    }
                }
                collect_strings(child, field, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_strings(item, field, out);
            }
        }
        _ => {}
    }
}

/// 从 `map` 里取子对象，等价上游对 `video_meta` / `encrypt_info` 的取值方式。
pub fn json_object<'a>(
    values: &'a serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    for key in keys {
        if let Some(serde_json::Value::Object(child)) = values.get(*key) {
            return Some(child);
        }
    }
    None
}

/// 统计 32 位整数二进制中 1 的个数（等价上游 `bitcount`）。
pub fn bitcount(value: u32) -> u32 {
    value.count_ones()
}

/// 便捷构造：把 `BTreeMap<&str, &str>` 转成 `BTreeMap<String, String>`。
pub fn extra_from_pairs<I, K, V>(pairs: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    pairs
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
}

/// 当前 Unix 时间戳（毫秒），等价 Go 的 `time.Now().UnixMilli()`。
pub fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(_) => 0,
    }
}

/// 用系统默认浏览器打开一个地址（扫码二次验证窗口用）。
///
/// * macOS：`open`
/// * Windows：`cmd /C start "" <url>`（`start` 会把第一个带引号的参数当窗口标题，
///   所以先传一个空标题，防止 URL 被吞）
/// * 其他（Linux/BSD）：`xdg-open`
///
/// 只负责「拉起」，不等待页面加载；返回错误时调用方可提示用户手动复制地址。
pub fn open_system_browser(url: &str) -> std::io::Result<()> {
    use std::process::Command;

    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };
    let status = command.status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "打开浏览器失败（exit={}）",
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_default()
        )));
    }
    Ok(())
}

/// 生成一个 v4 UUID 字符串（客户端在 `sug_search_id` 等场景用）。
///
/// 只用系统随机源，不引入 `rand`/`uuid` 依赖；读不到随机源时退化为时间戳，
/// 保证函数总能返回一个形状合法的 UUID。
pub fn random_uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    let filled = std::fs::File::open("/dev/urandom")
        .ok()
        .and_then(|mut file| {
            use std::io::Read;
            let mut buf = [0u8; 16];
            file.read_exact(&mut buf).ok().map(|_| buf)
        });
    match filled {
        Some(buf) => bytes = buf,
        None => {
            let seed = now_millis() as u128;
            bytes.copy_from_slice(&seed.to_le_bytes()[..16]);
        }
    }
    // 版本位 4、变体位 10xx（RFC 4122）
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}
