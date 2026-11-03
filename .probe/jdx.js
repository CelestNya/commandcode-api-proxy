/* jelly-dark 诊断：1420x813 复现，读 .mcard 盒模型/层叠/tooltip 底色，检测重叠
 * 用法: PROBE_BASE=http://127.0.0.1:8797/webui node .probe/jdx.js
 */
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9353;
const BASE = process.env.PROBE_BASE || "http://127.0.0.1:8797/webui";
const { spawn } = require("child_process");

async function main() {
  const chrome = spawn(CHROME, [
    "--headless", "--disable-gpu", "--no-sandbox", "--use-gl=swiftshader",
    `--remote-debugging-port=${debugPort}`, "--window-size=1420,813",
    "--user-data-dir=/tmp/cdp-profile-jdx", "about:blank",
  ], { stdio: "ignore" });
  process.on("exit", () => { try { chrome.kill(); } catch {} });

  let target = null;
  for (let i = 0; i < 40; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json();
      target = list.find((t) => t.type === "page");
      if (target) break;
    } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (m, p) => new Promise((res, rej) => {
    const mid = ++id;
    ws.onmessage = (e) => {
      const x = JSON.parse(e.data);
      if (x.id === mid) x.error ? rej(new Error(x.error.message)) : res(x.result);
    };
    ws.send(JSON.stringify({ id: mid, method: m, params: p }));
  });
  await send("Runtime.enable", {});
  await send("Page.enable", {});
  await send("Network.enable", {});
  await send("Network.setCacheDisabled", { cacheDisabled: true });
  const evalJs = async (expr) =>
    (await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true })).result.value;
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  async function shot(name) {
    const { data } = await send("Page.captureScreenshot", { format: "png" });
    require("fs").writeFileSync(`.probe/shots/${name}.png`, Buffer.from(data, "base64"));
    console.log(`shot: ${name}`);
  }

  await send("Emulation.setDeviceMetricsOverride", { width: 1420, height: 813, deviceScaleFactor: 1, mobile: false });

  async function inspect(theme) {
    await send("Page.navigate", { url: `${BASE}/?theme=${theme}#/stats` });
    await wait(2600);
    await evalJs(`document.documentElement.setAttribute("data-theme","${theme}")`);
    await wait(600);
    // 切 30 天窗口让图表有数据（与用户截图不是同一窗口，但布局一致）
    const diag = await evalJs(`(() => {
      const cards = [...document.querySelectorAll('.mcard')];
      const rects = cards.map((c, i) => {
        const r = c.getBoundingClientRect();
        const cs = getComputedStyle(c);
        return { i, cls: c.className, x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height),
          position: cs.position, zIndex: cs.zIndex, transform: cs.transform, willChange: cs.willChange,
          backdrop: cs.backdropFilter || cs.webkitBackdropFilter, boxShadow: cs.boxShadow, bg: cs.backgroundColor };
      });
      // 重叠检测：任意两卡矩形相交
      const overlaps = [];
      for (let a = 0; a < rects.length; a++) for (let b = a + 1; b < rects.length; b++) {
        const A = rects[a], B = rects[b];
        const ix = Math.min(A.x + A.w, B.x + B.w) - Math.max(A.x, B.x);
        const iy = Math.min(A.y + A.h, B.y + B.h) - Math.max(A.y, B.y);
        if (ix > 1 && iy > 1) overlaps.push({ a, b, ix, iy });
      }
      const grid = getComputedStyle(document.querySelector('.metric-cards'));
      const tip = document.getElementById('trend-tip');
      const tipCS = tip ? getComputedStyle(tip) : null;
      return { viewport: [innerWidth, innerHeight], gridCols: grid.gridTemplateColumns, cards: rects, overlaps,
        tip: tip ? { visible: tip.offsetParent !== null || getComputedStyle(tip).display !== 'none',
          bg: tipCS.backgroundColor, border: tipCS.borderColor, z: tipCS.zIndex, pos: tipCS.position,
          rect: (() => { const r = tip.getBoundingClientRect(); return [Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)]; })() } : null };
    })()`);
    await shot(`jdx-${theme}-1420`);
    return diag;
  }

  const dark = await inspect("jelly-dark");
  console.log("=== jelly-dark 诊断 ===");
  console.log(JSON.stringify(dark, null, 2));
  const light = await inspect("jelly");
  console.log("=== jelly(浅) 诊断 ===");
  console.log(JSON.stringify({ viewport: light.viewport, gridCols: light.gridCols, overlaps: light.overlaps, tip: light.tip }, null, 2));
  console.log("--- 浅色卡片前2张 ---");
  console.log(JSON.stringify(light.cards.slice(0, 2), null, 2));

  process.exit(0);
}
main().catch((e) => { console.error("ERR", e); process.exit(1); });
