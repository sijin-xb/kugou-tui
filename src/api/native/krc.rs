//! KRC 歌词解密。
//!
//! 来源：KuGouMusicApi v1.6.0 `util/util.js` 的 `decodeLyrics`
//! （commit a5a98013cce79fe0ae2ad65fc84b68176ebcfc1e）。
//!
//! # 算法（**不是 AES**）
//!
//! 任务描述里写的是「Base64 → AES-128-ECB → zlib」，与上游实际实现不符：
//!
//! 1. Base64 解码；
//! 2. **丢掉前 4 字节**（真实样本里是 ASCII `krc1`）；
//! 3. 与 16 字节固定密钥按 `index % 16` **循环异或**；
//! 4. zlib 解压（`pako.inflate`）；
//! 5. UTF-8 解码。
//!
//! 全程没有 AES。按任务描述去写 AES 会得到一个永远解不开的实现，
//! 且报错只会是「解压失败」这种无指向的信息。
//!
//! # 失败时的行为差异
//!
//! 上游 `decodeLyrics` 解不开时**返回空字符串**，不抛异常——调用方无法区分
//! 「没有歌词」和「解密失败」。这里改成返回 `Err`，由调用方决定怎么处理；
//! 阶段 4 接入歌词链路时再对齐成「空歌词」的语义。

use std::io::Read;

use base64::Engine;

use crate::error::{AppError, Result};

/// 上游 `util/util.js` 的 `enKey`。
const XOR_KEY: [u8; 16] = [
    64, 71, 97, 119, 94, 50, 116, 71, 81, 54, 49, 45, 206, 210, 110, 105,
];

/// 头部长度：上游 `bytes.slice(4)`。
const HEADER_LEN: usize = 4;

/// 解密 KRC 密文（Base64 文本）为 KRC 明文。
///
/// 对应上游 `util/util.js` 的 `decodeLyrics(val)`（string 分支）。
pub fn decode(encoded: &str) -> Result<String> {
    let compact: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
    // 上游用 `Buffer.from(val, 'base64')`，对缺省填充是宽容的；这里两种都试。
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&compact)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&compact))
        .map_err(|error| AppError::Other(format!("KRC Base64 解码失败：{error}")))?;
    decode_bytes(&bytes)
}

/// 解密 KRC 密文字节。
///
/// 对应上游 `util/util.js` 的 `decodeLyrics(val)`（Uint8Array/Buffer 分支）。
pub fn decode_bytes(bytes: &[u8]) -> Result<String> {
    if bytes.len() <= HEADER_LEN {
        return Err(AppError::Other(format!(
            "KRC 密文只有 {} 字节，不足以去掉 {HEADER_LEN} 字节头",
            bytes.len()
        )));
    }

    let mut payload = bytes[HEADER_LEN..].to_vec();
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= XOR_KEY[index % XOR_KEY.len()];
    }

    let mut inflater = flate2::read::ZlibDecoder::new(payload.as_slice());
    let mut plain = Vec::new();
    inflater
        .read_to_end(&mut plain)
        .map_err(|error| AppError::Other(format!("KRC zlib 解压失败：{error}")))?;

    String::from_utf8(plain)
        .map_err(|error| AppError::Other(format!("KRC 明文不是 UTF-8：{error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实样本：`/tmp/krc_probe.json` 的 `content` 字段。
    ///
    /// 取样本的方式（2026-10-08，本机 KuGouMusicApi）：
    ///
    /// ```text
    /// curl "http://127.0.0.1:3001/lyric?id=19525574&accesskey=0123456789ABCDEF0123456789ABCDEF&fmt=krc&charset=utf8&decode=true"
    /// ```
    ///
    /// 同一响应里 `decodeContent` 是上游 `decodeLyrics` 的输出，用作对照。
    const SAMPLE_B64: &str = "a3JjMTjbTPnlfLd3RbDGZ7PVaC9P+aX5MyuCETN0s6dEEyC1lGJFwxexhkkZxjchYppzeQIbRSlYQ+29W+5n4rwskfWveFusqf3djLQI7fg3ol68a+42YGTdg12Kj43DDGIbG/zpDef4pdWKJAWmJN/wfVWFHut+az5ujWL9SLPr7otLzbCrv2mc3OUJGFJGJQQBYcZDJNvA/26cm85zhGWww2JrKl4pRn6VnnizkuhbS3I2+7uT05FCrV5mVHY+tsJs4TJGF8BKNvBhUSjPkaOGPn0SqJ+ZwSbTRXWmSNo7qMl5JlNxb47fs8oTojit5ySYHrPlcbCW8VW9rM6RHQgerU0s2A7w3U3phg4V";

    /// 与样本同一响应的 `decodeContent`（上游解密结果）。
    const SAMPLE_PLAIN: &str = "\u{feff}[id:$00000000]\r\n[ar:arkady sevidov]\r\n[ti:June]\r\n[by:]\r\n[hash:4399c9872c7235b60b58ce88dc487897]\r\n[al:]\r\n[sign:]\r\n[qq:]\r\n[total:320317]\r\n[offset:0]\r\n[language:eyJjb250ZW50IjpbXSwidmVyc2lvbiI6MX0=]\r\n[1589,320317]<0,354,0>纯<354,505,0>音<859,406,0>乐<1265,304,0>，<1569,252,0>请<1821,405,0>欣<2226,303,0>赏\r\n";

    #[test]
    fn xor_key_matches_upstream() {
        assert_eq!(
            XOR_KEY,
            [
                64, 71, 97, 119, 94, 50, 116, 71, 81, 54, 49, 45, 206, 210, 110, 105
            ]
        );
    }

    #[test]
    fn rejects_a_short_payload() {
        assert!(decode_bytes(&[0x6b, 0x72, 0x63, 0x31]).is_err());
        assert!(decode("a3JjMQ==").is_err());
    }

    #[test]
    fn reports_a_clear_error_on_plain_text_input() {
        // 明文 KRC（本机 quickshell 缓存里的那些）必然解不开，且错误要能读。
        let plain = b"[id:$00000000]\r\n[ar:x]\r\n[1589,320317]<0,354,0>a";
        let error = decode_bytes(plain).expect_err("明文不该被解成 KRC");
        assert!(
            error.to_string().contains("zlib") || error.to_string().contains("UTF-8"),
            "错误信息应指向解压或编码：{error}"
        );
    }

    #[test]
    fn decodes_the_real_sample() {
        let plain = decode(SAMPLE_B64).expect("真实样本应当能解开");
        assert_eq!(plain, SAMPLE_PLAIN);
    }
}
