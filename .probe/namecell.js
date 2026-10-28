/* one-off: model name cell geometry at a few widths */
const { spawn } = require("child_process");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1243/chrome-headless-shell-win64/chrome-headless-shell.exe";
const port = 9337;
const BASE = "http://127.0.0.1:8799/webui/";
const EXPR = `(() => {
  const rows = [...document.querySelectorAll("#model-body tr")];
  return JSON.stringify(rows.map(tr => {
    const td = tr.querySelector(".m-name");
    const r = td.getBoundingClientRect();
    return { name: td.textContent.trim().slice(0, 24), cw: Math.round(r.width), sw: td.scrollWidth, ch: td.clientWidth };
  }));
})()`;
async function main() {
  const chrome = spawn(CHROME, ["--headless", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${port}`, "--window-size=1440,900", "--user-data-dir=/tmp/cdp-name", "about:blank"], { stdio: "ignore" });
  process.on("exit", () => { try { chrome.kill(); } catch {} });
  let target = null;
  for (let i = 0; i < 40; i++) {
    try { const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json(); target = list.find((t) => t.type === "page"); if (target) break; } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (m, p) => new Promise((res, rej) => { const mid = ++id; ws.onmessage = (e) => { const x = JSON.parse(e.data); if (x.id === mid) x.error ? rej(new Error(x.error.message)) : res(x.result); }; ws.send(JSON.stringify({ id: mid, method: m, params: p })); });
  await send("Runtime.enable", {}); await send("Page.enable", {});
  await send("Page.navigate", { url: BASE + "#/stats" });
  await new Promise((r) => setTimeout(r, 3000));
  for (const w of [1440, 1920]) {
    await send("Emulation.setDeviceMetricsOverride", { width: w, height: 900, deviceScaleFactor: 1, mobile: false });
    await new Promise((r) => setTimeout(r, 900));
    const r = await send("Runtime.evaluate", { expression: EXPR, returnByValue: true });
    console.log(`W${w}:`, r.result.value);
  }
  ws.close();
}
main().catch((e) => { console.error("ERR", e.message); process.exit(1); });
