// 生成阶段 5c/5d 云歌单与会员相关接口的 KAT（对应上游 module/ 下各文件）：
//   plazaPlaylists      /top/playlist        module/top_playlist.js
//   playlistTracks      /playlist/track/all  module/playlist_track_all.js
//   userPlaylists       /user/playlist       module/user_playlist.js
//   userPlaylistTracks  /playlist/track/all/new  module/playlist_track_all_new.js
//   artistLists         /artist/lists        module/artist_lists.js
//   artistAudios        /artist/audios       module/artist_audios.js（sort=hot/1 与 sort=2 各录一份）
//   rankList            /rank/list           module/rank_list.js
//   rankAudio           /rank/audio          module/rank_audio.js
//   monthVipRecord      /youth/month/vip/record  module/youth_month_vip_record.js
//   dayVip              /youth/day/vip           module/youth_day_vip.js
//   dayVipUpgrade       /youth/day/vip/upgrade   module/youth_day_vip_upgrade.js
//
// 与 kat_login.js 同一套手法：钉死 Math.random 与 Date.now，把 axios 换成只记录
// 配置的假实现，然后用上游真实的 module/*.js 跑一遍，抓出送进 axios 的
// method / baseURL / url / params 插入序 / headers / data / getUri。
//
// 输入全部用**字符串**，因为 NodeApi 是走 HTTP 打到本机 Node 服务，Express 解析
// 出来的 query 一律是字符串。传数字会改变上游 `params.x || fallback` 的分支
// （例如 `withsong: 0` 是 falsy 会被换成 1，而 `'0'` 是真值会原样保留）。
//
// 用法：KUGOU_UPSTREAM=/path/to/KuGouMusicApi node tools/kat/kat_cloud.js
const path = process.env.KUGOU_UPSTREAM || '/home/xibie/KuGouMusicApi';
const { spawnSync } = require('child_process');

// randomString 的字符池是 '1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ'（36 个）。
const SEQ = [0, 0.1, 0.5, 0.9, 0.25, 0.75];
let randomIndex = 0;
Math.random = () => SEQ[randomIndex++ % SEQ.length];

// request.js:77 的 Math.floor(Date.now() / 1000) 决定 clienttime，也决定
// top_playlist 的 dateTime 与 artist_audios 的 signParamsKey 输入。
//
// **必须连 `new Date()` 一起钉死**：`module/artist_audios.js` 用的是
// `Math.floor(new Date().getTime() / 1000)`，只覆盖 `Date.now` 的话它每次跑出
// 一个真实时间戳，签名不可复现（KAT 也就失去意义）。
const FIXED_CLIENTTIME = 1700000000;
const RealDate = Date;
global.Date = class extends RealDate {
  constructor(...args) {
    super(...(args.length === 0 ? [FIXED_CLIENTTIME * 1000] : args));
  }
  static now() {
    return FIXED_CLIENTTIME * 1000;
  }
};

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

// 固定输入，与 Rust 侧 KAT 常量一一对应。**必须是 `NodeApi` 真正发的那几个参数**：
// `catalog.rs` 的 `plaza_playlists` 传的是 `withsong=0&withtag=1`，少传的话上游
// `params?.withsong || 1` 会走默认分支，body 里就变成数字 1 而不是字符串 '0'/'1'。
const PLAZA = { category_id: '0', withsong: '0', withtag: '1', page: '1', pagesize: '30' };
const GLOBAL_ID = 'GLOBALCOLLECTIONIDFIXTURE';
const LIST_ID = '1234567890';
const ARTIST_ID = '12345';
const RANK_ID = '8888';
const DAY_VIP_DATE = '2026-09-23';

function ok(body) {
  return { data: { status: 1, error_code: 0, ...body }, headers: {} };
}

// 跑一个上游模块，返回它交给 axios 的配置。
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
  };
}

async function runPlatform(platform) {
  process.env.platform = platform;
  delete require.cache[require.resolve(path + '/util/crypto.js')];
  delete require.cache[require.resolve(path + '/util/index.js')];
  require(path + '/util/index.js');
  const out = { platform };

  // ---- 歌单广场：/top/playlist（POST /v2/special_recommend）----
  out.plazaPlaylists = await call(
    path + '/module/top_playlist.js',
    PLAZA,
    ok({ data: { special_list: [] } }),
  );

  // ---- 公开歌单歌曲：/playlist/track/all（GET）----
  out.playlistTracks = await call(
    path + '/module/playlist_track_all.js',
    { id: GLOBAL_ID, page: '1', pagesize: '30' },
    ok({ data: { info: [] } }),
  );

  // ---- 用户歌单：/user/playlist（POST /v7/get_all_list）----
  out.userPlaylists = await call(
    path + '/module/user_playlist.js',
    { page: '1', pagesize: '100' },
    ok({ data: { info: [] } }),
  );

  // ---- 用户歌单歌曲：/playlist/track/all/new（POST /v4/get_list_all_file）----
  out.userPlaylistTracks = await call(
    path + '/module/playlist_track_all_new.js',
    { listid: LIST_ID, page: '1', pagesize: '30' },
    ok({ data: { info: [] } }),
  );

  // ---- 歌手列表：/artist/lists（GET /ocean/v6/singer/list）----
  out.artistLists = await call(
    path + '/module/artist_lists.js',
    { sextypes: '0', type: '0', musician: '0', hotsize: '30' },
    ok({ data: { info: [] } }),
  );

  // ---- 歌手单曲：/artist/audios（POST，baseURL openapi.kugou.com）----
  out.artistAudios = await call(
    path + '/module/artist_audios.js',
    { id: ARTIST_ID, sort: 'hot', page: '1', pagesize: '30' },
    ok({ data: { info: [] } }),
  );
  out.artistAudiosNew = await call(
    path + '/module/artist_audios.js',
    { id: ARTIST_ID, sort: 'new', page: '1', pagesize: '30' },
    ok({ data: { info: [] } }),
  );

  // ---- 排行榜列表：/rank/list（GET /ocean/v6/rank/list）----
  out.rankList = await call(
    path + '/module/rank_list.js',
    { withsong: '0' },
    ok({ data: { info: [] } }),
  );

  // ---- 排行榜歌曲：/rank/audio（POST /openapi/kmr/v2/rank/audio）----
  out.rankAudio = await call(
    path + '/module/rank_audio.js',
    { rankid: RANK_ID, page: '1', pagesize: '30' },
    ok({ data: { info: [] } }),
  );

  // ---- 已领 VIP 日期：/youth/month/vip/record（GET）----
  out.monthVipRecord = await call(
    path + '/module/youth_month_vip_record.js',
    {},
    ok({ data: { list: [] } }),
  );

  // ---- 领取当天 VIP：/youth/day/vip（POST，无 body）----
  // `receive_day` 是要领的那一天，不是「今天」。
  out.dayVip = await call(
    path + '/module/youth_day_vip.js',
    { receive_day: DAY_VIP_DATE },
    ok({ data: {} }),
  );

  // ---- 升级当天 VIP：/youth/day/vip/upgrade（POST，无 body）----
  // `kugouid` 取 `Number(userid || 0)`，userid 来自 cookie。
  out.dayVipUpgrade = await call(
    path + '/module/youth_day_vip_upgrade.js',
    {},
    ok({ data: {} }),
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
