/* verify shell fix: sidebar left full-height, topbar beside it (not below) */
const { spawn } = require("child_process");
const fs = require("fs");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9336;
const W = Number(process.argv[2] || 1440), H = Number(process.argv[3] || 900);

async function main() {
  const chrome = spawn(CHROME, ["--headless", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${debugPort}`, `--window-size=${W},${H}`, "--user-data-dir=/tmp/cdp-profile-6", "about:blank"], { stdio: "ignore" });
  process.on("exit", () => { try { chrome.kill(); } catch {} });
  let target = null;
  for (let i = 0; i < 30; i++) {
    try { const list = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json(); target = list.find((t) => t.type === "page"); if (target) break; } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (m, p) => new Promise((res, rej) => { const mid = ++id; ws.onmessage = (e) => { const x = JSON.parse(e.data); if (x.id === mid) x.error ? rej(new Error(x.error.message)) : res(x.result); }; ws.send(JSON.stringify({ id: mid, method: m, params: p })); });
  await send("Runtime.enable", {}); await send("Page.enable", {});
  await send("Emulation.setDeviceMetricsOverride", { width: W, height: H, deviceScaleFactor: 1, mobile: false });
  await send("Page.navigate", { url: "http://127.0.0.1:8799/webui/#/stats" });
  await new Promise((r) => setTimeout(r, 2800));
  const r = await send("Runtime.evaluate", { expression: `(() => {
    const rect = (sel) => { const e = document.querySelector(sel); if (!e) return null; const b = e.getBoundingClientRect(); return { x: Math.round(b.x), y: Math.round(b.y), w: Math.round(b.width), h: Math.round(b.height), bottom: Math.round(b.bottom) }; };
    const out = { viewport: ${W} + "x" + ${H} };
    out.shell = rect(".shell");
    out.sidebar = rect(".sidebar");
    out.topbar = rect(".topbar");
    out.main = rect(".main");
    out.metricCards = rect(".metric-cards");
    out.metricTops = [...document.querySelectorAll(".metric-cards > *")].map(c => c.offsetTop);
    out.metricCols = getComputedStyle(document.querySelector(".metric-cards")).gridTemplateColumns.split(" ").length;
    out.ovGrid = rect(".ov-grid");
    out.ovCols = getComputedStyle(document.querySelector(".ov-grid")).gridTemplateColumns;
    out.modelRows = [...document.querySelectorAll("#model-body tr")].map(tr => Math.round(tr.getBoundingClientRect().bottom));
    out.shellDisplay = getComputedStyle(document.querySelector(".shell")).display;
    out.shellDir = getComputedStyle(document.querySelector(".shell")).flexDirection;
    out.docScrollH = document.documentElement.scrollHeight;
    return JSON.stringify(out);
  })()`, returnByValue: true });
  console.log(r.result.value);
  const shot = await send("Page.captureScreenshot", { format: "png" });
  fs.writeFileSync(`D:/Projects/cc-proxy/.probe/fix-shell-${W}.png`, Buffer.from(shot.data, "base64"));
  ws.close();
}
main().catch((e) => { console.error("ERR", e.message); process.exit(1); });
