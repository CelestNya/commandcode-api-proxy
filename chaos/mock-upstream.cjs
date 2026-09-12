// 混沌测试用 CC 上游模拟器：可编程故障注入。
// 用法：node mock-upstream.js   （监听 9888）
// 控制接口：POST /__mode {"mode":"..."}   GET /__stats
// 故障模式：ok refuse status_429 status_500 status_403 slow_headers
//           hang reset_mid error_event garbage trickle
const http = require("node:http");
const PORT = 9888;

let mode = "ok";
const stats = { hits: 0, byMode: {}, closedByUs: 0 };

const MODELS = { object: "list", data: [{ id: "deepseek/deepseek-v4-flash", object: "model" }] };
const ev = (type, data = {}) => ({ type, data });
const startEv = () => ev("start", { model: "mock" });
const deltaEv = (t) => ev("text-delta", { text: t });
const finishEv = () => ev("finish", { finishReason: "stop", usage: { promptTokens: 1, completionTokens: 1, totalTokens: 2 } });

function ndjsonHead(res) {
  res.writeHead(200, { "Content-Type": "application/json" });
}
function w(res, obj) {
  try { res.write("data: " + JSON.stringify(obj) + "\n"); } catch {}
}

const server = http.createServer((req, res) => {
  if (req.url.startsWith("/__stats")) {
    res.writeHead(200, { "Content-Type": "application/json" });
    return res.end(JSON.stringify(stats));
  }
  if (req.url.startsWith("/__mode")) {
    let b = "";
    req.on("data", (c) => (b += c));
    req.on("end", () => {
      try { mode = JSON.parse(b).mode || mode; } catch {}
      res.writeHead(200);
      res.end("mode=" + mode);
    });
    return;
  }
  if (req.url.startsWith("/provider/v1/models")) {
    res.writeHead(200, { "Content-Type": "application/json" });
    return res.end(JSON.stringify(MODELS));
  }
  if (!req.url.startsWith("/alpha/generate")) {
    res.writeHead(404);
    return res.end();
  }

  const m = mode;
  stats.hits += 1;
  stats.byMode[m] = (stats.byMode[m] || 0) + 1;

  if (m === "refuse") {
    req.socket.destroy();
    return;
  }
  if (m === "status_429" || m === "status_500" || m === "status_403") {
    const code = Number(m.split("_")[1]);
    res.writeHead(code, { "Content-Type": "application/json" });
    return res.end(JSON.stringify({ error: { message: "mock " + m } }));
  }
  if (m === "slow_headers") {
    // 6s 后才回头 —— 超过混沌代理的 4s 建连超时，用于测超时重试
    setTimeout(() => {
      try { ndjsonHead(res); w(res, startEv()); w(res, deltaEv("ok")); w(res, finishEv()); res.end(); } catch {}
    }, 6000);
    return;
  }

  ndjsonHead(res);
  if (m === "hang") {
    // 只发 start，然后永久沉默 —— 测 idle 超时
    w(res, startEv());
    return;
  }
  if (m === "reset_mid") {
    w(res, startEv());
    w(res, deltaEv("Hello "));
    setTimeout(() => {
      w(res, deltaEv("world"));
      setTimeout(() => { stats.closedByUs++; req.socket.destroy(); }, 150);
    }, 100);
    return;
  }
  if (m === "error_event") {
    w(res, startEv());
    w(res, ev("error", { message: "mock: upstream exploded" }));
    return res.end();
  }
  if (m === "garbage") {
    res.write("data: {{{不是json\n");
    res.write("data: also-not-json\n");
    return res.end();
  }
  if (m === "trickle") {
    // 慢滴流：每 300ms 一块，共 15 块，间隔远小于 idle 阈值
    let i = 0;
    const t = setInterval(() => {
      i += 1;
      if (i > 15) { clearInterval(t); w(res, finishEv()); return res.end(); }
      w(res, deltaEv("OK"));
    }, 300);
    req.socket.on("close", () => clearInterval(t));
    return;
  }
  // ok / 未知模式 → 正常完成
  w(res, startEv());
  w(res, deltaEv("好的"));
  w(res, finishEv());
  res.end();
});

server.listen(PORT, "127.0.0.1", () => console.log(`mock CC upstream on :${PORT}`));
