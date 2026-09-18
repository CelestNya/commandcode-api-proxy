// 缓存创建/命中循环验证 mock：监听 9889。
// 按次数轮换：第 1 次请求 = 创建缓存（cacheCreationTokens=100），
// 之后每次 = 命中缓存（cacheReadTokens=80）。
const http = require("node:http");
const PORT = 9889;
let hits = 0;
const ev = (type, data = {}) => ({ type, data });
const server = http.createServer((req, res) => {
  if (req.url.startsWith("/provider/v1/models")) {
    res.writeHead(200, { "Content-Type": "application/json" });
    return res.end(JSON.stringify({ object: "list", data: [{ id: "gemini-3.7-flash", object: "model" }] }));
  }
  if (!req.url.startsWith("/alpha/generate")) { res.writeHead(404); return res.end(); }
  hits += 1;
  const first = hits === 1;
  const usage = {
    promptTokens: 100,
    completionTokens: 10,
    totalTokens: 110,
    inputTokenDetails: first
      ? { cacheCreationTokens: 100, cacheReadTokens: 0 }
      : { cacheCreationTokens: 0, cacheReadTokens: 80 },
  };
  console.error(`[mock] generate hit #${hits} first=${first}`);
  res.writeHead(200, { "Content-Type": "application/json" });
  res.write("data: " + JSON.stringify(ev("start", { model: "gemini-3.7-flash" })) + "\n");
  res.write("data: " + JSON.stringify(ev("text-delta", { text: "ok" })) + "\n");
  res.write("data: " + JSON.stringify(ev("finish", { finishReason: "stop", usage })) + "\n");
  res.end();
});
server.listen(PORT, "127.0.0.1", () => console.error(`cache-cycle mock on :${PORT}`));
