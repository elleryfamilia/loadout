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

  // Tab active-state (chrome only; the swap itself is hx-driven). Delegated so it
  // survives fragment swaps. Clicking a [data-tab] marks it active among its peers.
  function wireTabs() {
    document.addEventListener("click", function (ev) {
      var tab = ev.target.closest ? ev.target.closest("[data-tab]") : null;
      if (!tab) return;
      var peers = tab.parentNode.querySelectorAll("[data-tab]");
      for (var i = 0; i < peers.length; i++) peers[i].classList.remove("active");
      tab.classList.add("active");
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

  document.addEventListener("DOMContentLoaded", function () {
    process(document.body);
    wireTabs();
    wireIconPicker();
  });
})();
