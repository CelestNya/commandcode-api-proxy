/* jelly-dark 修复后验证：1420x813，24h 空态 / 30d 有数据 / tooltip 底色
 * 用法: PROBE_BASE=http://127.0.0.1:8797/webui node .probe/jdv.js
 */
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9354;
const BASE = process.env.PROBE_BASE || "http://127.0.0.1:8797/webui";
const { spawn } = require("child_process");

async function main() {
  const chrome = spawn(CHROME, [
    "--headless", "--disable-gpu", "--no-sandbox", "--use-gl=swiftshader",
    `--remote-debugging-port=${debugPort}`, "--window-size=1420,813",
    "--user-data-dir=/tmp/cdp-profile-jdv", "about:blank",
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

  // 暗档 · 24h（用户截图同窗口 = 空态）
  await send("Page.navigate", { url: `${BASE}/?theme=jelly-dark#/stats` });
  await wait(2600);
  await evalJs(`document.documentElement.setAttribute("data-theme","jelly-dark")`);
  await wait(900);
  const cardBg = await evalJs(`(() => { const c = document.querySelector('.mcard'); const cs = getComputedStyle(c); return { bg: cs.backgroundColor, border: cs.borderColor, shadow: cs.boxShadow }; })()`);
  console.log("card computed:", JSON.stringify(cardBg));
  await shot("jdv-dark-24h");

  // 切 30 天 → 有数据
  await evalJs(`(() => { const a = document.querySelector('.window-picker a[data-h="720"]'); if (a) a.click(); return !!a; })()`);
  await wait(2800);
  await shot("jdv-dark-30d");

  // 强制显示 tooltip 验证底色（用样例内容，只看底面是否够实）
  await evalJs(`(() => {
    const t = document.getElementById('trend-tip'); if (!t) return false;
    t.style.display = 'block'; t.style.left = '380px'; t.style.top = '300px';
    t.innerHTML = "<div class='tt-head'><b>10-28 12:00</b><span>128 请求</span></div>" +
      "<div class='tt-row'><b>Input</b><span>12,340</span></div>" +
      "<div class='tt-row'><b>Output</b><span>4,560</span></div>" +
      "<div class='tt-row'><b>Cache Read</b><span>8,100</span></div>" +
      "<div class='tt-row'><b>费用</b><span class='tt-cost'>$0.0421</span></div>" +
      "<div class='tt-row'><b>命中率</b><span>76.1%</span></div>";
    const cs = getComputedStyle(t); return { bg: cs.backgroundColor, border: cs.borderColor };
  })()`).then(v => console.log("tip computed:", JSON.stringify(v)));
  await wait(500);
  await shot("jdv-dark-tooltip");

  process.exit(0);
}
main().catch(e => { console.error("ERR", e); process.exit(1); });
