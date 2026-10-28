/* logs filterbar: fresh load per width */
const { spawn } = require("child_process");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1243/chrome-headless-shell-win64/chrome-headless-shell.exe";
const port = 9341;
const BASE = "http://127.0.0.1:8799/webui/";
const EXPR = `(() => {
  const fb = document.querySelector("#view-logs .filterbar");
  if (!fb) return "no-logs-filterbar";
  const r = fb.getBoundingClientRect();
  const rows = {};
  [...fb.children].forEach(k => { const y = Math.round(k.getBoundingClientRect().y); (rows[y] = rows[y] || []).push((k.textContent || "").trim().slice(0, 4)); });
  return JSON.stringify({ w: Math.round(r.width), h: Math.round(r.height), rows });
})()`;
async function main() {
  const chrome = spawn(CHROME, ["--headless", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${port}`, "--window-size=1440,900", "--user-data-dir=/tmp/cdp-fb3", "about:blank"], { stdio: "ignore" });
  process.on("exit", () => { try { chrome.kill(); } catch {} });
  let target = null;
  for (let i = 0; i < 40; i++) {
    try { const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json(); target = list.find((t) => t.type === "page"); if (target) break; } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (m, p) => new Promise((res, rej) => { const mid = ++id; ws.onmessage = (e) => { const x = JSON.parse(e.data); if (x.id === mid) x.error ? rej(new Error(x.error.message)) : res(x.result); }; ws.send(JSON.stringify({ id: mid, method: m, params: p })); });
  await send("Runtime.enable", {}); await send("Page.enable", {});
  for (const w of [901, 1000, 1280, 1440, 1920]) {
    await send("Emulation.setDeviceMetricsOverride", { width: w, height: 900, deviceScaleFactor: 1, mobile: false });
    await send("Page.navigate", { url: BASE + "#/logs" });
    await new Promise((r) => setTimeout(r, 2600));
    const r = await send("Runtime.evaluate", { expression: EXPR, returnByValue: true });
    console.log(`W${w} LOG ${r.result.value}`);
  }
  ws.close();
}
main().catch((e) => { console.error("ERR", e.message); process.exit(1); });
