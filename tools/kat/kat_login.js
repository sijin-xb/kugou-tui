// 生成阶段 5b 四个接口的 KAT：/login/qr/key、/login/qr/check、
// /user/detail、/user/vip/detail。
//
// 与 kat_aes.js 同一套手法：钉死 Math.random 与 Date.now，把 axios 换成只记录
// 配置的假实现，然后用上游真实的 module/*.js 跑一遍，抓出送进 axios 的
// method / baseURL / url / params 插入序 / headers / data / getUri。
//
// 用法：KUGOU_UPSTREAM=/path/to/KuGouMusicApi node tools/kat/kat_login.js
const path = process.env.KUGOU_UPSTREAM || '/home/xibie/KuGouMusicApi';
const { spawnSync } = require('child_process');

// randomString 的字符池是 '1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ'（36 个）。
const SEQ = [0, 0.1, 0.5, 0.9, 0.25, 0.75];
let randomIndex = 0;
Math.random = () => SEQ[randomIndex++ % SEQ.length];

// request.js:77 的 Math.floor(Date.now() / 1000) 决定 clienttime 与
// user_detail 的 visit_time / RSA 明文里的 clienttime。
const FIXED_CLIENTTIME = 1700000000;
Date.now = () => FIXED_CLIENTTIME * 1000;

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

// 固定的二维码 key，替掉真实扫码流程。
const QR_KEY = 'QRKEYFIXTURE0123456789abcdef';

function ok(body) {
  return { data: { status: 1, error_code: 0, ...body }, headers: {} };
}

// 跑一个上游模块，返回它交给 axios 的配置（去掉易变字段之外的加工）。
async function call(modulePath, params, response) {
  delete require.cache[require.resolve(modulePath)];
  const mod = require(modulePath);
  captured = null;
  fakeResponse = response;
  const result = await mod({ ...params, cookie: COOKIE }, (config) => createRequest(config));
  const config = captured;
  const raw = Object.entries(config.params || {});
  const sorted = [...raw].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return {
    method: config.method,
    baseURL: config.baseURL,
    url: config.url,
    order: raw.map(([k]) => k).join(','),
    params: Object.fromEntries(sorted),
    headers: config.headers,
    data: config.data === undefined ? null : config.data,
    getUri: realAxios.getUri(config),
    status: result.status,
    body: result.body,
    pushedCookie: result.cookie,
  };
}

async function runPlatform(platform) {
  process.env.platform = platform;
  delete require.cache[require.resolve(path + '/util/crypto.js')];
  delete require.cache[require.resolve(path + '/util/index.js')];
  const utilIndex = require(path + '/util/index.js');
  const out = { platform };

  // ---- 1. /login/qr/key（encryptType: 'web'，appid 随平台）----
  out.qrKey = await call(
    path + '/module/login_qr_key.js',
    {},
    ok({ data: { qrcode: QR_KEY } }),
  );

  // ---- 2. /login/qr/check（encryptType: 'web'）----
  // 先来一次「等待扫码」，再来一次 status=4，好把 push 进 cookie 的
  // token / userid 一起锁进基准。
  out.qrCheckWaiting = await call(
    path + '/module/login_qr_check.js',
    { key: QR_KEY },
    ok({ data: { status: 1 } }),
  );
  out.qrCheckSuccess = await call(
    path + '/module/login_qr_check.js',
    { key: QR_KEY },
    {
      data: { status: 1, error_code: 0, data: { status: 4, token: 'TOKENFIXTURE', userid: '10001' } },
      headers: { 'set-cookie': ['dfid=DFIDFIXTURE0123456789ab; PATH=/'] },
    },
  );

  // ---- 3. /user/detail（裸 RSA，encryptType: 'android'）----
  // user_detail.js 在 require 时就把 cryptoRSAEncrypt 解构走了，所以必须在它
  // 之前包一层，才能看到送进裸 RSA 的原始 JSON 串（token 在前、clienttime 在后）。
  const realCryptoRSAEncrypt = utilIndex.cryptoRSAEncrypt;
  let rsaInput = null;
  utilIndex.cryptoRSAEncrypt = (data) => {
    rsaInput = typeof data === 'object' ? JSON.stringify(data) : data;
    return realCryptoRSAEncrypt(data);
  };
  out.userDetail = await call(
    path + '/module/user_detail.js',
    {},
    ok({ data: { nickname: 'NICKFIXTURE', pic: 'https://example.invalid/pic.jpg', p_grade: 12, duration: 79239 } }),
  );
  out.userDetail.rsaInput = rsaInput;
  out.userDetail.pHex = out.userDetail.data && out.userDetail.data.p;
  out.userDetail.bodyJson = out.userDetail.data ? JSON.stringify(out.userDetail.data) : null;

  // ---- 4. /user/vip/detail（encryptType: 'android'）----
  out.userVipDetail = await call(
    path + '/module/user_vip_detail.js',
    {},
    ok({ data: { is_vip: 1, vip_end_time: '2027-01-01 00:00:00' } }),
  );

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
