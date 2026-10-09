//! 上游 `util/crypto.js` 的加密原语。
//!
//! 来源：KuGouMusicApi v1.6.0 `util/crypto.js`
//! （commit a5a98013cce79fe0ae2ad65fc84b68176ebcfc1e，2026-08-14）。
//!
//! 只移植迁移范围用得到的四个：
//!
//! * `cryptoMd5` —— 全部签名的基础；
//! * `cryptoRSAEncrypt` / `rsaRawEncrypt` —— **裸 RSA，无填充**；
//! * `rsaEncrypt2` —— PKCS#1 v1.5，块类型 `0x02`；
//! * `playlistAesEncrypt` / `playlistAesDecrypt` —— AES-128-CBC + PKCS#7，
//!   密钥与 IV 由 6 字符 `key` 的 MD5 前 16 / 后 16 个十六进制字符充当。
//!
//! 上游的 `cryptoAesEncrypt`（另一个函数：key 取 MD5 全 32 字符、iv 取 key
//! 末 16）迁移范围用不到，没有移植。
//!
//! # 公钥从哪来
//!
//! 上游存的是 PEM（`util/crypto.js:4-5` 的 `publicRasKey` / `publicLiteRasKey`，
//! 1024 bit，指数 `AQAB`）。这里内联模数 `n` 与指数 `e`，值由 node-forge 从
//! **同一份 PEM** 解析得到。抄错一个字节测试就会失败——KAT 用的是上游算出的
//! 密文，不是本地自洽的结果。

use num_bigint::BigUint;
use std::sync::OnceLock;

use crate::error::{AppError, Result};
use crate::source::SourceKind;

/// 标准版公钥模数（1024 bit），对应上游 `publicRasKey`。
const STANDARD_MODULUS: &str = "c8006ed03842d2628209bd314984ca5ed6cfe06e30c95f9d4704d9c49791d7a935ba950ecb0bc8ebf5f5994f0bac927a7eb151b3c1de343303fa539c83136eccfd7d7e511e2dbce18eaa9f784c9b50d443e75865979e0a5e216e46c684066a8d6b998580bbaa22d73f5790286bb14742e83244e44db6d707ffe162c5c7002d45";

/// 概念版公钥模数，对应上游 `publicLiteRasKey`。
const LITE_MODULUS: &str = "c40a2d0da76511f3bb1cc2bbd3afbd8bea83b4d6b05b6c13eb8920c53f1af7679b32ba0d0edb843240ef1b836efed3ee240734c14c1399fd6594d16af22f52525d14d72e0155c6dcc8638d4f7bb94f3a0b1f4c29f991972f2a160a25eb0a9e724336be7f69bbd319ffab1c6dd8470b021dc434f3faba89f4a2a01b33bdbdd08b";

/// 公钥指数 `AQAB`。
const PUBLIC_EXPONENT: u32 = 65537;

/// 模数字节数。上游 `Math.ceil(key.n.bitLength() / 8)`，两个平台都是 128。
const KEY_BYTES: usize = 128;

/// MD5 → 32 位小写 hex。对应上游 `util/crypto.js` 的 `cryptoMd5`。
///
/// 上游在 `typeof data === 'object'` 时会先 `JSON.stringify`；本仓库的参数
/// 全是标量或已序列化的字符串，调用方自己保证这一点。
pub fn md5_hex(data: &[u8]) -> String {
    use md5::{Digest, Md5};

    let mut hasher = Md5::new();
    hasher.update(data);
    hex_encode(&hasher.finalize())
}

/// 字节 → 小写 hex。
pub fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

fn modulus(kind: SourceKind) -> &'static BigUint {
    static STANDARD: OnceLock<BigUint> = OnceLock::new();
    static LITE: OnceLock<BigUint> = OnceLock::new();
    let (cell, hex) = match kind {
        SourceKind::KugouConcept => (&LITE, LITE_MODULUS),
        _ => (&STANDARD, STANDARD_MODULUS),
    };
    cell.get_or_init(|| BigUint::parse_bytes(hex.as_bytes(), 16).expect("内联模数是合法 hex"))
}

/// 大整数 → 左侧补零到 `KEY_BYTES` 字节的 hex（256 字符）。
///
/// 对应 forge `rsa.js:543-551`：`y.toString(16)` 后按需补前导零字节。
fn padded_hex(value: &BigUint) -> String {
    let hex = value.to_str_radix(16);
    let width = KEY_BYTES * 2;
    if hex.len() >= width {
        hex
    } else {
        format!("{}{hex}", "0".repeat(width - hex.len()))
    }
}

fn encrypt_block(kind: SourceKind, block: &[u8]) -> String {
    let modulus = modulus(kind);
    let message = BigUint::from_bytes_be(block);
    let encrypted = message.modpow(&BigUint::from(PUBLIC_EXPONENT), modulus);
    padded_hex(&encrypted)
}

/// 裸 RSA：**无填充**，输入左侧对齐、右侧补零到 128 字节。
///
/// 对应上游 `util/crypto.js` 的 `cryptoRSAEncrypt` 与 `rsaRawEncrypt`。
/// 确定性函数——同样的输入永远得到同样的密文，可直接做 KAT。
#[allow(dead_code)] // 阶段 5b 的 /user/detail 用（module/user_detail.js 的 cryptoRSAEncrypt）
pub fn raw_rsa_encrypt(kind: SourceKind, data: &[u8]) -> Result<String> {
    if data.len() > KEY_BYTES {
        // 上游抛 `'Data length exceeds key size'`。
        return Err(AppError::Other(format!(
            "裸 RSA 输入 {} 字节，超过密钥长度 {KEY_BYTES} 字节",
            data.len()
        )));
    }
    let mut block = vec![0u8; KEY_BYTES];
    block[..data.len()].copy_from_slice(data);
    Ok(encrypt_block(kind, &block))
}

/// PKCS#1 v1.5 加密，块类型 `0x02`。
///
/// 对应上游 `util/crypto.js` 的 `rsaEncrypt2`。填充串 `PS` 必须是
/// **非零随机字节**；这里把随机源做成参数 `fill`，好让测试注入固定填充
/// 与 node-forge 对拍。生产路径传 [`random_fill`] 的输出。
///
/// `EB = 00 || 02 || PS || 00 || D`，`PS` 长 `k - 3 - len(D)` 字节
/// （node-forge `rsa.js:1562` 的 `_encodePkcs1_v1_5`）。
pub fn pkcs1_v15_encrypt(kind: SourceKind, data: &[u8], fill: &[u8]) -> Result<String> {
    let max = KEY_BYTES - 11;
    if data.len() > max {
        // 上游抛 `'Message is too long for PKCS#1 v1.5 padding.'`
        return Err(AppError::Other(format!(
            "PKCS#1 v1.5 消息 {} 字节，上限 {max} 字节",
            data.len()
        )));
    }
    if fill.is_empty() {
        return Err(AppError::Other("PKCS#1 v1.5 填充源为空".to_string()));
    }

    let pad_len = KEY_BYTES - 3 - data.len();
    let mut block = Vec::with_capacity(KEY_BYTES);
    block.push(0x00);
    block.push(0x02);

    // 与 forge 一样跳过零字节，直到填满 pad_len 个非零字节。
    let mut cursor = 0usize;
    let mut written = 0usize;
    let mut scanned = 0usize;
    while written < pad_len {
        if scanned >= fill.len() * 256 {
            // 填充源里一个非零字节都没有，凑不出 PS。
            return Err(AppError::Other("PKCS#1 v1.5 填充源全为零".to_string()));
        }
        let byte = fill[cursor % fill.len()];
        cursor += 1;
        scanned += 1;
        if byte != 0 {
            block.push(byte);
            written += 1;
        }
    }

    block.push(0x00);
    block.extend_from_slice(data);
    debug_assert_eq!(block.len(), KEY_BYTES);
    Ok(encrypt_block(kind, &block))
}

/// 由 `playlistAesEncrypt` 的 6 字符 `key` 派生 AES 的密钥与 IV。
///
/// 对应上游 `util/crypto.js:272-285`：
///
/// ```text
/// encryptKey = cryptoMd5(key).substring(0, 16)
/// iv         = cryptoMd5(key).substring(16, 32)
/// ```
///
/// **这两个值是那 16 个十六进制字符本身**（各 16 个 ASCII 字节），不是把
/// hex 解码后的 8 字节。上游 `utf8WordArray`（`util/crypto.js:134-136`）对
/// 字符串走 `CryptoJS.enc.Utf8.parse`，所以进 AES 的就是 ASCII 字节。
/// 按 hex 解码写会得到另一个密文，而且不会有任何报错。
pub fn playlist_key_material(key: &str) -> ([u8; 16], [u8; 16]) {
    let digest = md5_hex(key.as_bytes());
    let mut encrypt_key = [0u8; 16];
    let mut iv = [0u8; 16];
    encrypt_key.copy_from_slice(&digest.as_bytes()[..16]);
    iv.copy_from_slice(&digest.as_bytes()[16..32]);
    (encrypt_key, iv)
}

/// AES-128-CBC + PKCS#7 加密，返回 base64。
///
/// 对应上游 `util/crypto.js` 的 `playlistAesEncrypt`。上游把密文交给
/// `CryptoJS.enc.Base64.stringify`，即标准 base64（带 `=` 填充）。
///
/// 没有引 `cbc` crate（`Cargo.lock` 里也没有），CBC 的异或链在这里手写：
/// 每块先与前一块密文异或再送进 `Aes128::encrypt_block`。块数很少
/// （`/register/dev` 的明文约 1.2 KB），不需要分块并行。
pub fn aes_cbc_encrypt(key: &[u8], iv: &[u8], plain: &[u8]) -> Result<String> {
    use aes::Aes128;
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncrypt, KeyInit};

    if key.len() != 16 || iv.len() != 16 {
        return Err(AppError::Other(format!(
            "AES-CBC 需要 16 字节密钥与 IV，收到 {} 与 {}",
            key.len(),
            iv.len()
        )));
    }

    // PKCS#7：补到块大小整数倍，且**恰好是整数倍时也要补满一整块**
    // （CryptoJS `cipher-core.js:404` 的 `blockSizeBytes - sigBytes % blockSizeBytes`）。
    let pad = 16 - (plain.len() % 16);
    let mut buffer = Vec::with_capacity(plain.len() + pad);
    buffer.extend_from_slice(plain);
    buffer.extend(std::iter::repeat_n(pad as u8, pad));

    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut previous = [0u8; 16];
    previous.copy_from_slice(iv);

    for chunk in buffer.chunks_mut(16) {
        for (byte, chain) in chunk.iter_mut().zip(previous.iter()) {
            *byte ^= chain;
        }
        let block = GenericArray::from_mut_slice(chunk);
        cipher.encrypt_block(block);
        previous.copy_from_slice(chunk);
    }

    use base64::Engine as _;
    Ok(base64::engine::general_purpose::STANDARD.encode(&buffer))
}

/// AES-128-CBC + PKCS#7 解密，输入是 base64。
///
/// 对应上游 `util/crypto.js` 的 `playlistAesDecrypt`。上游的 `CryptoJS` 解填充
/// 只按最后一字节截断、**不校验**（`cipher-core.js:431-437`），所以密文不对时
/// 它给出的是垃圾而不是报错。这里行为一致：不校验填充内容，只按最后一字节截断，
/// 剩下的字节原样返回给调用方去 `JSON.parse` 或当文本用。
pub fn aes_cbc_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    use aes::Aes128;
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockDecrypt, KeyInit};

    if key.len() != 16 || iv.len() != 16 {
        return Err(AppError::Other(format!(
            "AES-CBC 需要 16 字节密钥与 IV，收到 {} 与 {}",
            key.len(),
            iv.len()
        )));
    }
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        return Err(AppError::Other(format!(
            "AES-CBC 密文长度 {} 不是 16 的正整数倍",
            ciphertext.len()
        )));
    }

    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut previous = [0u8; 16];
    previous.copy_from_slice(iv);

    let mut out = Vec::with_capacity(ciphertext.len());
    for chunk in ciphertext.chunks(16) {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        for (byte, chain) in block.iter_mut().zip(previous.iter()) {
            *byte ^= chain;
        }
        out.extend_from_slice(&block);
        previous.copy_from_slice(chunk);
    }

    let pad = usize::from(*out.last().expect("密文非空"));
    if pad > 0 && pad <= 16 && pad <= out.len() {
        out.truncate(out.len() - pad);
    }
    Ok(out)
}

/// base64 → 字节。
#[allow(dead_code)] // 阶段 5d 的云歌单写接口用（module/playlist_del.js 的响应解密）
pub fn base64_decode(text: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|error| AppError::Other(format!("base64 解码失败：{error}")))
}

/// 生产路径的 PKCS#1 v1.5 填充源。
///
/// 上游走 `forge.random.getBytes`（CSPRNG）。这里复用本仓库既有的
/// [`crate::util::random_u64`]——它是 xorshift64*，**不是密码学安全**。
/// 该填充只用于设备注册请求，不参与任何本机密钥或长期凭据；
/// 若将来拿它保护敏感数据，必须先换成系统 CSPRNG。
pub fn random_fill() -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    for _ in 0..8 {
        out.extend_from_slice(&crate::util::random_u64().to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 `/tmp/kat_rsa.js` 注入的填充一致：`[1, 2, …, 16]` 循环。
    const FIXED_FILL: [u8; 16] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        0x10,
    ];

    #[test]
    fn md5_matches_crypto_js() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn raw_rsa_matches_upstream_standard() {
        // 上游：cryptoRSAEncrypt('hello world')，platform=standard
        let got = raw_rsa_encrypt(SourceKind::Kugou, b"hello world").expect("加密");
        assert_eq!(
            got,
            "a3d0389f7b25d10adf2a6ad265eedf5e9276baebbe3fdb67f99b2500dc53ca8123ec2ae330b220361b9141af8780594a6d096d39dfc2f761af6b4d02298821b2aa4f18c1669674c8bd44f6d96a47d22dbe0036dabf57685afc4f66e8c06127d4f2e2477a4caef8adfd40d8b31914c5578fdbb375684936585e6923eda8ea85fc"
        );
        assert_eq!(got.len(), 256);
    }

    #[test]
    fn raw_rsa_matches_upstream_lite() {
        // 上游：cryptoRSAEncrypt('hello world')，platform=lite
        let got = raw_rsa_encrypt(SourceKind::KugouConcept, b"hello world").expect("加密");
        assert_eq!(
            got,
            "4907d21b3d98a308858cdf4c753018ef5f9da7f07c9b40460041006c28e255b80bdbc98b3d26cf4a8692fb84e73c56ab92149504e7130e51832276f327c0570698059d4adfbe287d42b01a3ee119c27630ff9d5a2e66f08f77a3ddef6c1a151afbb80db7473b6d65f9516e826ac3299bcf68aec0461cfa52940d3068ec524374"
        );
    }

    #[test]
    fn pkcs1_v15_matches_upstream_standard() {
        // 上游：rsaEncrypt2('hello world')，注入固定填充
        let got = pkcs1_v15_encrypt(SourceKind::Kugou, b"hello world", &FIXED_FILL).expect("加密");
        assert_eq!(
            got,
            "9ef27492776b149d3ca343a230accf28f7dc3b21de0928c050b4c12c603514f1eef968eef76fdfb42c6b5a93f0f0d4b0dfe373885fa81225c887848a61ec25abef50c13b5f443c0d883532ee92a101c0425f4d0d989b346a9131769c455d3dcfefa3eed92bfacac8a6b4a4eb70e7f158810ed1d83136a77cf58d64f6439247ab"
        );
    }

    #[test]
    fn pkcs1_v15_matches_upstream_lite() {
        let got =
            pkcs1_v15_encrypt(SourceKind::KugouConcept, b"hello world", &FIXED_FILL).expect("加密");
        assert_eq!(
            got,
            "9025398f6f9b6f61887303d23c62cc46c63125377042927bd6040648b524ffd663fdfd0088fc0ff3a73eff65d96aa7b3730e442c5729ca291170b6c81646b66a4991e6a693251cfdacd35fe96ca46b82ad6ab6e01bb83a18ddec0149df4a6b34ae6c774e5f4e016ec64135de0239f1b01420807b86343d7bc7967b5ad49654c4"
        );
    }

    #[test]
    fn pkcs1_v15_handles_empty_message() {
        let got = pkcs1_v15_encrypt(SourceKind::Kugou, b"", &FIXED_FILL).expect("加密");
        assert_eq!(
            got,
            "1183722c0925b7f1425a828b26636fc1780fc8298fa2416279a8cd91dbb6265e36ac43ede93f1d8330fa551f0dc1fa2a515d6cd67ac3b8122d33e99c06f649beb67f71289b83c5a5c8b067be68524e4d2531c98031ec5a8d08aa53b2c2bc024665f6866532827e7da90130f8965ed43ed5df2610d1afcb473be7b5cac0a00e93"
        );
    }

    #[test]
    fn pkcs1_v15_handles_maximum_length_message() {
        // 117 = 128 - 11，正好填满
        let message = vec![b'x'; 117];
        let got = pkcs1_v15_encrypt(SourceKind::Kugou, &message, &FIXED_FILL).expect("加密");
        assert_eq!(
            got,
            "65ccf0fc76dfd3a63dbc3c773fa006595350a2f04c4405e93ad809b9ff6cc756380eb2f695e68798a6c915a20bd21387bfb49f6bd4f85c4d78be806a65f3db9caaa8418a62cc8043eda7f3cf65189298a39dbcbfdab5241cf73c04f4568ed16e55d4b5d112f4418f513cc4e1538ec388f1b1373498bda5ae9516e414b492fdfb"
        );
        let lite =
            pkcs1_v15_encrypt(SourceKind::KugouConcept, &message, &FIXED_FILL).expect("加密");
        assert_eq!(
            lite,
            "02b5c5edf96118ad9f25ac65de18f19e383b27f2a196a09382096dc6c3ef7d3a0512012015a4ab852df4f735d5c7afaf6439fde0fbcb99339f9d04416015b904e47c49a8ec35cac86009adbf0a2ae021f198f9ffae17454c491cc52ef754a17ae7fcb48568e9e692be017996e22dce54fa8e4a01237f1be3359fd847286df6fd"
        );
    }

    #[test]
    fn pkcs1_v15_rejects_oversized_message() {
        let message = vec![b'x'; 118];
        assert!(pkcs1_v15_encrypt(SourceKind::Kugou, &message, &FIXED_FILL).is_err());
    }

    #[test]
    fn raw_rsa_rejects_oversized_input() {
        let message = vec![b'x'; 129];
        assert!(raw_rsa_encrypt(SourceKind::Kugou, &message).is_err());
    }

    #[test]
    fn pkcs1_fill_skips_zero_bytes() {
        // 填充源含零：应当跳过，结果与不含零的等价序列一致。
        let with_zeros: Vec<u8> = FIXED_FILL.iter().flat_map(|byte| [0u8, *byte]).collect();
        let got = pkcs1_v15_encrypt(SourceKind::Kugou, b"hello world", &with_zeros).expect("加密");
        let expected =
            pkcs1_v15_encrypt(SourceKind::Kugou, b"hello world", &FIXED_FILL).expect("加密");
        assert_eq!(got, expected);
    }

    /// `tools/kat/kat_aes.js` 钉死的 6 字符 key。
    const KAT_AES_KEY: &str = "15iw0r";

    #[test]
    fn playlist_key_material_matches_upstream() {
        // 上游：cryptoMd5('15iw0r') = bb2d33d8684710549dd97a8a58d90250
        //       encryptKey = 前 16 字符，iv = 后 16 字符（都是 ASCII 字节）
        let (key, iv) = playlist_key_material(KAT_AES_KEY);
        assert_eq!(&key, b"bb2d33d868471054");
        assert_eq!(&iv, b"9dd97a8a58d90250");
    }

    #[test]
    fn aes_cbc_matches_upstream_object() {
        // 上游 playlistAesEncrypt({"hello":"world","中文":"值","n":42,"nested":{"a":[1,2]}})
        let (key, iv) = playlist_key_material(KAT_AES_KEY);
        let plain = r#"{"hello":"world","中文":"值","n":42,"nested":{"a":[1,2]}}"#;
        let got = aes_cbc_encrypt(&key, &iv, plain.as_bytes()).expect("加密");
        assert_eq!(
            got,
            "NT568O7l3E83PNQy5gEbv6YZ3dXr7r2B0Xb7NmnWbZlBvfBfeJDec4hGNcv09JvgjGPxUBUWwAPxNmOHpwsiJg=="
        );
    }

    #[test]
    fn aes_cbc_matches_upstream_empty_object() {
        let (key, iv) = playlist_key_material(KAT_AES_KEY);
        let got = aes_cbc_encrypt(&key, &iv, b"{}").expect("加密");
        assert_eq!(got, "jMix3CfCIvfHcUxSdPqECg==");
    }

    #[test]
    fn aes_cbc_pads_a_full_block() {
        // 明文恰好 16 字节时 PKCS#7 要补满一整块，否则密文会短 16 字节。
        let (key, iv) = playlist_key_material(KAT_AES_KEY);
        let got = aes_cbc_encrypt(&key, &iv, b"0123456789abcdef").expect("加密");
        assert_eq!(got, "oeYFHQbj0GWrHP/FuCGatahrd7SNPCwLNpEnb/3bgmU=");
        assert_eq!(
            aes_cbc_decrypt(&key, &iv, &base64_decode(&got).expect("解码")).expect("解密"),
            b"0123456789abcdef"
        );
    }

    #[test]
    fn aes_cbc_matches_upstream_partial_block() {
        let (key, iv) = playlist_key_material(KAT_AES_KEY);
        let got = aes_cbc_encrypt(&key, &iv, b"0123456789abcde").expect("加密");
        assert_eq!(got, "ok0TkDTmEBc1Z2gwnome0Q==");
    }

    #[test]
    fn aes_cbc_matches_upstream_multibyte() {
        // 多字节按 UTF-8 字节数补齐，不是按字符数。
        let (key, iv) = playlist_key_material(KAT_AES_KEY);
        let got = aes_cbc_encrypt(&key, &iv, "酷狗".as_bytes()).expect("加密");
        assert_eq!(got, "29VD+Egh7lpkppfjSZ41gg==");
    }

    #[test]
    fn aes_cbc_decrypt_matches_upstream_response() {
        // 上游 playlistAesDecrypt 的对照：key='1jx5zx'，
        // 明文 {"status":1,"data":{"dfid":"DFIDFIXTURE0123456789ab"}}
        let (key, iv) = playlist_key_material("1jx5zx");
        let ciphertext = base64_decode(
            "02H1lHOQIzwMrj05HOAgLvLiPkqf9yl7uV+kZlcfSDuaw7BimABc+k0W8KH/NWgYcBAQu8TiWtQYtmsE6ZXj7g==",
        )
        .expect("解码");
        let plain = aes_cbc_decrypt(&key, &iv, &ciphertext).expect("解密");
        assert_eq!(
            String::from_utf8(plain).expect("utf8"),
            r#"{"status":1,"data":{"dfid":"DFIDFIXTURE0123456789ab"}}"#
        );
    }

    #[test]
    fn aes_cbc_rejects_bad_key_length() {
        assert!(aes_cbc_encrypt(b"short", b"0123456789abcdef", b"x").is_err());
        assert!(aes_cbc_decrypt(b"0123456789abcdef", b"short", &[0u8; 16]).is_err());
    }

    #[test]
    fn aes_cbc_rejects_unaligned_ciphertext() {
        assert!(aes_cbc_decrypt(b"0123456789abcdef", b"0123456789abcdef", &[0u8; 15]).is_err());
        assert!(aes_cbc_decrypt(b"0123456789abcdef", b"0123456789abcdef", b"").is_err());
    }
}
