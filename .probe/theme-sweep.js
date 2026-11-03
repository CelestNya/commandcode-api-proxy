/* theme-sweep: 合并后的三主题抽查 —— 每套主题下截概览页与设置页，
 * 并读出页面主色/卡片底色，确认主题层没有被新并入的控件样式盖掉。 */
const { spawn } = require("child_process");
const fs = require("fs");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const PORT_CDP = 9362;
const BASE = process.env.PROBE_BASE || "http://127.0.0.1:8892/webui";

async function main() {
  const chrome = spawn(CHROME, [
    "--headless", "--disable-gpu", "--no-sandbox", "--use-gl=swiftshader",
    `--remote-debugging-port=${PORT_CDP}`, "--window-size=1440,900",
    "--user-data-dir=/tmp/cdp-profile-themes", "about:blank",
  ], { stdio: "ignore" });
  process.on("exit", () => { try { chrome.kill(); } catch {} });
  let target = null;
  for (let i = 0; i < 40; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT_CDP}/json/list`)).json();
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
    ws.onmessage = (e) => { const x = JSON.parse(e.data); if (x.id === mid) x.error ? rej(new Error(x.error.message)) : res(x.result); };
    ws.send(JSON.stringify({ id: mid, method: m, params: p }));
  });
  const errors = [];
  await send("Runtime.enable", {});
  await send("Page.enable", {});
  const evalJs = async (expr) =>
    (await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true })).result.value;

  for (const theme of ["jelly", "jelly-dark", "light", "dark"]) {
    await send("Page.navigate", { url: `${BASE}/#/stats` });
    await new Promise((r) => setTimeout(r, 2200));
    await evalJs(`(() => {
      if (${JSON.stringify(theme)} === 'dark') document.documentElement.removeAttribute('data-theme');
      else document.documentElement.setAttribute('data-theme', ${JSON.stringify(theme)});
      document.getElementById('w-from').value='2026-09-01T00:00';
      document.getElementById('w-to').value='2026-11-04T00:00';
      document.getElementById('w-apply').click();
      return 1;
    })()`);
    await new Promise((r) => setTimeout(r, 1800));
    const look = await evalJs(`(() => {
      const cs = (sel, prop) => { const e = document.querySelector(sel); return e ? getComputedStyle(e)[prop] : ''; };
      const root = getComputedStyle(document.documentElement);
      return {
        attr: document.documentElement.getAttribute('data-theme'),
        bodyBg: cs('body','backgroundColor'),
        panelBg: cs('.panel','backgroundColor'),
        panelRadius: cs('.panel','borderTopLeftRadius'),
        sdTrigger: cs('.sd-trigger','backgroundColor') + ' / ' + cs('.sd-trigger','borderTopLeftRadius'),
        primary: root.getPropertyValue('--primary').trim(),
        activeNav: cs('.nav-item.active','backgroundColor'),
      };
    })()`);
    let s1 = await send("Page.captureScreenshot", { format: "png" });
    fs.writeFileSync(`.probe/shots/theme-${theme}-overview.png`, Buffer.from(s1.data, "base64"));
    await send("Page.navigate", { url: `${BASE}/#/settings` });
    await new Promise((r) => setTimeout(r, 2200));
    let s2 = await send("Page.captureScreenshot", { format: "png" });
    fs.writeFileSync(`.probe/shots/theme-${theme}-settings.png`, Buffer.from(s2.data, "base64"));
    console.log(theme, JSON.stringify(look));
  }
  ws.close(); chrome.kill();
}
main().catch((e) => { console.error(e); process.exit(1); });
