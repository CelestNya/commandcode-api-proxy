/* 明细渲染
 *
 * 迷你指标、分页、筛选与请求表格。
 */

/* ── mini stat row above the details table ── */
function renderMiniCards(s) {
  if (!s) return;
  var t = s.totals || {}, w = s.window || {};
  var cards = [
    { label: "窗口请求", value: fmtNum(w.attempts), sub: "累计 " + fmtNum(t.attempts) },
    { label: "窗口 Token", value: fmtTokens((w.promptTokens || 0) + (w.completionTokens || 0)), sub: "输入 " + fmtTokens(w.promptTokens) + " / 输出 " + fmtTokens(w.completionTokens) },
    { label: "缓存命中", value: fmtPct(w.hitRate), sub: fmtTokens(w.promptTokens || 0) + " 输入中命中 " + fmtTokens(w.cachedTokens || 0) },
    { label: "窗口费用", value: fmtUsd(w.costUsd), sub: "终身 " + fmtUsd(t.costUsd) + "（按官网价估算）" },
    { label: "窗口成功率", value: fmtPct(w.attempts > 0 ? ((w.ok || 0) / w.attempts) : null), sub: "ok " + fmtNum(w.ok) + " / " + fmtNum(w.attempts) }
  ];
  el("mini-cards").innerHTML = cards.map(function (c) {
    return '<div class="mini-card"><div class="mc-label">' + c.label + '</div>' +
      '<div class="mc-value">' + c.value + '</div><div class="mc-sub">' + c.sub + '</div></div>';
  }).join("");
}

/* ── details ── */
function pageList(cur, total) {
  var out = [];
  if (total <= 7) {
    for (var i = 1; i <= total; i++) out.push(i);
    return out;
  }
  out.push(1);
  var start = Math.max(2, cur - 1), end = Math.min(total - 1, cur + 1);
  if (start > 2) out.push("…");
  for (var j = start; j <= end; j++) out.push(j);
  if (end < total - 1) out.push("…");
  out.push(total);
  return out;
}

function renderPager(totalPages, shown) {
  var pager = el("pager");
  if (totalPages <= 1 && !shown) { pager.innerHTML = ""; return; }
  var h = '<span class="pg-count" title="筛选范围：账本最近 ' + attemptsLimit() + ' 条">共 ' + shown + ' 条</span>';
  h += '<button class="pg-btn" id="pg-prev">上一页</button>';
  pageList(state.page, totalPages).forEach(function (p) {
    if (p === "…") { h += '<span class="pg-ellipsis">…</span>'; return; }
    h += '<button class="pg-btn' + (p === state.page ? " active" : "") + '" data-page="' + p + '">' + p + "</button>";
  });
  h += '<button class="pg-btn" id="pg-next">下一页</button>';
  h += '<span class="pg-size">每页展示 <select id="pg-size">' +
       [50, 100, 200, 500].map(function (n) {
         return '<option value="' + n + '"' + (n === state.pageSize ? " selected" : "") + ">" + n + "</option>";
       }).join("") + " 条</select></span>";
  pager.innerHTML = h;
  el("pg-prev").disabled = state.page <= 1;
  el("pg-next").disabled = state.page >= totalPages;
  el("pg-prev").addEventListener("click", function () { setPage(state.page - 1); });
  el("pg-next").addEventListener("click", function () { setPage(state.page + 1); });
  pager.querySelectorAll(".pg-btn[data-page]").forEach(function (b) {
    b.addEventListener("click", function () { setPage(Number(b.getAttribute("data-page"))); });
  });
  el("pg-size").addEventListener("change", function () {
    state.pageSize = Number(el("pg-size").value);
    state.page = 1;
    renderAttempts(state.attempts);
    fetchAttempts(); /* widen the fetched window to match the new page size */
  });
  /* 分页条是整块重建的，里面的 select 每次都是新元素。 */
  Dropdown.enhanceAll(pager);
}

function setPage(n) {
  var rows = filterAttempts(state.attempts);
  var total = Math.max(1, Math.ceil(rows.length / state.pageSize));
  if (n < 1 || n > total) return;
  state.page = n;
  renderAttempts(state.attempts);
}

function filterAttempts(list) {
  var f = state.filters;
  return list.filter(function (a) {
    if (f.model && a.model !== f.model) return false;
    if (f.stream !== "" && String(a.stream) !== f.stream) return false;
    if (f.status && a.status !== f.status) return false;
    return true;
  });
}

function renderAttempts(list) {
  if (!Array.isArray(list)) list = [];
  var rows = filterAttempts(list);
  var totalPages = Math.max(1, Math.ceil(rows.length / state.pageSize));
  if (state.page > totalPages) state.page = totalPages;
  var start = (state.page - 1) * state.pageSize;
  var pageRows = rows.slice(start, start + state.pageSize);
  var body = el("attempts-body");
  body.innerHTML = pageRows.map(function (a, i) {
    var p = a.promptTokens, c = a.cachedTokens, o = a.completionTokens;
    // promptTokens already includes cached; net input = prompt - cached
    var net = (p == null) ? null : p - (c || 0);
    var hit = (p || 0) > 0 ? ((c || 0) / p * 100) : null;
    var hitTxt = (p == null && c == null) ? "未知" : (hit == null ? "无输入" : "命中 " + fmtTokens(c) + "（" + hit.toFixed(1) + "%）");
    var pTxt = (net == null) ? "?" : fmtNum(net);
    var oTxt = (o == null) ? "?" : fmtNum(o);
    var rid = a.reqId || "—";
    var ridShow = rid.length > 6 ? rid.slice(0, 6) + "…" : rid;
    var wire = (a.wire || "?").toUpperCase();
    var streamTxt = a.stream ? "流式" : "非流式";
    /* 延迟两行：单行 "首字 X / 总耗时 Y" 在 12% 列宽（≈148px）里必被
       ellipsis 截断，拆两行各 ~80px 才放得下。 */
    var lat = '<small>首字 ' + (a.ttfbMs != null ? fmtDur(a.ttfbMs) : "—") + '</small>' +
              '<small>总耗时 ' + (a.durationMs != null ? fmtDur(a.durationMs) : "—") + '</small>';
    var retry = (a.attempts || 1) > 1 ? '<span class="muted" title="' + a.attempts + ' 次尝试">x' + a.attempts + '</span>' : "";
    var costTxt = a.costUsd != null ? fmtUsd(a.costUsd) : "—";
    var statusCell = '<span class="badge ' + esc(a.status || "") + '">' + esc(a.status || "?") + '</span>';
    if (a.errorTag) statusCell += '<small class="err-tag" title="' + esc(a.errorTag) + '">' + esc(a.errorTag) + '</small>';
    return '<tr class="req-row" data-req="' + esc(rid) + '">' +
      '<td class="muted">' + (start + i + 1) + '</td>' +
      '<td class="rid-cell" title="' + esc(rid) + '">' + esc(ridShow) + '</td>' +
      '<td title="' + esc(a.model) + '">' + esc(a.model) + '</td>' +
      '<td>' + wire + '</td>' +
      '<td>' + streamTxt + '</td>' +
      '<td>' + statusCell + '</td>' +
      '<td class="tokens-cell"><b class="down">↓' + pTxt + '</b> <b class="up">↑' + oTxt + '</b><small>' + hitTxt + '</small></td>' +
      '<td class="cost-cell">' + costTxt + '</td>' +
      '<td class="lat-cell">' + lat + '</td>' +
      '<td>' + fmtTime(a.ts) + '</td>' +
      '<td>' + retry + '</td>' +
      '</tr>';
  }).join("");
  el("attempts-empty").style.display = rows.length ? "none" : "block";
  el("attempts-empty").textContent = state.attempts.length ? "没有符合条件的记录" : "暂无记录";
  renderPager(totalPages, rows.length);
  var models = {};
  list.forEach(function (a) { models[a.model] = true; });
  var sel = el("f-model");
  var cur = sel.value;
  sel.innerHTML = '<option value="">全部</option>' + Object.keys(models).sort().map(function (m) {
    return '<option value="' + esc(m) + '">' + esc(m) + '</option>';
  }).join("");
  sel.value = cur;
  wireRowClicks();
  /* the 2s poll re-renders rows; keep the open drawer fresh on the same req */
  if (openReqId) {
    var match = state.attempts.find(function (a) { return a.reqId === openReqId; });
    if (match && !el("req-drawer").hidden) openDrawer(match);
  }
}

/* ── request detail drawer ── */

/* Rows are re-rendered by the 2s poll; remember the open request so the
   drawer can be re-opened on the row that was clicked, and the re-render
   never closes it (R7: the list scroll position is untouched anyway because
   the drawer is an overlay, not a re-layout). */
var openReqId = null;

function wireRowClicks() {
  var rows = document.querySelectorAll("#attempts-body .req-row");
  rows.forEach(function (tr) {
    tr.addEventListener("click", function () {
      var rid = tr.getAttribute("data-req");
      openReqId = rid;
      var match = state.attempts.find(function (a) { return a.reqId === rid; });
      if (match) openDrawer(match);
    });
  });
  if (openReqId) {
    var sel2 = "#attempts-body .req-row[data-req=\"" + String(openReqId).replace(/\\/g, "\\\\").replace(/"/g, '\\"') + "\"]";
    var cur = document.querySelector(sel2);
    if (cur) cur.classList.add("selected");
  }
}

function openDrawer(a) {
  var drawer = el("req-drawer");
  drawer.hidden = false;
  el("drawer-body").innerHTML = drawerHtml(a);
}

function closeDrawer() {
  el("req-drawer").hidden = true;
  openReqId = null;
}

function drawerHtml(a) {
  var cb = a.costBreakdown || {};
  var rows = [
    '<h3>请求 ' + esc(a.reqId || "—") + '</h3>',
    '<table class="drawer-table">',
    '<tr><td>模型</td><td>' + esc(a.model || "—") + '</td></tr>',
    '<tr><td>端点</td><td>' + esc((a.wire || "?").toUpperCase()) + '</td></tr>',
    '<tr><td>类型</td><td>' + (a.stream ? "流式" : "非流式") + '</td></tr>',
    '<tr><td>时间</td><td>' + fmtTime(a.ts) + '</td></tr>',
    '<tr><td>状态</td><td><span class="badge ' + esc(a.status || "") + '">' + esc(a.status || "?") + '</span>' +
      (a.errorTag ? ' <small class="err-tag">' + esc(a.errorTag) + '</small>' : "") + '</td></tr>',
    '</table>'
  ].join("");

  /* token block: net input / output / cache read */
  var net = (a.promptTokens == null) ? "?" : fmtNum((a.promptTokens || 0) - (a.cachedTokens || 0));
  var hit = (a.promptTokens || 0) > 0 ? ((a.cachedTokens || 0) / a.promptTokens * 100).toFixed(1) + "%" : "—";
  rows += '<h4>Token</h4><div class="drawer-tokens">' +
    '<div><span>净输入</span><b>' + net + '</b></div>' +
    '<div><span>输出</span><b>' + fmtNum(a.completionTokens) + '</b></div>' +
    '<div><span>缓存读取</span><b>' + fmtNum(a.cachedTokens) + '（' + hit + '）</b></div>' +
    (a.cacheCreationTokens ? '<div><span>缓存创建</span><b>' + fmtNum(a.cacheCreationTokens) + '</b></div>' : "") +
    '</div>';

  /* cost formula (R4: every part of the amount is shown) */
  var usd = function (v) { return (v == null || isNaN(v)) ? "—" : "$" + Number(v).toFixed(6); };
  rows += '<h4>费用 <small>按官网峰/谷价估算</small></h4>' +
    '<table class="drawer-table cost-formula">' +
    '<tr><td>未缓存输入</td><td>' + usd(cb.uncached) + '</td></tr>' +
    '<tr><td>缓存读取</td><td>' + usd(cb.cached) + '</td></tr>' +
    (cb.cacheCreation ? '<tr><td>缓存创建</td><td>' + usd(cb.cacheCreation) + '</td></tr>' : "") +
    '<tr><td>输出完成</td><td>' + usd(cb.completion) + '</td></tr>' +
    '<tr class="drawer-total"><td>合计</td><td>' + (a.costUsd != null ? "$" + Number(a.costUsd).toFixed(4) : "—") + '</td></tr>' +
    '</table>';

  /* retry chain (R6) */
  var det = a.attemptsDetail || [];
  if (det.length > 1) {
    rows += '<h4>尝试链 <small>' + det.length + ' 次</small></h4><ol class="attempt-chain">' +
      det.map(function (d) {
        return '<li><span class="badge ' + esc(d.status || "") + '">' + esc(d.status || "?") + '</span>' +
          (d.errorTag ? ' <small class="err-tag">' + esc(d.errorTag) + '</small>' : "") +
          '<span class="muted">' + fmtTime(d.ts) + '</span>' +
          (d.durationMs != null ? ' · ' + fmtDur(d.durationMs) : "") + '</li>';
      }).join("") + '</ol>';
  }

  /* tools */
  var tools = a.tools || [];
  if (tools.length) {
    rows += '<h4>工具 <small>' + tools.length + '</small></h4><div class="drawer-tools">' +
      tools.map(function (t) { return '<span class="tool-chip">' + esc(t) + '</span>'; }).join("") + '</div>';
  }
  return rows;
}
