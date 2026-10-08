// 注入固定 Math.random，生成 guid / randomString 的 KAT。
// 上游 getGuid / randomString 都只依赖 Math.random，替换掉即可复现。
// 上游 KuGouMusicApi 的 clone 路径；默认按开发机布局，可用 KUGOU_UPSTREAM 覆盖。
const path = process.env.KUGOU_UPSTREAM || '/home/xibie/KuGouMusicApi';

// 固定序列：0, 0.1, 0.2, ... 循环；另生成一组包含 0 与 0.999... 的边界序列
function withFixedRandom(sequence, fn) {
  const original = Math.random;
  let i = 0;
  Math.random = () => sequence[i++ % sequence.length];
  try {
    return fn();
  } finally {
    Math.random = original;
  }
}

const util = require(path + '/util/util.js');

const seqA = Array.from({ length: 32 }, (_, i) => Number(((i % 10) / 10).toFixed(1)));
const seqB = [0, 0.9999999999999999, 0.5, 0.25, 0.75, 0.123456789, 0.987654321];

const out = {
  randomString: {
    seqA: withFixedRandom(seqA, () => util.randomString(16)),
    seqB: withFixedRandom(seqB, () => util.randomString(10)),
    len24: withFixedRandom(seqA, () => util.randomString(24)),
  },
  getGuid: {
    seqA: withFixedRandom(seqA, () => util.getGuid()),
    seqB: withFixedRandom(seqB, () => util.getGuid()),
  },
  // guid 是 md5(getGuid())；calculateMid 取 md5 hex 当 128bit 大整数转十进制
  midOf: {},
};

// 用 seqA 生成的 guid 再算 mid，形成一条完整链
const guidA = out.getGuid.seqA;
const { cryptoMd5 } = require(path + '/util/crypto.js');
out.midOf.guidA = { guid: guidA, md5: cryptoMd5(guidA), mid: util.calculateMid(guidA) };

out.sequences = { seqA, seqB };
console.log(JSON.stringify(out, null, 2));
