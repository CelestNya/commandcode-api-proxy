/* 诊断：趋势 tooltip 为什么会偏移。
 * 1) 找 #trend-tip 的 fixed 包含块（backdrop-filter/filter/transform 会创建）
 * 2) 派发真实 mousemove，比对期望位置与实际 rect
 * 用法: PROBE_BASE=http://127.0.0.1:8797/webui node .probe/tipdiag.js
 */
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9356;
const BASE = process.env.PROBE_BASE || "http://127.0.0.1:8797/webui";
const { spawn } = require("child_process");

async function main() {
  const chrome = spawn(CHROME, [
    "--headless", "--disable-gpu", "--no-sandbox", "--use-gl=swiftshader",
    `--remote-debugging-port=${debugPort}`, "--window-size=1420,813",
    "--user-data-dir=/tmp/cdp-profile-tipdiag", "about:blank",
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

  for (const theme of ["jelly-dark", "jelly", "dark"]) {
    await send("Page.navigate", { url: `${BASE}/?theme=${theme}#/stats` });
    await wait(2400);
    await evalJs(`document.documentElement.setAttribute("data-theme","${theme}")`);
    await wait(700);
    await evalJs(`(() => { const a = document.querySelector('.window-picker a[data-h="720"]'); if (a) a.click(); return !!a; })()`);
    await wait(2600);

    // ① 祖链上的「包含块创建者」
    const chain = await evalJs(`(() => {
      const t = document.getElementById('trend-tip');
      const out = []; let n = t.parentElement;
      while (n && n !== document.documentElement) {
        const cs = getComputedStyle(n);
        const why = [];
        if (cs.backdropFilter && cs.backdropFilter !== 'none') why.push('backdrop-filter:' + cs.backdropFilter);
        if (cs.filter && cs.filter !== 'none') why.push('filter:' + cs.filter);
        if (cs.transform && cs.transform !== 'none') why.push('transform:' + cs.transform);
        if (cs.willChange && cs.willChange !== 'auto') why.push('will-change:' + cs.willChange);
        if (cs.contain && cs.contain !== 'none') why.push('contain:' + cs.contain);
        if (why.length) out.push({ el: n.tagName + '.' + (n.className || ''), why: why.join(' | '), rect: n.getBoundingClientRect().toJSON() });
        n = n.parentElement;
      }
      return out;
    })()`);
    console.log(`\n=== ${theme} · 创建 fixed 包含块的祖先 ===`);
    if (!chain.length) console.log("  (无) —— tooltip 应相对视口定位");
    chain.forEach(c => console.log(`  ${c.el}  [${c.why}]  rect(${Math.round(c.rect.left)},${Math.round(c.rect.top)})`));

    // ② 派发真实鼠标事件，比对期望 vs 实际
    const geo = await evalJs(`(() => { const s = document.getElementById('trend-tokens'); const r = s.getBoundingClientRect(); return { left: r.left, top: r.top, w: r.width, h: r.height }; })()`);
    const mx = Math.round(geo.left + geo.w * 0.5);
    const my = Math.round(geo.top + geo.h * 0.5);
    await send("Input.dispatchMouseEvent", { type: "mouseMoved", x: mx, y: my, button: "none", pointerType: "mouse" });
    await wait(400);
    const res = await evalJs(`(() => {
      const t = document.getElementById('trend-tip');
      const r = t.getBoundingClientRect();
      return { hidden: t.hidden, left: t.style.left, top: t.style.top, rectLeft: r.left, rectTop: r.top, rectW: r.width, rectH: r.height, vw: innerWidth, vh: innerHeight };
    })()`);
    if (res.hidden) { console.log(`  鼠标(${mx},${my}) → tooltip 未显示`); continue; }
    // 期望：右侧 14px、垂直居中，三向视口内收边
    let expL = mx + 14;
    if (expL + res.rectW > res.vw - 8) expL = mx - res.rectW - 14;
    if (expL < 8) expL = 8;
    let expT = my - res.rectH / 2;
    if (expT + res.rectH > res.vh - 8) expT = res.vh - res.rectH - 8;
    if (expT < 8) expT = 8;
    console.log(`  鼠标(${mx},${my})  期望 left=${Math.round(expL)} top=${Math.round(expT)}`);
    console.log(`  实际 rect(${Math.round(res.rectLeft)},${Math.round(res.rectTop)})  style left=${res.left} top=${res.top}`);
    console.log(`  偏移 Δ(${Math.round(res.rectLeft - expL)}, ${Math.round(res.rectTop - expT)})`);
    await shot(`tipdiag-${theme}`);
  }
  process.exit(0);
}
main().catch(e => { console.error("ERR", e); process.exit(1); });
