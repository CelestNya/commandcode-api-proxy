/* 概览渲染
 *
 * 指标卡、汇总、趋势图（含 PCHIP 平滑与降采样）、模型分布（环形图 + 明细表）。
 */

/* ── overview ── */
function renderStats(s, animate) {
  state.stats = s;
  renderSummary(s);
  renderTrend(s.trend || [], animate === true);
  renderModels(s.models || []);
}

/* X-axis ticks: daily buckets are "10-16", hourly buckets drop the raw ISO
   "T" and read "10-16 15:00" — the machine stamp never reaches the axis. */
function fmtTickDate(s) {
  if (!s) return "";
  var i = s.indexOf("T");
  if (i < 0) return s.slice(5);
  return s.slice(5, i) + " " + s.slice(i + 1, 16);
}

/* Everything in the overview except the trend chart. The live stream calls
   this on every snapshot regardless of the selected window — lifetime
   numbers are window-independent; the window cards follow the picker. */
function renderSummary(s) {
  var t = s.totals || {}, w = s.window || {};
  // AI SDK accounting: promptTokens already includes cached tokens and
  // completionTokens already includes reasoning tokens, so the totals are
  // prompt + completion; cached is shown as a subset of input.
  var tTok = (t.promptTokens || 0) + (t.completionTokens || 0);
  var wTok = (w.promptTokens || 0) + (w.completionTokens || 0);
  var fail = (t.error || 0) + (t.aborted || 0) + (t.interrupted || 0);
  /* Cache hit rate is the health signal this panel exists for: colour the
     value by threshold so a drop is visible at a glance (掉缓存诊断口径). */
  var hr = w.hitRate;
  var hrCls = hr == null ? "" : hr >= 0.8 ? " ok" : hr >= 0.5 ? "" : hr > 0 ? " warn" : " err";

  var cards = [
    { label: "窗口请求", value: fmtNum(w.attempts), sub: "总计: " + fmtNum(t.attempts), cls: "" },
    { label: "窗口 Token", value: fmtTokens(wTok), sub: "输入 " + fmtTokens(w.promptTokens) + "（含缓存 " + fmtTokens(w.cachedTokens) + "）/ 输出 " + fmtTokens(w.completionTokens), cls: "" },
    { label: "累计 Token", value: fmtTokens(tTok), sub: "输入 " + fmtTokens(t.promptTokens) + "（含缓存 " + fmtTokens(t.cachedTokens) + "）/ 输出 " + fmtTokens(t.completionTokens), cls: "accent" },
    { label: "费用", value: fmtUsd(w.costUsd), sub: "终身 " + fmtUsd(t.costUsd), cls: "cost" },
    { label: "缓存命中率", value: fmtPct(w.hitRate), sub: "窗口内命中 / 输入", cls: hrCls },
    { label: "总尝试", value: fmtNum(t.attempts), sub: "成功 " + fmtNum(t.ok) + " / 失败 " + fmtNum(fail), cls: "" }
  ];
  el("metric-cards").innerHTML = cards.map(function (c) {
    return '<div class="mcard ' + c.cls + '"><div class="mlabel">' + c.label + '</div>' +
           '<div class="mvalue">' + c.value + '</div><div class="msub">' + c.sub + '</div></div>';
  }).join("");

  /* top-bar consumption chip follows the selected window (the money number
     the user is looking at) */
  el("chip-cost").textContent = fmtUsd(w.costUsd != null ? w.costUsd : t.costUsd);
  renderMiniCards(s);
}

/* ── 趋势拆分（按量 / 按率） ──
 *
 * 绝对量与比率不挤一张图：Token 与费用是"量"，柱状图各自独立量程最直观；
 * 命中率是"率"，固定 0–100% 折线才诚实（轴被截断会把 93% 的平稳夸成波动）。
 * 三张图共用同一套桶几何（viewBox 宽、左右边距一致），x 轴刻度只画在最底部。
 *
 * 口径：cachedTokens 是 promptTokens 的子集（命中率 = cached / prompt），
 * 所以 Token 图三根柱（Input / Output / Cache Read）互不叠加，Cache Read
 * 只是 Input 的一部分 —— 脚注里明说，避免读者把三根柱加总。
 */

/* 桶数超过 maxGroups 时合并相邻桶：绝对量按求和（柱高 = 覆盖时段总量），
 * 命中率必须用合并后的 cached/prompt 重算而非取平均——一个小桶会把大桶
 * 的率拉偏。费用任一子桶计价即求和，全未计价保持 null。 */
function foldBuckets(raw, maxGroups) {
  var list = (raw || []).map(function (d) {
    return {
      date: d.date,
      attempts: d.attempts || 0,
      prompt: d.prompt || 0,
      completion: d.completion || 0,
      cached: d.cached || 0,
      costUsd: d.costUsd == null ? null : d.costUsd,
      hitRate: d.hitRate == null ? null : d.hitRate
    };
  });
  if (list.length <= maxGroups) return list;
  var step = Math.ceil(list.length / maxGroups);
  var out = [];
  for (var i = 0; i < list.length; i += step) {
    var chunk = list.slice(i, i + step);
    var t = { date: chunk[0].date, attempts: 0, prompt: 0, completion: 0, cached: 0, cost: 0, priced: false };
    chunk.forEach(function (c) {
      t.attempts += c.attempts;
      t.prompt += c.prompt;
      t.completion += c.completion;
      t.cached += c.cached;
      if (c.costUsd != null) { t.cost += c.costUsd; t.priced = true; }
    });
    t.costUsd = t.priced ? t.cost : null;
    t.hitRate = t.prompt > 0 ? t.cached / t.prompt : null;
    out.push(t);
  }
  return out;
}

/* 取整刻度：段长取 1/2/2.5/5/10 × 10^n，柱顶不顶格浪费 */
function niceScale(max, divs) {
  if (max <= 0) max = 1;
  var raw = max / divs;
  var exp = Math.pow(10, Math.floor(Math.log10(raw)));
  var f = raw / exp;
  var nice = f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10;
  return { step: nice * exp, top: nice * exp * divs };
}

/* 三张图共用的取色（CSS 变量，改 token 即换色，见 tokens.css 的 JS 契约） */
var TREND_COLORS = {
  input: "var(--primary)", output: "var(--ok)", cached: "var(--chart-7)",
  cost: "var(--cost)", hit: "var(--chart-hit)", reqBg: "var(--surface-3)"
};

/* 悬停状态：三张图共享桶索引与十字线。每次渲染重建线元素后按 idx 复原，
 * 轮询每秒重画时悬停不闪断。 */
var trendHover = { idx: -1, lines: [] };

function trendBucketX(i, n, ML, MR, W) {
  var slot = (W - ML - MR) / n;
  return ML + slot * (i + 0.5);
}

function renderTrend(raw, animate) {
  var svgT = el("trend-tokens"), svgC = el("trend-cost"), svgR = el("trend-rate");
  var narrow = svgT.clientWidth > 0 && svgT.clientWidth < 520;
  var W = narrow ? 420 : 900;
  var ML = narrow ? 40 : 48, MR = narrow ? 44 : 48;
  var buckets = foldBuckets(raw, narrow ? 24 : 48);
  var n = buckets.length;
  trendHover.lines = [];

  /* 悬停明细（含命中率与费用），跟随光标。 */
  var tip = el("trend-tip");
  function showTip(d, ev) {
    var totalIn = (d.prompt || 0);
    tip.innerHTML =
      "<div class='tt-head'><b>" + esc(fmtTickDate(d.date)) + "</b><span>" + fmtNum(d.attempts) + " 请求</span></div>" +
      "<div class='tt-row'><b>Input</b><span>" + fmtNum(d.prompt) + (d.cached > 0 ? "（含缓存 " + fmtNum(d.cached) + "）" : "") + "</span></div>" +
      "<div class='tt-row'><b>Output</b><span>" + fmtNum(d.completion) + "</span></div>" +
      "<div class='tt-row'><b>Cache Read</b><span>" + fmtNum(d.cached) + "</span></div>" +
      "<div class='tt-row'><b>费用</b><span class='tt-cost'>" + (d.costUsd != null ? fmtUsd(d.costUsd) : "—") + "</span></div>" +
      "<div class='tt-row'><b>命中率</b><span>" + fmtPct(d.hitRate) + "</span></div>" +
      /* 均费只在费用已知时给：costUsd 为 null 表示「没计价」，不是「零成本」
         ——null / attempts 在 JS 里等于 0,会把未知误报成 $0.0000。 */
      (totalIn > 0 && d.attempts > 0 && d.costUsd != null
        ? "<div class='tt-row'><b>单请求均费</b><span class='tt-cost'>" + fmtUsd(d.costUsd / d.attempts) + "</span></div>"
        : "");
    tip.hidden = false;
    var tw = tip.offsetWidth, th = tip.offsetHeight;
    var left = ev.clientX + 14, top = ev.clientY - th - 10;
    if (left + tw > window.innerWidth - 8) left = ev.clientX - tw - 14;
    if (top < 8) top = ev.clientY + 16;
    tip.style.left = left + "px";
    tip.style.top = top + "px";
  }
  function hideTip() {
    tip.hidden = true;
    trendHover.idx = -1;
    trendHover.lines.forEach(function (l) { l.setAttribute("opacity", 0); });
  }
  function moveHover(ev) {
    if (!n) return;
    var r = svgT.getBoundingClientRect();
    if (!r.width) return;
    var x = (ev.clientX - r.left) * (W / r.width);
    var slot = (W - ML - MR) / n;
    var idx = Math.max(0, Math.min(n - 1, Math.floor((x - ML) / slot)));
    trendHover.idx = idx;
    var cx = trendBucketX(idx, n, ML, MR, W);
    trendHover.lines.forEach(function (l) {
      l.setAttribute("x1", cx); l.setAttribute("x2", cx); l.setAttribute("opacity", 1);
    });
    showTip(buckets[idx], ev);
  }
  [svgT, svgC, svgR].forEach(function (svg) {
    svg.onmousemove = moveHover;
    svg.onmouseleave = hideTip;
  });

  /* 空态：三张图各给一句短文案，不再画装饰折线 */
  if (!n || buckets.every(function (d) { return d.attempts === 0 && !d.prompt && !d.completion && !d.cached; })) {
    function emptySvg(svg, H, msg) {
      svg.setAttribute("viewBox", "0 0 " + W + " " + H);
      svg.innerHTML = "<text x='" + W / 2 + "' y='" + H / 2 + "' fill='var(--muted)' font-size='12' text-anchor='middle'>" + msg + "</text>";
    }
    emptySvg(svgT, 210, "所选时段内暂无请求");
    emptySvg(svgC, 64, "无费用数据");
    emptySvg(svgR, 172, "无请求数据");
    el("trend-legend").innerHTML = "";
    el("trend-rate-legend").innerHTML = "";
    el("trend-bucket-note").textContent = "";
    el("trend-foot").textContent = "";
    [svgT, svgC, svgR].forEach(function (svg) { svg.classList.remove("trend-animate"); });
    return;
  }

  /* 图一：Token 分组柱（Input / Output / Cache Read，同一量程） */
  el("trend-legend").innerHTML =
    "<span><i class='k-sq' style='background:" + TREND_COLORS.input + "'></i>Input</span>" +
    "<span><i class='k-sq' style='background:" + TREND_COLORS.output + "'></i>Output</span>" +
    "<span><i class='k-sq' style='background:" + TREND_COLORS.cached + "'></i>Cache Read</span>";
  drawTokenBars(svgT, buckets, W, ML, MR, narrow, animate);
  /* 图二：费用条（同 x 轴，独立量程） */
  drawCostBars(svgC, buckets, W, ML, MR, animate);
  /* 图三：命中率折线 + 请求数淡柱（x 轴刻度画在这里） */
  el("trend-rate-legend").innerHTML =
    "<span><i style='background:" + TREND_COLORS.hit + "'></i>命中率</span>" +
    "<span><i class='k-sq' style='background:" + TREND_COLORS.reqBg + "'></i>请求数</span>";
  drawRateChart(svgR, buckets, W, ML, MR, narrow, animate);

  var active = buckets.filter(function (d) { return d.attempts > 0; }).length;
  el("trend-bucket-note").textContent = active + "/" + n + " 桶有数据";
  el("trend-foot").textContent =
    (narrow ? "" : "Cache Read 含在 Input 内，三根柱互不叠加 · ") +
    "空桶无柱、命中率断开 · 悬停看整桶明细";
}

/* Token 分组柱：三根一组，同一量程，取整刻度。 */
function drawTokenBars(svg, buckets, W, ML, MR, narrow, animate) {
  var H = 210, MT = 10, MB = 6;
  var pw = W - ML - MR, ph = H - MT - MB;
  svg.setAttribute("viewBox", "0 0 " + W + " " + H);
  var maxTok = 0;
  buckets.forEach(function (d) {
    maxTok = Math.max(maxTok, d.prompt, d.completion, d.cached);
  });
  var sc = niceScale(maxTok, 5), top = sc.top;
  var out = "";
  for (var g = 0; g <= 5; g++) {
    var y = MT + ph * g / 5;
    out += "<line x1='" + ML + "' y1='" + y + "' x2='" + (W - MR) + "' y2='" + y + "' stroke='var(--outline-variant)' stroke-width='1'/>";
    out += "<text x='" + (ML - 8) + "' y='" + (y + 3.5) + "' fill='var(--muted)' font-size='10' text-anchor='end'>" + fmtTokens(Math.round(top * (1 - g / 5))) + "</text>";
  }
  var n = buckets.length, slot = pw / n;
  var gw = Math.min(slot * 0.66, 34);           /* 宽桶（≤10 根）不铺满，居中留白 */
  var bw = Math.max(1, (gw - 3) / 3);
  var vals = ["prompt", "completion", "cached"], cols = [TREND_COLORS.input, TREND_COLORS.output, TREND_COLORS.cached];
  buckets.forEach(function (d, idx) {
    var x0 = ML + slot * idx + (slot - gw) / 2;
    vals.forEach(function (k, j) {
      var v = d[k] || 0;
      if (v <= 0) return;
      var bh = Math.max(1.5, ph * v / top);
      out += "<rect class='t-bar' style='--i:" + j + "' x='" + (x0 + j * (bw + 1.5)).toFixed(1) + "' y='" + (MT + ph - bh).toFixed(1) +
             "' width='" + bw.toFixed(1) + "' height='" + bh.toFixed(1) + "' fill='" + cols[j] + "' rx='1.5'/>";
    });
  });
  out += "<line class='trend-cross' x1='0' y1='" + MT + "' x2='0' y2='" + (MT + ph) + "' stroke='var(--outline-strong)' stroke-width='1' stroke-dasharray='3 3' opacity='0'/>";
  svg.innerHTML = out;
  trendHover.lines.push(svg.querySelector(".trend-cross"));
  applyAnimate(svg, animate);
}

/* 费用条：独立量程，只标最大值。未计价窗口给一句说明而不是空图。 */
function drawCostBars(svg, buckets, W, ML, MR, animate) {
  var H = 64, MT = 8, MB = 6;
  var pw = W - ML - MR, ph = H - MT - MB;
  svg.setAttribute("viewBox", "0 0 " + W + " " + H);
  var maxCost = 0, anyPriced = false;
  buckets.forEach(function (d) {
    if (d.costUsd != null) { maxCost = Math.max(maxCost, d.costUsd); anyPriced = true; }
  });
  if (!anyPriced) {
    svg.innerHTML = "<text x='" + W / 2 + "' y='" + (H / 2 + 4) + "' fill='var(--muted)' font-size='11' text-anchor='middle'>窗口内无计价数据（未定价模型或未启用计费）</text>";
    svg.classList.remove("trend-animate");
    return;
  }
  var top = maxCost * 1.1;
  var out = "<line x1='" + ML + "' y1='" + (MT + ph) + "' x2='" + (W - MR) + "' y2='" + (MT + ph) + "' stroke='var(--outline-variant)' stroke-width='1'/>";
  out += "<text x='" + (ML - 8) + "' y='" + (MT + 3.5) + "' fill='var(--muted)' font-size='10' text-anchor='end'>" + fmtUsd(top) + "</text>";
  var n = buckets.length, slot = pw / n;
  var bw = Math.min(slot * 0.6, 26);
  buckets.forEach(function (d, idx) {
    if (!(d.costUsd > 0)) return;
    var bh = Math.max(1.5, ph * d.costUsd / top);
    out += "<rect class='t-bar' style='--i:3' x='" + (ML + slot * idx + (slot - bw) / 2).toFixed(1) + "' y='" + (MT + ph - bh).toFixed(1) +
           "' width='" + bw.toFixed(1) + "' height='" + bh.toFixed(1) + "' fill='" + TREND_COLORS.cost + "' opacity='.85' rx='1.5'/>";
  });
  out += "<line class='trend-cross' x1='0' y1='" + MT + "' x2='0' y2='" + (MT + ph) + "' stroke='var(--outline-strong)' stroke-width='1' stroke-dasharray='3 3' opacity='0'/>";
  svg.innerHTML = out;
  trendHover.lines.push(svg.querySelector(".trend-cross"));
  applyAnimate(svg, animate);
}

/* 命中率：固定 0–100% 折线，null 断线分段；背景淡柱是请求数（独立右轴）。
 * 只有孤点（前后都断）才画点，线本身不撒点。x 轴刻度只画这张图。 */
function drawRateChart(svg, buckets, W, ML, MR, narrow, animate) {
  var H = 172, MT = 10, MB = 26;
  var pw = W - ML - MR, ph = H - MT - MB;
  svg.setAttribute("viewBox", "0 0 " + W + " " + H);
  var maxReqs = 0;
  buckets.forEach(function (d) { maxReqs = Math.max(maxReqs, d.attempts); });
  var out = "";
  for (var g = 0; g <= 4; g++) {
    var y = MT + ph * g / 4;
    out += "<line x1='" + ML + "' y1='" + y + "' x2='" + (W - MR) + "' y2='" + y + "' stroke='var(--outline-variant)' stroke-width='1'/>";
    out += "<text x='" + (ML - 8) + "' y='" + (y + 3.5) + "' fill='var(--muted)' font-size='10' text-anchor='end'>" + (100 - g * 25) + "%</text>";
    if (maxReqs > 0) {
      out += "<text x='" + (W - MR + 8) + "' y='" + (y + 3.5) + "' fill='var(--muted)' font-size='10'>" + Math.round(maxReqs * (1 - g / 4)) + "</text>";
    }
  }
  var n = buckets.length, slot = pw / n;
  var bw = Math.min(slot * 0.6, 26);
  buckets.forEach(function (d, idx) {
    if (d.attempts <= 0) return;
    var bh = ph * d.attempts / maxReqs;
    out += "<rect class='t-bar' style='--i:4' x='" + (ML + slot * idx + (slot - bw) / 2).toFixed(1) + "' y='" + (MT + ph - bh).toFixed(1) +
           "' width='" + bw.toFixed(1) + "' height='" + bh.toFixed(1) + "' fill='" + TREND_COLORS.reqBg + "' rx='1.5'/>";
  });
  /* 命中率线：null 断开，分段绘制（不插值） */
  var seg = [], segs = [];
  buckets.forEach(function (d, idx) {
    var cx = ML + slot * (idx + 0.5);
    if (d.hitRate == null) { if (seg.length) segs.push(seg); seg = []; return; }
    seg.push([cx, MT + ph * (1 - d.hitRate)]);
  });
  if (seg.length) segs.push(seg);
  var lineIdx = 0;
  segs.forEach(function (s) {
    if (s.length >= 2) {
      out += "<polyline class='trend-line' style='--i:" + lineIdx + "' points='" +
             s.map(function (p) { return p[0].toFixed(1) + "," + p[1].toFixed(1); }).join(" ") +
             "' fill='none' stroke='" + TREND_COLORS.hit + "' stroke-width='2' stroke-linejoin='round' stroke-linecap='round'/>";
      lineIdx++;
    } else if (s.length === 1) {
      out += "<circle class='trend-dot' style='--i:" + lineIdx + "' cx='" + s[0][0].toFixed(1) + "' cy='" + s[0][1].toFixed(1) +
             "' r='2.4' fill='" + TREND_COLORS.hit + "' stroke='var(--bg)' stroke-width='1'/>";
      lineIdx++;
    }
  });
  /* x 轴刻度（全面板唯一一处） */
  var step = Math.max(1, Math.ceil(n / (narrow ? 4 : 8)));
  buckets.forEach(function (d, idx) {
    if (idx % step !== 0 && idx !== n - 1) return;
    out += "<text x='" + (ML + slot * (idx + 0.5)).toFixed(1) + "' y='" + (H - 8) + "' fill='var(--muted)' font-size='10' text-anchor='middle'>" + fmtTickDate(d.date) + "</text>";
  });
  out += "<line class='trend-cross' x1='0' y1='" + MT + "' x2='0' y2='" + (MT + ph) + "' stroke='var(--outline-strong)' stroke-width='1' stroke-dasharray='3 3' opacity='0'/>";
  svg.innerHTML = out;
  trendHover.lines.push(svg.querySelector(".trend-cross"));
  applyAnimate(svg, animate);
}

/* 入场动画只在刻意刷新时加 class；轮询的 quiet 路径移除它，
 * 否则每秒重画都会重播一次（旧图 twitch 的教训，见 git blame）。 */
function applyAnimate(svg, animate) {
  if (animate) {
    svg.classList.add("trend-animate");
    void svg.getBoundingClientRect(); // restart the animation
  } else {
    svg.classList.remove("trend-animate");
  }
}

/* ── model distribution（环形图 + 明细表） ──
 *
 * 数据来自 stats API 的 `models`（后端已按峰/谷分桶计价）。占比口径：
 * 全部模型都有价格时按费用，否则退化为按 Token——未定价的模型不会把
 * 别人占比压成 0。环形图用 stroke-dasharray 分段，扁平无动画。 */
function renderModels(models) {
  var list = Array.isArray(models) ? models.slice() : [];
  var donut = el("model-donut");
  var note = el("model-note");
  var body = el("model-body");
  if (!list.length) {
    donut.innerHTML =
      '<text x="100" y="104" fill="var(--muted)" font-size="12" text-anchor="middle" font-family="var(--font-mono)">暂无模型数据</text>' +
      '<circle cx="100" cy="100" r="70" fill="none" stroke="var(--surface-3)" stroke-width="20"/>';
    note.textContent = "";
    body.innerHTML = "";
    return;
  }
  var costSum = 0, tokSum = 0, allPriced = true;
  list.forEach(function (m) {
    if (m.costUsd != null) costSum += m.costUsd; else allPriced = false;
    tokSum += m.tokens || 0;
  });
  var byCost = allPriced && costSum > 0;
  var total = byCost ? costSum : tokSum;
  note.textContent = byCost
    ? "按费用占比 · 窗口合计 " + fmtUsd(costSum)
    : "按 Token 占比" + (allPriced ? "" : "（含未定价模型）");

  /* donut: one dash segment per model, clockwise from 12 o'clock */
  var R = 70, C = 2 * Math.PI * R;
  var out = '<circle cx="100" cy="100" r="' + R + '" fill="none" stroke="var(--surface-3)" stroke-width="20"/>';
  var acc = 0;
  list.forEach(function (m, i) {
    var share = total > 0 ? ((byCost ? (m.costUsd || 0) : (m.tokens || 0)) / total) : 0;
    if (share <= 0) return;
    var seg = Math.max(share * C, 0); /* 0-width models simply skip */
    out += '<circle cx="100" cy="100" r="' + R + '" fill="none" stroke="' + PALETTE[i % PALETTE.length] + '"' +
           ' stroke-width="20" stroke-dasharray="' + seg.toFixed(2) + ' ' + (C - seg).toFixed(2) + '"' +
           ' stroke-dashoffset="' + (-acc).toFixed(2) + '" transform="rotate(-90 100 100)"/>';
    acc += seg;
  });
  out += '<text x="100" y="96" fill="var(--text)" font-size="15" font-weight="700" text-anchor="middle" font-family="var(--font-mono)">' +
         (byCost ? fmtUsd(costSum) : fmtTokens(tokSum)) + "</text>";
  out += '<text x="100" y="114" fill="var(--muted)" font-size="10" text-anchor="middle" font-family="var(--font-mono)">' +
         (byCost ? "窗口费用" : "窗口 Token") + "</text>";
  donut.innerHTML = out;

  /* table: 模型（色点对齐环形图分段）/ 请求 / Token / 费用——占比由环形图表达，
     不再单列一栏，窄面板下避免嵌套横滚把数据挤出可视区。 */
  body.innerHTML = list.map(function (m, i) {
    var color = PALETTE[i % PALETTE.length];
    return "<tr>" +
      '<td class="m-name" title="' + esc(m.model) + '"><i class="m-dot" style="background:' + color + '"></i>' + esc(m.model) + "</td>" +
      "<td>" + fmtNum(m.attempts) + "</td>" +
      "<td>" + fmtTokens(m.tokens) + "</td>" +
      '<td class="m-cost">' + (m.costUsd != null ? fmtUsd(m.costUsd) : "—") + "</td>" +
      "</tr>";
  }).join("");
}
