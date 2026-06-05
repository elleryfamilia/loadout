// Minimal htmx-compatible processor for rosita studio.
//
// Implements the htmx attribute subset we use: hx-get/hx-post/hx-delete drive a
// same-origin fetch; hx-target selects where the returned fragment is swapped
// (innerHTML); hx-trigger picks the event(s) (default: submit for forms, click
// otherwise; `load` fires once; `delay:Nms` debounces); hx-confirm gates on a
// window.confirm. Swapped-in content is re-processed so nested controls work.
// Drop in real htmx later and the markup is unchanged.
(function () {
  "use strict";

  function serialize(form) {
    return new URLSearchParams(new FormData(form)).toString();
  }

  // The swapped fragment is HTML our own server rendered with maud, which
  // escapes every dynamic value; requests are same-origin only (cookie + Host +
  // Origin guards). So innerHTML here is server-trusted, escaped output — the
  // same model htmx uses. Never swap in content from any other origin.
  function swap(selector, html) {
    var el = selector ? document.querySelector(selector) : null;
    if (!el) return;
    el.innerHTML = html;
    process(el);
    enhanceCode(el);
  }

  function methodOf(el) {
    if (el.hasAttribute("hx-post")) return ["POST", el.getAttribute("hx-post")];
    if (el.hasAttribute("hx-delete")) return ["DELETE", el.getAttribute("hx-delete")];
    if (el.hasAttribute("hx-put")) return ["PUT", el.getAttribute("hx-put")];
    if (el.hasAttribute("hx-get")) return ["GET", el.getAttribute("hx-get")];
    return null;
  }

  function parseTrigger(el, isForm) {
    var spec = el.getAttribute("hx-trigger") || (isForm ? "submit" : "click");
    var delay = 0;
    var m = spec.match(/delay:(\d+)ms/);
    if (m) delay = parseInt(m[1], 10);
    var events = spec.split(",").map(function (c) { return c.trim().split(/\s+/)[0]; });
    return { events: events, delay: delay };
  }

  function bind(el) {
    if (el.dataset.hxBound) return;
    var route = methodOf(el);
    if (!route) return;
    el.dataset.hxBound = "1";
    var method = route[0];
    var url = route[1];
    var target = el.getAttribute("hx-target");
    var confirmMsg = el.getAttribute("hx-confirm");
    var isForm = el.tagName === "FORM";
    var form = isForm ? el : el.closest("form");
    var trig = parseTrigger(el, isForm);
    var timer;

    function send() {
      var opts = { method: method, headers: {} };
      if (method !== "GET") {
        opts.headers["Content-Type"] = "application/x-www-form-urlencoded";
        opts.body = form ? serialize(form) : "";
      }
      fetch(url, opts)
        .then(function (r) { return r.text(); })
        .then(function (t) { swap(target, t); })
        .catch(function () { /* leave the last good fragment in place */ });
    }

    function fire(ev) {
      if (ev) ev.preventDefault();
      // hx-confirm gates the request on a themed dialog (not window.confirm).
      if (confirmMsg) { confirmThen(el, url, method, confirmMsg, send); return; }
      clearTimeout(timer);
      timer = setTimeout(send, trig.delay);
    }

    trig.events.forEach(function (ev) {
      if (ev === "load") fire();
      else el.addEventListener(ev, fire);
    });
  }

  function process(root) {
    var sel = "[hx-get],[hx-post],[hx-delete],[hx-put]";
    if (root.matches && root.matches(sel)) bind(root);
    root.querySelectorAll(sel).forEach(bind);
  }

  // A themed confirmation dialog (replaces the native window.confirm). The
  // message is our own server-rendered hx-confirm string; it's set via
  // textContent (never innerHTML). Confirm runs onOk(); Cancel / Escape /
  // backdrop dismiss. Enter confirms. Tone is inferred (DELETE / danger class).
  function confirmThen(srcEl, url, method, message, onOk) {
    var danger = method === "DELETE" ||
      /(^|\s)(btn-danger|danger)(\s|$)/.test(srcEl.className || "");
    var apply = /\/apply$/.test(url);
    var okLabel = danger ? "Delete" : apply ? "Apply" : "Confirm";
    var title = danger ? "Confirm removal" : apply ? "Apply changes" : "Please confirm";

    var root = document.createElement("div");
    root.className = "confirm-root" + (danger ? " danger" : "");
    var backdrop = document.createElement("div");
    backdrop.className = "confirm-backdrop";
    var card = document.createElement("div");
    card.className = "confirm";
    card.setAttribute("role", "alertdialog");
    card.setAttribute("aria-modal", "true");
    var h = document.createElement("h2");
    h.className = "confirm-title";
    h.textContent = title;
    var p = document.createElement("p");
    p.className = "confirm-msg";
    p.textContent = message;
    var foot = document.createElement("div");
    foot.className = "confirm-foot";
    var cancel = document.createElement("button");
    cancel.type = "button";
    cancel.className = "btn btn-ghost";
    cancel.textContent = "Cancel";
    var ok = document.createElement("button");
    ok.type = "button";
    ok.className = "btn " + (danger ? "btn-danger" : "btn-primary");
    ok.textContent = okLabel;
    foot.appendChild(cancel);
    foot.appendChild(ok);
    card.appendChild(h);
    card.appendChild(p);
    card.appendChild(foot);
    root.appendChild(backdrop);
    root.appendChild(card);
    document.body.appendChild(root);
    ok.focus();

    function close() {
      document.removeEventListener("keydown", onKey, true);
      if (root.parentNode) root.parentNode.removeChild(root);
    }
    function onKey(e) {
      if (e.key === "Escape") { e.preventDefault(); close(); }
      else if (e.key === "Enter") { e.preventDefault(); close(); onOk(); }
    }
    cancel.addEventListener("click", close);
    backdrop.addEventListener("click", close);
    ok.addEventListener("click", function () { close(); onOk(); });
    document.addEventListener("keydown", onKey, true);
  }

  // Active-state for the top tabs and the profile rail (chrome only; the swap
  // itself is hx-driven). Delegated so it survives fragment swaps: clicking a
  // [data-tab] or [data-profile] marks it active among its same-attribute peers.
  function wireActiveGroups() {
    document.addEventListener("click", function (ev) {
      var el = ev.target.closest
        ? ev.target.closest("[data-tab],[data-profile]")
        : null;
      if (!el) return;
      var attr = el.hasAttribute("data-tab") ? "data-tab" : "data-profile";
      var peers = el.parentNode.querySelectorAll("[" + attr + "]");
      for (var i = 0; i < peers.length; i++) peers[i].classList.remove("active");
      el.classList.add("active");
    });
  }

  // Icon-picker dropdown: reflect the chosen icon in the trigger and close the
  // popover on selection. Delegated, so it survives dialog swaps.
  function wireIconPicker() {
    document.addEventListener("change", function (ev) {
      var input = ev.target;
      if (!input.matches || !input.matches(".icon-dd input[type=radio]")) return;
      var dd = input.closest(".icon-dd");
      if (!dd) return;
      // Mirror the chosen icon into the trigger by cloning the SVG node (no
      // innerHTML — the source is our own static, server-rendered markup).
      var svg = input.parentNode.querySelector(".icon-cell svg");
      var sel = dd.querySelector(".icon-cell-sel");
      if (svg && sel) sel.replaceChildren(svg.cloneNode(true));
      dd.open = false;
    });
  }

  // --- script editor: bash syntax highlighting -------------------------------
  // A transparent-text <textarea> over a <pre> backdrop we repaint on input. All
  // values are escaped before they reach innerHTML (the backdrop never receives
  // raw user text), matching the rest of studio's no-unescaped-innerHTML rule.

  function escapeHtml(s) {
    return s.replace(/[&<>]/g, function (c) {
      return c === "&" ? "&amp;" : c === "<" ? "&lt;" : "&gt;";
    });
  }

  var BASH_KW = {
    if: 1, then: 1, elif: 1, else: 1, fi: 1, for: 1, in: 1, do: 1, done: 1,
    while: 1, until: 1, case: 1, esac: 1, function: 1, return: 1, exit: 1,
    break: 1, continue: 1, local: 1, select: 1, time: 1,
  };
  var BASH_BUILTIN = {
    echo: 1, printf: 1, cd: 1, command: 1, read: 1, export: 1, set: 1, unset: 1,
    eval: 1, source: 1, test: 1, trap: 1, shift: 1, getopts: 1, hash: 1,
    type: 1, alias: 1, declare: 1, kill: 1, wait: 1,
  };

  // Pragmatic bash tokenizer — good enough to read by, not a full grammar.
  function highlightBash(src) {
    var re = /(#[^\n]*)|("(?:[^"\\]|\\.)*"?)|('[^']*'?)|(\$\{[^}]*\}|\$[A-Za-z_][A-Za-z0-9_]*|\$[0-9*@?#!$-])|(\b\d+\b)|([A-Za-z_][A-Za-z0-9_]*)|([^\sA-Za-z0-9_#"'$]+)/g;
    var out = "";
    var last = 0;
    var m;
    while ((m = re.exec(src))) {
      if (m.index > last) out += escapeHtml(src.slice(last, m.index));
      last = re.lastIndex;
      var t = m[0];
      var cls = null;
      if (m[1]) cls = "c";
      else if (m[2] || m[3]) cls = "s";
      else if (m[4]) cls = "v";
      else if (m[5]) cls = "n";
      else if (m[6]) cls = BASH_KW[t] ? "k" : BASH_BUILTIN[t] ? "b" : null;
      else if (m[7]) cls = "o";
      out += cls
        ? '<span class="t-' + cls + '">' + escapeHtml(t) + "</span>"
        : escapeHtml(t);
      if (re.lastIndex === m.index) re.lastIndex++; // never spin on a zero-width match
    }
    if (last < src.length) out += escapeHtml(src.slice(last));
    return out;
  }

  function enhanceCode(root) {
    if (!root || !root.querySelectorAll) return;
    root.querySelectorAll("textarea.code-edit").forEach(function (ta) {
      if (ta.dataset.codeBound) return;
      ta.dataset.codeBound = "1";
      var wrap = ta.closest(".code-edit-wrap");
      var code = wrap && wrap.querySelector(".code-hl code");
      if (!code) return;
      wrap.classList.add("code-live");
      function paint() { code.innerHTML = highlightBash(ta.value); }
      function sync() {
        var pre = code.parentNode;
        pre.scrollTop = ta.scrollTop;
        pre.scrollLeft = ta.scrollLeft;
      }
      ta.addEventListener("input", function () { paint(); sync(); });
      ta.addEventListener("scroll", sync);
      paint();
      sync();
    });
  }

  document.addEventListener("DOMContentLoaded", function () {
    process(document.body);
    wireActiveGroups();
    wireIconPicker();
    enhanceCode(document.body);
  });
})();
