// 证明 song_url / search_lyric 的 notSign:true 不生效：签名照旧走 android。
// 做法：用假的 useAxios 截获模块真正传出的 options，再按 request.js 的逻辑复算签名。
// 上游 KuGouMusicApi 的 clone 路径；默认按开发机布局，可用 KUGOU_UPSTREAM 覆盖。
const path = process.env.KUGOU_UPSTREAM || '/home/xibie/KuGouMusicApi';

// 固定 Math.random，让 song_url 内部 randomString(24) 的 dfid 可复现
const seqA = Array.from({ length: 32 }, (_, i) => Number(((i % 10) / 10).toFixed(1)));
function withFixedRandom(sequence, fn) {
  const original = Math.random;
  let i = 0;
  Math.random = () => sequence[i++ % sequence.length];
  try { return fn(); } finally { Math.random = original; }
}

const { appid, clientver } = require(path + '/util');
const helper = require(path + '/util/helper.js');

function capture(moduleFile, params, platform) {
  let captured = null;
  const useAxios = (options) => { captured = options; return options; };
  delete require.cache[require.resolve(path + moduleFile)];
  const mod = require(path + moduleFile);
  process.env.platform = platform;   // 模块与 helper.js 都在调用时读它
  try { mod(params, useAxios); } finally { delete process.env.platform; }
  return captured;
}

// --- request.js 的关键变换，逐行照抄 ---
function resolveRequest(options, platform) {
  process.env.platform = platform;
  const isLite = platform === 'lite';
  const dfid = options?.cookie?.dfid || '-';
  const mid = `${options?.cookie?.KUGOU_API_MID}`;
  const uuid = '-';
  const clienttime = 1700000000; // 固定

  const defaultParams = {
    dfid,
    mid,
    uuid,
    appid: isLite ? 3116 : appid,
    clientver: isLite ? 11440 : clientver,
    clienttime,
  };
  let p = options?.clearDefaultParams
    ? Object.assign({}, options?.params || {})
    : Object.assign({}, defaultParams, options?.params || {});

  if (options?.encryptKey) {
    p['key'] = helper.signKey(p['hash'], p['mid'], p['userid'], p['appid']);
  }

  const data = typeof options?.data === 'object' ? JSON.stringify(options.data) : (options?.data || '');

  // request.js:126 —— 只读 options.notSignature，从不读 options.notSign
  let signature = null;
  if (!p['signature'] && !options.notSignature) {
    if (options.encryptType === 'register') signature = helper.signatureRegisterParams(p);
    else if (options.encryptType === 'web') signature = helper.signatureWebParams(p);
    else signature = helper.signatureAndroidParams(p, data);
  }

  delete process.env.platform;
  return {
    notSignOption: options.notSign === true,
    notSignatureOption: options.notSignature === true,
    encryptType: options.encryptType,
    hasSignature: signature !== null,
    signature,
    paramKeys: Object.keys(p).sort(),
    dfid,
  };
}

const out = {};

out.songUrl = {};
out.songUrl.standard = withFixedRandom(seqA, () =>
  resolveRequest(capture('/module/song_url.js', {
    hash: '6af00fbd4d444a82c005843eef9dc2d4',
    album_id: 0, album_audio_id: 0, quality: 128,
    cookie: { KUGOU_API_MID: '12345678901234567890123456789012' },
  }), 'standard'), 'standard');

out.songUrl.lite = withFixedRandom(seqA, () =>
  resolveRequest(capture('/module/song_url.js', {
    hash: '6af00fbd4d444a82c005843eef9dc2d4',
    album_id: 0, album_audio_id: 0, quality: 128,
    cookie: { KUGOU_API_MID: '12345678901234567890123456789012' },
  }, 'lite'), 'lite'), 'lite');

out.searchLyric = resolveRequest(capture('/module/search_lyric.js', {
  album_audio_id: 0,
  duration: 243722,
  hash: '6af00fbd4d444a82c005843eef9dc2d4',
  keywords: 'Letter - arkady sevidov',
  man: 'yes',
  cookie: {},
}), 'standard', 'standard');

console.log(JSON.stringify(out, null, 2));
