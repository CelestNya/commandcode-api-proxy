/* 明细渲染
 *
 * 迷你指标、分页、筛选与请求表格。
 */

/* ── mini stat row above the details table ── */
function renderMiniCards(s) {
  if (!s) return;
  var t = s.totals || {}, td = s.today || {};
  var succRate = t.attempts > 0 ? (t.ok / t.attempts) : null;
  var cards = [
    { label: "今日请求", value: fmtNum(td.attempts), sub: "累计 " + fmtNum(t.attempts) },
    { label: "成功率", value: fmtPct(succRate), sub: "ok " + fmtNum(t.ok) },
    { label: "平均耗时", value: fmtDur(t.avgDurationMs), sub: "首字 " + fmtDur(t.avgTtfbMs) },
    { label: "缓存命中", value: fmtPct(t.hitRate), sub: fmtTokens(t.promptTokens || 0) + " 输入中命中 " + fmtTokens(t.cachedTokens || 0) },
    { label: "消耗金额", value: fmtUsd(t.costUsd), sub: "按官网价估算" }
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
    // promptTokens already includes cached; hit rate is cached / prompt.
    var hit = (p || 0) > 0 ? ((c || 0) / p * 100) : null;
    var hitTxt = (p == null && c == null) ? "未知" : (hit == null ? "无输入" : fmtTokens(p) + " 输入, 命中 " + hit.toFixed(1) + "%");
    if (a.cacheCreationTokens != null && a.cacheCreationTokens > 0) {
      hitTxt += " · 缓存创建 " + fmtTokens(a.cacheCreationTokens);
    }
    var pTxt = (p == null) ? "?" : fmtNum(p);
    var oTxt = (o == null) ? "?" : fmtNum(o);
    /* the ledger deliberately stores the request id, never the API key */
    var rid = a.reqId || "—";
    var ridShow = rid.length > 16 ? rid.slice(0, 16) + "…" : rid;
    var wire = (a.wire || "?").toUpperCase();
    var streamTxt = a.stream ? "流式" : "非流式";
    var lat = (a.ttfbMs != null ? "首字 " + fmtDur(a.ttfbMs) : "首字 —") + " / " +
              (a.durationMs != null ? "总耗时 " + fmtDur(a.durationMs) : "总耗时 —");
    var retry = (a.attempt || 1) > 1 ? '<span class="muted" title="第 ' + a.attempt + ' 次尝试">x' + a.attempt + '</span>' : "";
    return '<tr>' +
      '<td class="muted">' + (start + i + 1) + '</td>' +
      '<td title="' + esc(rid) + '">' + esc(ridShow) + '</td>' +
      '<td title="' + esc(a.model) + '">' + esc(a.model) + '</td>' +
      '<td>' + wire + '</td>' +
      '<td>' + streamTxt + '</td>' +
      '<td><span class="badge ' + esc(a.status || "") + '">' + esc(a.status || "?") + '</span></td>' +
      '<td class="tokens-cell"><b class="down">↓' + pTxt + '</b> <b class="up">↑' + oTxt + '</b><small>' + hitTxt + '</small></td>' +
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
}
