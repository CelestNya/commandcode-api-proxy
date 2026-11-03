/* verify probe: re-check all fixed points + capture screenshots */
const { spawn } = require("child_process");
const fs = require("fs");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9335;

async function main() {
  const chrome = spawn(CHROME, ["--headless", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${debugPort}`, "--window-size=1440,900", "--user-data-dir=/tmp/cdp-profile-5", "about:blank"], { stdio: "ignore" });
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
  await send("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });

  // ---- stats view ----
  await send("Page.navigate", { url: "http://127.0.0.1:8799/webui/#/stats" });
  await new Promise((r) => setTimeout(r, 2800));
  let r = await send("Runtime.evaluate", { expression: `(() => {
    const out = {};
    const mc = document.querySelector(".metric-cards");
    out.metricCardTops = [...mc.children].map(c => c.offsetTop);
    out.metricCardCols = getComputedStyle(mc).gridTemplateColumns.split(" ").length;
    const rows = [...document.querySelectorAll("#model-body tr")];
    out.modelRowBottoms = rows.map(tr => Math.round(tr.getBoundingClientRect().bottom));
    out.viewport = 900;
    const fills = [...document.querySelectorAll("#trend-chart path[fill-opacity]")];
    out.areaFills = fills.map(f => ({ stroke: f.getAttribute("stroke"), op: f.getAttribute("fill-opacity") }));
    const lines = [...document.querySelectorAll("#trend-chart path.trend-line")];
    out.lineSpans = lines.map(l => { const d = l.getAttribute("d"); const nums = d.match(/-?\\d+\\.\\d+/g) || []; return { series: l.getAttribute("data-series"), x0: nums[0], xN: nums[nums.length - 2] }; });
    return JSON.stringify(out);
  })()`, returnByValue: true });
  console.log("STATS:", r.result.value);
  let shot = await send("Page.captureScreenshot", { format: "png" });
  fs.writeFileSync("D:/Projects/cc-proxy/.probe/v2-stats-1440.png", Buffer.from(shot.data, "base64"));

  // ---- attempts view ----
  await send("Page.navigate", { url: "http://127.0.0.1:8799/webui/#/attempts" });
  await new Promise((r) => setTimeout(r, 2800));
  r = await send("Runtime.evaluate", { expression: `(() => {
    const out = {};
    out.aFrom = document.getElementById("a-from").value;
    out.aTo = document.getElementById("a-to").value;
    const lat = document.querySelector(".lat-cell");
    out.latHtml = lat ? lat.innerHTML.slice(0, 120) : "none";
    const latR = lat ? lat.getBoundingClientRect() : null;
    out.latTruncated = lat ? (lat.scrollWidth > lat.clientWidth + 1) : null;
    const mc = document.querySelector(".mini-cards");
    out.miniTops = [...mc.children].map(c => c.offsetTop);
    out.miniCols = getComputedStyle(mc).gridTemplateColumns.split(" ").length;
    return JSON.stringify(out);
  })()`, returnByValue: true });
  console.log("ATTEMPTS:", r.result.value);
  shot = await send("Page.captureScreenshot", { format: "png" });
  fs.writeFileSync("D:/Projects/cc-proxy/.probe/v2-attempts-1440.png", Buffer.from(shot.data, "base64"));
  ws.close();
}
main().catch((e) => { console.error("ERR", e.message); process.exit(1); });
