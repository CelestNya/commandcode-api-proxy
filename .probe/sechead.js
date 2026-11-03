/* measure: gap between section title and caption (render-verified, not eyeballed) */
const { spawn } = require("child_process");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const port = 9345;
const URL = "file:///D:/Projects/cc-proxy/webui-concepts/trend-split.html";

async function main() {
  const chrome = spawn(CHROME, ["--headless", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${port}`, "--window-size=1200,1000", "--user-data-dir=/tmp/cdp-split2", "about:blank"], { stdio: "ignore" });
  process.on("exit", () => { try { chrome.kill(); } catch {} });
  let target = null;
  for (let i = 0; i < 40; i++) {
    try { const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json(); target = list.find((t) => t.type === "page"); if (target) break; } catch {}
    await new Promise((r) => setTimeout(r, 400));
  }
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (m, p) => new Promise((res, rej) => { const mid = ++id; ws.onmessage = (e) => { const x = JSON.parse(e.data); if (x.id === mid) x.error ? rej(new Error(x.error.message)) : res(x.result); }; ws.send(JSON.stringify({ id: mid, method: m, params: p })); });
  await send("Runtime.enable", {});
  await send("Page.navigate", { url: URL });
  await new Promise((r) => setTimeout(r, 900));
  const r = await send("Runtime.evaluate", { expression: `JSON.stringify([...document.querySelectorAll('.sec-head')].map(h=>{const k=[...h.children].map(c=>{const b=c.getBoundingClientRect();return {t:c.textContent.trim().slice(0,10),l:+b.left.toFixed(1),r:+b.right.toFixed(1),w:+b.width.toFixed(1)}});return {k, headW:+h.getBoundingClientRect().width.toFixed(1)}}))`, returnByValue: true });
  console.log(r.result.value);
  ws.close();
}
main().catch((e) => { console.error("ERR", e.message); process.exit(1); });
