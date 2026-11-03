// 模拟"上游 200 但返回 HTML 错误页"（captive portal / 中间代理注入）。
const http = require("node:http");
const PORT = 9890;
http.createServer((req, res) => {
  let b = "";
  req.on("data", (c) => (b += c));
  req.on("end", () => {
    res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
    res.end("<!DOCTYPE html><html><head><title>502 Bad Gateway</title></head><body><h1>Bad Gateway</h1><p>The proxy could not reach the upstream server.</p></body></html>");
  });
}).listen(PORT, "127.0.0.1", () => console.log("html mock on " + PORT));
