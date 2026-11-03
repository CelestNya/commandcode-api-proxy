/* refshot probe: 截取 Auroraqua-UI 参考实现的真实渲染
 * 目的：把参考仓库的仪表盘/设置页渲染成截图 + 提取计算样式，
 * 作为果冻主题复刻的对照基准（颜色、材质、圆角、间距一律以实测为准）。
 */
const { spawn } = require("child_process");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9344;
const BASE = "http://127.0.0.1:5199";

async function main() {
  const chrome = spawn(CHROME, [
    "--headless", "--disable-gpu", "--no-sandbox", "--use-gl=swiftshader",
    `--remote-debugging-port=${debugPort}`, "--window-size=1600,1000",
    "--user-data-dir=/tmp/cdp-profile-ref", "about:blank",
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

  // ---------- 1. 仪表盘（aurora 方案默认） ----------
  await send("Page.navigate", { url: `${BASE}/case3/dashboard` });
  await new Promise((r) => setTimeout(r, 4000));
  await shot("ref-dashboard");

  // ---------- 2. 提取计算样式 ----------
  const styles = await evalJs(`(() => {
    const cs = (el) => el ? getComputedStyle(el) : null;
    const pick = (el, keys) => {
      if (!el) return null;
      const s = cs(el);
      const o = {};
      keys.forEach(k => { o[k] = s.getPropertyValue(k); });
      return o;
    };
    const boxKeys = ["background-color", "background-image", "border-color", "border-width",
      "border-radius", "box-shadow", "backdrop-filter", "-webkit-backdrop-filter",
      "padding", "gap", "color", "font-size", "font-weight"];
    const body = pick(document.body, ["background-image", "background-color", "color", "font-family"]);
    const root = {};
    ["--bg-primary", "--color-primary", "--color-secondary", "--color-accent",
     "--text-heading", "--text-body", "--text-secondary", "--text-muted",
     "--color-neutral-50", "--color-neutral-100", "--color-neutral-200", "--color-neutral-300",
     "--color-neutral-400", "--color-neutral-500", "--color-neutral-600", "--color-neutral-700",
     "--color-neutral-800", "--color-neutral-900"].forEach(k => {
      root[k] = cs(document.documentElement).getPropertyValue(k).trim();
    });
    // 玻璃卡（liquid-glass）
    const glass = document.querySelector(".liquid-glass");
    const glassStyle = pick(glass, boxKeys);
    // 侧栏
    const sidebar = document.querySelector(".sidebar-panel") || document.querySelector(".liquid-glass.rounded-2xl");
    const sidebarStyle = pick(sidebar, boxKeys);
    // 统计卡文字
    const statLabel = document.querySelector(".text-muted");
    const statValue = document.querySelector(".text-heading");
    const txt = {
      label: pick(statLabel, ["color", "font-size", "font-weight"]),
      value: pick(statValue, ["color", "font-size", "font-weight"]),
    };
    // 表格行
    const row = document.querySelector(".bg-white\\/20");
    const rowStyle = pick(row, boxKeys);
    // 背景光斑
    const blobs = [...document.querySelectorAll(".animate-gradient-drift")].slice(0, 4).map(b => ({
      w: b.style.width, h: b.style.height, top: b.style.top, right: b.style.right,
      bottom: b.style.bottom, left: b.style.left, opacity: b.style.opacity,
      bg: b.style.background, anim: b.style.animationDuration,
      delay: b.style.animationDelay, radius: cs(b).borderRadius,
    }));
    // 表格容器
    const tbl = document.querySelector(".grid.grid-cols-4");
    const tblStyle = pick(tbl, boxKeys);
    // 通用：哪些元素用了 backdrop-filter
    const blurs = {};
    document.querySelectorAll("*").forEach(el => {
      const s = cs(el);
      const bf = s.backdropFilter || s.webkitBackdropFilter;
      if (bf && bf !== "none") {
        const cls = (el.className || "").toString().split(" ").slice(0, 2).join(".");
        blurs[cls] = blurs[cls] || { bf, count: 0 };
        blurs[cls].count++;
      }
    });
    return { body, root, glassStyle, sidebarStyle, txt, rowStyle, blobs, tblStyle, blurs };
  })()`);
  console.log("=== 计算样式 ===");
  console.log(JSON.stringify(styles, null, 1));

  // ---------- 3. 设置页 ----------
  await send("Page.navigate", { url: `${BASE}/settings` });
  await new Promise((r) => setTimeout(r, 3500));
  await shot("ref-settings");

  process.exit(0);
}
main().catch((e) => { console.error("ERR", e); process.exit(1); });
