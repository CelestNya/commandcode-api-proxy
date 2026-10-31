/* trendsplit probe: 拆分后的三张趋势图（Token 柱 / 费用条 / 命中率线）
 * 验证：渲染结构、x 对齐、悬停 tooltip、窗口切换、窄屏、主题切换。 */
const { spawn } = require("child_process");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1243/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9341;

async function main() {
  const chrome = spawn(CHROME, ["--headless", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${debugPort}`, "--window-size=1440,900", "--user-data-dir=/tmp/cdp-profile-trend", "about:blank"], { stdio: "ignore" });
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
  const evalJs = async (expr) => (await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true })).result.value;

  async function shot(name) {
    const { data } = await send("Page.captureScreenshot", { format: "png" });
    require("fs").writeFileSync(`.probe/shots/${name}.png`, Buffer.from(data, "base64"));
    console.log(`shot: .probe/shots/${name}.png`);
  }

  // ---- 1440 宽，先切到 30 天窗口（探针库数据在 10 月中下旬，24h 窗口为空） ----
  await send("Page.navigate", { url: "http://127.0.0.1:8796/webui/#/stats" });
  await new Promise((r) => setTimeout(r, 2500));
  await evalJs(`(async () => {
    document.querySelector('.window-picker a[data-h="720"]').click();
    await new Promise(r => setTimeout(r, 1500));
    return 1;
  })()`);
  let r = await evalJs(`(() => {
    const out = {};
    const ids = ["trend-tokens", "trend-cost", "trend-rate"];
    out.svgs = ids.map(id => {
      const s = document.getElementById(id);
      if (!s) return { missing: true };
      return { id, viewBox: s.getAttribute("viewBox"), children: s.childNodes.length, w: s.clientWidth };
    });
    out.legend1 = document.getElementById("trend-legend")?.innerText;
    out.legend2 = document.getElementById("trend-rate-legend")?.innerText;
    out.bucketNote = document.getElementById("trend-bucket-note")?.innerText;
    out.foot = document.getElementById("trend-foot")?.innerText;
    out.title = document.querySelector(".trend-chart")?.closest(".panel")?.querySelector("h2")?.childNodes[0]?.textContent?.trim();
    // x 对齐：三张图第一根网格线 x1 应一致
    out.gridX = ids.map(id => {
      const l = document.querySelector("#" + id + " line");
      return l ? l.getAttribute("x1") : null;
    });
    // 柱数量：tokens 三根一组
    const bars = document.querySelectorAll("#trend-tokens .t-bar");
    out.tokenBars = bars.length;
    out.costBars = document.querySelectorAll("#trend-cost .t-bar").length;
    out.reqBars = document.querySelectorAll("#trend-rate .t-bar").length;
    out.rateLine = document.querySelectorAll("#trend-rate .trend-line").length;
    // 桶数与窗口
    out.windowLabel = document.getElementById("window-range")?.innerText;
    return out;
  })()`);
  console.log("== 1440 24h ==", JSON.stringify(r, null, 1));
  await shot("trend-1440");

  // ---- 悬停 tooltip：悬到一根真实的柱子上（空桶的均费无意义）----
  r = await evalJs(`(async () => {
    const svg = document.getElementById("trend-tokens");
    const bars = [...svg.querySelectorAll(".t-bar")];
    const bar = bars[Math.floor(bars.length / 2)] || bars[0];
    const rect = bar.getBoundingClientRect();
    const cx = rect.left + rect.width / 2, cy = rect.top + rect.height / 2;
    svg.dispatchEvent(new MouseEvent("mousemove", { clientX: cx, clientY: cy, bubbles: true }));
    await new Promise(r => setTimeout(r, 150));
    const tip = document.getElementById("trend-tip");
    const out = { tipHidden: tip.hidden, tipText: tip.innerText, barColor: getComputedStyle(bar).fill };
    return out;
  })()`);
  console.log("== hover ==", JSON.stringify(r, null, 1));
  await evalJs(`(() => { const s = document.getElementById("trend-tokens"); s.dispatchEvent(new MouseEvent("mouseleave", { bubbles: true })); return 1; })()`);

  // ---- 切 7 天窗口 ----
  r = await evalJs(`(async () => {
    document.querySelector('.window-picker a[data-h="168"]').click();
    await new Promise(r => setTimeout(r, 1200));
    return {
      bucketNote: document.getElementById("trend-bucket-note")?.innerText,
      tokenBars: document.querySelectorAll("#trend-tokens .t-bar").length,
      rateLine: document.querySelectorAll("#trend-rate .trend-line").length,
      xticks: [...document.querySelectorAll("#trend-rate text")].filter(t => /\d{2}-\d{2}/.test(t.textContent)).map(t => t.textContent),
    };
  })()`);
  console.log("== 7d ==", JSON.stringify(r));
  await shot("trend-7d");

  // ---- 切 30 天 ----
  r = await evalJs(`(async () => {
    document.querySelector('.window-picker a[data-h="720"]').click();
    await new Promise(r => setTimeout(r, 1200));
    return {
      bucketNote: document.getElementById("trend-bucket-note")?.innerText,
      tokenBars: document.querySelectorAll("#trend-tokens .t-bar").length,
    };
  })()`);
  console.log("== 30d ==", JSON.stringify(r));
  await shot("trend-30d");

  // ---- 浅色主题 ----
  r = await evalJs(`(async () => {
    document.documentElement.setAttribute("data-theme", "light");
    await new Promise(r => setTimeout(r, 300));
    const rect = document.querySelector(".trend-chart").closest(".panel").getBoundingClientRect();
    return { ok: true };
  })()`);
  console.log("== light ==", JSON.stringify(r));
  await shot("trend-light");

  // ---- 果冻主题 ----
  r = await evalJs(`(async () => {
    document.documentElement.setAttribute("data-theme", "jelly");
    await new Promise(r => setTimeout(r, 300));
    return { ok: true };
  })()`);
  await shot("trend-jelly");
  await evalJs(`document.documentElement.removeAttribute("data-theme"); 1`);

  // ---- 900 窄屏 ----
  await send("Emulation.setDeviceMetricsOverride", { width: 900, height: 900, deviceScaleFactor: 1, mobile: false });
  await new Promise((r) => setTimeout(r, 800));
  r = await evalJs(`(() => {
    const svg = document.getElementById("trend-tokens");
    return { w: svg.clientWidth, viewBox: svg.getAttribute("viewBox"), overflowX: document.documentElement.scrollWidth > 900 };
  })()`);
  console.log("== 900 narrow ==", JSON.stringify(r));
  await shot("trend-900");

  process.exit(0);
}
main();
