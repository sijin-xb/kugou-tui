// 生成 PKCS1 v1.5 的 KAT：把 forge 的随机源换成固定填充，使其可复现。
// 同时提取公钥的 n / e 十六进制，供 Rust 侧硬编码。
// 上游 KuGouMusicApi 的 clone 路径；默认按开发机布局，可用 KUGOU_UPSTREAM 覆盖。
const path = process.env.KUGOU_UPSTREAM || '/home/xibie/KuGouMusicApi';
const forge = require(path + '/node_modules/node-forge');

const { cryptoRSAEncrypt, publicRasKey, publicLiteRasKey, rsaEncrypt2 } = require(path + '/util/crypto.js');

function keyInfo(pem) {
  const key = forge.pki.publicKeyFromPem(pem);
  return {
    n: key.n.toString(16),
    e: key.e.toString(16),
    bits: key.n.bitLength(),
    k: Math.ceil(key.n.bitLength() / 8),
  };
}

// 固定填充：forge.random.getBytes 被替换成确定性序列
function withFixedPadding(fill, fn) {
  const original = forge.random.getBytes;
  let i = 0;
  forge.random.getBytes = (count) => {
    let out = '';
    for (let j = 0; j < count; j++) {
      out += String.fromCharCode(fill[(i++) % fill.length]);
    }
    return out;
  };
  try {
    return fn();
  } finally {
    forge.random.getBytes = original;
  }
}

const FILL = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10];

const out = {
  standard: keyInfo(publicRasKey),
  lite: keyInfo(publicLiteRasKey),
  pkcs1: {},
  raw: {},
};

for (const platform of ['standard', 'lite']) {
  process.env.platform = platform;
  delete require.cache[require.resolve(path + '/util/crypto.js')];
  const crypto = require(path + '/util/crypto.js');

  // 消息长度 11 → padNum = 128 - 3 - 11 = 114
  const message = 'hello world';
  out.pkcs1[platform] = withFixedPadding(FILL, () => crypto.rsaEncrypt2(message));

  // 裸 RSA：确定性，直接算
  out.raw[platform] = crypto.cryptoRSAEncrypt(message);

  // 边界：消息正好 k-11 = 117 字节
  const maxMessage = 'x'.repeat(117);
  out.pkcs1[platform + '_maxlen'] = withFixedPadding(FILL, () => crypto.rsaEncrypt2(maxMessage));
}

// 空消息 + 固定填充
process.env.platform = 'standard';
delete require.cache[require.resolve(path + '/util/crypto.js')];
{
  const crypto = require(path + '/util/crypto.js');
  out.pkcs1.empty = withFixedPadding(FILL, () => crypto.rsaEncrypt2(''));
  out.pkcs1.fill = FILL;
}

console.log(JSON.stringify(out, null, 2));
