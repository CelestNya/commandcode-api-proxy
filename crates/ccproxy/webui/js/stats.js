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

/* Cost axis ticks: no trailing zeros and a stable digit count, so the right
   scale reads "$1 / $0.75 / $0.5 / $0" instead of mixing $1.00 with $0.7500.
   Cards keep the richer fmtUsd; this is axis-only. */
function fmtTickUsd(v) {
  if (v == null || isNaN(v)) return "";
  if (v >= 1) return "$" + v.toFixed(2);
  var r = Math.round(v * 1000) / 1000;
  return "$" + String(r);
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

/* Reduce the daily series to at most `n` points by *picking* days, never by
   averaging groups of them.

   Averaging was the source of the lumpy curves: a 14-day window bucketed into
   12 points merged some adjacent days and not others (bucket sizes 1 and 2 in
   the same series), so a day with 1 token vanished into a bucket that averaged
   it with a 200k day, and the two busiest days collapsed into a single mean.
   Picking one representative day per slot keeps every plotted value a real
   day's value, so the curve's shape is the data's shape. */
function downsampleTrend(arr, n) {
  var L = arr.length;
  if (L <= n) return arr;
  var out = [];
  var step = L / n;
  for (var i = 0; i < n; i++) {
    // The slot's first day is its representative; slots are even in count, so
    // the gaps between picks stay even too.
    out.push(arr[Math.floor(i * step)]);
  }
  // Always keep the newest day, which is the one the user is looking at.
  if (out[out.length - 1] !== arr[L - 1]) out[out.length - 1] = arr[L - 1];
  return out;
}

function renderTrend(raw, animate) {
  var trend = downsampleTrend(raw, 12);
  var LEG = [
    { key: "prompt", name: "Input", color: "var(--primary)" },
    { key: "completion", name: "Output", color: "var(--ok)" },
    { key: "cached", name: "Cache Read", color: "var(--aborted)" },
    { key: "costUsd", name: "Cost", color: "var(--cost)" }
  ];
  el("trend-legend").innerHTML = LEG.map(function (l) {
    return '<span><i style="background:' + l.color + '"></i>' + l.name + '</span>';
  }).join("");
  var svg = el("trend-chart");
  /* The viewBox shrinks to a phone-friendly aspect on narrow screens: a 900x300
     box scaled into a 300px-wide column renders its 10px labels at ~3px. A
     squarer box keeps the type legible, at the cost of a shorter time axis. */
  var narrow = svg.clientWidth > 0 && svg.clientWidth < 520;
  var W = narrow ? 420 : 900, H = narrow ? 300 : 300;
  var ML = narrow ? 56 : 78, MR = narrow ? 46 : 62, MT = 14, MB = 34;
  svg.setAttribute("viewBox", "0 0 " + W + " " + H);
  var pw = W - ML - MR, ph = H - MT - MB;
  if (!trend.length) {
    svg.innerHTML =
      '<g stroke="var(--outline-variant)" stroke-width="1.6" fill="none" stroke-linecap="round" stroke-linejoin="round">' +
      '<path d="M' + (W / 2 - 100) + ' 152 L' + (W / 2 - 55) + ' 152 L' + (W / 2 - 10) + ' 128 L' + (W / 2 + 35) + ' 176 L' + (W / 2 + 80) + ' 176"/>' +
      '<circle cx="' + (W / 2 - 100) + '" cy="152" r="2.4" fill="var(--bg)"/>' +
      '<circle cx="' + (W / 2 - 55) + '" cy="152" r="2.4" fill="var(--bg)"/>' +
      '<circle cx="' + (W / 2 - 10) + '" cy="128" r="2.4" fill="var(--bg)"/>' +
      '<circle cx="' + (W / 2 + 35) + '" cy="176" r="2.4" fill="var(--bg)"/>' +
      '<circle cx="' + (W / 2 + 80) + '" cy="176" r="2.4" fill="var(--bg)"/>' +
      '</g>' +
      '<text x="' + (W / 2) + '" y="200" fill="var(--muted)" font-size="13" text-anchor="middle">所选时段内暂无请求</text>';
    return;
  }
  var maxTok = 0, maxCost = 0;
  trend.forEach(function (d) {
    maxTok = Math.max(maxTok, (d.prompt || 0), (d.completion || 0), (d.cached || 0));
    if (d.costUsd != null) maxCost = Math.max(maxCost, d.costUsd);
  });
  if (maxTok <= 0) maxTok = 1;
  if (maxCost <= 0) maxCost = 1;
  function x(i) { return ML + (trend.length === 1 ? pw / 2 : pw * i / (trend.length - 1)); }
  function yTok(v) { return MT + ph - (v / maxTok) * ph; }
  function yCost(v) { return MT + ph - ((v == null ? 0 : v) / maxCost) * ph; }
  var out = "";
  for (var g = 0; g <= 4; g++) {
    var gy = MT + ph * g / 4;
    var val = maxTok * (1 - g / 4);
    out += '<line x1="' + ML + '" y1="' + gy + '" x2="' + (W - MR) + '" y2="' + gy + '" stroke="var(--outline-variant)" stroke-width="1"/>';
    out += '<text x="' + (ML - 8) + '" y="' + (gy + 4) + '" fill="var(--muted)" font-size="10" text-anchor="end">' + fmtTokens(Math.round(val)) + '</text>';
  }
  /* right axis: cost, 0 → maxCost across the window */
  for (var p = 0; p <= 4; p++) {
    var py = MT + ph * p / 4;
    var cv = maxCost * (1 - p / 4);
    out += '<text x="' + (W - MR + 8) + '" y="' + (py + 4) + '" fill="var(--muted)" font-size="10">' + fmtTickUsd(cv) + '</text>';
  }
  var step = Math.max(1, Math.ceil(trend.length / 8));
  trend.forEach(function (d, i) {
    if (i % step !== 0 && i !== trend.length - 1) return;
    out += '<text x="' + x(i) + '" y="' + (H - 14) + '" fill="var(--muted)" font-size="10" text-anchor="middle">' + fmtTickDate(d.date) + '</text>';
  });
  function pts(key, yfn) {
    var arr = [];
    trend.forEach(function (d, i) {
      var v = d[key];
      if (v == null) return;
      arr.push([x(i), yfn(v)]);
    });
    return arr;
  }
  /* Shape-preserving cubic (Fritsch-Carlson PCHIP) with rounded extrema.
     Plain PCHIP forces the tangent to zero at every local extremum, which
     guarantees the curve cannot overshoot but turns each peak into a flat top
     and each trough into a sharp corner — the visible "kinks" reported.

     The fix keeps the no-overshoot guarantee and relaxes only the flatness:
     at an extremum the tangent is set to a small fraction of the monotonicity
     bound `3*min(|adjacent secants|)` instead of 0. The curve leaves the peak
     with a gentle slope, so the vertex is round, and because the slope stays
     under the bound the segment still cannot exceed the data's envelope. */
  var EXTREMUM_TANGENT = 0.35; // fraction of the monotonicity bound

  function pchipSlopes(xs, ys) {
    var n = xs.length, m = new Array(n);
    if (n === 1) { m[0] = 0; return m; }
    var d = new Array(n - 1);
    for (var i = 0; i < n - 1; i++) d[i] = (ys[i + 1] - ys[i]) / (xs[i + 1] - xs[i]);
    if (n === 2) { m[0] = d[0]; m[1] = d[0]; return m; }
    for (var i = 1; i < n - 1; i++) {
      var left = d[i - 1], right = d[i];
      if (left === 0 || right === 0) { m[i] = 0; continue; }
      if (left * right < 0) {
        // Local extremum: a bounded non-zero tangent rounds the vertex while
        // staying inside the envelope (the bound is 3*min(|d|) for a
        // monotone-preserving cubic).
        var bound = 3 * Math.min(Math.abs(left), Math.abs(right));
        m[i] = Math.sign(right) * bound * EXTREMUM_TANGENT;
        continue;
      }
      var h0 = xs[i] - xs[i - 1], h1 = xs[i + 1] - xs[i];
      var w1 = 2 * h1 + h0, w2 = h1 + 2 * h0;
      m[i] = (w1 + w2) / (w1 / left + w2 / right);
    }
    m[0] = endSlope(xs[0], xs[1], xs[2], ys[0], ys[1], ys[2], d[0], d[1]);
    m[n - 1] = endSlope(
      xs[n - 1], xs[n - 2], xs[n - 3],
      ys[n - 1], ys[n - 2], ys[n - 3],
      d[n - 2], d[n - 3]
    );
    return m;
  }
  /* One-sided three-point endpoint slope, clamped so it cannot overshoot.
     `h1`/`d1` belong to the segment next to the endpoint, `h2`/`d2` the one
     after it. */
  function endSlope(x0, x1, x2, y0, y1, y2, d1, d2) {
    var h1 = x1 - x0, h2 = x2 - x1;
    var s = ((2 * h1 + h2) * d1 - h1 * d2) / (h1 + h2);
    if (s * d1 <= 0) return 0;                       // sign change: flat
    if (d1 * d2 <= 0 && Math.abs(s) > Math.abs(3 * d1)) return 3 * d1;
    return s;
  }
  function pchipPath(ps) {
    var n = ps.length;
    if (!n) return "";
    if (n === 1) return "M" + ps[0][0].toFixed(1) + " " + ps[0][1].toFixed(1);
    if (n === 2) return "M" + ps[0][0].toFixed(1) + " " + ps[0][1].toFixed(1) + " L" + ps[1][0].toFixed(1) + " " + ps[1][1].toFixed(1);
    var xs = [], ys = [];
    ps.forEach(function (pt) { xs.push(pt[0]); ys.push(pt[1]); });
    var m = pchipSlopes(xs, ys);
    var d = "M" + xs[0].toFixed(1) + " " + ys[0].toFixed(1);
    for (var i = 0; i < n - 1; i++) {
      var h = xs[i + 1] - xs[i];
      var c1x = xs[i] + h / 3, c1y = ys[i] + m[i] * h / 3;
      var c2x = xs[i + 1] - h / 3, c2y = ys[i + 1] - m[i + 1] * h / 3;
      d += " C" + c1x.toFixed(1) + " " + c1y.toFixed(1) + "," + c2x.toFixed(1) + " " + c2y.toFixed(1) + "," + xs[i + 1].toFixed(1) + " " + ys[i + 1].toFixed(1);
    }
    return d;
  }
  var sIn = pts("prompt", yTok), sOut = pts("completion", yTok), sCa = pts("cached", yTok);
  var sCost = pts("costUsd", yCost);
  /* Lines are drawn in a fixed order and each carries its series name in a
     data attribute, so a CSS stagger can animate them one after another and
     the legend could highlight them later. */
  var SERIES = [
    { pts: sCost, color: "var(--cost)", width: 1.8, name: "costUsd" },
    { pts: sOut, color: "var(--ok)", width: 2, name: "completion", fill: 0.09 },
    { pts: sCa, color: "var(--aborted)", width: 2, name: "cached", fill: 0.1 },
    { pts: sIn, color: "var(--primary)", width: 2, name: "prompt", fill: 0.12 }
  ];
  /* Area fills under the three token series, drawn before the lines. Cost
     stays a bare line: it lives on the right axis, and a filled amber area
     would read as token volume. The fills exist for the tails — after the
     last request the curves descend to the baseline as visible sloped areas
     instead of vanishing, which is what made the right side of the chart
     look broken. */
  var baseY = MT + ph;
  SERIES.forEach(function (se) {
    if (!se.fill || se.pts.length < 2) return;
    var d = pchipPath(se.pts);
    if (!d) return;
    out += '<path d="' + d +
           ' L' + se.pts[se.pts.length - 1][0].toFixed(1) + ' ' + baseY.toFixed(1) +
           ' L' + se.pts[0][0].toFixed(1) + ' ' + baseY.toFixed(1) + ' Z" ' +
           'fill="' + se.color + '" fill-opacity="' + se.fill + '" stroke="none"/>';
  });
  SERIES.forEach(function (se, idx) {
    var d = pchipPath(se.pts);
    out += '<path class="trend-line" data-series="' + se.name + '" style="--i:' + idx + '" ' +
           'd="' + d + '" fill="none" stroke="' + se.color + '" stroke-width="' + se.width + '" ' +
           'stroke-linejoin="round" stroke-linecap="round"/>';
  });
  /* data points, denser for short windows and sampled for long ones */
  var dotStep = Math.max(1, Math.ceil(trend.length / 14));
  SERIES.forEach(function (se, si) {
    var r = se.name === "costUsd" ? 2 : 2.6;
    se.pts.forEach(function (pt, idx) {
      if (idx % dotStep !== 0 && idx !== se.pts.length - 1) return;
      out += '<circle class="trend-dot" style="--i:' + si + '" cx="' + pt[0].toFixed(1) + '" cy="' + pt[1].toFixed(1) + '" r="' + r + '" fill="' + se.color + '" stroke="var(--bg)" stroke-width="1"/>';
    });
  });
  svg.innerHTML = out;
  /* Only animate when asked, and always say so explicitly on the class: the
     poll re-renders every second, and a rebuild under a stale `trend-animate`
     class replays the draw-in on fresh paths every single time (the chart
     twitched nonstop until this became an if/else). Removing the class on the
     quiet path renders the final state immediately; the forced reflow on the
     animated path restarts it from scratch. */
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
