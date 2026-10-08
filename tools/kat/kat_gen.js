// 用上游 KuGouMusicApi 的真实实现生成 KAT 基准值。
// 只用于开发期对照，不进入仓库。
// 上游 KuGouMusicApi 的 clone 路径；默认按开发机布局，可用 KUGOU_UPSTREAM 覆盖。
const path = process.env.KUGOU_UPSTREAM || '/home/xibie/KuGouMusicApi';
const { cryptoMd5, cryptoRSAEncrypt, rsaEncrypt2 } = require(path + '/util/crypto.js');

const FIXED = {
  dfid: '1234567890abcdef12345678',
  mid: '12345678901234567890123456789012',
  uuid: '-',
  clienttime: 1700000000,
  hash: '6af00fbd4d444a82c005843eef9dc2d4',
};

function androidParams(extra) {
  return Object.assign(
    {
      dfid: FIXED.dfid,
      mid: FIXED.mid,
      uuid: FIXED.uuid,
      appid: process.env.platform === 'lite' ? 3116 : 1005,
      clientver: process.env.platform === 'lite' ? 11440 : 20489,
      clienttime: FIXED.clienttime,
    },
    extra,
  );
}

function run(platform) {
  process.env.platform = platform;
  // 每个平台都要重新 require，因为 helper.js 在模块顶层捕获了 config.json
  delete require.cache[require.resolve(path + '/util/helper.js')];
  const helper = require(path + '/util/helper.js');
  const out = {};

  // --- android 签名：普通参数 ---
  const p1 = androidParams({ keyword: '周杰伦', page: 1, pagesize: 30 });
  out.androidSearch = helper.signatureAndroidParams(p1);

  // --- android 签名：lite/标准 各自的 song_url 参数（含 encryptKey 产出的 key） ---
  const songUrlParams = androidParams({
    hash: FIXED.hash,
    album_id: 0,
    album_audio_id: 0,
    quality: 128,
  });
  songUrlParams.key = helper.signKey(
    songUrlParams.hash,
    songUrlParams.mid,
    undefined,
    songUrlParams.appid,
  );
  out.songUrlKey = songUrlParams.key;
  out.songUrlSign = helper.signatureAndroidParams(songUrlParams);

  // --- android 签名：search_lyric（clearDefaultParams，只有模块自己的参数） ---
  const lyricParams = {
    album_audio_id: 0,
    appid: 1005,
    clientver: 20489,
    duration: 243722,
    hash: FIXED.hash,
    keyword: 'Letter - arkady sevidov',
    lrctxt: 1,
    man: 'yes',
  };
  out.searchLyricSign = helper.signatureAndroidParams(lyricParams);

  // --- android 签名：artist_audios 带 signParamsKey ---
  const artistParams = androidParams({ artistid: 3520, sort: 1, page: 1, pagesize: 30 });
  artistParams.sign = helper.signParamsKey(FIXED.clienttime);
  out.artistAudiosSignKey = artistParams.sign;
  out.artistAudiosSign = helper.signatureAndroidParams(artistParams);

  // --- web 签名 ---
  out.webSign = helper.signatureWebParams({
    appid: 1005,
    clientver: 20489,
    clienttime: FIXED.clienttime,
    mid: FIXED.mid,
    uuid: '-',
  });

  // --- register 签名 ---
  out.registerSign = helper.signatureRegisterParams({
    part: 1,
    platid: 1,
    p: 'abcdef',
  });

  // --- signParams / signCloudKey（不在迁移范围，但顺手锁住，成本为零） ---
  out.signParams = helper.signParams({ a: '1', b: '2' }, 'body');
  out.signCloudKey = helper.signCloudKey(FIXED.hash, 2);

  // --- 裸 RSA：确定性 ---
  out.rawRsa = cryptoRSAEncrypt('hello world');

  // --- calculateMid / getGuid 形态 ---
  const util = require(path + '/util/util.js');
  out.midFromGuid = util.calculateMid('5f2b1c3d4e5f60718293a4b5c6d7e8f9');
  out.md5OfGuid = cryptoMd5('5f2b1c3d4e5f60718293a4b5c6d7e8f9');

  return out;
}

const result = { standard: run('standard'), lite: run('lite') };
console.log(JSON.stringify(result, null, 2));
