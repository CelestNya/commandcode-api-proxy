// 验证设置页「外观」面板：卡片渲染、点击切换、data-theme 与 localStorage 同步
const { spawn } = require("child_process");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const port = 9421;
const BASE = "http://127.0.0.1:18924/webui/#/settings";

const chrome = spawn(CHROME, [
  "--headless", "--disable-gpu", "--no-sandbox",
  "--use-gl=angle", "--use-angle=swiftshader", "--enable-unsafe-swiftshader",
  `--remote-debugging-port=${port}`,
  "--user-data-dir=/tmp/cdp-theme-pick",
  "about:blank",
], { stdio: "ignore" });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function run() {
  let target = null;
  for (let i = 0; i < 40; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      target = list.find((t) => t.type === "page");
      if (target) break;
    } catch {}
    await sleep(300);
  }
  if (!target) throw new Error("no target");

  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (method, params) => new Promise((res, rej) => {
    const mid = ++id;
    const handler = (e) => {
      const msg = JSON.parse(e.data);
      if (msg.id === mid) {
        ws.removeEventListener("message", handler);
        msg.error ? rej(new Error(msg.error.message)) : res(msg.result);
      }
    };
    ws.addEventListener("message", handler);
    ws.send(JSON.stringify({ id: mid, method, params }));
  });
  const evalJs = async (expr) => {
    const r = await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + " " + (r.exceptionDetails.exception?.description || ""));
    return r.result.value;
  };

  await send("Page.enable", {});
  await send("Runtime.enable", {});
  await send("Emulation.setDeviceMetricsOverride", { width: 1440, height: 980, deviceScaleFactor: 1, mobile: false });

  // 1. 默认状态（无 localStorage → 深色）
  await send("Page.navigate", { url: BASE });
  await sleep(3500);
  console.log("--- 默认状态 ---");
  console.log(await evalJs(`JSON.stringify({
    dataTheme: document.documentElement.getAttribute("data-theme"),
    cards: document.querySelectorAll(".theme-card").length,
    pressed: Array.from(document.querySelectorAll(".theme-card")).map(b => b.getAttribute("data-theme-key") + "=" + b.getAttribute("aria-pressed")),
    settingsVisible: !!document.querySelector("#view-settings.active"),
    swatches: document.querySelectorAll(".theme-swatch").length
  })`));

  // 2. 点击「浅瓷」
  await evalJs(`document.querySelector('.theme-card[data-theme-key="light"]').click(); "ok"`);
  await sleep(600);
  console.log("--- 点击「浅瓷」后 ---");
  console.log(await evalJs(`JSON.stringify({
    dataTheme: document.documentElement.getAttribute("data-theme"),
    stored: localStorage.getItem("ccproxy.theme"),
    pressed: Array.from(document.querySelectorAll(".theme-card")).map(b => b.getAttribute("data-theme-key") + "=" + b.getAttribute("aria-pressed")),
    bodyBg: getComputedStyle(document.body).backgroundColor,
    cardBg: getComputedStyle(document.querySelector(".panel")).backgroundColor
  })`));

  // 3. 切到果冻
  await evalJs(`document.querySelector('.theme-card[data-theme-key="jelly"]').click(); "ok"`);
  await sleep(600);
  console.log("--- 点击「冰态果冻」后 ---");
  console.log(await evalJs(`JSON.stringify({
    dataTheme: document.documentElement.getAttribute("data-theme"),
    stored: localStorage.getItem("ccproxy.theme"),
    pressed: Array.from(document.querySelectorAll(".theme-card")).map(b => b.getAttribute("data-theme-key") + "=" + b.getAttribute("aria-pressed"))
  })`));

  // 4. 切回深色
  await evalJs(`document.querySelector('.theme-card[data-theme-key="dark"]').click(); "ok"`);
  await sleep(500);
  console.log("--- 点击「深色」后（应回到无 data-theme）---");
  console.log(await evalJs(`JSON.stringify({
    dataTheme: document.documentElement.getAttribute("data-theme"),
    stored: localStorage.getItem("ccproxy.theme")
  })`));

  // 5. 刷新后是否记住选择（localStorage 持久化）
  await evalJs(`document.querySelector('.theme-card[data-theme-key="jelly"]').click(); "ok"`);
  await sleep(400);
  await send("Page.navigate", { url: BASE });
  await sleep(3000);
  console.log("--- 刷新后（应记住果冻）---");
  console.log(await evalJs(`JSON.stringify({
    dataTheme: document.documentElement.getAttribute("data-theme"),
    stored: localStorage.getItem("ccproxy.theme")
  })`));

  ws.close();
  chrome.kill();
}

run().then(() => process.exit(0)).catch((e) => {
  console.error("ERR", e.message);
  try { chrome.kill(); } catch {}
  process.exit(1);
});
