// 开发期探针：把上游 util/request.js 的真实出站请求录下来。
// 做法：本地起一个假 gateway，把 baseURL 指过去，再用真实 module 函数
// 配合真实 createRequest 发一次，记录 URL / 参数 / 请求头。
//
// 用途：native 侧要复刻这些请求，header 与参数必须逐项对齐。凭记忆写会错。
//
// 用法：
//   KUGOU_UPSTREAM=/path/to/KuGouMusicApi node tools/kat/probe_outbound.js
//   ... node tools/kat/probe_outbound.js lite      # 只跑 lite
const http = require('http');
const path = process.env.KUGOU_UPSTREAM || '/home/xibie/KuGouMusicApi';

const captured = [];
const server = http.createServer((req, res) => {
  captured.push({ method: req.method, url: req.url, headers: req.headers });
  res.writeHead(200, { 'Content-Type': 'application/json' });
  res.end(JSON.stringify({ status: 1, error_code: 0, data: { total: 0, lists: [] } }));
});

// 固定 Math.random，让 song_url 内部 randomString(24) 的 dfid 可复现
const seqA = Array.from({ length: 32 }, (_, i) => Number(((i % 10) / 10).toFixed(1)));
function withFixedRandom(sequence, fn) {
  const original = Math.random;
  let i = 0;
  Math.random = () => sequence[i++ % sequence.length];
  try { return fn(); } finally { Math.random = original; }
}

// server.js 中间件注入的那组 cookie，正是 Node 服务内部真正带上的身份。
// mid 由 calculateMid(guid) 得出，guid 是启动时生成的（每次重启都变）。
const util = require(path + '/util/util.js');
const GUID = '5f2b1c3d4e5f60718293a4b5c6d7e8f9';
const MID = util.calculateMid(GUID);

function serverCookies(platform, withDfid) {
  const c = {
    KUGOU_API_PLATFORM: platform,
    KUGOU_API_MID: MID,
    KUGOU_API_GUID: GUID,
    KUGOU_API_DEV: 'ABCDEFGHIJ',
    KUGOU_API_MAC: '02:00:00:00:00:00',
    KUGOU_API_WEBGL: 'deadbeef',
  };
  if (withDfid) c.dfid = '1234567890abcdef12345678';
  return c;
}

async function main() {
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const base = `http://127.0.0.1:${server.address().port}`;
  const { createRequest } = require(path + '/util/request.js');

  const platforms = process.argv[2] ? [process.argv[2]] : ['standard', 'lite'];

  for (const platform of platforms) {
    process.env.platform = platform;
    const cookie = serverCookies(platform, true);

    const cases = [
      // 入参对齐 kugou-tui 的实际调用：`page_size` 取配置默认以外的 100、
      // 音质取 flac、hash 取真实一首歌的——参数值会影响签名，随便填对不上。
      ['search', '/module/search.js', { keywords: '周杰伦', page: 1, pagesize: 100, cookie }],
      ['song_url', '/module/song_url.js', {
        hash: '0a69169202de95aaf24a9944ccf0730d',
        album_id: 0, album_audio_id: 0, quality: 'flac', cookie,
      }],
      ['search_lyric', '/module/search_lyric.js', {
        album_audio_id: 0, duration: 243722,
        hash: '0a69169202de95aaf24a9944ccf0730d',
        keywords: 'Letter - arkady sevidov', man: 'yes', cookie,
      }],
      ['lyric', '/module/lyric.js', {
        id: 19525574, accesskey: '0123456789ABCDEF0123456789ABCDEF',
        fmt: 'krc', decode: true, cookie,
      }],
    ];

    for (const [name, file, params] of cases) {
      delete require.cache[require.resolve(path + file)];
      const mod = require(path + file);
      captured.length = 0;
      try {
        await withFixedRandom(seqA, () => mod(params, (cfg) => createRequest({ ...cfg, baseURL: base })));
      } catch (error) {
        console.error(`[${platform}/${name}] 请求失败：`, error && error.message);
      }
      console.log(`\n===== ${platform} / ${name} =====`);
      for (const item of captured) {
        const url = new URL(item.url, base);
        // 不排序：参数顺序本身就是签名输入（signatureAndroidParams 先 sort(key)
        // 再拼串），按字母序打印会把真实顺序盖掉，对比时看不出顺序错。
        const entries = [...url.searchParams.entries()];
        console.log('path:', url.pathname, '| param 个数:', entries.length);
        for (const [k, v] of entries) console.log(`  ${k} = ${v}`);
        console.log('UA:', item.headers['user-agent']);
        const rest = { ...item.headers };
        for (const drop of ['user-agent', 'accept', 'accept-encoding', 'connection', 'host']) delete rest[drop];
        // 逐项列出，缺 clienttime 也要看得见
        console.log('headers:', JSON.stringify(rest));
      }
    }
  }

  server.close();
}

main();
