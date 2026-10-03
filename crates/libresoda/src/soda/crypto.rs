//! 汽水音频解密：`play_auth`（spade）密钥还原 + MP4/CENC 样本解密。
//!
//! 逐函数对应上游 `soda/crypto.go`。

use crate::error::{Result, SodaError};
use crate::util::bitcount;
use aes::cipher::{KeyIvInit, StreamCipher};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

const AES_BLOCK_SIZE: usize = 16;

/// 防御性上限：单个 box 允许的最大样本条目数。
///
/// MP4 头里的 `sample_count` 是 32 位字段，损坏或被截断的文件可以声明约 43 亿条，
/// 直接按它预分配会申请十几 GB 内存（Linux 上会触发 OOM / zram 交换风暴）。
/// 正常音频（48kHz、每帧约 1024 采样）即使数小时也只有几万条，4M 足够宽裕。
const MAX_SAMPLE_ENTRIES: usize = 4 * 1024 * 1024;

/// MP4 box 视图（相对 `data` 的偏移/长度 + 内容切片）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mp4Box<'a> {
    pub offset: usize,
    pub size: usize,
    pub data: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SencSubsample {
    pub clear: u16,
    pub encrypted: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SencSample {
    pub iv: Vec<u8>,
    pub subsamples: Vec<SencSubsample>,
}

/// 等价 `DecryptAudio`：整文件解密（`play_auth` 解出 AES 密钥，再按 senc 样本解密）。
pub fn decrypt_audio(file_data: &[u8], play_auth: &str) -> Result<Vec<u8>> {
    let hex_key = extract_key(play_auth)?;
    let key_bytes = hex_decode(&hex_key).ok_or_else(|| SodaError::crypto("invalid hex key"))?;
    decrypt_audio_with_key(file_data, &key_bytes)
}

/// 与 [`decrypt_audio`] 相同，但直接接收 16 字节 AES 密钥。
///
/// 便于调用方自己管理密钥（例如已经用 [`extract_key`] 解过一次），也方便测试。
pub fn decrypt_audio_with_key(file_data: &[u8], key_bytes: &[u8]) -> Result<Vec<u8>> {
    if key_bytes.len() != 16 {
        return Err(SodaError::crypto("invalid aes key length"));
    }

    let data = file_data;
    let moov = find_box(data, b"moov", 0, data.len())
        .ok_or_else(|| SodaError::crypto("moov box not found"))?;

    let mut stbl = find_box(data, b"stbl", moov.offset, moov.offset + moov.size);
    if stbl.is_none() {
        // 退化路径：moov → trak → mdia → minf → stbl
        if let Some(trak) = find_box(data, b"trak", moov.offset + 8, moov.offset + moov.size) {
            if let Some(mdia) = find_box(data, b"mdia", trak.offset + 8, trak.offset + trak.size) {
                if let Some(minf) =
                    find_box(data, b"minf", mdia.offset + 8, mdia.offset + mdia.size)
                {
                    stbl = find_box(data, b"stbl", minf.offset + 8, minf.offset + minf.size);
                }
            }
        }
    }
    let stbl = stbl.ok_or_else(|| SodaError::crypto("stbl box not found"))?;

    let stsz = find_box(data, b"stsz", stbl.offset + 8, stbl.offset + stbl.size)
        .ok_or_else(|| SodaError::crypto("stsz box not found"))?;
    let sample_sizes = parse_stsz(stsz.data);

    let senc = find_box(data, b"senc", moov.offset + 8, moov.offset + moov.size)
        .or_else(|| find_box(data, b"senc", stbl.offset + 8, stbl.offset + stbl.size))
        .ok_or_else(|| SodaError::crypto("senc box not found"))?;
    let per_sample_iv_size = default_per_sample_iv_size(data, stbl.offset, stbl.offset + stbl.size);
    let senc_samples = parse_senc(senc.data, per_sample_iv_size);

    let mdat = find_box(data, b"mdat", 0, data.len())
        .ok_or_else(|| SodaError::crypto("mdat box not found"))?;

    let mut decrypted_data = data.to_vec();

    let mut read_ptr = mdat.offset + 8;
    // 防御：mdat 的长度同样来自文件头，不能直接拿来预分配 —— 以真实文件长度为上限。
    let mdat_capacity = mdat.size.saturating_sub(8).min(data.len());
    let mut decrypted_mdat: Vec<u8> = Vec::with_capacity(mdat_capacity);

    for (index, size) in sample_sizes.iter().enumerate() {
        let size = *size as usize;
        if read_ptr + size > decrypted_data.len() {
            break;
        }
        let chunk = &decrypted_data[read_ptr..read_ptr + size];

        if let Some(sample) = senc_samples.get(index) {
            let decrypted = decrypt_senc_sample(key_bytes, chunk, sample);
            decrypted_mdat.extend_from_slice(&decrypted);
        } else {
            decrypted_mdat.extend_from_slice(chunk);
        }
        read_ptr += size;
    }

    if decrypted_mdat.len() == mdat.size - 8 {
        let start = mdat.offset + 8;
        decrypted_data[start..start + decrypted_mdat.len()].copy_from_slice(&decrypted_mdat);
    } else {
        return Err(SodaError::crypto("decrypted size mismatch"));
    }

    // 把 stsd 里的 `enca` 还原成原始编码格式（`frma` 指向的 4 字节）
    if let Some(stsd) = find_box(data, b"stsd", stbl.offset + 8, stbl.offset + stbl.size) {
        let start = stsd.offset;
        let end = stsd.offset + stsd.size;
        if let Some(index) = find_subslice(&decrypted_data[start..end], b"enca") {
            let original = encrypted_sample_original_format(&decrypted_data[start..end]);
            let target = start + index;
            if target + 4 <= decrypted_data.len() {
                decrypted_data[target..target + 4].copy_from_slice(&original);
            }
        }
    }

    Ok(decrypted_data)
}

/// 等价 `encryptedSampleOriginalFormat`：从 `frma` box 里读原始格式，失败则退回 `mp4a`。
pub fn encrypted_sample_original_format(stsd_data: &[u8]) -> [u8; 4] {
    let Some(idx) = find_subslice(stsd_data, b"frma") else {
        return *b"mp4a";
    };
    if idx < 4 || idx + 8 > stsd_data.len() {
        return *b"mp4a";
    }
    let size = u32::from_be_bytes([
        stsd_data[idx - 4],
        stsd_data[idx - 3],
        stsd_data[idx - 2],
        stsd_data[idx - 1],
    ]) as usize;
    if size < 12 || idx - 4 + size > stsd_data.len() {
        return *b"mp4a";
    }
    [
        stsd_data[idx + 4],
        stsd_data[idx + 5],
        stsd_data[idx + 6],
        stsd_data[idx + 7],
    ]
}

/// 等价 `defaultPerSampleIVSize`：默认 8 字节，允许 16。
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

/// 等价 `decryptSencSample`：按 subsample 描述，明文段直通、密文段做 AES-CTR。
pub fn decrypt_senc_sample(key: &[u8], chunk: &[u8], sample: &SencSample) -> Vec<u8> {
    let mut iv = [0u8; AES_BLOCK_SIZE];
    let copy_len = sample.iv.len().min(AES_BLOCK_SIZE);
    iv[..copy_len].copy_from_slice(&sample.iv[..copy_len]);

    let mut cipher = Aes128Ctr::new(key.into(), (&iv).into());

    if sample.subsamples.is_empty() {
        let mut dst = chunk.to_vec();
        cipher.apply_keystream(&mut dst);
        return dst;
    }

    let mut dst = chunk.to_vec();
    let mut pos = 0usize;
    for sub in &sample.subsamples {
        let mut clear_bytes = sub.clear as usize;
        if clear_bytes > chunk.len().saturating_sub(pos) {
            clear_bytes = chunk.len() - pos;
        }
        // 明文段：原样拷贝（dst 已初始化为 chunk 拷贝，这里显式写出便于对照上游行为）
        dst[pos..pos + clear_bytes].copy_from_slice(&chunk[pos..pos + clear_bytes]);
        pos += clear_bytes;
        if pos >= chunk.len() {
            break;
        }

        let mut encrypted_bytes = sub.encrypted as usize;
        if encrypted_bytes > chunk.len().saturating_sub(pos) {
            encrypted_bytes = chunk.len() - pos;
        }
        let segment = &mut dst[pos..pos + encrypted_bytes];
        cipher.apply_keystream(segment);
        pos += encrypted_bytes;
        if pos >= chunk.len() {
            break;
        }
    }
    if pos < chunk.len() {
        dst[pos..].copy_from_slice(&chunk[pos..]);
    }
    dst
}

/// 等价 `findBox`：只在 `[start, end)` 这一层里找 box。
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

/// 等价 `findBoxDeep`：递归进入已知容器 box。
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
        if let Some(child_start) = box_child_start(current_type, pos, header_size) {
            if child_start < pos + size {
                if let Some(found) = find_box_deep(data, box_type, child_start, pos + size) {
                    return Some(found);
                }
            }
        }
        pos += size;
    }
    None
}

/// 等价 `boxChildStart`：哪些 box 带子 box，以及子 box 的起始偏移。
pub fn box_child_start(box_type: &[u8], offset: usize, header_size: usize) -> Option<usize> {
    match box_type {
        b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"sinf" | b"schi" => {
            Some(offset + header_size)
        }
        b"stsd" => Some(offset + header_size + 8),
        b"enca" | b"mp4a" | b"alac" | b"fLaC" => Some(offset + header_size + 28),
        _ => None,
    }
}

/// 等价 `parseStsz`。
pub fn parse_stsz(data: &[u8]) -> Vec<u32> {
    if data.len() < 12 {
        return Vec::new();
    }
    let sample_size_fixed = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let declared_count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
    // 防御：不信任头部声明的条目数。
    // * 变长样本：按"实际可读条目数"（12 + n*4 <= data.len()）取上限；
    // * 定长样本：没有额外数据可校验，用硬上限兜底（此时不会读取尺寸表）。
    let sample_count = if sample_size_fixed != 0 {
        declared_count.min(MAX_SAMPLE_ENTRIES)
    } else {
        let available = data.len().saturating_sub(12) / 4;
        declared_count.min(available).min(MAX_SAMPLE_ENTRIES)
    };
    let mut sizes = vec![0u32; sample_count];
    if sample_size_fixed != 0 {
        for slot in sizes.iter_mut() {
            *slot = sample_size_fixed as u32;
        }
    } else {
        for (index, slot) in sizes.iter_mut().enumerate() {
            let start = 12 + index * 4;
            if start + 4 <= data.len() {
                *slot = u32::from_be_bytes([
                    data[start],
                    data[start + 1],
                    data[start + 2],
                    data[start + 3],
                ]);
            }
        }
    }
    sizes
}

/// 等价 `parseSenc`。
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
    let has_subsamples = (flags & 0x02) != 0;
    let declared_count = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
    // 防御：每个样本至少要占 iv_size 字节（带 subsample 时还要 2 字节计数），
    // 据此把声明值夹到实际数据能容纳的范围内，再套硬上限。
    let per_sample = if has_subsamples { iv_size + 2 } else { iv_size };
    let max_by_data = data.len().saturating_sub(8) / per_sample.max(1);
    let sample_count = declared_count.min(max_by_data).min(MAX_SAMPLE_ENTRIES);

    let mut samples = Vec::with_capacity(sample_count);
    let mut ptr = 8usize;
    for _ in 0..sample_count {
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

/// 等价 `decodeBase36`：非 0-9a-z 返回 `0xFF`。
pub fn decode_base36(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'z' => byte - b'a' + 10,
        _ => 0xFF,
    }
}

/// 等价 `decryptSpadeInner`。
pub fn decrypt_spade_inner(key_bytes: &[u8]) -> Vec<u8> {
    let mut buff = Vec::with_capacity(key_bytes.len() + 2);
    buff.push(0xFA);
    buff.push(0x55);
    buff.extend_from_slice(key_bytes);

    let mut result = Vec::with_capacity(key_bytes.len());
    for index in 0..key_bytes.len() {
        let mut value =
            (key_bytes[index] ^ buff[index]) as i32 - bitcount(index as u32) as i32 - 21;
        while value < 0 {
            value += 255;
        }
        result.push(value as u8);
    }
    result
}

/// 等价 `extractKey`：从 `play_auth` 还原出十六进制 AES 密钥。
pub fn extract_key(play_auth: &str) -> Result<String> {
    let bytes_data = BASE64_STANDARD
        .decode(play_auth.trim())
        .map_err(|err| SodaError::crypto(format!("base64 decode failed: {err}")))?;
    if bytes_data.len() < 3 {
        return Err(SodaError::crypto("auth data too short"));
    }

    let padding_len = (bytes_data[0] ^ bytes_data[1] ^ bytes_data[2]) as i32 - 48;
    if padding_len < 0 {
        return Err(SodaError::crypto("invalid padding length"));
    }
    let padding_len = padding_len as usize;
    if bytes_data.len() < padding_len + 2 {
        return Err(SodaError::crypto("invalid padding length"));
    }

    let inner_input = &bytes_data[1..bytes_data.len() - padding_len];
    let tmp_buff = decrypt_spade_inner(inner_input);
    if tmp_buff.is_empty() {
        return Err(SodaError::crypto("decryption failed"));
    }

    let skip_bytes = decode_base36(tmp_buff[0]) as usize;
    let end_index = 1 + (bytes_data.len() - padding_len - 2) - skip_bytes;
    if end_index > tmp_buff.len() || end_index < 1 {
        return Err(SodaError::crypto("index out of bounds"));
    }
    Ok(String::from_utf8_lossy(&tmp_buff[1..end_index]).to_string())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 十六进制解码（避免额外依赖）。
pub fn hex_decode(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    if bytes.len() % 2 != 0 {
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
