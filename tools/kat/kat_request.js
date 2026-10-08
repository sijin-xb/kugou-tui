// 开发期 KAT：固定 Date.now 与 Math.random，录下上游 util/request.js 最终交给
// axios 的 requestOptions（URL / 参数 / 头 / body），供 Rust 侧写 known-answer 测试。
//
// 为什么不直接用 probe_outbound.js：那个脚本发真实 HTTP 请求，clienttime 取的是
// 真实时间，没法固化成断言。这里把时间钉死，只做「构造」，不发包。
//
// 用法：KUGOU_UPSTREAM=/path/to/KuGouMusicApi node tools/kat/kat_request.js
const path = process.env.KUGOU_UPSTREAM || '/home/xibie/KuGouMusicApi';

// 1700000000000ms = 1700000000s，与 tools/kat/kat_gen.js 的 clienttime 对齐。
const FIXED_NOW_MS = 1700000000000;
const SEQ = Array.from({ length: 32 }, (_, i) => Number(((i % 10) / 10).toFixed(1)));

Date.now = () => FIXED_NOW_MS;
let randomIndex = 0;
Math.random = () => SEQ[randomIndex++ % SEQ.length];

// 每个用例前把随机序列归零：`randomString` 的消费次数取决于模块内部实现，
// 不归零的话「第几个用例」会影响结果，基准值就不可复现了。
function resetRandom() {
  randomIndex = 0;
}

// 把 axios 换成只记录配置的假实现。必须在 require request.js 之前装好。
const axiosPath = require.resolve(path + '/node_modules/axios');
const realAxios = require(axiosPath);
let captured = null;
const fake = function (config) {
  captured = config;
  return Promise.resolve({ data: { status: 1, error_code: 0 }, headers: {} });
};
for (const key of Object.keys(realAxios)) {
  try {
    fake[key] = realAxios[key];
  } catch {
    // 只读属性，忽略
  }
}
require.cache[axiosPath] = {
  id: axiosPath, filename: axiosPath, loaded: true, exports: fake, children: [], paths: [],
};

const { createRequest } = require(path + '/util/request.js');

// server.js 中间件注入的那组 cookie。mid 由 calculateMid(guid) 得出，这里写死。
// token / userid 是**脱敏的固定值**，只为验证注入行为。
const COOKIE = {
  dfid: '1234567890abcdef12345678',
  KUGOU_API_MID: '231699103997194646178265604655475531917',
  KUGOU_API_GUID: '5f2b1c3d4e5f60718293a4b5c6d7e8f9',
  KUGOU_API_DEV: 'ABCDEFGHIJ',
  KUGOU_API_MAC: '02:00:00:00:00:00',
  KUGOU_API_WEBGL: 'deadbeef',
  token: 'TOKENFIXTURE',
  userid: '10001',
};

async function run(label, modulePath, params) {
  resetRandom();
  delete require.cache[require.resolve(path + modulePath)];
  const mod = require(path + modulePath);
  captured = null;
  await mod(params, (config) => createRequest(config));
  const config = captured;
  console.log(`\n===== ${label} =====`);
  console.log('method :', config.method);
  console.log('baseURL:', config.baseURL);
  console.log('url    :', config.url);
  const raw = Object.entries(config.params || {});
  const sorted = [...raw].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  // axios 按对象的**插入序**拼 query，所以插入序才是要比对的那个。
  console.log('order  :', raw.map(([key]) => key).join(','));
  console.log('params :', JSON.stringify(Object.fromEntries(sorted)));
  console.log('headers:', JSON.stringify(config.headers));
  console.log('data   :', config.data === undefined ? '(none)' : JSON.stringify(config.data));
  // 用 axios 自己的 buildURL 产出最终 URL 字符串：percent-encoding 的细节
  // （哪些字符保留、空格是 `%20` 还是 `+`）只有它能给出权威答案。
  console.log('getUri :', realAxios.getUri(config));
}

// 注意：`util/index.js` 在 **require 时**就把 `isLite` 算成常量并导出
// `appid`/`clientver`（`const isLite = process.env.platform === 'lite'`）。
// 所以同一个 node 进程里切 `process.env.platform` 是没用的——第二个平台会拿到
// 第一个平台缓存的 appid/clientver。必须**一个平台一个进程**：
// 父进程用 KAT_PLATFORM 逐个 spawn 自己。
async function runPlatform(platform) {
  process.env.platform = platform;
  await run(`${platform}/search`, '/module/search.js', {
    keywords: '周杰伦', page: 1, pagesize: 30, cookie: COOKIE,
  });
  await run(`${platform}/search_lyric`, '/module/search_lyric.js', {
    album_audio_id: 0, duration: 243722,
    hash: '6af00fbd4d444a82c005843eef9dc2d4',
    keywords: 'Letter - arkady sevidov', man: 'yes', cookie: COOKIE,
  });
  await run(`${platform}/privilege_lite`, '/module/privilege_lite.js', {
    hash: '6af00fbd4d444a82c005843eef9dc2d4,11111111111111111111111111111111',
    cookie: COOKIE,
  });
  await run(`${platform}/song_url`, '/module/song_url.js', {
    hash: '6af00fbd4d444a82c005843eef9dc2d4', quality: 128,
    ppage_id: '356753938', cookie: COOKIE,
  });
  // cookie 里没有 dfid 时，song_url 会自己补一个 randomString(24)。
  const withoutDfid = { ...COOKIE };
  delete withoutDfid.dfid;
  await run(`${platform}/song_url_nodfid`, '/module/song_url.js', {
    hash: '6af00fbd4d444a82c005843eef9dc2d4', quality: 128,
    ppage_id: '356753938', cookie: withoutDfid,
  });
}

(async () => {
  if (process.env.KAT_PLATFORM) {
    await runPlatform(process.env.KAT_PLATFORM);
    return;
  }
  const { spawnSync } = require('child_process');
  for (const platform of ['standard', 'lite']) {
    const result = spawnSync(process.execPath, [__filename], {
      env: { ...process.env, KAT_PLATFORM: platform },
      encoding: 'utf8',
    });
    process.stdout.write(result.stdout || '');
    process.stderr.write(result.stderr || '');
    if (result.status !== 0) process.exitCode = result.status;
  }
})();
