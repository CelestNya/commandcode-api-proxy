/* 设置页
 *
 * 同步策略：不建立长连接。页面进入时取一次文档，改动期间只记在本地（脏值），
 * 按「应用」才写回；保存时带上载入时的 revision，服务端不一致就回 409 并把
 * 当前文档带回来——这是「两个窗口同时改」唯一诚实的处理方式。
 *
 * 更新同样是「按钮 + 轮询」：只有任务在跑的时候才轮询，跑完就停，页面关掉
 * 什么都没有留下。
 */

var setState = {
  doc: null,        /* 最近一次从服务端取到的文档 */
  dirty: {},        /* key -> 新值 */
  busy: false,
  updateTimer: null,
  update: null,     /* /webui/api/update 的响应 */
};

function setStatus(text, kind) {
  var node = el("set-status");
  if (!node) return;
  node.textContent = text || "";
  node.className = "set-status" + (kind ? " " + kind : "");
}

/* ── 表单渲染 ── */

function fieldControl(f) {
  var v = setState.dirty.hasOwnProperty(f.key) ? setState.dirty[f.key] : f.value;
  var dis = f.envValue != null ? " disabled" : "";
  if (f.kind === "bool") {
    return '<input type="checkbox" data-key="' + esc(f.key) + '"' +
      (v ? " checked" : "") + dis + ">";
  }
  if (f.kind === "choice") {
    var opts = (f.options || []).map(function (o) {
      return '<option value="' + esc(o) + '"' + (o === v ? " selected" : "") + ">" + esc(o) + "</option>";
    }).join("");
    return '<select data-key="' + esc(f.key) + '"' + dis + ">" + opts + "</select>";
  }
  if (f.kind === "int") {
    return '<input type="number" data-key="' + esc(f.key) + '" value="' + esc(v) +
      '" min="' + f.min + '" max="' + f.max + '" step="1"' + dis + ">" +
      (f.unit ? '<span class="unit">' + esc(f.unit) + "</span>" : "");
  }
  return '<input type="text" data-key="' + esc(f.key) + '" value="' + esc(v) + '"' + dis + ">";
}

function renderSettings(doc) {
  setState.doc = doc;
  var host = el("set-fields");
  if (!host) return;
  var groups = [];
  var byGroup = {};
  doc.fields.forEach(function (f) {
    if (!byGroup[f.group]) { byGroup[f.group] = []; groups.push(f.group); }
    byGroup[f.group].push(f);
  });
  var html = groups.map(function (g) {
    var rows = byGroup[g].map(function (f) {
      var dirty = setState.dirty.hasOwnProperty(f.key);
      var shadowed = f.envValue != null;
      var env = shadowed
        ? '<div class="set-env">环境变量 ' + esc(f.env) + "=" + esc(f.envValue) + " 覆盖了此项，保存后不会生效</div>"
        : "";
      return '<div class="set-row' + (dirty ? " dirty" : "") + (shadowed ? " shadowed" : "") +
        '" data-row="' + esc(f.key) + '">' +
        '<div class="set-text"><span class="set-label">' + esc(f.label) + "</span>" +
        '<div class="set-note">' + esc(f.note) + "</div>" + env + "</div>" +
        '<div class="set-control">' + fieldControl(f) + "</div></div>";
    }).join("");
    return '<div class="set-group"><h3>' + esc(g) + "</h3>" + rows + "</div>";
  }).join("");
  host.innerHTML = html;

  host.querySelectorAll("[data-key]").forEach(function (input) {
    input.addEventListener("input", onFieldInput);
    input.addEventListener("change", onFieldInput);
  });

  var pending = doc.revision !== doc.runningRevision;
  el("set-rev").textContent = "配置 " + doc.path;
  el("set-apply").disabled = true;
  el("set-revert").disabled = true;
  if (doc.corrupt) {
    setStatus("配置文件无法解析 —— 保存会用当前表单重写它", "warn");
  } else if (pending) {
    setStatus("有改动尚未生效（需重启代理）", "warn");
  } else {
    setStatus("");
  }
}

function onFieldInput(ev) {
  var input = ev.currentTarget;
  var key = input.getAttribute("data-key");
  var value = input.type === "checkbox" ? input.checked
    : input.type === "number" ? Number(input.value) : input.value;
  var field = (setState.doc.fields || []).filter(function (f) { return f.key === key; })[0];
  if (!field) return;
  /* 与已存值相同就不算脏——避免"点开又改回去"也触发一次写入与重启。 */
  if (JSON.stringify(value) === JSON.stringify(field.value)) delete setState.dirty[key];
  else setState.dirty[key] = value;
  var row = document.querySelector('[data-row="' + key + '"]');
  if (row) row.classList.toggle("dirty", setState.dirty.hasOwnProperty(key));
  var count = Object.keys(setState.dirty).length;
  el("set-apply").disabled = count === 0 || setState.busy;
  el("set-revert").disabled = count === 0 || setState.busy;
  if (count > 0) setStatus("有 " + count + " 项改动未应用");
}

/* ── 读写 ── */

function loadSettings() {
  return fetch("/webui/api/settings").then(function (r) { return r.json(); })
    .then(function (doc) { renderSettings(doc); })
    .catch(function () { setStatus("无法读取设置（代理可能正在重启）", "bad"); });
}

function applySettings() {
  if (!setState.doc || setState.busy) return;
  var payload = { baseRevision: setState.doc.revision, values: setState.dirty, restart: true };
  setState.busy = true;
  el("set-apply").disabled = true;
  setStatus("正在保存…");
  fetch("/webui/api/settings", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  }).then(function (r) {
    return r.json().then(function (body) { return { status: r.status, body: body }; });
  }).then(function (res) {
    setState.busy = false;
    if (res.status === 409) {
      /* 别人改过这个文件：以服务端为准重画，本地改动随之丢弃——静默覆盖
         别人的修改比丢一次点击更糟。 */
      setState.dirty = {};
      if (res.body.document) renderSettings(res.body.document);
      setStatus(res.body.error || "配置文件已被其他窗口修改，已重新载入", "warn");
      return;
    }
    if (res.status !== 200) {
      setStatus(res.body.error || "保存失败", "bad");
      el("set-apply").disabled = false;
      return;
    }
    setState.dirty = {};
    if (res.body.document) renderSettings(res.body.document);
    setStatus(res.body.note || "已保存", "good");
  }).catch(function () {
    setState.busy = false;
    setStatus("保存失败：无法连接代理", "bad");
  });
}

function revertSettings() {
  setState.dirty = {};
  if (setState.doc) renderSettings(setState.doc);
  setStatus("已还原未应用的改动");
}

function restartProxy() {
  setStatus("正在请求重启…");
  fetch("/webui/api/restart", { method: "POST", headers: { "Content-Type": "application/json" }, body: "{}" })
    .then(function (r) { return r.json().then(function (b) { return { status: r.status, body: b }; }); })
    .then(function (res) {
      if (res.status !== 200) { setStatus(res.body.error || "重启请求失败", "bad"); return; }
      setStatus("已请求重启，代理将在约 1 秒内重新监听；本页会短暂断开", "good");
      /* 只有这里需要等：重启后重新取一次文档，把「待生效」状态清掉。 */
      setTimeout(loadSettings, 2500);
    })
    .catch(function () { setStatus("重启请求失败：无法连接代理", "bad"); });
}

/* ── 更新 ── */

function renderUpdate() {
  var data = setState.update;
  if (!data) return;
  var u = data.update, job = data.job || {};
  el("upd-current").textContent = "v" + u.current;
  var note = el("upd-note");
  note.className = "upd-note";
  var installable = u.installable === true;
  el("upd-install").disabled = !installable || job.phase === "running";

  if (u.staged) {
    el("upd-tag").textContent = "已就绪";
    note.textContent = "v" + u.staged.version + " 已下载到 " + u.staged.root +
      "，托盘将自动切换（配置、日志、缓存都会保留）。";
    note.classList.add("good");
    el("upd-install").disabled = true;
    return;
  }
  if (job.phase === "running") {
    el("upd-tag").textContent = "进行中";
    note.textContent = job.message || "正在处理…";
    return;
  }
  if (u.available) {
    el("upd-tag").textContent = "有更新可用";
    note.textContent = "新版本 v" + u.latest + (u.size ? "（" + (u.size / 1048576).toFixed(1) + " MB）" : "") +
      "。更新会先下载并校验，再切换进程，不影响已保存的配置与日志。";
    if (!installable) {
      note.textContent = "新版本 v" + u.latest + " 没有适用于本平台的安装包。";
      note.classList.add("warn");
    }
    return;
  }
  if (u.checkedAt == null) {
    el("upd-tag").textContent = "";
    note.textContent = "尚未检查过更新。代理会每 6 小时静默检查一次，不打扰使用。";
    return;
  }
  el("upd-tag").textContent = "已是最新";
  note.textContent = "上次检查：" + fmtTime(u.checkedAt * 1000) + "。";
  if (u.error) {
    note.textContent += " 上次检查失败：" + u.error;
    note.classList.add(u.error ? "warn" : "");
  }
}

function pollUpdateUntilDone() {
  if (setState.updateTimer) return;
  setState.updateTimer = setTimeout(function tick() {
    setState.updateTimer = null;
    fetch("/webui/api/update").then(function (r) { return r.json(); }).then(function (data) {
      setState.update = data;
      renderUpdate();
      var job = data.job || {};
      var bar = el("upd-progress");
      bar.hidden = job.phase !== "running";
      el("upd-msg").textContent = job.message || "";
      if (job.phase === "running") {
        setState.updateTimer = setTimeout(tick, 1200);
      } else if (job.phase === "failed") {
        el("upd-msg").textContent = job.message || "任务失败";
        el("upd-check").disabled = false;
        el("upd-install").disabled = !(data.update && data.update.installable);
      } else if (job.phase === "done") {
        /* 安装完成 ⇒ 代理已把接管请求交给托盘，切换马上发生。 */
        if (data.update && data.update.staged) {
          setStatus("更新已暂存，托盘正在切换…", "good");
        }
      }
    }).catch(function () {
      /* 切换期间连接会断；不再重试，避免页面在后台空转。 */
      el("upd-progress").hidden = true;
    });
  }, 300);
}

function onUpdateCheck() {
  el("upd-check").disabled = true;
  el("upd-progress").hidden = false;
  el("upd-msg").textContent = "正在检查更新…";
  fetch("/webui/api/update", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ action: "check" }) })
    .then(function (r) { return r.json().then(function (b) { return { status: r.status, body: b }; }); })
    .then(function (res) {
      if (res.status !== 200) {
        el("upd-msg").textContent = res.body.error || "检查失败";
        el("upd-progress").hidden = true;
        el("upd-check").disabled = false;
        return;
      }
      pollUpdateUntilDone();
      setTimeout(function () { el("upd-check").disabled = false; }, 400);
    })
    .catch(function () {
      el("upd-msg").textContent = "检查失败：无法连接代理";
      el("upd-progress").hidden = true;
      el("upd-check").disabled = false;
    });
}

function onUpdateInstall() {
  if (!confirm("下载新版本并切换到它？\n\n当前代理会重启约 1 秒，配置与日志会保留。")) return;
  el("upd-install").disabled = true;
  el("upd-progress").hidden = false;
  el("upd-msg").textContent = "正在下载…";
  fetch("/webui/api/update", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ action: "install" }) })
    .then(function (r) { return r.json().then(function (b) { return { status: r.status, body: b }; }); })
    .then(function (res) {
      if (res.status !== 200) {
        el("upd-msg").textContent = res.body.error || "安装失败";
        el("upd-progress").hidden = true;
        el("upd-install").disabled = false;
        return;
      }
      pollUpdateUntilDone();
    })
    .catch(function () {
      el("upd-msg").textContent = "安装失败：无法连接代理";
      el("upd-progress").hidden = true;
      el("upd-install").disabled = false;
    });
}

/* 进入设置页时取一次数据（不是轮询）。 */
function onEnterSettings() {
  loadSettings();
  fetch("/webui/api/update").then(function (r) { return r.json(); }).then(function (data) {
    setState.update = data;
    renderUpdate();
    var job = (data.job || {});
    el("upd-progress").hidden = job.phase !== "running";
    el("upd-msg").textContent = job.message || "";
    /* 离开再回来时任务可能还在跑：接着轮询。 */
    if (job.phase === "running") pollUpdateUntilDone();
  }).catch(function () { /* 首帧失败不打扰；按钮仍在 */ });
}

function initSettings() {
  el("set-apply").addEventListener("click", applySettings);
  el("set-revert").addEventListener("click", revertSettings);
  el("set-restart").addEventListener("click", restartProxy);
  el("upd-check").addEventListener("click", onUpdateCheck);
  el("upd-install").addEventListener("click", onUpdateInstall);
}
