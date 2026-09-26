/* 概览渲染
 *
 * 指标卡、汇总、趋势图（含 PCHIP 平滑与降采样）、模型分布表。
 */

/* ── overview ── */
function renderStats(s, animate) {
  state.stats = s;
  renderSummary(s);
  renderTrend(s.trend || [], animate === true);
}

/* Everything in the overview except the trend chart. The live stream calls
   this on every snapshot regardless of the selected trend window — lifetime
   and today numbers are window-independent, and freezing the spend chip and
   mini-cards for up to 30s just because the user picked 近7天 is wrong. */
function renderSummary(s) {
  var t = s.totals || {}, td = s.today || {};
  // AI SDK accounting: promptTokens already includes cached tokens and
  // completionTokens already includes reasoning tokens, so the totals are
  // prompt + completion; cached is shown as a subset of input.
  var tTok = (t.promptTokens || 0) + (t.completionTokens || 0);
  var tTokIn = t.promptTokens || 0;
  var dTok = (td.promptTokens || 0) + (td.completionTokens || 0);
  var fail = (t.error || 0) + (t.aborted || 0) + (t.interrupted || 0);
  var succRate = t.attempts > 0 ? (t.ok / t.attempts) : null;

  var cards = [
    { label: "今日请求", value: fmtNum(td.attempts), sub: "总计: " + fmtNum(t.attempts), cls: "" },
    { label: "今日 Token", value: fmtTokens(dTok), sub: "输入 " + fmtTokens(td.promptTokens) + "（含缓存 " + fmtTokens(td.cachedTokens) + "）/ 输出 " + fmtTokens(td.completionTokens), cls: "" },
    { label: "累计 Token", value: fmtTokens(tTok), sub: "输入 " + fmtTokens(tTokIn) + "（含缓存 " + fmtTokens(t.cachedTokens) + "）/ 输出 " + fmtTokens(t.completionTokens), cls: "accent" },
    { label: "成功率", value: fmtPct(succRate), sub: "失败 " + fmtNum(fail) + " 次", cls: (succRate == null || succRate >= 0.95) ? "ok" : "warn" },
    { label: "平均响应", value: fmtDur(t.avgDurationMs), sub: "总耗时", cls: "" },
    { label: "平均首字", value: fmtDur(t.avgTtfbMs), sub: "首字节延迟", cls: "" },
    { label: "缓存命中率", value: fmtPct(t.hitRate), sub: "缓存命中 / 输入", cls: "" },
    { label: "总尝试", value: fmtNum(t.attempts), sub: "成功 " + fmtNum(t.ok) + " / 失败 " + fmtNum(fail), cls: "accent" }
  ];
  el("metric-cards").innerHTML = cards.map(function (c) {
    return '<div class="mcard ' + c.cls + '"><div class="mlabel">' + c.label + '</div>' +
           '<div class="mvalue">' + c.value + '</div><div class="msub">' + c.sub + '</div></div>';
  }).join("");


  renderModels(s.models || []);
  var sts = [
    { k: "ok", v: t.ok || 0, c: "var(--ok)" },
    { k: "error", v: t.error || 0, c: "var(--error)" },
    { k: "aborted", v: t.aborted || 0, c: "var(--aborted)" },
    { k: "interrupted", v: t.interrupted || 0, c: "var(--interrupted)" }
  ];
  sts.sort(function (a, b) { return b.v - a.v; }); /* biggest first */
  var tot = t.attempts || 0;
  el("status-summary").textContent = fmtNum(tot) + " 次尝试";
  el("status-bars").innerHTML = sts.map(function (st) {
    var pct = tot > 0 ? (st.v / tot * 100) : 0;
    return '<div class="status-bar-row"><div class="sbr-head"><b>' + st.k + '</b>' +
      '<span>' + fmtNum(st.v) + ' · ' + pct.toFixed(1) + '%</span></div>' +
      '<div class="sbr-track"><div class="sbr-fill" style="width:' + pct.toFixed(1) + '%;background:' + st.c + '"></div></div></div>';
  }).join("");

  /* top-bar consumption chip */
  el("chip-cost").textContent = fmtUsd(t.costUsd);
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
    { key: "prompt", name: "Input", color: "#8AB4F8" },
    { key: "completion", name: "Output", color: "#81C995" },
    { key: "cached", name: "Cache Read", color: "#FDD663" },
    { key: "hitRate", name: "Cache Hit Rate", color: "#C58AF9" }
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
    svg.innerHTML = '<text x="450" y="150" fill="var(--muted)" font-size="13" text-anchor="middle">暂无数据</text>';
    return;
  }
  var maxTok = 0;
  trend.forEach(function (d) {
    maxTok = Math.max(maxTok, (d.prompt || 0), (d.completion || 0), (d.cached || 0));
  });
  if (maxTok <= 0) maxTok = 1;
  function x(i) { return ML + (trend.length === 1 ? pw / 2 : pw * i / (trend.length - 1)); }
  function yTok(v) { return MT + ph - (v / maxTok) * ph; }
  function yPct(v) { return MT + ph - (v == null ? 0 : v) * ph; }
  var out = "";
  for (var g = 0; g <= 4; g++) {
    var gy = MT + ph * g / 4;
    var val = maxTok * (1 - g / 4);
    out += '<line x1="' + ML + '" y1="' + gy + '" x2="' + (W - MR) + '" y2="' + gy + '" stroke="var(--outline-variant)" stroke-width="1"/>';
    out += '<text x="' + (ML - 8) + '" y="' + (gy + 4) + '" fill="var(--muted)" font-size="10" text-anchor="end">' + fmtTokens(Math.round(val)) + '</text>';
  }
  for (var p = 0; p <= 5; p++) {
    var py = MT + ph * p / 5;
    out += '<text x="' + (W - MR + 8) + '" y="' + (py + 4) + '" fill="var(--muted)" font-size="10">' + Math.round(100 - p * 20) + '%</text>';
  }
  var step = Math.max(1, Math.ceil(trend.length / 8));
  trend.forEach(function (d, i) {
    if (i % step !== 0 && i !== trend.length - 1) return;
    out += '<text x="' + x(i) + '" y="' + (H - 14) + '" fill="var(--muted)" font-size="10" text-anchor="middle">' + d.date.slice(5) + '</text>';
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
  var sHr = pts("hitRate", yPct);
  /* Lines are drawn in a fixed order and each carries its series name in a
     data attribute, so a CSS stagger can animate them one after another and
     the legend could highlight them later. */
  var SERIES = [
    { pts: sHr, color: "#C58AF9", width: 1.6, name: "hitRate" },
    { pts: sOut, color: "#81C995", width: 2, name: "completion" },
    { pts: sCa, color: "#FDD663", width: 2, name: "cached" },
    { pts: sIn, color: "#8AB4F8", width: 2, name: "prompt" }
  ];
  SERIES.forEach(function (se, idx) {
    var d = pchipPath(se.pts);
    out += '<path class="trend-line" data-series="' + se.name + '" style="--i:' + idx + '" ' +
           'd="' + d + '" fill="none" stroke="' + se.color + '" stroke-width="' + se.width + '" ' +
           'stroke-linejoin="round" stroke-linecap="round"/>';
  });
  /* data points, denser for short windows (7d) and sampled for long ones */
  var dotStep = Math.max(1, Math.ceil(trend.length / 14));
  SERIES.forEach(function (se, si) {
    var r = se.color === "#C58AF9" ? 2 : 2.6;
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

function renderModels(models) {
  el("model-count").textContent = models.length + " 个模型";
  var tot = 0;
  models.forEach(function (m) { tot += m.tokens || 0; });
  el("model-table").innerHTML = models.map(function (m, i) {
    return '<tr><td title="' + esc(m.model) + '">' + esc(m.model) + '</td>' +
      '<td>' + fmtNum(m.attempts) + '</td><td>' + fmtTokens(m.tokens) + '</td>' +
      '<td>' + fmtUsd(m.costUsd) + '</td></tr>';
  }).join("") || '<tr><td colspan="4" class="muted">暂无数据</td></tr>';
  var svg = el("model-donut");
  var C = 2 * Math.PI * 62;
  if (!tot) {
    svg.innerHTML = '<text x="100" y="104" fill="var(--muted)" font-size="13" text-anchor="middle">暂无数据</text>';
    return;
  }
  var out = '<circle cx="100" cy="100" r="62" fill="none" stroke="var(--surface-variant)" stroke-width="26"/>';
  var acc = 0;
  models.forEach(function (m, i) {
    var frac = (m.tokens || 0) / tot;
    var dash = frac * C - 2;
    if (dash < 0.5) dash = 0;
    out += '<circle cx="100" cy="100" r="62" fill="none" stroke="' + PALETTE[i % PALETTE.length] + '" stroke-width="26" ' +
      'stroke-dasharray="' + dash.toFixed(2) + ' ' + (C - dash).toFixed(2) + '" stroke-dashoffset="' + (-acc * C).toFixed(2) + '" ' +
      'transform="rotate(-90 100 100)"/>';
    acc += frac;
  });
  out += '<text x="100" y="97" fill="var(--text)" font-size="20" font-weight="650" text-anchor="middle">' + fmtTokens(tot) + '</text>' +
    '<text x="100" y="116" fill="var(--muted)" font-size="11" text-anchor="middle">累计 Token</text>';
  svg.innerHTML = out;
}
