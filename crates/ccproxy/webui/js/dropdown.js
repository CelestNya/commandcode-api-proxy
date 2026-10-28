/* 下拉列表（自绘）
 *
 * 原生 <select> 的弹出层是系统画的：白底、系统蓝的高亮、零圆角、行高与字体
 * 都不受控制。箭头能换，option 不能——这是浏览器一贯的限制，所以"只美化箭头"
 * 的成果是：合上时像这个面板，展开时像 Windows 98。
 *
 * 这里的做法是把 select 降级成只存值的元件（display:none），另画一个触发器
 * 和一张列表。保留原生 select 而不是彻底替换，理由有三条：
 *
 *   1. 调用点全是 el("f-model").value 与 addEventListener("change")，保留它就
 *      一行都不用改；
 *   2. <option> 由别处动态重建（模型列表、每页条数），订阅 childList 即可同步；
 *   3. 禁用、表单语义、无 JS 时的退路都还在。
 *
 * 与控件层其余成员同一条约定：不给 JS 攒第二份状态。选中读 selectedIndex，
 * 禁用读 disabled，展开与否只看菜单挂没挂在文档上。
 */
var Dropdown = (function () {
  var open = null; // 当前展开的 { select, trigger, menu, ... }
  var valueDesc = null;

  function labelOf(sel) {
    var o = sel.options[sel.selectedIndex];
    return o ? o.textContent : "";
  }

  function build(box) {
    var sel = box.select;
    var at = sel.selectedIndex;
    var h = "";
    for (var i = 0; i < sel.options.length; i++) {
      h += '<div class="sd-opt' + (i === at ? " on" : "") + '" role="option" data-i="' + i +
        '" aria-selected="' + (i === at) + '">' + esc(sel.options[i].textContent) + "</div>";
    }
    box.menu.innerHTML = h;
    box.index = at;
  }

  /* 触发器上的文字与禁用态。select 的选项或值被程序改写后都要走这里。 */
  function refresh(sel) {
    var box = sel.__sd;
    if (!box) return;
    var text = labelOf(sel);
    box.label.textContent = text;
    box.trigger.title = text;
    box.trigger.disabled = sel.disabled;
    if (box.menu.parentNode) build(box);
  }

  function place(box) {
    var r = box.trigger.getBoundingClientRect();
    var m = box.menu;
    m.style.minWidth = Math.round(r.width) + "px";
    m.style.top = "0px";
    m.style.left = "0px";
    var h = m.offsetHeight;
    /* 默认向下展开；下面放不下且上面放得下时翻到上方。 */
    var top = r.bottom + 4;
    if (window.innerHeight - r.bottom < h + 8 && r.top > h + 8) top = r.top - 4 - h;
    m.style.top = Math.round(top) + "px";
    m.style.left = Math.round(r.left) + "px";
    /* 贴右边界时往左收，避免横向出界。 */
    var over = r.left + m.offsetWidth - (window.innerWidth - 8);
    if (over > 0) m.style.left = Math.round(r.left - over) + "px";
  }

  function close() {
    if (!open) return;
    var box = open;
    open = null;
    if (box.menu.parentNode) box.menu.parentNode.removeChild(box.menu);
    box.trigger.setAttribute("aria-expanded", "false");
    box.trigger.classList.remove("open");
  }

  function show(box) {
    if (open === box) { close(); return; }
    close();
    build(box);
    document.body.appendChild(box.menu);
    box.trigger.setAttribute("aria-expanded", "true");
    box.trigger.classList.add("open");
    open = box;
    place(box);
  }

  function moveTo(box, i) {
    var n = box.menu.children.length;
    if (!n) return;
    i = Math.max(0, Math.min(n - 1, i));
    box.index = i;
    for (var k = 0; k < n; k++) {
      box.menu.children[k].classList.toggle("on", k === i);
    }
    var cur = box.menu.children[i];
    if (cur && cur.scrollIntoView) cur.scrollIntoView({ block: "nearest" });
  }

  function pick(box, i) {
    var sel = box.select;
    if (i < 0 || i >= sel.options.length) return;
    var was = sel.selectedIndex;
    close();
    sel.selectedIndex = i;
    /* 只有真的换了才通知 —— 点当前项不该触发一次查询。 */
    if (was !== i) sel.dispatchEvent(new Event("change", { bubbles: true }));
    refresh(sel);
    box.trigger.focus();
  }

  function onKey(e) {
    var box = this.__sd;
    var sel = box.select;
    var n = sel.options.length;

    if (e.key === "Escape") {
      if (open === box) { close(); e.preventDefault(); }
      return;
    }
    if (e.key === "Enter" || e.key === " " || e.key === "Spacebar") {
      e.preventDefault();
      if (open === box) pick(box, box.index); else show(box);
      return;
    }
    if (open === box) {
      if (e.key === "ArrowDown") { e.preventDefault(); moveTo(box, box.index + 1); }
      else if (e.key === "ArrowUp") { e.preventDefault(); moveTo(box, box.index - 1); }
      else if (e.key === "Home") { e.preventDefault(); moveTo(box, 0); }
      else if (e.key === "End") { e.preventDefault(); moveTo(box, n - 1); }
      else if (e.key === "Tab") { close(); }
      return;
    }
    /* 收起状态下方向键直接改值：下拉只有几项时，这比"展开再选"少一次操作。 */
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      var next = sel.selectedIndex + (e.key === "ArrowDown" ? 1 : -1);
      if (next < 0 || next >= n) return;
      sel.selectedIndex = next;
      sel.dispatchEvent(new Event("change", { bubbles: true }));
      refresh(sel);
    }
  }

  /* value 是 property 不是 attribute，MutationObserver 看不见它。别处会用
     el("f-model").value = "" 这种写法复位筛选（btn-reset），不拦这一下，触发
     器上的文字就会停在旧值上。只在本实例上覆盖，不动原型。 */
  function hookValue(sel) {
    if (valueDesc === null) {
      valueDesc = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value");
    }
    if (!valueDesc || !valueDesc.get || !valueDesc.set) return;
    try {
      Object.defineProperty(sel, "value", {
        configurable: true,
        get: function () { return valueDesc.get.call(sel); },
        set: function (v) { valueDesc.set.call(sel, v); refresh(sel); }
      });
    } catch (err) {
      /* 拦不住只影响程序改值后的文案刷新，不影响功能。 */
    }
  }

  function enhance(sel) {
    if (!sel || sel.tagName !== "SELECT") return;
    if (sel.__sd) { refresh(sel); return; }
    if (!sel.parentNode) return;

    sel.classList.add("sd-native");

    var trigger = document.createElement("button");
    trigger.type = "button";
    trigger.className = "sd-trigger";
    trigger.setAttribute("role", "combobox");
    trigger.setAttribute("aria-haspopup", "listbox");
    trigger.setAttribute("aria-expanded", "false");

    var label = document.createElement("span");
    label.className = "sd-label";
    trigger.appendChild(label);

    var menu = document.createElement("div");
    menu.className = "sd-menu";
    menu.setAttribute("role", "listbox");

    var box = { select: sel, trigger: trigger, menu: menu, label: label, index: -1 };
    sel.__sd = box;
    trigger.__sd = box;

    sel.parentNode.insertBefore(trigger, sel.nextSibling);

    trigger.addEventListener("click", function (e) {
      e.preventDefault();
      if (sel.disabled) return;
      show(box);
    });
    trigger.addEventListener("keydown", onKey);
    /* mousedown 先拦下来：否则按下的一瞬触发器失焦，菜单在 click 之前就关了。 */
    menu.addEventListener("mousedown", function (e) { e.preventDefault(); });
    menu.addEventListener("click", function (e) {
      var t = e.target.closest ? e.target.closest(".sd-opt") : null;
      if (t) pick(box, Number(t.getAttribute("data-i")));
    });

    hookValue(sel);
    refresh(sel);

    /* 选项重建（模型列表、每页条数）和 disabled 的增删都要跟：前者换的是内容，
       后者决定触发器可不可点。两样都不是我们主动改的，只能观察。 */
    if (window.MutationObserver) {
      new MutationObserver(function () { refresh(sel); })
        .observe(sel, {
          childList: true,
          subtree: true,
          attributes: true,
          attributeFilter: ["disabled"]
        });
    }
  }

  /* 页面重绘后调用：新的 select 被增强，旧的刷新文案。幂等。 */
  function enhanceAll(root) {
    var scope = root || document;
    var list = scope.querySelectorAll ? scope.querySelectorAll("select") : [];
    Array.prototype.forEach.call(list, function (s) {
      if (s.__sd) refresh(s); else enhance(s);
    });
  }

  document.addEventListener("mousedown", function (e) {
    if (!open) return;
    if (open.menu.contains(e.target) || open.trigger.contains(e.target)) return;
    close();
  });
  /* 页面滚动时菜单要和触发器保持贴合。这里重算位置而不是直接关：日志页每
     0.7 秒自动滚到底，一滚就关会让菜单根本点不到。只有触发器自己被滚出视口
     时才关。用 rAF 合帧，滚动一串只算一次。 */
  var scoot = 0;
  function follow() {
    if (!open) return;
    var r = open.trigger.getBoundingClientRect();
    if (r.bottom < 0 || r.top > window.innerHeight || r.right < 0 || r.left > window.innerWidth) {
      close();
      return;
    }
    place(open);
  }
  window.addEventListener("scroll", function (e) {
    if (!open) return;
    if (open.menu.contains(e.target)) return;
    if (scoot) return;
    scoot = window.requestAnimationFrame(function () { scoot = 0; follow(); });
  }, true);
  window.addEventListener("resize", function () { close(); });

  return { enhance: enhance, enhanceAll: enhanceAll, close: close };
})();
