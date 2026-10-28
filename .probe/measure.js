/* CDP probe: measure real computed layout of the attempts view + trend data shape */
const PAGE = process.argv[2] || "http://127.0.0.1:8799/webui/#/attempts";
const debugPort = 9222;

async function main() {
  // wait for the page target
  let target = null;
  for (let i = 0; i < 20; i++) {
    const list = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json();
    target = list.find((t) => t.type === "page");
    if (target) break;
    await new Promise((r) => setTimeout(r, 500));
  }
  if (!target) throw new Error("no page target");
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (method, params) => new Promise((res) => {
    const mid = ++id;
    ws.onmessage = (e) => { const m = JSON.parse(e.data); if (m.id === mid) res(m.result); };
    ws.send(JSON.stringify({ id: mid, method, params }));
  });
  await send("Runtime.enable", {});
  await send("Page.enable", {});
  await send("Page.navigate", { url: PAGE });
  await new Promise((r) => setTimeout(r, 2500));

  const expr = `(async () => {
    const out = {};
    const mc = document.querySelector(".mini-cards");
    const cs = getComputedStyle(mc);
    out.miniCards = { display: cs.display, cols: cs.gridTemplateColumns, width: mc.clientWidth,
      count: mc.children.length,
      cards: [...mc.children].map(c => ({ w: c.offsetWidth, top: c.offsetTop, cls: c.className })) };
    const oc = document.querySelector(".metric-cards");
    const ocs = getComputedStyle(oc);
    out.metricCards = { display: ocs.display, cols: ocs.gridTemplateColumns, width: oc.clientWidth,
      count: oc.children.length,
      tops: [...oc.children].map(c => c.offsetTop) };
    out.from = document.getElementById("a-from").value;
    out.to = document.getElementById("a-to").value;
    const j = await (await fetch("/webui/api/stats")).json();
    const tr = j.trend || [];
    out.trend = { len: tr.length, first: tr[0] && tr[0].date, last: tr[tr.length-1] && tr[tr.length-1].date,
      promptSum: tr.reduce((a,d)=>a+(d.prompt||0),0) };
    return JSON.stringify(out);
  })()`;
  const r = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true });
  console.log(r.result.value);
  ws.close();
}
main().catch((e) => { console.error("ERR", e.message); process.exit(1); });
