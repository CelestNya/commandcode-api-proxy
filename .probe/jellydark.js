/* jelly-dark 验证探针：抓暗夜档三视图 + 浅色同布局对照
 * 用法: PROBE_BASE=http://127.0.0.1:8797/webui node .probe/jellydark.js
 */
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9352;
const BASE = process.env.PROBE_BASE || "http://127.0.0.1:8797/webui";
const { spawn } = require("child_process");

async function main() {
  const chrome = spawn(CHROME, [
    "--headless", "--disable-gpu", "--no-sandbox", "--use-gl=swiftshader",
    `--remote-debugging-port=${debugPort}`, "--window-size=1440,900",
    "--user-data-dir=/tmp/cdp-profile-jd", "about:blank",
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
  async function setViewport(w, h) {
    await send("Emulation.setDeviceMetricsOverride", { width: w, height: h, deviceScaleFactor: 1, mobile: false });
  }
  async function open(hash, theme, win = "720") {
    await send("Page.navigate", { url: `${BASE}/?theme=${theme}${hash}` });
    await wait(2600);
    await evalJs(`document.documentElement.setAttribute("data-theme","${theme}")`);
    await wait(400);
    if (win) {
      await evalJs(`(() => { const a = document.querySelector('.window-picker a[data-h="${win}"]'); if (a) a.click(); return !!a; })()`);
      await wait(2600);
    }
  }

  // ① 暗夜档 · 概览（有图表）
  await setViewport(1440, 900);
  await open("#/stats", "jelly-dark");
  await shot("jd-dark-stats");

  // ② 暗夜档 · 设置页外观
  await open("#/settings", "jelly-dark", null);
  await evalJs(`(() => { const g = document.getElementById('theme-grid'); if (g) g.scrollIntoView({ block: 'center' }); return !!g; })()`);
  await wait(900);
  await shot("jd-dark-themes");

  // ③ 暗夜档 · 明细
  await open("#/attempts", "jelly-dark");
  await evalJs(`(() => { const f = document.getElementById('a-from'), t = document.getElementById('a-to'), b = document.getElementById('a-apply'); if (!f||!t||!b) return false; f.value='2026-10-01T00:00'; t.value='2026-11-03T23:59'; b.click(); return true; })()`);
  await wait(2600);
  await shot("jd-dark-attempts");

  // ④ 浅色 jelly 同布局对照（验证结构同源、只翻色调）
  await setViewport(1440, 900);
  await open("#/stats", "jelly");
  await shot("jd-light-stats");

  process.exit(0);
}
main().catch((e) => { console.error("ERR", e); process.exit(1); });
