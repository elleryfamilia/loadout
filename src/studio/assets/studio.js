// Minimal htmx-compatible processor for loadout studio.
//
// Implements the htmx attribute subset we use: hx-get/hx-post/hx-delete drive a
// same-origin fetch; hx-target selects where the returned fragment is swapped
// (innerHTML); hx-trigger picks the event(s) (default: submit for forms, click
// otherwise; `load` fires once; `delay:Nms` debounces); hx-confirm gates on a
// window.confirm. A response may override the destination with an `HX-Retarget`
// header (htmx-compatible) — used so a modal form's error renders inside the
// modal instead of replacing the page behind it. Swapped-in content is
// re-processed so nested controls work. Drop in real htmx later and the markup
// is unchanged.
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
      // Mark the trigger in-flight (htmx convention) so it can show progress;
      // on success the target is replaced (so the class goes with it), and the
      // finally cleans up on error or when the element survives the swap.
      el.classList.add("htmx-request");
      var retarget = null;
      fetch(url, opts)
        .then(function (r) {
          // A response can redirect its own swap (e.g. a validation error into
          // the modal's error slot). Same-origin, so the header is readable.
          retarget = r.headers.get("HX-Retarget");
          return r.text();
        })
        .then(function (t) { swap(retarget || target, t); })
        .catch(function () { /* leave the last good fragment in place */ })
        .finally(function () { el.classList.remove("htmx-request"); });
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

  // --- theme toggle ----------------------------------------------------------
  // Preference (auto/light/dark) lives in localStorage; the inline <head> script
  // already stamped <html data-theme/-pref> to avoid a flash. Here we cycle the
  // preference on click, persist it, and (in auto mode) re-resolve when the OS
  // setting flips. CSS keys the visible glyph off data-theme-pref.
  var THEME_KEY = "loadout-theme";
  var THEME_ORDER = ["auto", "light", "dark"];

  function applyTheme(pref) {
    var sysLight =
      window.matchMedia && matchMedia("(prefers-color-scheme: light)").matches;
    var eff = pref === "auto" ? (sysLight ? "light" : "dark") : pref;
    var root = document.documentElement;
    root.dataset.theme = eff;
    root.dataset.themePref = pref;
    try { localStorage.setItem(THEME_KEY, pref); } catch (e) { /* private mode */ }
    var btn = document.getElementById("theme-toggle");
    if (btn) {
      btn.title =
        "Theme: " + pref + (pref === "auto" ? " (" + eff + ")" : "");
    }
  }

  function wireTheme() {
    var btn = document.getElementById("theme-toggle");
    if (btn && !btn.dataset.themeBound) {
      btn.dataset.themeBound = "1";
      btn.addEventListener("click", function () {
        var cur = document.documentElement.dataset.themePref || "auto";
        var next = THEME_ORDER[(THEME_ORDER.indexOf(cur) + 1) % THEME_ORDER.length];
        applyTheme(next);
      });
    }
    // Re-resolve on OS theme change while the preference is "auto".
    if (window.matchMedia) {
      var mq = matchMedia("(prefers-color-scheme: light)");
      var onChange = function () {
        if ((document.documentElement.dataset.themePref || "auto") === "auto") {
          applyTheme("auto");
        }
      };
      if (mq.addEventListener) mq.addEventListener("change", onChange);
      else if (mq.addListener) mq.addListener(onChange);
    }
    // Sync the button tooltip with whatever the inline init resolved.
    applyTheme(document.documentElement.dataset.themePref || "auto");
  }

  document.addEventListener("DOMContentLoaded", function () {
    process(document.body);
    wireActiveGroups();
    wireTheme();
    enhanceCode(document.body);
  });
})();
