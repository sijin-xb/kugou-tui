# KAT 对照脚本（开发期专用）

这里的脚本**不参与构建**，也不是测试夹具的运行依赖。它们唯一的用途是：
从上游 KuGouMusicApi 的 JavaScript 实现里，用固定输入算出基准值，
供 `src/api/native/*.rs` 里的 known-answer 测试断言。

之所以要留在这里而不是随手丢在 `/tmp`：那些期望值是**从上游源码跑出来的**，
不是手抄的常量。哪天上游改了算法、或者要复核某个签名，得能重新跑出同一份
基准再比对；没有脚本就只剩一堆来历不明的十六进制字符串。

## 依赖的上游版本

```
仓库：https://github.com/MakcRe/KuGouMusicApi
版本：v1.6.0
commit：a5a98013cce79fe0ae2ad65fc84b68176ebcfc1e
日期：2026-08-14
```

`kat_rsa.js` 还依赖上游 `node_modules/node-forge`，所以必须在一个装过依赖的
clone 里跑（`npm install`）。

## 用法

```sh
export KUGOU_UPSTREAM=/path/to/KuGouMusicApi   # 默认 /home/xibie/KuGouMusicApi
node tools/kat/kat_gen.js       # 三套签名、signKey/signParamsKey、裸 RSA、calculateMid
node tools/kat/kat_rsa.js       # PKCS1 v1.5（注入固定填充）、公钥 n/e/bitLength
node tools/kat/kat_device.js    # randomString / getGuid / calculateMid（注入固定 Math.random）
node tools/kat/kat_notsign.js   # 截获 song_url / search_lyric 真实 options，复算签名
node tools/kat/kat_object_order.js  # 对象型参数值的 JSON 键序（preserve_order 的依据）
node tools/kat/kat_request.js       # 固定时间与随机，录 request.js 构造出的整份请求
node tools/kat/probe_outbound.js    # 录真实出站请求：URL / 参数 / 头
node tools/kat/probe_outbound.js lite   # 只跑 lite 平台
```

各脚本把结果以 JSON 打到 stdout，Rust 侧对应测试的期望值就是从这里抄进去的。

## 脚本清单

| 脚本 | 对照的 Rust 模块 | 做法 |
|---|---|---|
| `kat_gen.js` | `src/api/native/sign.rs`、`crypto.rs` | 直接调上游 `util/helper.js`，两平台各跑一遍 |
| `kat_rsa.js` | `src/api/native/crypto.rs` | 把 `forge.random.getBytes` 换成 `[0x01..0x10]` 循环，让 PKCS1 填充可复现 |
| `kat_device.js` | `src/api/native/device.rs` | 替换 `Math.random` 为固定序列，锁定 `randomString`/`getGuid` |
| `kat_notsign.js` | `src/api/native/sign.rs` | 假 `useAxios` 截获 `module/song_url.js`、`module/search_lyric.js` 真正发出的整份参数 |
| `kat_object_order.js` | `src/api/native/sign.rs` | 对象型参数值的键序：证明上游用插入序而非字典序 |
| `kat_request.js` | `src/api/native/transport.rs`、`mod.rs` | 钉死 `Date.now`/`Math.random`，录 `request.js` 构造出的 URL/参数/头/body，供逐字节断言 |
| `probe_outbound.js` | `src/api/native/`（网络层） | 本地假 gateway 录真实出站请求，供 native 逐项对齐 |

`probe_outbound.js` 的注入身份是 `server.js:230-266` 中间件那组 cookie：
`KUGOU_API_MID = calculateMid(guid)`、`KUGOU_API_GUID`、`KUGOU_API_DEV`、
`KUGOU_API_MAC`、`KUGOU_API_WEBGL`、`dfid`。`Math.random` 同样被固定，
否则 `song_url` 内部的 `randomString(24)` 每次不一样，无法比对。

`krc_probe.json` 是 `kat_notsign.js` 之外的一份真实 `/lyric` 响应样本，
`krc.rs` 的解密 KAT 用的是它的 `content` 字段（逐字歌词、含 `[language:]` 标签）。

## 已知坑

- **`process.env.platform` 的时序**：上游 `util/helper.js` 与各 module 在调用时
  才读 `platform`，但 `require` 缓存会让 helper 顶层捕获的值粘住。`kat_gen.js`
  每个平台都 `delete require.cache`，`kat_notsign.js` 的 `capture()` 显式收
  `platform` 参数——早期版本漏传过一次，混出了标准版参数配 lite 盐的
  不存在的签名（`df1eda84…`），查了很久。
- **`notSign: true` 是死参数**：`util/request.js:126` 读的是
  `options.notSignature`，全仓库无一处读 `notSign`。别照字面「跳过签名」。
- **`probe_outbound.js` 不排序参数、入参要对齐**：参数顺序本身就是签名输入
  （`signatureAndroidParams` 先 `sort(key)` 再拼串），早期版本按字母序打印，
  把真实顺序盖掉了；另外入参值（`pagesize`、`quality`、`hash`）会影响签名，
  随便填得到的是另一条请求，没法跟 native 比。
- **axios 的 query 编码不是 `encodeURIComponent`**：
  `node_modules/axios/lib/helpers/buildURL.js` 的 `encode()` 是在
  `encodeURIComponent` 之后放行 `:` `$` `,`，并把 `%20` 写成 `+`。照
  `encodeURIComponent` 写，`,`、`:`、`$` 会被多编码一层，URL 与 Node 版不再
  逐字节相同。注意 `AxiosURLSearchParams.js` 里那个同名的 `encode` 是**另一个
  函数**（只在显式传 encoder 时才用），不要照它写。基准由本地起 server 同时
  打印 `axios.getUri()` 与 `req.url` 抓出，两者逐字符相同（axios 1.20.0）。
- **参数顺序即签名输入**：`signatureAndroidParams` 先 `sort(key)` 再
  `.map(\`${k}=${v}\`)`；`signatureWebParams` 先 `.map(\`${k}=${v}\`)` 再
  `.sort()`（排的是渲染串，不是 key）。两者顺序不同，别写反。
