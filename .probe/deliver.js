/* deliver probe: 交付截图（有数据的视图 + 设置页外观面板）
 * 用法: PROBE_BASE=http://127.0.0.1:8796/webui node .probe/deliver.js
 */
const { spawn } = require("child_process");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9351;
const BASE = process.env.PROBE_BASE || "http://127.0.0.1:8796/webui";

async function main() {
  const chrome = spawn(CHROME, [
    "--headless", "--disable-gpu", "--no-sandbox", "--use-gl=swiftshader",
    `--remote-debugging-port=${debugPort}`, "--window-size=1440,900",
    "--user-data-dir=/tmp/cdp-profile-dl", "about:blank",
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
  // 统一以果冻主题打开，并切到 30 天窗口（验证数据最新到 10-28）
  async function open(hash, theme = "jelly", win = "720") {
    await send("Page.navigate", { url: `${BASE}/?theme=${theme}${hash}` });
    await wait(2600);
    await evalJs(`document.documentElement.setAttribute("data-theme","${theme}")`);
    await wait(400);
    if (win) {
      await evalJs(`(() => {
        const a = document.querySelector('.window-picker a[data-h="${win}"]');
        if (a) a.click();
        return !!a;
      })()`);
      await wait(2600);
    }
  }

  // ① 概览页（有图表数据）
  await setViewport(1440, 900);
  await open("#/stats");
  await shot("dl-jelly-stats");

  // ② 设置页 · 外观面板
  await open("#/settings", "jelly", null);
  await evalJs(`(() => {
    const g = document.getElementById('theme-grid');
    if (g) g.scrollIntoView({ block: 'center' });
    return !!g;
  })()`);
  await wait(900);
  await shot("dl-jelly-themes");

  // ③ 明细页（有记录：明细页有自己的日期输入，不吃概览的时间窗）
  await open("#/attempts");
  await evalJs(`(() => {
    const f = document.getElementById('a-from'), t = document.getElementById('a-to');
    const b = document.getElementById('a-apply');
    if (!f || !t || !b) return false;
    f.value = '2026-10-01T00:00';
    t.value = '2026-11-03T23:59';
    b.click();
    return true;
  })()`);
  await wait(2600);
  await shot("dl-jelly-attempts");

  // ④ 同一布局的浅色主题，便于对照
  await setViewport(1440, 900);
  await open("#/stats", "light");
  await shot("dl-light-stats");

  // ⑤ 窄屏
  await setViewport(900, 900);
  await open("#/stats", "jelly");
  await shot("dl-jelly-900");

  process.exit(0);
}
main().catch((e) => { console.error("ERR", e); process.exit(1); });
