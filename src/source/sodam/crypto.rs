//! 汽水音频解密：`play_auth`（spade）密钥还原 + MP4/CENC 样本解密。
//!
//! # 为什么要解密
//!
//! 汽水下发的播放流**不是可直接解码的音频**，而是套了 MP4/CENC（Common Encryption）
//! 的加密流：密钥不在数据里，而在取流接口额外返回的 `play_auth` 字段中。这个字段
//! 是一段 base64 编码的 spade 密文，还原后得到 16 字节十六进制 AES 密钥。
//!
//! 拿到密钥后，按 MP4 里 `senc` box 描述的「每个样本的 IV + 哪些字节是明文」逐样本
//! 做 AES-128-CTR，密文段解出来、明文段原样保留，才能得到一个能被 rodio 解码的
//! 标准 MP4/M4A。
//!
//! # 与酷狗/网易云的根本差别
//!
//! 另两个音源给的是**明文直链**，丢给下载器就能播。汽水的直链必须先整首下完、
//! 解密、落盘，才能交给播放器——所以取链那一层不能只返回一个 URL，
//! 必须把「下载 + 解密」整件事做完（见 [`crate::source::sodam`]）。
//!
//! # 防御性取值
//!
//! MP4 里所有长度、样本数都是 32 位字段，损坏或恶意构造的文件可以声明出离谱的
//! 数值。这里对每个声明值都做**按实际可读数据量夹紧 + 硬上限**双重限制，
//! 避免一开口就申请十几 GB 内存。

use aes::cipher::{KeyIvInit, StreamCipher};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

use crate::error::{AppError, Result};

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// AES 分组长度（字节）。密钥与 IV 都是这个长度。
const AES_BLOCK_SIZE: usize = 16;

/// 防御性上限：单个 box 允许的最大样本条目数。
///
/// `stsz` / `senc` 里的样本数是 32 位字段，损坏文件可以声明约 43 亿条——按它
/// 预分配会直接申请十几 GB（Linux 上触发 OOM / zram 交换风暴）。
/// 正常音频（48kHz、每帧约 1024 采样）几小时也只有几万条，4M 足够宽裕。
const MAX_SAMPLE_ENTRIES: usize = 4 * 1024 * 1024;

/// MP4 box 视图：相对整份数据的偏移、总长、以及内容切片（已跳过 8 字节头）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mp4Box<'a> {
    /// box 头（size + type）在整份数据里的起始偏移。
    pub offset: usize,
    /// 整个 box 的字节数（含 8 字节头）。
    pub size: usize,
    /// box 的**内容**（不含头）。空 box 时长度为 0。
    pub data: &'a [u8],
}

/// CENC 里的一条 subsample 描述：前 `clear` 字节是明文，接着 `encrypted` 字节是密文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SencSubsample {
    pub clear: u16,
    pub encrypted: u32,
}

/// `senc` box 里的一个样本条目：自己的 IV，加上明文/密文分布。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SencSample {
    pub iv: Vec<u8>,
    pub subsamples: Vec<SencSubsample>,
}

/// 十六进制字符对应的数值。`0xFF` 表示不是合法的 base36 字符。
fn decode_base36(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'z' => byte - b'a' + 10,
        _ => 0xFF,
    }
}

/// 十六进制解码。不引额外依赖（`hex` 只有几十行代码，不值一个依赖）。
pub fn hex_decode(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        out.push((high * 16 + low) as u8);
    }
    Some(out)
}

/// spade 内层变换。
///
/// 输入是 `play_auth` 解出的中间字节，输出是**含有密钥长度前缀**的字节串：
/// 首字节是 base36 字符，指明末尾要跳过几个字节（尾部填充），后面才是密钥本身。
fn decrypt_spade_inner(key_bytes: &[u8]) -> Vec<u8> {
    // 前缀 0xFA 0x55 是这段算法的固定魔数，逐字节参与异或。
    let mut buff = Vec::with_capacity(key_bytes.len() + 2);
    buff.push(0xFA);
    buff.push(0x55);
    buff.extend_from_slice(key_bytes);

    let mut result = Vec::with_capacity(key_bytes.len());
    for (index, &byte) in key_bytes.iter().enumerate() {
        // 异或后减去「下标的置位数 + 21」；负值循环加 255 回到无符号区间。
        let mut value = (byte ^ buff[index]) as i32 - count_bits(index as u32) as i32 - 21;
        while value < 0 {
            value += 255;
        }
        result.push(value as u8);
    }
    result
}

/// 32 位整数二进制里 1 的个数。
fn count_bits(value: u32) -> u32 {
    value.count_ones()
}

/// 从 `play_auth` 还原十六进制 AES 密钥。
///
/// 流程：`base64` 解码 → 用首三字节异或算出尾部填充长度 → 去掉填充 →
///
/// spade 变换 → 读首字节的 base36 值得到「密钥有效长度」→ 截出密钥的十六进制文本。
pub fn extract_key(play_auth: &str) -> Result<String> {
    let raw = BASE64_STANDARD
        .decode(play_auth.trim())
        .map_err(|error| AppError::Other(format!("汽水 play_auth 不是合法 base64：{error}")))?;

    if raw.len() < 3 {
        return Err(AppError::Other("汽水 play_auth 长度不足".to_string()));
    }

    // 尾部填充长度藏在首三字节的异或里。
    let padding_len = (raw[0] ^ raw[1] ^ raw[2]) as i32 - 48;
    if padding_len < 0 || raw.len() < padding_len as usize + 2 {
        return Err(AppError::Other("汽水 play_auth 填充长度非法".to_string()));
    }
    let padding_len = padding_len as usize;

    let inner_input = &raw[1..raw.len() - padding_len];
    let decoded = decrypt_spade_inner(inner_input);
    if decoded.is_empty() {
        return Err(AppError::Other("汽水 play_auth 解密失败".to_string()));
    }

    // 首字节是 base36 字符，值即「末尾要跳过的字节数」（尾部填充）。
    let skip = decode_base36(decoded[0]) as usize;
    if decoded[0] == 0xFF {
        return Err(AppError::Other("汽水 play_auth 长度前缀非法".to_string()));
    }
    let end = decoded
        .len()
        .checked_sub(skip)
        .filter(|end| *end >= 1)
        .ok_or_else(|| AppError::Other("汽水 play_auth 长度前缀与实际长度不符".to_string()))?;

    Ok(String::from_utf8_lossy(&decoded[1..end]).to_string())
}

/// 整份音频解密：先从 `play_auth` 还原密钥，再逐样本解密。
pub fn decrypt_audio(file_data: &[u8], play_auth: &str) -> Result<Vec<u8>> {
    let hex_key = extract_key(play_auth)?;
    let key = hex_decode(&hex_key)
        .ok_or_else(|| AppError::Other("汽水 AES 密钥不是合法十六进制".to_string()))?;
    decrypt_audio_with_key(file_data, &key)
}

/// 与 [`decrypt_audio`] 相同，但直接接收 16 字节密钥（省掉一次 base64 + spade 往返）。
pub fn decrypt_audio_with_key(file_data: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    if key.len() != AES_BLOCK_SIZE {
        return Err(AppError::Other(format!(
            "汽水 AES 密钥长度应为 {AES_BLOCK_SIZE} 字节，实际 {}",
            key.len()
        )));
    }

    let moov = find_box(file_data, b"moov", 0, file_data.len())
        .ok_or_else(|| AppError::Other("汽水音频缺少 moov box".to_string()))?;

    // stbl 的深度不固定：正常布局是 moov → trak → mdia → minf → stbl，
    // 但也有封装直接把它挂在 moov 下。用递归查找一次覆盖两种，
    // 免得为「猜对层级」再写一遍退化路径。
    let stbl = find_box_deep(file_data, b"stbl", moov.offset + 8, moov.offset + moov.size)
        .ok_or_else(|| AppError::Other("汽水音频缺少 stbl box".to_string()))?;

    let stsz = find_box(file_data, b"stsz", stbl.offset + 8, stbl.offset + stbl.size)
        .ok_or_else(|| AppError::Other("汽水音频缺少 stsz box（样本尺寸表）".to_string()))?;
    let sample_sizes = parse_stsz(stsz.data);

    // senc 在不同封装里可能挂在 moov 下，也可能挂在 stbl 下，两处都找。
    let senc = find_box(file_data, b"senc", moov.offset + 8, moov.offset + moov.size)
        .or_else(|| find_box(file_data, b"senc", stbl.offset + 8, stbl.offset + stbl.size))
        .ok_or_else(|| {
            AppError::Other("汽水音频缺少 senc box（无加密信息，已下架？）".to_string())
        })?;
    let iv_size = default_per_sample_iv_size(file_data, stbl.offset, stbl.offset + stbl.size);
    let senc_samples = parse_senc(senc.data, iv_size);

    let mdat = find_box(file_data, b"mdat", 0, file_data.len())
        .ok_or_else(|| AppError::Other("汽水音频缺少 mdat box（音频数据）".to_string()))?;

    let mut output = file_data.to_vec();
    let data_start = mdat.offset + 8;
    // mdat 的长度同样来自文件头，不能直接拿来预分配 —— 以真实数据长度为上限。
    let capacity = mdat.size.saturating_sub(8).min(file_data.len());
    let mut decrypted_mdat: Vec<u8> = Vec::with_capacity(capacity);

    let mut read_ptr = data_start;
    for (index, &size) in sample_sizes.iter().enumerate() {
        let size = size as usize;
        if read_ptr.saturating_add(size) > output.len() {
            // 尺寸表与实际数据不符：到此为止，剩下的原样保留。
            break;
        }
        let chunk = &output[read_ptr..read_ptr + size];
        match senc_samples.get(index) {
            Some(sample) => {
                decrypted_mdat.extend_from_slice(&decrypt_senc_sample(key, chunk, sample))
            }
            None => decrypted_mdat.extend_from_slice(chunk),
        }
        read_ptr += size;
    }

    // 全部样本都处理完才能安全原地替换，否则会写坏后半段未解密的数据。
    if decrypted_mdat.len() != mdat.size.saturating_sub(8) {
        return Err(AppError::Other(format!(
            "汽水音频解密长度不匹配：算出 {} 字节，声明 {} 字节",
            decrypted_mdat.len(),
            mdat.size.saturating_sub(8)
        )));
    }
    output[data_start..data_start + decrypted_mdat.len()].copy_from_slice(&decrypted_mdat);

    // stsd 里的样本描述符被标成了 `enca`（加密音频），解码器不认。
    // 把它改回 `frma` 指向的原始格式（通常是 `mp4a`），否则 rodio 会直接跳过这条流。
    if let Some(stsd) = find_box(file_data, b"stsd", stbl.offset + 8, stbl.offset + stbl.size) {
        let start = stsd.offset;
        let end = stsd.offset + stsd.size;
        if let Some(index) = find_subslice(&output[start..end], b"enca") {
            let original = encrypted_sample_original_format(&output[start..end]);
            let target = start + index;
            if target + 4 <= output.len() {
                output[target..target + 4].copy_from_slice(&original);
            }
        }
    }

    Ok(output)
}

/// 单个样本的解密：按 subsample 描述，明文段直通、密文段做 AES-CTR。
///
/// 注意 AES-CTR 是**流式**的：同一个密钥+IV 下的所有段共用一条密钥流，
/// 所以明文段不能「跳过」（那会让后面的密文段错位），只能原样拷贝、
/// 但仍然要把它们消耗掉的密钥流字节走掉。
pub fn decrypt_senc_sample(key: &[u8], chunk: &[u8], sample: &SencSample) -> Vec<u8> {
    let mut iv = [0u8; AES_BLOCK_SIZE];
    let copy_len = sample.iv.len().min(AES_BLOCK_SIZE);
    iv[..copy_len].copy_from_slice(&sample.iv[..copy_len]);

    let mut cipher = Aes128Ctr::new(key.into(), (&iv).into());
    let mut dst = chunk.to_vec();

    if sample.subsamples.is_empty() {
        cipher.apply_keystream(&mut dst);
        return dst;
    }

    let mut pos = 0usize;
    for sub in &sample.subsamples {
        // 声明的长度可能超出实际数据（损坏文件），逐段夹紧。
        let clear = (sub.clear as usize).min(dst.len().saturating_sub(pos));
        // 明文段：dst 已是 chunk 的拷贝，这里显式写出来便于对照算法语义。
        if clear > 0 {
            dst[pos..pos + clear].copy_from_slice(&chunk[pos..pos + clear]);
        }
        pos += clear;
        if pos >= dst.len() {
            break;
        }

        let encrypted = (sub.encrypted as usize).min(dst.len() - pos);
        if encrypted > 0 {
            cipher.apply_keystream(&mut dst[pos..pos + encrypted]);
        }
        pos += encrypted;
        if pos >= dst.len() {
            break;
        }
    }
    // 声明的段没覆盖到样本末尾：剩余部分原样保留。
    if pos < dst.len() {
        dst[pos..].copy_from_slice(&chunk[pos..]);
    }
    dst
}

/// 从 `frma` box 读原始样本格式，失败则退回 `mp4a`。
pub fn encrypted_sample_original_format(stsd_data: &[u8]) -> [u8; 4] {
    let Some(index) = find_subslice(stsd_data, b"frma") else {
        return *b"mp4a";
    };
    // frma 是标准 box：索引往前 4 字节是它的 size。
    if index < 4 || index + 8 > stsd_data.len() {
        return *b"mp4a";
    }
    let size = u32::from_be_bytes([
        stsd_data[index - 4],
        stsd_data[index - 3],
        stsd_data[index - 2],
        stsd_data[index - 1],
    ]) as usize;
    if size < 12 || index - 4 + size > stsd_data.len() {
        return *b"mp4a";
    }
    [
        stsd_data[index + 4],
        stsd_data[index + 5],
        stsd_data[index + 6],
        stsd_data[index + 7],
    ]
}

/// 每个样本的 IV 长度：默认 8 字节，`tenc` 里可能声明 16。
pub fn default_per_sample_iv_size(data: &[u8], start: usize, end: usize) -> usize {
    match find_box_deep(data, b"tenc", start, end) {
        Some(tenc) if tenc.data.len() >= 8 => {
            let iv_size = tenc.data[7] as usize;
            if iv_size == 8 || iv_size == 16 {
                iv_size
            } else {
                8
            }
        }
        _ => 8,
    }
}

/// 在 `[start, end)` 这一层里找 box（不进子 box）。
pub fn find_box<'a>(
    data: &'a [u8],
    box_type: &[u8; 4],
    start: usize,
    end: usize,
) -> Option<Mp4Box<'a>> {
    let end = end.min(data.len());
    let mut pos = start;
    while pos + 8 <= end {
        let size =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        if size < 8 {
            break;
        }
        if &data[pos + 4..pos + 8] == box_type {
            let box_end = (pos + size).min(data.len());
            return Some(Mp4Box {
                offset: pos,
                size,
                data: &data[pos + 8..box_end],
            });
        }
        pos += size;
    }
    None
}

/// 递归查找：进入已知容器 box 逐层下探。
///
/// 与 [`find_box`] 的区别是它会**穿过** `moov`/`trak`/`mdia`/`minf`/`stbl` 这些容器，
/// 用于找不保证挂在固定层级的 box（`tenc` 就常见于 `stbl → stsd → enca → sinf → schi`）。
pub fn find_box_deep<'a>(
    data: &'a [u8],
    box_type: &[u8; 4],
    start: usize,
    end: usize,
) -> Option<Mp4Box<'a>> {
    let end = end.min(data.len());
    let mut pos = start;
    while pos + 8 <= end {
        let mut size =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        // size == 1 表示真实长度放在后面的 64 位字段里（ largesize ）。
        let mut header_size = 8usize;
        if size == 1 {
            if pos + 16 > end {
                break;
            }
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&data[pos + 8..pos + 16]);
            let size64 = u64::from_be_bytes(bytes);
            if size64 > (end - pos) as u64 {
                break;
            }
            size = size64 as usize;
            header_size = 16;
        }
        if size < header_size || pos + size > end {
            break;
        }
        let current_type = &data[pos + 4..pos + 8];
        if current_type == box_type {
            return Some(Mp4Box {
                offset: pos,
                size,
                data: &data[pos + header_size..pos + size],
            });
        }
        if let Some(child_start) = box_child_start(current_type, pos, header_size)
            && child_start < pos + size
            && let Some(found) = find_box_deep(data, box_type, child_start, pos + size)
        {
            return Some(found);
        }
        pos += size;
    }
    None
}

/// 哪些 box 带子 box，以及子 box 的起始偏移。
///
/// 偏移不是「头长 + 8」那么简单：`stsd` 前面还有版本/条目数字节，
/// `enca`/`mp4a` 前面还有 6 个 reserved + 2 字节数据引用索引。
fn box_child_start(box_type: &[u8], offset: usize, header_size: usize) -> Option<usize> {
    match box_type {
        b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"sinf" | b"schi" => {
            Some(offset + header_size)
        }
        b"stsd" => Some(offset + header_size + 8),
        b"enca" | b"mp4a" | b"alac" | b"fLaC" => Some(offset + header_size + 28),
        _ => None,
    }
}

/// 解析 `stsz`（样本尺寸表）。
///
/// `sample_size` 非 0 表示所有样本等长（表里没有尺寸数组）；为 0 时后面才是
/// 每样本 4 字节的尺寸数组。
pub fn parse_stsz(data: &[u8]) -> Vec<u32> {
    if data.len() < 12 {
        return Vec::new();
    }
    let fixed_size = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let declared = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;

    // 不信任头部声明的条目数：变长时按「实际能读多少条」夹紧，定长时用硬上限兜底。
    let count = if fixed_size != 0 {
        declared.min(MAX_SAMPLE_ENTRIES)
    } else {
        let available = data.len().saturating_sub(12) / 4;
        declared.min(available).min(MAX_SAMPLE_ENTRIES)
    };

    let mut sizes = vec![0u32; count];
    if fixed_size != 0 {
        sizes.fill(fixed_size as u32);
    } else {
        for (index, slot) in sizes.iter_mut().enumerate() {
            let start = 12 + index * 4;
            if start + 4 > data.len() {
                break;
            }
            *slot = u32::from_be_bytes([
                data[start],
                data[start + 1],
                data[start + 2],
                data[start + 3],
            ]);
        }
    }
    sizes
}

/// 解析 `senc` box。
///
/// flags 的第 1 位（`0x02`）表示每个样本后面还跟着 subsample 列表；
/// 没有这一位时整个样本按 `iv_size` 对齐、按连续密文处理。
pub fn parse_senc(data: &[u8], iv_size: usize) -> Vec<SencSample> {
    if data.len() < 8 {
        return Vec::new();
    }
    let iv_size = if iv_size == 8 || iv_size == 16 {
        iv_size
    } else {
        8
    };
    let flags = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) & 0x00FF_FFFF;
    let has_subsamples = flags & 0x02 != 0;
    let declared = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;

    // 每个样本至少占 iv_size 字节（带 subsample 时还要 2 字节计数），
    // 据此把声明值夹到数据能容纳的范围，再套硬上限。
    let per_sample = if has_subsamples { iv_size + 2 } else { iv_size };
    let max_by_data = data.len().saturating_sub(8) / per_sample.max(1);
    let count = declared.min(max_by_data).min(MAX_SAMPLE_ENTRIES);

    let mut samples = Vec::with_capacity(count);
    let mut ptr = 8usize;
    for _ in 0..count {
        if ptr + iv_size > data.len() {
            break;
        }
        let mut sample = SencSample {
            iv: data[ptr..ptr + iv_size].to_vec(),
            subsamples: Vec::new(),
        };
        ptr += iv_size;

        if has_subsamples {
            if ptr + 2 > data.len() {
                break;
            }
            let sub_count = u16::from_be_bytes([data[ptr], data[ptr + 1]]) as usize;
            ptr += 2;
            if ptr + sub_count * 6 > data.len() {
                break;
            }
            for _ in 0..sub_count {
                sample.subsamples.push(SencSubsample {
                    clear: u16::from_be_bytes([data[ptr], data[ptr + 1]]),
                    encrypted: u32::from_be_bytes([
                        data[ptr + 2],
                        data[ptr + 3],
                        data[ptr + 4],
                        data[ptr + 5],
                    ]),
                });
                ptr += 6;
            }
        }
        samples.push(sample);
    }
    samples
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::KeyIvInit;

    /// 造一个最小可解的「加密 MP4」：moov/stbl 下挂 stsz + senc，mdat 里放若干样本。
    ///
    /// `iv8` 是**每个样本的 IV**（8 字节，CENC 的常见长度）。这里不带 `tenc` box，
    /// 所以 `default_per_sample_iv_size` 会回落到 8——`senc` 里就必须写 8 字节，
    /// 否则解析出来的 IV 长度与实际写入的不一致，后面全错位。
    fn build_encrypted_mp4(key: &[u8; 16], iv8: &[u8; 8], samples: &[&[u8]]) -> Vec<u8> {
        // AES-CTR 的 IV 凑满 16 字节：不足的部分补 0（与 `decrypt_senc_sample` 一致）
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(iv8);

        let mut out: Vec<u8> = Vec::new();

        let mut senc_content: Vec<u8> = Vec::new();
        senc_content.extend_from_slice(&2u32.to_be_bytes()); // flags: has_subsamples
        senc_content.extend_from_slice(&(samples.len() as u32).to_be_bytes());
        for sample in samples {
            senc_content.extend_from_slice(iv8);
            senc_content.extend_from_slice(&1u16.to_be_bytes()); // 1 个 subsample
            senc_content.extend_from_slice(&0u16.to_be_bytes()); // clear = 0
            senc_content.extend_from_slice(&(sample.len() as u32).to_be_bytes());
        }

        // stsz：变长样本，每样本 4 字节
        let mut stsz_content: Vec<u8> = Vec::new();
        stsz_content.extend_from_slice(&0u32.to_be_bytes()); // version + flags
        stsz_content.extend_from_slice(&0u32.to_be_bytes()); // sample_size = 0 → 变长
        stsz_content.extend_from_slice(&(samples.len() as u32).to_be_bytes());
        for sample in samples {
            stsz_content.extend_from_slice(&(sample.len() as u32).to_be_bytes());
        }

        let mut stbl_content: Vec<u8> = Vec::new();
        push_box(&mut stbl_content, b"stsz", &stsz_content);
        push_box(&mut stbl_content, b"senc", &senc_content);

        let mut moov_content: Vec<u8> = Vec::new();
        push_box(&mut moov_content, b"stbl", &stbl_content);

        let mut mdat_content: Vec<u8> = Vec::new();
        for sample in samples {
            // clear=0，整段都是密文 —— 每个样本用**各自**的 IV 重新起流
            let mut cipher = Aes128Ctr::new(key.into(), (&iv).into());
            let mut buf = sample.to_vec();
            cipher.apply_keystream(&mut buf);
            mdat_content.extend_from_slice(&buf);
        }

        push_box(&mut out, b"moov", &moov_content);
        push_box(&mut out, b"mdat", &mdat_content);
        out
    }

    fn push_box(out: &mut Vec<u8>, box_type: &[u8; 4], content: &[u8]) {
        out.extend_from_slice(&((content.len() + 8) as u32).to_be_bytes());
        out.extend_from_slice(box_type);
        out.extend_from_slice(content);
    }

    #[test]
    fn hex_decode_pairs_nibbles() {
        assert_eq!(hex_decode("00ff10"), Some(vec![0x00, 0xff, 0x10]));
        // 奇数长度 / 非法字符都要被拒，而不是悄悄解出半个字节
        assert_eq!(hex_decode("abc"), None);
        assert_eq!(hex_decode("zz"), None);
        assert_eq!(hex_decode(""), None);
    }

    #[test]
    fn extract_key_rejects_garbage() {
        assert!(extract_key("不是 base64!!!").is_err());
        assert!(extract_key("").is_err());
        // 合法 base64 但长度不足 3 字节
        assert!(extract_key("YQ==").is_err());
    }

    #[test]
    fn extract_key_rejects_invalid_padding_length() {
        // 首字节 0xFF 异或 0xFF 异或 0xFF = 0xFF，减去 48 得正数，
        // 但 0xFF - 48 = 207，远大于数据长度 → 必须报错而不是 panic
        let raw = vec![0xFFu8, 0xFF, 0xFF, 0x01, 0x02];
        let encoded = BASE64_STANDARD.encode(&raw);
        assert!(extract_key(&encoded).is_err());
    }

    /// 反向构造一段合法的 `play_auth`（正向构造的逆运算）。
    ///
    /// `skip` 是「中间串首字节减 '0'」的值——它同时决定末尾要跳过几个字节。
    /// 取 0 与 3 各测一次：非零的 skip 会走到 `extract_key` 里那段
    /// 「按首字节回退长度」的分支，只测 0 覆盖不到。
    fn build_play_auth(key_hex: &str, skip: u8) -> String {
        // 中间串：首字节是 base36 字符（编码 skip），随后是密钥文本，
        // 末尾再补 skip 个填充字节。
        let mut tmp: Vec<u8> = Vec::new();
        tmp.push(b'0' + skip);
        tmp.extend_from_slice(key_hex.as_bytes());
        tmp.resize(tmp.len() + skip as usize, 0u8);

        // 正向是 out[i] = ((in[i] ^ buff[i]) - bits(i) - 21) mod 255，
        // 故 in[i] = (out[i] + bits(i) + 21) mod 255 ^ buff[i]，
        // 其中 buff = [0xFA, 0x55] + in 自身（所以 i>=2 时 buff[i] = in[i-2]）。
        let desired = |index: usize| -> u8 {
            ((tmp[index] as i32 + (index as u32).count_ones() as i32 + 21) % 255) as u8
        };

        let mut input = vec![0u8; tmp.len()];
        input[0] = desired(0) ^ 0xFA;
        if tmp.len() > 1 {
            input[1] = desired(1) ^ 0x55;
        }
        for index in 2..tmp.len() {
            input[index] = desired(index) ^ input[index - 2];
        }

        // 尾部填充长度藏在首三字节的异或里：b0 ^ b1 ^ b2 - 48 == padding_len。
        // 这里让 padding_len = 0（首字节与后两字节的异或恰为 48）。
        let b1 = input[0];
        let b2 = input[1];
        let b0 = 0x30u8 ^ b1 ^ b2;

        let mut bytes_data = vec![b0, b1, b2];
        bytes_data.extend_from_slice(&input[2..]);
        BASE64_STANDARD.encode(bytes_data)
    }

    #[test]
    fn extract_key_roundtrips_a_synthesized_payload() {
        let key_hex = "00112233445566778899aabbccddeeff";
        for skip in [0u8, 3u8] {
            let play_auth = build_play_auth(key_hex, skip);
            assert_eq!(
                extract_key(&play_auth).unwrap(),
                key_hex,
                "skip = {skip} 时应还原出同一个密钥"
            );
        }
    }

    #[test]
    fn parse_stsz_clamps_declared_count_to_available_data() {
        // 声明 1000 条，但实际只带 1 条尺寸 → 只能解出 1 条
        let mut data = Vec::new();
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&1000u32.to_be_bytes());
        data.extend_from_slice(&64u32.to_be_bytes());
        assert_eq!(parse_stsz(&data), vec![64]);
    }

    #[test]
    fn parse_stsz_handles_fixed_size() {
        let mut data = Vec::new();
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&128u32.to_be_bytes()); // 所有样本 128 字节
        data.extend_from_slice(&3u32.to_be_bytes());
        assert_eq!(parse_stsz(&data), vec![128, 128, 128]);
    }

    #[test]
    fn parse_stsz_on_truncated_input_is_empty() {
        assert!(parse_stsz(&[0u8; 4]).is_empty());
    }

    #[test]
    fn parse_senc_clamps_to_available_bytes() {
        // 声明 5 个样本，但数据只够 1 个完整的（1 个 iv + 1 个 subsample 条目）
        let mut data = vec![0u8, 0, 0, 2]; // flags 的第 1 位置 1 = 带 subsample
        data.extend_from_slice(&5u32.to_be_bytes());
        data.extend_from_slice(&[0u8; 8]); // iv
        data.extend_from_slice(&1u16.to_be_bytes()); // subsample 条目数
        data.extend_from_slice(&0u16.to_be_bytes()); // clear
        data.extend_from_slice(&16u32.to_be_bytes()); // encrypted
        let samples = parse_senc(&data, 8);
        assert_eq!(
            samples.len(),
            1,
            "必须按实际可读数据裁剪，不能按声明的 5 条"
        );
        assert_eq!(samples[0].iv.len(), 8);
        assert_eq!(samples[0].subsamples.len(), 1);
    }

    /// 没有 subsample 标志位时，整个样本按连续密文处理（不读 subsample 列表）。
    #[test]
    fn parse_senc_without_subsample_flag_uses_iv_only() {
        let mut data = vec![0u8, 0, 0, 0]; // flags = 0
        data.extend_from_slice(&2u32.to_be_bytes());
        data.extend_from_slice(&[1u8; 8]);
        data.extend_from_slice(&[2u8; 8]);
        let samples = parse_senc(&data, 8);
        assert_eq!(samples.len(), 2);
        assert!(samples.iter().all(|s| s.subsamples.is_empty()));
    }

    #[test]
    fn parse_senc_on_empty_is_empty() {
        assert!(parse_senc(&[], 8).is_empty());
        assert!(parse_senc(&[0u8; 4], 8).is_empty());
    }

    #[test]
    fn decrypt_senc_sample_without_subsamples_decrypts_whole_chunk() {
        let key = [7u8; 16];
        let iv = [3u8; 16];
        let plain = b"hello soda music";

        let mut cipher = Aes128Ctr::new((&key).into(), (&iv).into());
        let mut encrypted = plain.to_vec();
        cipher.apply_keystream(&mut encrypted);

        let sample = SencSample {
            iv: iv.to_vec(),
            subsamples: Vec::new(),
        };
        assert_eq!(decrypt_senc_sample(&key, &encrypted, &sample), plain);
    }

    /// 明文段必须原样保留、密文段必须解开——且两段拼起来要等于整个样本。
    ///
    /// 构造时**只对密文段**加 keystream：按 CENC 语义，明文段是**没有**被加密的
    /// （它本来就是明文，比如 MP4 的头部），密钥流只覆盖密文段那些字节。
    /// 若把整段一起加密，解密时明文段会被解成乱码——这正是本用例要守住的行为。
    #[test]
    fn decrypt_senc_sample_preserves_clear_segments() {
        let key = [0x11u8; 16];
        let iv = [0x22u8; 16];
        let clear_part: &[u8] = b"HEAD";
        let secret_part: &[u8] = b"secret-payload";

        let mut cipher = Aes128Ctr::new((&key).into(), (&iv).into());
        let mut encrypted_secret = secret_part.to_vec();
        cipher.apply_keystream(&mut encrypted_secret);

        // 明文段原样在前，密文段在后
        let mut encrypted = clear_part.to_vec();
        encrypted.extend_from_slice(&encrypted_secret);

        let sample = SencSample {
            iv: iv.to_vec(),
            subsamples: vec![SencSubsample {
                clear: clear_part.len() as u16,
                encrypted: secret_part.len() as u32,
            }],
        };
        let decrypted = decrypt_senc_sample(&key, &encrypted, &sample);
        assert_eq!(&decrypted[..clear_part.len()], clear_part, "明文段应原样");
        assert_eq!(&decrypted[clear_part.len()..], secret_part, "密文段应解开");
    }

    /// 声明的段长度超出实际数据时必须夹紧，不能 panic 也不能越界写。
    #[test]
    fn decrypt_senc_sample_clamps_overlong_declarations() {
        let key = [0x33u8; 16];
        let iv = [0x44u8; 16];
        let chunk = b"0123456789";
        let sample = SencSample {
            iv: iv.to_vec(),
            subsamples: vec![SencSubsample {
                clear: 9999,
                encrypted: 9999,
            }],
        };
        let out = decrypt_senc_sample(&key, chunk, &sample);
        assert_eq!(out.len(), chunk.len(), "长度必须与输入一致");
    }

    /// 端到端：造一个加密 MP4，解密后应拿回原始样本字节。
    #[test]
    fn decrypt_audio_restores_plaintext_samples() {
        let key = [0x5Au8; 16];
        let iv = [0x6Bu8; 8];
        let s1: &[u8] = &[0x11u8; 32];
        let s2: &[u8] = &[0x22u8; 48];
        let encrypted = build_encrypted_mp4(&key, &iv, &[s1, s2]);

        let out = decrypt_audio_with_key(&encrypted, &key).unwrap();
        // mdat 起点：moov(stbl(stsz, senc)) 之后
        let mdat = find_box(&out, b"mdat", 0, out.len()).unwrap();
        let data = &out[mdat.offset + 8..mdat.offset + mdat.size];
        assert_eq!(&data[..32], s1, "第一个样本应还原");
        assert_eq!(&data[32..], s2, "第二个样本应还原");
    }

    /// 用**错误**的密钥解密，结果必须与原文不同——否则说明「什么都没做」也算过。
    #[test]
    fn decrypt_audio_with_wrong_key_does_not_return_plaintext() {
        let key = [0x5Au8; 16];
        let iv = [0x6Bu8; 8];
        let sample: &[u8] = &[0x11u8; 32];
        let encrypted = build_encrypted_mp4(&key, &iv, &[sample]);

        let out = decrypt_audio_with_key(&encrypted, &[0x00u8; 16]).unwrap();
        let mdat = find_box(&out, b"mdat", 0, out.len()).unwrap();
        let data = &out[mdat.offset + 8..mdat.offset + mdat.size];
        assert_ne!(data, sample, "错密钥不该还原出原文");
    }

    #[test]
    fn decrypt_audio_rejects_wrong_key_length() {
        let err = decrypt_audio_with_key(&[0u8; 8], &[0u8; 8]).unwrap_err();
        assert!(err.to_string().contains("密钥长度"), "实际：{err}");
    }

    #[test]
    fn decrypt_audio_reports_missing_boxes() {
        let err = decrypt_audio_with_key(&[0u8; 64], &[0u8; 16]).unwrap_err();
        assert!(err.to_string().contains("moov"), "实际：{err}");
    }

    #[test]
    fn find_box_only_scans_current_level() {
        let data = build_test_nested();
        // moov 在顶层能找到
        assert!(find_box(&data, b"moov", 0, data.len()).is_some());
        // stbl 嵌在 moov 里，同层扫描（start=0）找不到
        assert!(find_box(&data, b"stbl", 0, data.len()).is_none());
        // 递归查找能穿透容器
        assert!(find_box_deep(&data, b"stbl", 0, data.len()).is_some());
    }

    fn build_test_nested() -> Vec<u8> {
        let mut stbl = Vec::new();
        push_box(&mut stbl, b"stsd", &[0u8; 4]);
        let mut moov = Vec::new();
        push_box(&mut moov, b"stbl", &stbl);
        let mut out = Vec::new();
        push_box(&mut out, b"moov", &moov);
        out
    }

    #[test]
    fn original_format_falls_back_to_mp4a() {
        assert_eq!(encrypted_sample_original_format(b"no frma here"), *b"mp4a");
        assert_eq!(encrypted_sample_original_format(&[]), *b"mp4a");
    }

    #[test]
    fn original_format_reads_frma() {
        let mut stsd = vec![0u8; 8];
        push_box(&mut stsd, b"frma", b"mp4a");
        assert_eq!(encrypted_sample_original_format(&stsd), *b"mp4a");
    }
}
