/* 边界验证：三张图分别悬停 + 右缘翻转 + 顶部翻转 + 900 窄屏
 * 用法: PROBE_BASE=http://127.0.0.1:8797/webui node .probe/tipedge.js
 */
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9357;
const BASE = process.env.PROBE_BASE || "http://127.0.0.1:8797/webui";
const { spawn } = require("child_process");

async function main() {
  const chrome = spawn(CHROME, [
    "--headless", "--disable-gpu", "--no-sandbox", "--use-gl=swiftshader",
    `--remote-debugging-port=${debugPort}`, "--window-size=1420,813",
    "--user-data-dir=/tmp/cdp-profile-tipedge", "about:blank",
  ], { stdio: "ignore" });
  process.on("exit", () => { try { chrome.kill(); } catch {} });
  let target = null;
  for (let i = 0; i < 40; i++) {
    try { const list = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json(); target = list.find(t => t.type === "page"); if (target) break; } catch {}
    await new Promise(r => setTimeout(r, 500));
  }
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (m, p) => new Promise((res, rej) => { const mid = ++id; ws.onmessage = e => { const x = JSON.parse(e.data); if (x.id === mid) x.error ? rej(new Error(x.error.message)) : res(x.result); }; ws.send(JSON.stringify({ id: mid, method: m, params: p })); });
  await send("Runtime.enable", {}); await send("Page.enable", {}); await send("Network.enable", {}); await send("Network.setCacheDisabled", { cacheDisabled: true });
  const evalJs = async (expr) => (await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true })).result.value;
  const wait = ms => new Promise(r => setTimeout(r, ms));
  async function shot(name) { const { data } = await send("Page.captureScreenshot", { format: "png" }); require("fs").writeFileSync(`.probe/shots/${name}.png`, Buffer.from(data, "base64")); console.log("shot:", name); }

  await send("Emulation.setDeviceMetricsOverride", { width: 1420, height: 813, deviceScaleFactor: 1, mobile: false });
  await send("Page.navigate", { url: `${BASE}/?theme=jelly-dark#/stats` });
  await wait(2400);
  await evalJs(`document.documentElement.setAttribute("data-theme","jelly-dark")`);
  await wait(700);
  await evalJs(`(() => { const a = document.querySelector('.window-picker a[data-h="720"]'); if (a) a.click(); return !!a; })()`);
  await wait(2600);

  async function hover(svgId, fx, fy, label) {
    const g = await evalJs(`(() => { const s = document.getElementById('${svgId}'); const r = s.getBoundingClientRect(); return { left: r.left, top: r.top, w: r.width, h: r.height, vw: innerWidth, vh: innerHeight }; })()`);
    const mx = Math.round(g.left + g.w * fx), my = Math.round(g.top + g.h * fy);
    await send("Input.dispatchMouseEvent", { type: "mouseMoved", x: mx, y: my, button: "none", pointerType: "mouse" });
    await wait(320);
    const r = await evalJs(`(() => {
      const t = document.getElementById('trend-tip');
      if (t.hidden) return null;
      const b = t.getBoundingClientRect();
      return { l: b.left, t: b.top, r: b.right, b: b.bottom, w: b.width, h: b.height };
    })()`);
    if (!r) { console.log(`  ${label}: 未显示`); return; }
    let expL = mx + 14;
    if (expL + r.w > g.vw - 8) expL = mx - r.w - 14;
    if (expL < 8) expL = 8;
    let expT = my - r.h / 2;
    if (expT + r.h > g.vh - 8) expT = g.vh - r.h - 8;
    if (expT < 8) expT = 8;
    const flipped = r.r > g.vw - 8 && expL < mx ? " (右缘翻转)" : "";
    const below = expT === 8 ? " (顶部内收)" : "";
    const off = `Δ(${Math.round(r.l - expL)},${Math.round(r.t - expT)})`;
    const inView = (r.l >= 0 && r.t >= 0 && r.r <= g.vw + 1 && r.b <= g.vh + 1) ? "视口内" : "❌出视口";
    console.log(`  ${label}: 鼠标(${mx},${my}) rect(${Math.round(r.l)},${Math.round(r.t)})-(${Math.round(r.r)},${Math.round(r.b)}) ${off}${flipped}${below} ${inView}`);
  }

  console.log("=== 1420x813 · 三张图逐个悬停 ===");
  await hover("trend-tokens", 0.20, 0.5, "Token 图 左段");
  await hover("trend-tokens", 0.55, 0.5, "Token 图 中段");
  await hover("trend-tokens", 0.97, 0.5, "Token 图 右缘");
  await hover("trend-cost", 0.55, 0.5, "费用图 中段");
  await hover("trend-rate", 0.55, 0.5, "命中率图 中段");
  await hover("trend-tokens", 0.55, 0.04, "Token 图 顶缘");
  await shot("tipedge-1420");

  // 切视图：浮层挂在 body 上，必须由 route() 收起
  await evalJs(`(() => { location.hash = '#/attempts'; return true; })()`);
  await wait(1300);
  const stillShown = await evalJs(`!document.getElementById('trend-tip').hidden`);
  console.log(`=== 切到明细页后浮层: ${stillShown ? "❌ 仍显示（会浮在新视图上）" : "✅ 已收起"} ===`);
  await evalJs(`location.hash = '#/stats'`);
  await wait(1800);

  console.log("=== 900 窄屏 ===");
  await send("Emulation.setDeviceMetricsOverride", { width: 900, height: 813, deviceScaleFactor: 1, mobile: false });
  await wait(1400);
  await evalJs(`(() => { const a = document.querySelector('.window-picker a[data-h="720"]'); if (a) a.click(); return !!a; })()`);
  await wait(2400);
  await hover("trend-tokens", 0.5, 0.5, "900 Token 图 中段");
  await hover("trend-tokens", 0.97, 0.5, "900 Token 图 右缘");
  await shot("tipedge-900");

  process.exit(0);
}
main().catch(e => { console.error("ERR", e); process.exit(1); });
