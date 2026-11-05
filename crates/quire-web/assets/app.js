// Quire web app: small, plain JavaScript. Everything works without it
// except the quick-open box, lazy folders, live preview and code colours.
(function () {
  "use strict";
  var root = document.documentElement;
  try {
    var saved = localStorage.getItem("quire-theme");
    if (saved === "light" || saved === "dark") root.dataset.theme = saved;
  } catch (e) { /* storage off: follow the system */ }

  function $(sel, el) { return (el || document).querySelector(sel); }
  function $$(sel, el) { return Array.prototype.slice.call((el || document).querySelectorAll(sel)); }
  function esc(s) { return String(s).replace(/[&<>"']/g, function (c) { return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]; }); }

  function dark() {
    if (root.dataset.theme) return root.dataset.theme === "dark";
    return window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;
  }

  function setupTheme() {
    var b = $("#theme");
    if (!b) return;
    var label = function () { b.setAttribute("aria-pressed", dark() ? "true" : "false"); };
    label();
    b.addEventListener("click", function () {
      var next = dark() ? "light" : "dark";
      root.dataset.theme = next;
      try { localStorage.setItem("quire-theme", next); } catch (e) { /* ignore */ }
      label();
    });
  }

  function setupMenu() {
    var b = $("#menu"), side = $("#side");
    if (!b || !side) return;
    b.addEventListener("click", function () {
      var open = document.body.classList.toggle("side-open");
      b.setAttribute("aria-expanded", open ? "true" : "false");
    });
    document.addEventListener("keydown", function (e) {
      if (e.key === "Escape" && document.body.classList.contains("side-open")) {
        document.body.classList.remove("side-open");
        b.setAttribute("aria-expanded", "false");
        b.focus();
      }
    });
    var current = $('a[aria-current="page"]', side);
    if (current && current.scrollIntoView) current.scrollIntoView({ block: "center" });
  }

  // Folders load their contents when first opened.
  function setupTree() {
    document.addEventListener("toggle", function (e) {
      var d = e.target;
      if (!d || d.tagName !== "DETAILS" || !d.open || !d.dataset.dir) return;
      var ul = d.querySelector(":scope > ul");
      if (!ul || ul.children.length || d.dataset.loaded) return;
      d.dataset.loaded = "1";
      fetch("/api/tree?dir=" + encodeURIComponent(d.dataset.dir), { credentials: "same-origin" })
        .then(function (r) { return r.ok ? r.text() : ""; })
        .then(function (html) {
          var tmp = document.createElement("div");
          tmp.innerHTML = html;
          var got = tmp.querySelector("ul");
          if (got) ul.replaceWith(got);
        });
    }, true);
  }

  // Ctrl+K (or ⌘K): open a note by name.
  function setupPalette() {
    var dlg = $("#palette"), input = $("#palette-q"), list = $("#palette-list"), btn = $("#quick");
    if (!dlg || !input || !list) return;
    var hits = [], sel = 0, timer = 0, seq = 0;
    function render() {
      list.innerHTML = hits.map(function (h, i) {
        var folder = h.path.indexOf("/") >= 0 ? h.path.slice(0, h.path.lastIndexOf("/")) : "";
        return '<li role="option" id="pal-' + i + '" aria-selected="' + (i === sel) + '" data-url="' + esc(h.url) + '"><span>' +
          esc(h.title) + "</span><small>" + esc(folder) + "</small></li>";
      }).join("") || '<li class="muted" role="option" aria-disabled="true">No notes match</li>';
      input.setAttribute("aria-activedescendant", hits.length ? "pal-" + sel : "");
    }
    function load() {
      var n = ++seq;
      fetch("/api/quick?q=" + encodeURIComponent(input.value), { credentials: "same-origin" })
        .then(function (r) { return r.json(); })
        .then(function (j) { if (n === seq) { hits = j.hits || []; sel = 0; render(); } })
        .catch(function () { /* offline: keep what's shown */ });
    }
    function open() {
      if (dlg.open) return;
      input.value = "";
      dlg.showModal();
      input.focus();
      load();
    }
    function go(i) { if (hits[i]) window.location.href = hits[i].url; }
    if (btn) btn.addEventListener("click", open);
    document.addEventListener("keydown", function (e) {
      if ((e.ctrlKey || e.metaKey) && (e.key === "k" || e.key === "K")) { e.preventDefault(); open(); }
    });
    input.addEventListener("input", function () { clearTimeout(timer); timer = setTimeout(load, 90); });
    input.addEventListener("keydown", function (e) {
      if (e.key === "ArrowDown") { e.preventDefault(); sel = Math.min(sel + 1, hits.length - 1); render(); }
      else if (e.key === "ArrowUp") { e.preventDefault(); sel = Math.max(sel - 1, 0); render(); }
      else if (e.key === "Enter") { e.preventDefault(); go(sel); }
    });
    list.addEventListener("click", function (e) {
      var li = e.target.closest("li[data-url]");
      if (li) window.location.href = li.dataset.url;
    });
    dlg.addEventListener("click", function (e) { if (e.target === dlg) dlg.close(); });
  }

  function setupEditor() {
    var form = $("#editor"), text = $("#text"), pv = $("#preview"), pbtn = $("#preview-btn");
    if (!form || !text) return;
    var start = text.value, sending = false;
    form.addEventListener("submit", function () { sending = true; });
    window.addEventListener("beforeunload", function (e) {
      if (!sending && text.value !== start) { e.preventDefault(); e.returnValue = ""; }
    });
    text.addEventListener("keydown", function (e) {
      if ((e.ctrlKey || e.metaKey) && (e.key === "s" || e.key === "S")) {
        e.preventDefault();
        var save = form.querySelector('button[value="save"]');
        if (save) save.click();
      }
    });
    if (!pbtn || !pv) return;
    var shown = !pv.hidden, timer = 0;
    function refresh() {
      var body = new URLSearchParams();
      body.set("_csrf", form.querySelector('input[name="_csrf"]').value);
      body.set("text", text.value);
      body.set("path", decodeURIComponent(form.getAttribute("action").replace(/^\/edit\//, "")));
      fetch("/api/preview", { method: "POST", credentials: "same-origin", body: body })
        .then(function (r) { return r.ok ? r.text() : "<p>Preview isn't available.</p>"; })
        .then(function (html) { pv.innerHTML = html; highlight(pv); folds(pv); });
    }
    pbtn.addEventListener("click", function (e) {
      e.preventDefault();
      shown = !shown;
      pv.hidden = !shown;
      pv.parentElement.classList.toggle("split", shown);
      pbtn.setAttribute("aria-pressed", shown ? "true" : "false");
      if (shown) refresh();
    });
    text.addEventListener("input", function () { if (shown) { clearTimeout(timer); timer = setTimeout(refresh, 350); } });
  }

  // Folding callouts ("> [!note]-").
  function folds(el) {
    $$(".callout[data-callout-fold]", el).forEach(function (c) {
      if (c.dataset.ready) return;
      c.dataset.ready = "1";
      var t = $(".callout-title", c);
      if (c.dataset.calloutFold === "-") c.classList.add("folded");
      if (!t) return;
      t.setAttribute("role", "button");
      t.setAttribute("tabindex", "0");
      t.setAttribute("aria-expanded", c.classList.contains("folded") ? "false" : "true");
      var flip = function () {
        var f = c.classList.toggle("folded");
        t.setAttribute("aria-expanded", f ? "false" : "true");
      };
      t.addEventListener("click", flip);
      t.addEventListener("keydown", function (e) { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); flip(); } });
    });
  }

  // A small highlighter: comments, strings, numbers, keywords, calls.
  var KEYWORDS = ("as async await break case catch class const continue def default defer del do elif else enum export extends " +
    "false fn for from func function go if impl import in interface let loop match mod module mut new nil none not null or " +
    "package pass pub raise return self static struct super switch then this throw true try type typeof use var where while with yield").split(" ");
  var KW = {};
  KEYWORDS.forEach(function (k) { KW[k] = 1; });
  function tokenize(src, lang) {
    var hash = /^(py|python|sh|bash|shell|zsh|toml|yaml|yml|ruby|rb|r|make|dockerfile|nix|perl)$/.test(lang);
    var dash = /^(sql|lua|haskell|hs)$/.test(lang);
    var out = "", i = 0, n = src.length;
    var wrap = function (cls, s) { return '<span class="tok-' + cls + '">' + esc(s) + "</span>"; };
    while (i < n) {
      var c = src[i], rest = src.slice(i), m;
      if ((c === "/" && src[i + 1] === "/" && !hash) || (c === "#" && hash) || (c === "-" && src[i + 1] === "-" && dash)) {
        var e = src.indexOf("\n", i); if (e < 0) e = n;
        out += wrap("c", src.slice(i, e)); i = e; continue;
      }
      if (c === "/" && src[i + 1] === "*") {
        var e2 = src.indexOf("*/", i + 2); e2 = e2 < 0 ? n : e2 + 2;
        out += wrap("c", src.slice(i, e2)); i = e2; continue;
      }
      if (c === '"' || c === "'" || c === "`") {
        var j = i + 1;
        while (j < n && src[j] !== c && !(src[j] === "\n" && c !== "`")) { if (src[j] === "\\") j++; j++; }
        out += wrap("s", src.slice(i, j + 1)); i = j + 1; continue;
      }
      if ((m = /^\d[\d_.xXa-fA-F]*/.exec(rest)) && !/[\w$]/.test(src[i - 1] || "")) { out += wrap("n", m[0]); i += m[0].length; continue; }
      if ((m = /^[A-Za-z_$][\w$]*/.exec(rest))) {
        var w = m[0];
        if (KW[w]) out += wrap("k", w);
        else if (src[i + w.length] === "(") out += wrap("f", w);
        else out += esc(w);
        i += w.length; continue;
      }
      out += esc(c); i++;
    }
    return out;
  }
  function highlight(el) {
    $$('pre > code[class*="language-"]', el).forEach(function (code) {
      if (code.dataset.hl) return;
      var lang = (/language-([\w+#-]+)/.exec(code.className) || [])[1] || "";
      if (/^(mermaid|text|txt|plain|markdown|md)$/.test(lang) || code.textContent.length > 60000) return;
      code.dataset.hl = "1";
      code.innerHTML = tokenize(code.textContent, lang.toLowerCase());
    });
  }

  document.addEventListener("DOMContentLoaded", function () {
    setupTheme();
    setupMenu();
    setupTree();
    setupPalette();
    setupEditor();
    highlight(document);
    folds(document);
  });
})();
