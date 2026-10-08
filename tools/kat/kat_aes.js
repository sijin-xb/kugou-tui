// 生成 AES-CBC + PKCS7 与 /register/dev 的 KAT。
//
// 两个随机源都要钉死，否则基准不可复现：
//   1. playlistAesEncrypt 用 randomString(6) 生成 key → 替换 Math.random
//   2. rsaEncrypt2 用 forge.random.getBytes 填 PKCS1 的 PS → 替换它
//
// 用法：KUGOU_UPSTREAM=/path/to/KuGouMusicApi node tools/kat/kat_aes.js
const path = process.env.KUGOU_UPSTREAM || '/home/xibie/KuGouMusicApi';
const { spawnSync } = require('child_process');
const forge = require(path + '/node_modules/node-forge');
const CryptoJS = require(path + '/node_modules/crypto-js');

// randomString 的字符池是 '1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ'（36 个）。
// ceil(35 * r) 决定取哪个字符：r=0→'1'，r=0.1→'5'，r=0.5→'J'，r=0.9→'X'。
const SEQ = [0, 0.1, 0.5, 0.9, 0.25, 0.75];
let randomIndex = 0;
Math.random = () => SEQ[randomIndex++ % SEQ.length];

// clienttime 由 request.js:77 的 Math.floor(Date.now() / 1000) 产生，钉死它
// 才能让 signature 与 URL 逐字节可复现。
const FIXED_CLIENTTIME = 1700000000;
const realNow = Date.now;
Date.now = () => FIXED_CLIENTTIME * 1000;

const FILL = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10];
function withFixedPadding(fn) {
  const original = forge.random.getBytes;
  let i = 0;
  forge.random.getBytes = (count) => {
    let out = '';
    for (let j = 0; j < count; j++) out += String.fromCharCode(FILL[(i++) % FILL.length]);
    return out;
  };
  try {
    return fn();
  } finally {
    forge.random.getBytes = original;
  }
}

// 把 axios 换成只记录配置的假实现，必须在 require request.js 之前装好。
const axiosPath = require.resolve(path + '/node_modules/axios');
const realAxios = require(axiosPath);
let captured = null;
let fakeResponse = null;
const fake = function (config) {
  captured = config;
  if (fakeResponse) return Promise.resolve(fakeResponse);
  return Promise.resolve({ data: { status: 1, error_code: 0 }, headers: {} });
};
for (const key of Object.keys(realAxios)) {
  try { fake[key] = realAxios[key]; } catch { /* 只读属性 */ }
}
require.cache[axiosPath] = {
  id: axiosPath, filename: axiosPath, loaded: true, exports: fake, children: [], paths: [],
};

const { createRequest } = require(path + '/util/request.js');

// server.js 中间件注入的那组 cookie。token / userid 是脱敏的固定值。
const COOKIE = {
  dfid: '1234567890abcdef12345678',
  KUGOU_API_MID: '231699103997194646178265604655475531917',
  KUGOU_API_GUID: '5f2b1c3d4e5f60718293a4b5c6d7e8f9',
  KUGOU_API_DEV: 'ABCDEFGHIJ',
  token: 'TOKENFIXTURE',
  userid: '10001',
};

// 用指定 key 直接做 AES-CBC/PKCS7 加密，绕开 randomString。
// key 是 6 个字符的明文随机串；encryptKey/iv 各取 md5(key) 的 16 个**十六进制字符**。
function encryptWithKey(key, plain) {
  const md5 = CryptoJS.MD5(key).toString(CryptoJS.enc.Hex);
  const encryptKey = md5.substring(0, 16);
  const iv = md5.substring(16, 32);
  const encrypted = CryptoJS.AES.encrypt(CryptoJS.enc.Utf8.parse(plain),
    CryptoJS.enc.Utf8.parse(encryptKey),
    { iv: CryptoJS.enc.Utf8.parse(iv), mode: CryptoJS.mode.CBC, padding: CryptoJS.pad.Pkcs7 });
  return { key, encryptKey, iv, str: CryptoJS.enc.Base64.stringify(encrypted.ciphertext) };
}

async function runPlatform(platform) {
  process.env.platform = platform;
  delete require.cache[require.resolve(path + '/util/crypto.js')];
  delete require.cache[require.resolve(path + '/util/index.js')];
  const crypto = require(path + '/util/crypto.js');
  const out = { platform };

  // ---- 1. playlistAesEncrypt：注入固定随机序列 ----
  randomIndex = 0;
  const plainObject = { hello: 'world', 中文: '值', n: 42, nested: { a: [1, 2] } };
  const enc = crypto.playlistAesEncrypt(plainObject);
  out.aes = {
    key: enc.key,
    md5: crypto.cryptoMd5(enc.key),
    encryptKey: crypto.cryptoMd5(enc.key).substring(0, 16),
    iv: crypto.cryptoMd5(enc.key).substring(16, 32),
    plain: JSON.stringify(plainObject),
    cipherB64: enc.str,
    roundTrip: JSON.stringify(crypto.playlistAesDecrypt({ str: enc.str, key: enc.key })),
  };

  // 空对象
  randomIndex = 0;
  const empty = crypto.playlistAesEncrypt({});
  out.aesEmpty = { key: empty.key, cipherB64: empty.str };

  // 明文恰好 16 字节：PKCS7 必须再加一整块 0x10
  randomIndex = 0;
  const block = crypto.playlistAesEncrypt('0123456789abcdef');
  out.aesBlock = { key: block.key, cipherB64: block.str };

  // 明文 15 字节：只补 1 字节
  randomIndex = 0;
  const nearBlock = crypto.playlistAesEncrypt('0123456789abcde');
  out.aesNearBlock = { key: nearBlock.key, cipherB64: nearBlock.str };

  // 多字节 UTF-8：确认按**字节**补齐，不是按字符
  randomIndex = 0;
  const cjk = crypto.playlistAesEncrypt('酷狗');
  out.aesCjk = { key: cjk.key, cipherB64: cjk.str, plain: '酷狗' };

  // ---- 2. 响应解密：Rust 侧要能解开这段 ----
  const responsePlain = { status: 1, data: { dfid: 'DFIDFIXTURE0123456789ab' } };
  const responseEnc = encryptWithKey('1jx5zx', JSON.stringify(responsePlain));
  out.response = {
    key: '1jx5zx',
    cipherB64: responseEnc.str,
    plain: JSON.stringify(responsePlain),
  };

  // ---- 3. /register/dev：完整请求 ----
  // register_dev 在 require 时就把 rsaEncrypt2 解构走了，所以必须在它之前
  // 包一层，才能看到送进 PKCS1 的原始 JSON 串（uid 是字符串还是数字）。
  const utilIndex = require(path + '/util/index.js');
  const realRsaEncrypt2 = utilIndex.rsaEncrypt2;
  let rsaInput = null;
  utilIndex.rsaEncrypt2 = (data) => {
    rsaInput = typeof data === 'object' ? JSON.stringify(data) : data;
    return realRsaEncrypt2(data);
  };

  delete require.cache[require.resolve(path + '/module/register_dev.js')];
  const mod = require(path + '/module/register_dev.js');

  // 模块进来第一件事就是 playlistAesEncrypt(dataMap) → randomString(6)，
  // 中间没有别的随机消费。所以先把序列走一遍算出它将要用的 key，
  // 再用同一个 key 加密响应——否则模块拿自己的 key 解我们的密文会崩。
  const moduleKey = (() => {
    randomIndex = 0;
    const keyString = '1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ';
    let s = '';
    for (let i = 0; i < 6; i++) {
      s += keyString[Math.ceil((keyString.length - 1) * Math.random())];
    }
    return s.toLowerCase();
  })();
  const moduleResponse = encryptWithKey(moduleKey, JSON.stringify(responsePlain));
  // responseType 是 arraybuffer，真实 axios 给的是 Buffer；createRequest 会先试
  // JSON.parse(body.toString())，对二进制必然失败，于是 answer.body 就是那个 Buffer。
  fakeResponse = { data: Buffer.from(moduleResponse.str, 'base64'), headers: {} };

  captured = null;
  randomIndex = 0;
  const result = await withFixedPadding(() =>
    mod({ cookie: COOKIE }, (config) => createRequest(config)));

  const config = captured;
  const raw = Object.entries(config.params || {});
  const sorted = [...raw].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  out.registerDev = {
    method: config.method,
    baseURL: config.baseURL,
    url: config.url,
    order: raw.map(([k]) => k).join(','),
    params: Object.fromEntries(sorted),
    headers: config.headers,
    data: config.data,
    getUri: realAxios.getUri(config),
    decryptedBody: result.body,
    pushedCookie: result.cookie,
    rsaInput,
  };

  // 模块内部的 aesEncrypt.key 没有暴露出来，但上面已经按同一条随机序列复算出来了。
  // 用它把请求体解回明文，拿到 31 个键的**插入序**与值。
  out.registerDev.key = moduleKey;
  out.registerDev.dataMapPlain = JSON.stringify(
    crypto.playlistAesDecrypt({ str: config.data, key: moduleKey }));
  out.registerDev.dataMapKeys = Object.keys(
    JSON.parse(out.registerDev.dataMapPlain));

  return out;
}

(async () => {
  // 一个平台一个进程：util/index.js 在 require 时就把 appid/clientver 定死了。
  if (process.argv.includes('--platform')) {
    const platform = process.argv[process.argv.indexOf('--platform') + 1];
    console.log(JSON.stringify(await runPlatform(platform), null, 2));
    return;
  }
  const result = {};
  for (const platform of ['standard', 'lite']) {
    const r = spawnSync(process.execPath, [__filename, '--platform', platform], {
      env: { ...process.env },
      encoding: 'utf8',
      maxBuffer: 16 * 1024 * 1024,
    });
    if (r.status !== 0) {
      process.stderr.write(r.stderr || '');
      process.exitCode = r.status;
      continue;
    }
    result[platform] = JSON.parse(r.stdout);
  }
  console.log(JSON.stringify(result, null, 2));
})();
