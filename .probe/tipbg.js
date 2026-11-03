/* 读取三主题下 #trend-tip 的实际计算样式，确认覆盖是否生效 */
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9358;
const BASE = process.env.PROBE_BASE || "http://127.0.0.1:8797/webui";
const { spawn } = require("child_process");

async function main() {
  const chrome = spawn(CHROME, ["--headless", "--disable-gpu", "--no-sandbox", "--use-gl=swiftshader",
    `--remote-debugging-port=${debugPort}`, "--window-size=1420,813",
    "--user-data-dir=/tmp/cdp-profile-tipbg", "about:blank"], { stdio: "ignore" });
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
  const evalJs = async (e) => (await send("Runtime.evaluate", { expression: e, returnByValue: true, awaitPromise: true })).result.value;
  const wait = ms => new Promise(r => setTimeout(r, ms));

  for (const theme of ["jelly", "jelly-dark"]) {
    await send("Page.navigate", { url: `${BASE}/?theme=${theme}#/stats` });
    await wait(2200);
    await evalJs(`document.documentElement.setAttribute("data-theme","${theme}")`);
    await wait(600);
    const cs = await evalJs(`(() => {
      const t = document.getElementById('trend-tip');
      const s = getComputedStyle(t);
      return { theme: document.documentElement.getAttribute('data-theme'),
               bg: s.backgroundColor, border: s.borderColor, shadow: s.boxShadow,
               zIndex: s.zIndex, parent: t.parentElement.tagName };
    })()`);
    console.log(theme, "->", JSON.stringify(cs));
  }
  process.exit(0);
}
main().catch(e => { console.error("ERR", e); process.exit(1); });
