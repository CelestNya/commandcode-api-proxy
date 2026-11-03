/* jellycmp probe: 果冻主题复刻度对照
 * 目标：量化本项目果冻主题与 Auroraqua-UI 参考实现的差异。
 * 检查项：背景渐层、光斑、玻璃材质（bg/blur/border/shadow）、文字色、对比度。
 */
const { spawn } = require("child_process");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9346;

async function main() {
  const chrome = spawn(CHROME, [
    "--headless", "--disable-gpu", "--no-sandbox", "--use-gl=swiftshader",
    `--remote-debugging-port=${debugPort}`, "--window-size=1440,900",
    "--user-data-dir=/tmp/cdp-profile-jelly", "about:blank",
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
  const evalJs = async (expr) =>
    (await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true })).result.value;

  async function shot(name) {
    const { data } = await send("Page.captureScreenshot", { format: "png" });
    require("fs").writeFileSync(`.probe/shots/${name}.png`, Buffer.from(data, "base64"));
    console.log(`shot: .probe/shots/${name}.png`);
  }

  // 打开概览页并切到果冻主题
  await send("Page.navigate", { url: "http://127.0.0.1:8795/webui/#/stats?theme=jelly" });
  await new Promise((r) => setTimeout(r, 3000));
  await evalJs(`(async () => {
    document.documentElement.setAttribute("data-theme", "jelly");
    await new Promise(r => setTimeout(r, 400));
    return 1;
  })()`);
  await new Promise((r) => setTimeout(r, 800));
  await shot("jelly-now");

  const out = await evalJs(`(() => {
    const cs = (el) => el ? getComputedStyle(el) : null;
    const pick = (el, keys) => {
      if (!el) return null;
      const s = cs(el);
      const o = {};
      keys.forEach(k => { o[k] = s.getPropertyValue(k).trim(); });
      return o;
    };
    const mat = ["background-color", "background-image", "border-color", "border-radius",
      "box-shadow", "backdrop-filter", "-webkit-backdrop-filter", "color"];
    const root = {};
    ["--bg","--surface","--surface-2","--surface-3","--outline","--outline-variant",
     "--outline-strong","--text","--muted","--primary","--cost","--primary-container",
     "--on-primary-container","--primary-border","--ok","--warn","--error",
     "--interrupted","--aborted","--chart-hit","--chart-6","--chart-7","--chart-8",
     "--chart-9","--chart-10","--log-bg"].forEach(k => {
      root[k] = cs(document.documentElement).getPropertyValue(k).trim();
    });
    return {
      root,
      body: pick(document.body, ["background-image", "background-color", "color", "font-family"]),
      mcard: pick(document.querySelector(".mcard"), mat),
      panel: pick(document.querySelector(".panel"), mat),
      sidebar: pick(document.querySelector(".sidebar"), mat),
      topbar: pick(document.querySelector(".topbar"), mat),
      filterbar: pick(document.querySelector(".filterbar"), mat),
      tableWrap: pick(document.querySelector(".table-wrap"), mat),
      th: pick(document.querySelector("th"), mat),
      chip: pick(document.querySelector(".chip"), mat),
      navActive: pick(document.querySelector(".nav-item.active"), mat),
      metricLabel: pick(document.querySelector(".mcard .muted, .mcard .lbl, .mcard span"), mat),
    };
  })()`);
  console.log(JSON.stringify(out, null, 1));
  process.exit(0);
}
main().catch((e) => { console.error("ERR", e); process.exit(1); });
