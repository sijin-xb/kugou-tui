// 生成阶段 5d 四个写接口的 KAT（对应上游 module/ 下四个文件）：
//   playlistAdd       /playlist/add         module/playlist_add.js
//   playlistDel       /playlist/del         module/playlist_del.js
//   playlistTracksAdd /playlist/tracks/add  module/playlist_tracks_add.js
//   playlistTracksDel /playlist/tracks/del  module/playlist_tracks_del.js
//
// 与 kat_aes.js 同一套手法：钉死 Math.random、Date.now 与 forge 的随机填充，
// 但**不用假 axios**，而是把 baseURL 指到本地假服务器，从而录到真实出站：
// URL、请求头（含 axios 在 dispatchRequest 里补的 Content-Type）与请求体。
// Content-Type 只在真正发送时才会被 axios 补上，假 axios 看不到它。
//
// 用法：KUGOU_UPSTREAM=/path/to/KuGouMusicApi node tools/kat/kat_write.js
const http = require('http');
const path = process.env.KUGOU_UPSTREAM || '/home/xibie/KuGouMusicApi';
const { spawnSync } = require('child_process');
const forge = require(path + '/node_modules/node-forge');
const CryptoJS = require(path + '/node_modules/crypto-js');

// randomString 的字符池是 '1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ'（36 个）。
// ceil(35 * r) 决定取哪个字符：r=0→'1'，r=0.1→'5'，r=0.5→'J'，r=0.9→'X'。
const SEQ = [0, 0.1, 0.5, 0.9, 0.25, 0.75];
let randomIndex = 0;
Math.random = () => SEQ[randomIndex++ % SEQ.length];

// clienttime 由 request.js 的 Math.floor(Date.now() / 1000) 产生，钉死它
// 才能让 signature 与 URL 逐字节可复现。
const FIXED_CLIENTTIME = 1700000000;
Date.now = () => FIXED_CLIENTTIME * 1000;

// rsaEncrypt2 的 PKCS1 填充来自 forge 的随机源，同样要钉死。
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

// 复算 randomString(len)：模块内部拿不到它生成的 key，但序列是确定的。
function replayKey(len) {
  const saved = randomIndex;
  randomIndex = 0;
  const keyString = '1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ';
  let s = '';
  for (let i = 0; i < len; i++) {
    s += keyString[Math.ceil((keyString.length - 1) * Math.random())];
  }
  randomIndex = saved;
  return s.toLowerCase();
}

// 用指定 key 做 AES-CBC/PKCS7 加密。key 是 6 字符明文随机串；
// encryptKey/iv 各取 md5(key) 的 16 个**十六进制字符**（不是 hex 解码后的字节）。
function encryptWithKey(key, plain) {
  const md5 = CryptoJS.MD5(key).toString(CryptoJS.enc.Hex);
  const encrypted = CryptoJS.AES.encrypt(CryptoJS.enc.Utf8.parse(plain),
    CryptoJS.enc.Utf8.parse(md5.substring(0, 16)),
    {
      iv: CryptoJS.enc.Utf8.parse(md5.substring(16, 32)),
      mode: CryptoJS.mode.CBC,
      padding: CryptoJS.pad.Pkcs7,
    });
  return { str: CryptoJS.enc.Base64.stringify(encrypted.ciphertext) };
}

// server.js 中间件注入的那组 cookie。token / userid 是脱敏的固定值。
const COOKIE = {
  dfid: '1234567890abcdef12345678',
  KUGOU_API_MID: '231699103997194646178265604655475531917',
  KUGOU_API_GUID: '5f2b1c3d4e5f60718293a4b5c6d7e8f9',
  KUGOU_API_DEV: 'ABCDEFGHIJ',
  token: 'TOKENFIXTURE',
  userid: '10001',
};

// 本地假服务器：录下真实出站的请求，并按用例返回预置响应。
let wire = null;
let pending = null;
const server = http.createServer((req, res) => {
  const chunks = [];
  req.on('data', (chunk) => chunks.push(chunk));
  req.on('end', () => {
    wire = {
      method: req.method,
      url: req.url,
      headers: req.headers,
      body: Buffer.concat(chunks).toString('utf8'),
    };
    res.writeHead(pending.status, pending.headers);
    res.end(pending.body);
  });
});

function jsonResponse(payload) {
  return {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
    body: Buffer.from(JSON.stringify(payload)),
  };
}

async function runPlatform(platform, base) {
  process.env.platform = platform;
  for (const mod of ['/util/index.js', '/util/crypto.js', '/util/request.js',
    '/module/playlist_add.js', '/module/playlist_del.js',
    '/module/playlist_tracks_add.js', '/module/playlist_tracks_del.js']) {
    delete require.cache[require.resolve(path + mod)];
  }
  const { createRequest } = require(path + '/util/request.js');
  const out = { platform };

  // playlist_del 在 require 时就把 rsaEncrypt2 解构走了，必须提前包一层，
  // 才能看到送进 PKCS1 的原始 JSON 串（键序 aes/uid/token）。
  const utilIndex = require(path + '/util/index.js');
  const realRsaEncrypt2 = utilIndex.rsaEncrypt2;
  let rsaInput = null;
  utilIndex.rsaEncrypt2 = (data) => {
    rsaInput = typeof data === 'object' ? JSON.stringify(data) : data;
    return realRsaEncrypt2(data);
  };

  async function call(moduleFile, params, response) {
    delete require.cache[require.resolve(path + moduleFile)];
    const mod = require(path + moduleFile);
    wire = null;
    pending = response;
    randomIndex = 0;
    const result = await withFixedPadding(() =>
      mod({ ...params, cookie: COOKIE },
        (config) => createRequest({ ...config, baseURL: base })));
    const url = wire.url;
    const query = url.includes('?') ? url.slice(url.indexOf('?') + 1) : '';
    const pairs = query ? query.split('&').map((kv) => kv.split('=')) : [];
    return {
      method: wire.method,
      // 上游真实的 baseURL 是网关，这里被换成本地服务器，故只录 path + query。
      url,
      order: pairs.map(([k]) => k).join(','),
      params: Object.fromEntries(pairs),
      headers: wire.headers,
      body: wire.body,
      status: result.status,
      responseBody: result.body,
    };
  }

  // 1) /playlist/add：新建歌单。TUI 侧只传 name 与 type（Express 给的是字符串 '0'）。
  out.playlistAdd = await call('/module/playlist_add.js',
    { name: 'KATFIXTURE临时歌单', type: '0' },
    jsonResponse({ status: 1, error_code: 0, data: { listid: 1234567890 } }));

  // 2) /playlist/del：取消收藏。响应是 arraybuffer，要用模块自己将生成的 key 加密。
  const delKey = replayKey(6);
  out.playlistDel = await call('/module/playlist_del.js',
    { listid: '1234567890' },
    {
      status: 200,
      headers: { 'Content-Type': 'application/octet-stream' },
      body: Buffer.from(
        encryptWithKey(delKey, JSON.stringify({ status: 1, data: { listid: 1234567890 } })).str,
        'base64'),
    });
  out.playlistDel.key = delKey;
  out.playlistDel.rsaInput = rsaInput;
  out.playlistDel.dataMapPlain = JSON.stringify(
    CryptoJS.AES.decrypt(
      CryptoJS.lib.CipherParams.create({ ciphertext: CryptoJS.enc.Base64.parse(out.playlistDel.body) }),
      CryptoJS.enc.Utf8.parse(CryptoJS.MD5(delKey).toString(CryptoJS.enc.Hex).substring(0, 16)),
      {
        iv: CryptoJS.enc.Utf8.parse(CryptoJS.MD5(delKey).toString(CryptoJS.enc.Hex).substring(16, 32)),
        mode: CryptoJS.mode.CBC,
        padding: CryptoJS.pad.Pkcs7,
      }).toString(CryptoJS.enc.Utf8));

  // 3) /playlist/tracks/add：加歌。data 是 `歌名|hash|专辑id|album_audio_id`。
  out.playlistTracksAdd = await call('/module/playlist_tracks_add.js',
    { listid: '1234567890', data: '晴天|6af00fbd4d444a82c005843eef9dc2d4|1234567|8901234' },
    jsonResponse({ status: 1, error_code: 0 }));

  // 4) /playlist/tracks/del：删歌。fileids 是歌单条目的 fileid，不是 hash。
  out.playlistTracksDel = await call('/module/playlist_tracks_del.js',
    { listid: '1234567890', fileids: '111111,222222' },
    jsonResponse({ status: 1, error_code: 0 }));

  return out;
}

(async () => {
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const base = `http://127.0.0.1:${server.address().port}`;

  // 一个平台一个进程：util/index.js 在 require 时就把 appid/clientver 定死了。
  if (process.argv.includes('--platform')) {
    const platform = process.argv[process.argv.indexOf('--platform') + 1];
    console.log(JSON.stringify(await runPlatform(platform, base), null, 2));
    server.close();
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
  server.close();
})();
