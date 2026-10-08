// 证明「对象型参数值参与 android 签名时用的是插入序 JSON」。
// 上游 util/helper.js:61-80 对 typeof === 'object' 的值先 JSON.stringify，
// 而 JS 对象键序是插入序。Rust 侧若用默认的 BTreeMap（字典序）就会得到别的串。
//
// 用法：KUGOU_UPSTREAM=/path/to/KuGouMusicApi node tools/kat/kat_object_order.js
const path = process.env.KUGOU_UPSTREAM || '/home/xibie/KuGouMusicApi';
const helper = require(path + '/util/helper.js');

const out = {};

// 故意让插入序与字典序不同：z 在 a 前面。
const obj = {};
obj.zeta = 1;
obj.alpha = 2;
obj.mid = 3;

out.insertionOrderJson = JSON.stringify(obj);
out.sortedOrderJson = JSON.stringify(Object.fromEntries(Object.entries(obj).sort()));

for (const platform of ['standard', 'lite']) {
  process.env.platform = platform;
  const params = { keyword: 'x', nested: obj };
  out[platform] = {
    android: helper.signatureAndroidParams(params, ''),
    paramsString: Object.keys(params)
      .sort()
      .map((k) => `${k}=${typeof params[k] === 'object' ? JSON.stringify(params[k]) : params[k]}`)
      .join(''),
  };
}
delete process.env.platform;

console.log(JSON.stringify(out, null, 2));
