/* loadout plan viewer — progressive enhancement over server-rendered HTML.
   Pure core first (no DOM), DOM layer second, #selftest harness last. */
(function () {
  "use strict";

  const core = {
    parseIsland(text) { return JSON.parse(text); },
    /* `blocking` replaces the old 4-way `type` taxonomy: a comment either
       blocks approval or it doesn't -- the free-form `text` carries whatever
       nuance a category label used to gesture at. Defaults to false so
       existing non-blocking callers don't need to pass it. */
    makeComment(ref, quote, text, blocking) {
      return { ref: ref, quote: quote || null, text: text, blocking: !!blocking };
    },
    buildFeedback(plan, fingerprint, comments) {
      const doc = {
        format: "loadout.plan-feedback/1",
        plan_id: plan.meta.id,
        plan_hash: fingerprint,
        verdict: comments.some(c => c.blocking) ? "request_changes" : "comment",
        comments: comments.map((c, i) => ({
          id: "c-" + (i + 1),
          ref: c.ref, quote: c.quote, text: c.text, blocking: !!c.blocking,
        })),
      };
      const lines = ["## Plan feedback — " + plan.meta.id, ""];
      for (const c of doc.comments) {
        lines.push("### " + c.ref + (c.blocking ? " — BLOCKS APPROVAL" : ""));
        /* Blockquote every line of free-form comment text so a "```" line
           in it reads as "> ```" -- that can't open a top-level fence, and
           any fence it does open stays contained inside the blockquote. */
        for (const textLine of c.text.split("\n")) lines.push("> " + textLine);
        if (c.quote) {
          /* Collapse whitespace (incl. newlines) to single spaces so the
             quote is safe to embed inline -- a mid-line ``` can't open a
             fence. */
          lines.push('_(re: "' + c.quote.replace(/\s+/g, " ") + '")_');
        }
        lines.push("");
      }
      const json = JSON.stringify(doc, null, 2);
      const markdown = lines.join("\n");
      /* Human-readable mirror first, canonical JSON after: the person
         pasting reads the top; the agent needs the fenced block (stable
         refs, plan_hash, blocking flags) and is told not to lose it. */
      const combined = markdown
        + "\n---\n\n"
        + "Machine-readable block — paste everything, leave this intact:\n\n"
        + "```json\n" + json + "\n```\n";
      return { json: json, markdown: markdown, combined: combined };
    },
  };
  window.loadoutPlan = core;

  function selftest() {
    const results = [];
    const pending = [];
    function check(name, fn) {
      try { fn(); results.push("PASS " + name); }
      catch (e) { results.push("FAIL " + name + ": " + e.message); }
    }
    function checkAsync(name, promise) {
      pending.push(Promise.resolve(promise).then(
        function (ok) { results.push((ok ? "PASS " : "FAIL ") + name); },
        function (e) {
          results.push("FAIL " + name + ": " + (e && e.message ? e.message : String(e)));
        }
      ));
    }
    /* Build the real page first. The harness used to run INSTEAD of init(),
       which kept it honest about the pure core but blind to everything the
       page actually mounts -- the theme toggle, the comment editors, the
       reviewed toggles. Running init() here means a throw during mount is a
       reported failure rather than a silent one, and lets the checks below
       measure the live DOM. Everything after this point may assume it ran. */
    check("page initialises", function () { init(); });
    check("island parses", function () {
      const plan = core.parseIsland(document.getElementById("plan-data").textContent);
      if (!plan.meta || !plan.meta.id) throw new Error("no meta.id");
    });
    check("feedback round-trips", function () {
      const plan = core.parseIsland(document.getElementById("plan-data").textContent);
      const fp = document.body.getAttribute("data-plan-fingerprint");
      const fb = core.buildFeedback(plan, fp,
        [core.makeComment("task:t-session-store", "q", "needs work", true)]);
      const parsed = JSON.parse(fb.json);
      if (parsed.verdict !== "request_changes") throw new Error("verdict");
      if (parsed.comments[0].blocking !== true) throw new Error("blocking");
      if (parsed.plan_hash !== fp) throw new Error("hash");
      if (fb.combined.indexOf("## Plan feedback") !== 0) throw new Error("combined starts with mirror");
      if (fb.combined.indexOf("```json") === -1) throw new Error("combined carries the JSON block");
    });
    check("refs exist in dom", function () {
      if (!document.querySelector('[data-plan-ref="task:t-session-store"]'))
        throw new Error("missing data-plan-ref");
    });
    check("storage guarded", function () {
      /* Under an opaque origin localStorage ACCESS throws; the guards must
         swallow that and hand back an empty array, not break the page. */
      const drafts = loadDrafts("selftest-plan", "selftest-fp");
      if (!Array.isArray(drafts)) throw new Error("loadDrafts must return an array");
    });
    check("comment editor fills its box", function () {
      /* Regression: .comment-box was a block container, so the textarea sat
         at its intrinsic cols="20" (~205px) however wide the box was; and
         inside an acceptance <li> (a two-column grid) the box was placed in
         the 1.75rem counter column, ~28px wide. Both looked like styling
         nobody had finished. Assert every editor is at least most of its
         own box. */
      const boxes = document.querySelectorAll(".comment-box");
      if (!boxes.length) throw new Error("no comment editors mounted");
      /* Both invariants below are RELATIVE -- an editor is judged against the
         room its own container offers, never against a pixel count. An
         absolute floor here would really be an assertion about the viewport,
         and it duly failed in CI, where this page is framed in a narrow
         sandboxed iframe rather than a desktop window. */

      /* Most editors live inside a collapsed <details>, where nothing has
         layout at all. Open every phase for the measurement -- the acceptance
         grid is exactly where the worse of the two bugs was -- then put the
         page back the way it was found. */
      const wasOpen = [].map.call(document.querySelectorAll("details.phase"), function (d) {
        const o = d.open; d.open = true; return o;
      });
      let failure = null;
      boxes.forEach(function (box) {
        const wasHidden = box.hasAttribute("hidden");
        if (wasHidden) box.removeAttribute("hidden");
        const parent = box.parentElement;
        const pcs = window.getComputedStyle(parent);
        const avail = parent.clientWidth
          - (parseFloat(pcs.paddingLeft) || 0)
          - (parseFloat(pcs.paddingRight) || 0);
        const cap = parseFloat(window.getComputedStyle(box).maxWidth);
        const bw = box.getBoundingClientRect().width;
        const tw = box.querySelector("textarea").getBoundingClientRect().width;
        if (wasHidden) box.setAttribute("hidden", "");
        /* Nothing here is laid out (a zero-size frame): no claim to make. */
        if (avail < 1 || failure) return;
        /* (1) The box takes the width its container offers, up to its own
           max-width. Catches an editor placed into a narrow grid column --
           inside an acceptance criterion the box became an ordinary item in
           that row's two-column grid and rendered 28px wide. */
        const expected = isNaN(cap) ? avail : Math.min(avail, cap);
        if (bw < expected - 2) {
          failure = "editor " + Math.round(bw) + "px in a container offering "
            + Math.round(expected) + "px";
          return;
        }
        /* (2) The textarea fills the box. Catches the box being a block
           container, which left the textarea at its intrinsic cols="20". */
        if (tw < bw - 2) {
          failure = "textarea " + Math.round(tw) + "px inside a " + Math.round(bw) + "px box";
        }
      });
      document.querySelectorAll("details.phase").forEach(function (d, i) { d.open = wasOpen[i]; });
      if (failure) throw new Error(failure);
    });
    check("theme toggle offers system, light and dark", function () {
      const modes = [].map.call(
        document.querySelectorAll("[data-theme-set]"),
        function (b) { return b.getAttribute("data-theme-set"); }
      );
      if (modes.join("|") !== "|light|dark") throw new Error("modes were " + modes.join("|"));
      /* System must be reachable AGAIN after an override, or "follow the OS"
         is a state a reader can only ever leave. */
      applyTheme("dark", false);
      if (document.documentElement.getAttribute("data-theme") !== "dark") {
        throw new Error("dark did not apply");
      }
      applyTheme("", false);
      if (document.documentElement.getAttribute("data-theme")) {
        throw new Error("system did not clear the override");
      }
      if (document.querySelector('[data-theme-set=""]').getAttribute("aria-pressed") !== "true") {
        throw new Error("system not marked pressed");
      }
    });
    check("phases open and shut with motion", function () {
      const phase = document.querySelector("details.phase");
      if (!phase) throw new Error("no phases on the page");
      const body = phase.querySelector(".phase-body");
      if (!body) throw new Error("phase has no body to animate");
      /* No Web Animations API, or a reader who asked for reduced motion:
         instant IS the correct behaviour, and there would be nothing for
         `heightAnimations` to report. Nothing to assert. */
      if (!canDisclose() || !body.getAnimations) return;

      setPhaseOpen(phase, false, false);

      setPhaseOpen(phase, true, true);
      if (!phase.open) throw new Error("opening did not open the phase");
      if (phase.classList.contains("is-closing")) throw new Error("opening left is-closing set");
      const opening = heightAnimations(body);
      if (!opening.length) throw new Error("opening was not animated");
      /* It must start from collapsed, or the box would appear at full size
         and merely fade -- which is the jump, with a fade over it. */
      const first = opening[0].effect.getKeyframes()[0];
      if (parseFloat(first.height) !== 0 || parseFloat(first.paddingBottom) !== 0) {
        throw new Error("opening starts at height=" + first.height
          + " paddingBottom=" + first.paddingBottom + ", not from nothing");
      }
      opening.forEach(function (a) { a.finish(); });

      /* The three things that make a close look right, and each of which
         has to be got wrong deliberately to break: the body is still on the
         page so there is something to shrink, the open styling is ALREADY
         off so the chevron turns with the click rather than after it, and
         the height really is being animated. */
      setPhaseOpen(phase, false, true);
      if (!phase.open) throw new Error("closing dropped the body before it could shrink");
      if (!phase.classList.contains("is-closing")) throw new Error("closing left the open styling on");
      const closing = heightAnimations(body);
      if (!closing.length) throw new Error("closing was not animated");
      const last = closing[0].effect.getKeyframes().slice(-1)[0];
      if (parseFloat(last.height) !== 0 || parseFloat(last.paddingBottom) !== 0) {
        throw new Error("closing ends at height=" + last.height
          + " paddingBottom=" + last.paddingBottom + ", not at nothing");
      }

      /* And the instant path settles everything the animated one leaves in
         flight -- otherwise a print or a second click could strand a phase
         half-open, or leave its body permanently clipped. */
      setPhaseOpen(phase, false, false);
      if (phase.open) throw new Error("the instant path left the phase open");
      if (phase.classList.contains("is-closing")) throw new Error("the instant path left is-closing set");
      if (body.style.overflow) throw new Error("the instant path left the body clipped");
    });
    if (location.protocol !== "file:" || window.parent !== window) {
      /* Non-plain-file contexts only — a real http(s) serving, or the CI
         harness that frames this page in a sandboxed iframe to give it the
         same opaque origin the studio's sandbox-CSP header does. Prove the
         CSP wall is live and the copy flow terminates in a handled state
         (clipboard success OR the manual fallback rendered) — never a
         silent dead-end. */
      checkAsync("fetch blocked by CSP", new Promise(function (resolve) {
        try {
          fetch("/selftest-probe").then(function () { resolve(false); }, function () { resolve(true); });
        } catch (e) { resolve(true); }
      }));
      checkAsync("copy terminates handled", new Promise(function (resolve) {
        let succeeded = false;
        copyToClipboard("selftest-probe", function () { succeeded = true; resolve(true); });
        setTimeout(function () {
          if (!succeeded) resolve(!!document.getElementById("manual-copy"));
        }, 800);
      }));
    }
    function finish() {
      const failed = results.some(r => r.indexOf("FAIL") === 0);
      const marker = document.createElement("pre");
      marker.id = "selftest-result";
      marker.textContent =
        (failed ? "LOADOUT_SELFTEST_FAIL" : "LOADOUT_SELFTEST_PASS") + "\n" + results.join("\n");
      document.body.appendChild(marker);
      /* When framed (the CI harness), relay the verdict to the parent:
         a sandboxed iframe's DOM is invisible to --dump-dom (it only
         serializes the top document), but postMessage crosses the opaque-
         origin boundary by design. No-op in every top-level context. */
      if (window.parent !== window) {
        try { window.parent.postMessage("LOADOUT_SELFTEST_RELAY\n" + marker.textContent, "*"); }
        catch (e) { /* relay is test-harness sugar, never load-bearing */ }
      }
    }
    Promise.all(pending).then(finish, finish);
  }

  /* ---- DOM layer -------------------------------------------------- */

  const BANNER_TEXT = "comments live in this page — copy feedback before closing";

  const SVG_NS = "http://www.w3.org/2000/svg";

  /* A small stroke-based icon built via createElementNS -- never innerHTML,
     so the markup can't smuggle anything through it -- `paths` is a list of
     `d` attribute strings, one <path> per entry. */
  function svgIcon(className, paths) {
    const svg = document.createElementNS(SVG_NS, "svg");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("width", "16");
    svg.setAttribute("height", "16");
    svg.setAttribute("fill", "none");
    svg.setAttribute("stroke", "currentColor");
    svg.setAttribute("stroke-width", "2");
    svg.setAttribute("stroke-linecap", "round");
    svg.setAttribute("stroke-linejoin", "round");
    svg.setAttribute("aria-hidden", "true");
    svg.setAttribute("focusable", "false");
    svg.setAttribute("class", className);
    paths.forEach(function (d) {
      const path = document.createElementNS(SVG_NS, "path");
      path.setAttribute("d", d);
      svg.appendChild(path);
    });
    return svg;
  }

  /* Speech-bubble icon for the comment button: bubble outline plus two
     short lines standing in for text. */
  function commentIcon() {
    return svgIcon("comment-btn-icon", [
      "M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z",
      "M7 8h10M7 12h6",
    ]);
  }

  /* Warning-triangle icon for the "Blocks approval" checkbox: triangle
     outline plus an exclamation mark (stem + dot as one path). */
  function warningIcon() {
    return svgIcon("blocking-icon", [
      "M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z",
      "M12 9v4M12 17h.01",
    ]);
  }

  /* ---- theme --------------------------------------------------------

     Three states, and System is the default: with no explicit choice the
     page carries no data-theme attribute at all, so plan.css's
     prefers-color-scheme block decides and the page simply IS whatever the
     OS is set to -- including when the OS flips while the page is open
     (see the matchMedia listener below; the colours change on their own,
     only the toggle's highlight needs telling).

     Light and Dark record an override on <html data-theme>, which plan.css
     ranks above its prefers-color-scheme block in both directions. System
     is not the absence of a choice in the UI -- it is a choice a reader can
     come BACK to, which is the whole reason it is a button rather than just
     the initial state.

     The choice is per-plan-viewer, not per-plan, so it lives under one
     fixed key and carries across every rendered plan a reader opens.

     Storage is best-effort throughout: a file:// document in some browsers
     has an opaque origin where localStorage throws on access, and a theme
     toggle is not worth breaking the page over. */
  const THEME_KEY = "loadout-plan:theme";

  /* The explicit override, or "" for System. Note the return is the stored
     MODE, not the colour being shown -- with System selected those differ,
     and it is the mode the toggle highlights. */
  function storedTheme() {
    try {
      const v = window.localStorage.getItem(THEME_KEY);
      return v === "light" || v === "dark" ? v : "";
    } catch (e) { return ""; }
  }

  function darkMedia() {
    return window.matchMedia ? window.matchMedia("(prefers-color-scheme: dark)") : null;
  }

  function applyTheme(mode, animate) {
    const root = document.documentElement;
    /* Colour transitions are gated on a class so first paint lands on the
       final colours instantly -- without the gate the page would visibly
       fade in from whatever the previous theme was. */
    if (animate) {
      root.classList.add("theme-anim");
      window.setTimeout(function () { root.classList.remove("theme-anim"); }, 260);
    }
    if (mode) {
      root.setAttribute("data-theme", mode);
      try { window.localStorage.setItem(THEME_KEY, mode); } catch (e) { /* see above */ }
    } else {
      root.removeAttribute("data-theme");
      try { window.localStorage.removeItem(THEME_KEY); } catch (e) { /* see above */ }
    }
    syncToggle();
  }

  /* Highlight the button for the selected MODE (System included), and tell
     the System button which way it currently resolves -- that is the one
     thing a reader cannot otherwise read off the control. */
  function syncToggle() {
    const mode = document.documentElement.getAttribute("data-theme") || "";
    document.querySelectorAll("[data-theme-set]").forEach(function (b) {
      b.setAttribute("aria-pressed", String(b.getAttribute("data-theme-set") === mode));
    });
    const sys = document.querySelector('[data-theme-set=""]');
    if (sys) sys.setAttribute("title", "Follow the system setting (currently " + current() + ")");
  }

  /* What the page is actually showing right now -- the explicit override if
     there is one, else whatever the OS is asking for. */
  function current() {
    const explicit = document.documentElement.getAttribute("data-theme");
    if (explicit) return explicit;
    const mq = darkMedia();
    return mq && mq.matches ? "dark" : "light";
  }

  function mountThemeToggle() {
    const host = document.querySelector(".pv-topbar-right");
    if (!host) return;
    const group = document.createElement("div");
    group.className = "pv-theme";
    group.setAttribute("role", "group");
    group.setAttribute("aria-label", "Colour theme");
    [["", "System"], ["light", "Light"], ["dark", "Dark"]].forEach(function (pair) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.setAttribute("data-theme-set", pair[0]);
      btn.textContent = pair[1];
      btn.addEventListener("click", function () { applyTheme(pair[0], true); });
      group.appendChild(btn);
    });
    host.appendChild(group);
    applyTheme(storedTheme(), false);

    /* Follow the OS live while System is selected. The CSS repaints itself
       -- this listener exists only so the System button's tooltip and the
       highlight stay truthful, and so the change reads as deliberate rather
       than as the page flickering. */
    const mq = darkMedia();
    if (mq && mq.addEventListener) {
      mq.addEventListener("change", function () {
        if (document.documentElement.getAttribute("data-theme")) return;
        const root = document.documentElement;
        root.classList.add("theme-anim");
        window.setTimeout(function () { root.classList.remove("theme-anim"); }, 260);
        syncToggle();
      });
    }
  }

  /* ---- reading chrome -----------------------------------------------

     Two position cues, both driven by IntersectionObserver rather than a
     scroll handler so neither costs anything per frame:

       1. the topbar lifts off the page once it is no longer at the top,
       2. the phase ledger marks the row for the phase currently on screen.

     Both are decoration over information the page already carries, so a
     browser without IntersectionObserver simply gets neither. */
  function mountScrollCues() {
    const bar = document.querySelector(".pv-topbar");
    if (!bar || !("IntersectionObserver" in window)) return;

    /* A zero-height probe above the topbar: once it scrolls out of view the
       bar is stuck. Cheaper and steadier than reading scrollY. */
    const probe = document.createElement("div");
    probe.setAttribute("aria-hidden", "true");
    probe.style.cssText = "position:absolute;top:0;height:1px;width:1px;";
    bar.parentNode.insertBefore(probe, bar);
    new IntersectionObserver(function (entries) {
      bar.classList.toggle("is-stuck", !entries[0].isIntersecting);
    }).observe(probe);

    const phases = document.querySelectorAll("details.phase");
    if (!phases.length) return;
    /* Fire when a phase crosses the upper third of the viewport: the row
       highlights for the phase a reader is reading, not the one that
       happens to be tallest on screen. */
    const spy = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (entry) {
          if (!entry.isIntersecting) return;
          const id = entry.target.getAttribute("data-plan-ref");
          if (!id) return;
          const phaseId = id.slice("phase:".length);
          document.querySelectorAll("[data-phase-row]").forEach(function (row) {
            row.classList.toggle("is-current", row.getAttribute("data-phase-row") === phaseId);
          });
        });
      },
      { rootMargin: "-10% 0px -70% 0px" }
    );
    phases.forEach(function (p) { spy.observe(p); });
  }

  /* ---- disclosure ----------------------------------------------------

     A <details> element has no motion of its own: its body appears and
     disappears between one frame and the next, so opening a phase shoves
     everything below it down the page in a single jump, and shutting one
     yanks it back. `setPhaseOpen` gives that change a duration by animating
     the body's height, and every path a READER can trigger goes through it
     -- the summary itself, the expand-all and collapse-all buttons, and a
     comment button that has to open the phase it lives on.

     Programmatic paths deliberately do not. Printing and the self-test pass
     `false` for `animate` and stay instant: they want the end state, not a
     transition to it.

     Nothing here is load-bearing. A browser with no Web Animations API, or
     a reader who has asked their system for reduced motion, falls straight
     through to setting `.open` -- which is exactly the behaviour this page
     had before. */

  /* The animation currently running on a phase, if any, so a second click
     part-way through can take the phase over rather than fight it. */
  const disclosing = new WeakMap();

  /* Duration and easing for a disclosure, read out of plan.css. A value that
     is missing (no stylesheet) or unparseable falls back to a sane pair
     rather than to a zero-length -- and therefore invisible -- animation. */
  function discloseTiming() {
    const root = window.getComputedStyle(document.documentElement);
    const raw = root.getPropertyValue("--t-disclose").trim();
    const ms = raw.slice(-2) === "ms" ? parseFloat(raw) : parseFloat(raw) * 1000;
    return {
      duration: ms > 0 ? ms : 240,
      easing: root.getPropertyValue("--ease").trim() || "ease",
    };
  }

  /* Whether to animate at all. plan.css's prefers-reduced-motion block can
     only reach CSS animations and transitions, so motion driven from here
     has to consult the same query itself or it would ignore the one setting
     the whole budget is meant to answer to. */
  function canDisclose() {
    if (!window.Element || typeof Element.prototype.animate !== "function") return false;
    if (!window.matchMedia) return true;
    return !window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  }

  /* Whether a phase reads as SHUT to the person looking at it: either really
     closed, or open-but-mid-close. A click during a close has to re-open. */
  function phaseIsShut(details) {
    return !details.open || details.classList.contains("is-closing");
  }

  function bottomPadding(el) {
    return window.getComputedStyle(el).paddingBottom || "0px";
  }

  /* The disclosure animations on an element, and only those. `getAnimations`
     also returns anything CSS has left lying around with a fill, so a bare
     length check would pass whether or not this file animated anything --
     the keyframes are what identify ours. */
  function heightAnimations(el) {
    if (!el.getAnimations) return [];
    return el.getAnimations().filter(function (a) {
      const frames = a.effect && a.effect.getKeyframes ? a.effect.getKeyframes() : [];
      return frames.some(function (f) { return f.height !== undefined; });
    });
  }

  /* Open or shut one phase.

     Opening is the straightforward direction: set `open` (so the chevron,
     the numeral and every other [open] rule in plan.css start their own
     transitions on the same frame), then grow the body from nothing into
     its natural height.

     Closing cannot do that in reverse, because clearing `open` removes the
     body from the page immediately and leaves nothing to shrink. So `open`
     stays set until the animation finishes, and the `is-closing` class tells
     plan.css to treat the phase as visually shut in the meantime -- see the
     `:not(.is-closing)` rules on the summary. */
  function setPhaseOpen(details, want, animate) {
    const body = details.querySelector(".phase-body");
    const running = disclosing.get(details);

    if (!body || animate === false || !canDisclose()) {
      if (running) { disclosing.delete(details); running.cancel(); }
      if (body) body.style.overflow = "";
      details.classList.remove("is-closing");
      details.open = want;
      return;
    }

    /* Clip first, measure second. `overflow: hidden` makes the body a block
       formatting context, so its children's margins stop collapsing through
       it -- measuring before setting it would return a height the box never
       actually has while animating, and the phase would jump at the end by
       the difference. */
    body.style.overflow = "hidden";
    /* Where the box is NOW. Mid-animation these read the animated values, so
       a click part-way through a close carries on from where it got to
       instead of restarting. Note the state is taken from `open`, never from
       the measurement: a shut <details> keeps reporting the size its body
       had when it was last open. */
    const from = details.open
      ? { h: body.getBoundingClientRect().height, pad: bottomPadding(body) }
      : { h: 0, pad: "0px" };
    if (running) { disclosing.delete(details); running.cancel(); }

    if (want) {
      details.classList.remove("is-closing");
      details.open = true;
    } else {
      details.classList.add("is-closing");
    }
    /* And where it is going. Measured after the cancel above, so these are
       the element's own resting values, not another animation's. */
    const to = want
      ? { h: body.getBoundingClientRect().height, pad: bottomPadding(body) }
      : { h: 0, pad: "0px" };

    const timing = discloseTiming();
    const anim = body.animate(
      /* Padding travels with the height. The body carries 2.25rem of bottom
         padding and `box-sizing: border-box`, so a height of 0 alone still
         leaves a 36px stub on screen -- the last thing a close did was drop
         it, which is precisely the jump this is here to remove. Opacity
         travels with them so the content fades rather than being sliced off
         by the clip edge. */
      [
        { height: from.h + "px", paddingBottom: from.pad, opacity: want ? 0 : 1 },
        { height: to.h + "px", paddingBottom: to.pad, opacity: want ? 1 : 0 },
      ],
      {
        duration: timing.duration,
        easing: timing.easing,
        /* A close holds its last frame. Without the fill the body would
           spring back to full height for the one frame between the
           animation ending and `open` being cleared below. */
        fill: want ? "none" : "forwards",
      }
    );
    disclosing.set(details, anim);
    anim.onfinish = function () {
      /* A later call already took this phase over; the cleanup is its job
         now. (Cancelling does not fire this handler, so reaching here with
         a stale animation means the phase was re-clicked mid-flight.) */
      if (disclosing.get(details) !== anim) return;
      disclosing.delete(details);
      if (!want) {
        details.open = false;
        details.classList.remove("is-closing");
      }
      body.style.overflow = "";
      anim.cancel();
    };
  }

  /* Where a commentable block wants its comment button and its draft box.
     Every block type on the page pairs a heading row (where the button
     belongs, on the title's line) with a body column (where the box
     belongs, under the text it is about). Returning nulls is fine: the
     caller falls back to the block itself. */
  function commentSlots(el) {
    if (el.classList.contains("task")) {
      return { btn: el.querySelector(".task-head"), box: el.querySelector(".task-body") };
    }
    if (el.classList.contains("pv-row")) {
      const body = el.querySelector(".pv-row-body");
      return { btn: el.querySelector(".pv-row-head") || body, box: body };
    }
    if (el.classList.contains("plan-summary")) {
      const exec = el.querySelector(".summary-exec");
      return { btn: exec, box: exec };
    }
    if (el.tagName === "DETAILS") {
      return {
        btn: el.querySelector("summary .phase-head-line"),
        box: el.querySelector(".phase-body"),
      };
    }
    return { btn: null, box: null };
  }

  /* First 80 chars of the element's heading text: the element itself when
     it is a heading, else the first h1–h6 descendant, else its own
     trimmed text as a last-resort fallback. */
  function elementQuote(el) {
    let source = el;
    if (!/^h[1-6]$/i.test(el.tagName)) {
      source = el.querySelector("h1, h2, h3, h4, h5, h6") || el;
    }
    const text = (source.textContent || "").trim().replace(/\s+/g, " ");
    return text.slice(0, 80);
  }

  function draftKey(planId, fingerprint) {
    return "loadout-plan:" + planId + ":" + fingerprint;
  }

  function loadDrafts(planId, fingerprint) {
    try {
      const raw = window.localStorage.getItem(draftKey(planId, fingerprint));
      if (!raw) return [];
      const stored = JSON.parse(raw);
      if (!stored || stored.fingerprint !== fingerprint || !Array.isArray(stored.comments)) {
        return [];
      }
      /* Old draft shape carried a `type` field (blocker/question/suggestion/
         change_request) instead of a `blocking` boolean. Restoring one of
         those as-is would silently resurrect the retired taxonomy, so
         discard the whole draft rather than partially restore it broken --
         the fingerprint gate above already covers the "plan changed"
         case; this covers "the draft's own shape changed". */
      const hasOldShape = stored.comments.some(function (c) {
        return c && Object.prototype.hasOwnProperty.call(c, "type");
      });
      if (hasOldShape) return [];
      return stored.comments;
    } catch (e) {
      return [];
    }
  }

  function saveDrafts(planId, fingerprint, comments) {
    try {
      window.localStorage.setItem(
        draftKey(planId, fingerprint),
        JSON.stringify({ fingerprint: fingerprint, comments: comments })
      );
    } catch (e) {
      /* best-effort only — quota errors, disabled storage, file:// origin, etc. */
    }
  }

  function reviewedKey(planId, fingerprint) {
    return "loadout-plan-reviewed:" + planId + ":" + fingerprint;
  }

  function loadReviewed(planId, fingerprint) {
    try {
      const raw = window.localStorage.getItem(reviewedKey(planId, fingerprint));
      if (!raw) return [];
      const stored = JSON.parse(raw);
      if (!stored || stored.fingerprint !== fingerprint || !Array.isArray(stored.refs)) {
        return [];
      }
      return stored.refs;
    } catch (e) {
      return [];
    }
  }

  function saveReviewed(planId, fingerprint, refs) {
    try {
      window.localStorage.setItem(
        reviewedKey(planId, fingerprint),
        JSON.stringify({ fingerprint: fingerprint, refs: refs })
      );
    } catch (e) {
      /* best-effort only — same caveats as saveDrafts above */
    }
  }

  function copyToClipboard(text, done) {
    function fallback() {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.setAttribute("readonly", "");
      ta.style.position = "fixed";
      ta.style.top = "-1000px";
      ta.style.left = "-1000px";
      document.body.appendChild(ta);
      ta.focus();
      ta.select();
      let copied = false;
      try { copied = document.execCommand("copy"); } catch (e) { /* ignore */ }
      document.body.removeChild(ta);
      if (copied) done(); else showManualCopy(text);
    }
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, fallback);
    } else {
      fallback();
    }
  }

  /* Terminal fallback: when BOTH clipboard paths fail (e.g. an opaque-origin
     sandboxed document with no clipboard permission), surface the payload for
     a manual Cmd/Ctrl-C — the paste-back loop must never dead-end silently. */
  function showManualCopy(text) {
    let panel = document.getElementById("manual-copy");
    if (panel) {
      panel.querySelector("textarea").value = text;
    } else {
      panel = document.createElement("div");
      panel.id = "manual-copy";
      const hint = document.createElement("p");
      hint.textContent =
        "Automatic copy is blocked here — select the text below and copy it manually.";
      const ta = document.createElement("textarea");
      ta.setAttribute("readonly", "");
      ta.value = text;
      const close = document.createElement("button");
      close.type = "button";
      close.textContent = "Close";
      close.addEventListener("click", function () { panel.remove(); });
      panel.appendChild(hint);
      panel.appendChild(ta);
      panel.appendChild(close);
      document.body.appendChild(panel);
    }
    const ta = panel.querySelector("textarea");
    ta.focus();
    ta.select();
  }

  function init() {
    /* Chrome first, and outside the island guard: the theme toggle and the
       scroll cues are properties of the page, not of the plan data, so a
       document whose island failed to parse still gets a usable shell. */
    mountThemeToggle();
    mountScrollCues();

    const islandEl = document.getElementById("plan-data");
    if (!islandEl) return;
    const plan = core.parseIsland(islandEl.textContent);
    const fingerprint = document.body.getAttribute("data-plan-fingerprint") || "";

    let comments = loadDrafts(plan.meta.id, fingerprint);
    let restoredCount = comments.length;

    function persist() {
      saveDrafts(plan.meta.id, fingerprint, comments);
    }

    /* ---- feedback bar ---- */
    const bar = document.createElement("div");
    bar.className = "feedback-bar";

    const banner = document.createElement("span");
    banner.className = "feedback-bar-banner";
    banner.textContent = BANNER_TEXT;
    bar.appendChild(banner);

    if (restoredCount > 0) {
      const restoredNote = document.createElement("span");
      restoredNote.className = "feedback-bar-restored";
      restoredNote.textContent = "restored " + restoredCount + " draft comments";
      bar.appendChild(restoredNote);
    }

    /* Blocking comments get their own readout ahead of the total: the
       difference between "4 comments" and "4 comments, one of which blocks
       approval" is the whole verdict the feedback document will carry. */
    const blockingCount = document.createElement("span");
    blockingCount.className = "feedback-bar-blocking";
    bar.appendChild(blockingCount);

    const count = document.createElement("span");
    count.className = "feedback-bar-count";
    bar.appendChild(count);

    const reviewedCount = document.createElement("span");
    reviewedCount.className = "feedback-bar-reviewed";
    bar.appendChild(reviewedCount);

    const copyBtn = document.createElement("button");
    copyBtn.type = "button";
    copyBtn.className = "feedback-bar-copy";
    copyBtn.textContent = "Copy feedback";
    bar.appendChild(copyBtn);

    document.body.appendChild(bar);

    function renderCount() {
      count.textContent = comments.length + (comments.length === 1 ? " comment" : " comments");
      const blocking = comments.filter(function (c) { return c.blocking; }).length;
      blockingCount.textContent = blocking > 0 ? blocking + " blocking" : "";
      /* Nothing to copy until something has been added. */
      copyBtn.disabled = comments.length === 0;
      copyBtn.title = comments.length === 0 ? "Add a comment or answer first" : "";
    }
    renderCount();

    copyBtn.addEventListener("click", function () {
      const feedback = core.buildFeedback(plan, fingerprint, comments);
      copyToClipboard(feedback.combined, function () {
        const original = "Copy feedback";
        copyBtn.textContent = "Copied ✓";
        /* One short pulse: the label change alone is easy to miss on a
           button the reader is still looking at when it fires. */
        copyBtn.classList.add("is-copied");
        window.setTimeout(function () {
          copyBtn.textContent = original;
          copyBtn.classList.remove("is-copied");
        }, 2000);
      });
    });

    /* ---- per-element comment buttons ---- */
    const refEls = document.querySelectorAll("[data-plan-ref]");
    refEls.forEach(function (el) {
      const ref = el.getAttribute("data-plan-ref");
      /* Snapshot the quote before any UI (comment button/box) is appended
         into el -- elementQuote's no-heading fallback reads el.textContent,
         which would otherwise pick up the injected chrome text. */
      const quote = elementQuote(el);

      /* The CTA names the action the element actually invites: an open
         question wants an answer, everything else wants a comment. Both
         produce the same feedback-contract comment — only the label (and
         placeholder) differ. */
      const kind = ref.split(":")[0];
      const isQuestion = kind === "question";
      const label = isQuestion ? "Answer" : "Comment";
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "comment-btn";
      btn.appendChild(commentIcon());
      /* The label rides in a span so CSS can collapse the button to
         icon-only where a full button doesn't fit (the per-criterion
         line anchors); title + aria-label keep the name either way. */
      const labelSpan = document.createElement("span");
      labelSpan.className = "comment-btn-label";
      labelSpan.textContent = label;
      btn.appendChild(labelSpan);
      btn.title = label;
      btn.setAttribute("aria-label", isQuestion ? "Answer this question" : "Add comment");

      const box = document.createElement("div");
      box.className = "comment-box";
      box.hidden = true;

      const textarea = document.createElement("textarea");
      textarea.placeholder = isQuestion ? "Answer…" : "Add a comment…";
      textarea.rows = 3;

      const blockingRow = document.createElement("label");
      blockingRow.className = "comment-box-blocking";

      const blockingBox = document.createElement("input");
      blockingBox.type = "checkbox";

      blockingRow.appendChild(blockingBox);
      blockingRow.appendChild(warningIcon());
      blockingRow.appendChild(document.createTextNode("Blocks approval"));

      const actions = document.createElement("div");
      actions.className = "comment-box-actions";

      const addBtn = document.createElement("button");
      addBtn.type = "button";
      addBtn.textContent = "Add";

      const cancelBtn = document.createElement("button");
      cancelBtn.type = "button";
      cancelBtn.textContent = "Cancel";

      actions.appendChild(addBtn);
      actions.appendChild(cancelBtn);
      box.appendChild(textarea);
      box.appendChild(blockingRow);
      box.appendChild(actions);

      btn.addEventListener("click", function (e) {
        /* The same guard the reviewed toggle carries below, for the same
           reason: on a PHASE this button is placed inside the <summary>
           (see commentSlots), and a click that reaches the summary triggers
           its toggle. Commenting on an open phase used to shut it, hiding
           the very box the click had just opened. */
        e.stopPropagation();
        box.hidden = !box.hidden;
        if (box.hidden) return;
        if (el.tagName === "DETAILS" && phaseIsShut(el)) {
          /* The button is in the summary but the box is in the body, which
             only exists while the phase is open -- so open it. Focus without
             scrolling: the body is mid-animation and clipped, and scrolling
             into it now would leave the clip box scrolled once it settles. */
          setPhaseOpen(el, true, true);
          textarea.focus({ preventScroll: true });
          return;
        }
        textarea.focus();
      });

      cancelBtn.addEventListener("click", function () {
        textarea.value = "";
        blockingBox.checked = false;
        box.hidden = true;
      });

      addBtn.addEventListener("click", function () {
        const text = textarea.value.trim();
        if (!text) return;
        comments.push(core.makeComment(ref, quote, text, blockingBox.checked));
        textarea.value = "";
        blockingBox.checked = false;
        box.hidden = true;
        renderCount();
        persist();
      });

      /* Each commentable block is a grid or a flex column with named
         slots, so the button and the box go into the slots rather than
         being appended to the block itself -- appending to a .task would
         make them extra grid items and blow the two-column layout apart.
         Unknown shapes fall back to the block itself, which is always
         valid markup even if the placement is plain. */
      const slots = commentSlots(el);
      (slots.btn || el).appendChild(btn);
      (slots.box || el).appendChild(box);
    });

    /* ---- expand/collapse all ---- */
    const firstPhase = document.querySelector("details.phase");
    if (firstPhase) {
      function collapsibles() {
        return document.querySelectorAll("details.phase");
      }

      /* A <summary>'s built-in default action is "toggle, instantly". Take
         it over so a click gets the animation instead. Keyboard activation
         (Enter or Space on the focused summary) dispatches a click of its
         own, so it comes through the same handler and behaves the same. */
      collapsibles().forEach(function (d) {
        const summary = d.querySelector("summary");
        if (!summary) return;
        summary.addEventListener("click", function (e) {
          if (e.defaultPrevented) return;
          e.preventDefault();
          setPhaseOpen(d, phaseIsShut(d), true);
        });
      });

      /* The markup ships an empty actions slot on the Phases section rule
         (render.rs's `section_rule`), so these land on the divider rather
         than floating above the list as a stray toolbar. */
      const ctl = document.getElementById("phases-actions") || document.createElement("div");

      const expandBtn = document.createElement("button");
      expandBtn.type = "button";
      expandBtn.className = "pv-textbtn";
      expandBtn.textContent = "expand all";
      expandBtn.addEventListener("click", function () {
        collapsibles().forEach(function (d) { setPhaseOpen(d, true, true); });
      });

      const collapseBtn = document.createElement("button");
      collapseBtn.type = "button";
      collapseBtn.className = "pv-textbtn";
      collapseBtn.textContent = "collapse all";
      collapseBtn.addEventListener("click", function () {
        collapsibles().forEach(function (d) { setPhaseOpen(d, false, true); });
      });

      ctl.appendChild(expandBtn);
      ctl.appendChild(collapseBtn);
      if (!ctl.isConnected) firstPhase.parentNode.insertBefore(ctl, firstPhase);

      /* Printing (or a reader stepping through word-by-word with find-in-
         page) needs every phase/graph visible -- expand everything just
         before print, then restore whatever state the user had before. */
      let preprintState = null;
      window.addEventListener("beforeprint", function () {
        /* `phaseIsShut`, not `.open`: a phase caught mid-close is still
           technically open, and restoring it as open afterwards would leave
           the reader with a phase they had just clicked shut. */
        preprintState = Array.from(collapsibles()).map(function (d) { return !phaseIsShut(d); });
        collapsibles().forEach(function (d) { setPhaseOpen(d, true, false); });
      });
      window.addEventListener("afterprint", function () {
        if (!preprintState) return;
        collapsibles().forEach(function (d, i) { setPhaseOpen(d, preprintState[i], false); });
        preprintState = null;
      });
    }

    /* ---- reviewed-state checkboxes ---- */
    const reviewed = new Set(loadReviewed(plan.meta.id, fingerprint));
    function persistReviewed() {
      saveReviewed(plan.meta.id, fingerprint, Array.from(reviewed));
    }

    /* Only real task cards (data-plan-ref="task:…") count toward the K/N
       ratio in the feedback bar. Phases also get a reviewed checkbox (on
       their summary line) so a reviewer can mark a whole phase read at a
       glance, but a phase isn't a task, so folding it into the same
       denominator would mix units and complicate the arithmetic — N stays
       exactly "how many tasks", full stop. */
    const taskEls = document.querySelectorAll('.task[data-plan-ref^="task:"]');

    function renderReviewedCount() {
      let k = 0;
      taskEls.forEach(function (el) {
        if (reviewed.has(el.getAttribute("data-plan-ref"))) k++;
      });
      reviewedCount.textContent = k + "/" + taskEls.length + " reviewed";
    }

    function addReviewedBox(container, ref) {
      /* A bare checkbox read as decoration at first glance (first-dogfood
         feedback) -- the visible label says what checking it does, and
         flips to a past-tense confirmation once checked. */
      const label = document.createElement("label");
      label.className = "reviewed-toggle";
      const box = document.createElement("input");
      box.type = "checkbox";
      box.className = "reviewed-box";
      const text = document.createElement("span");
      text.className = "reviewed-toggle-text";
      function sync() {
        text.textContent = box.checked ? "Reviewed" : "Mark reviewed";
        container.classList.toggle("is-reviewed", box.checked);
      }
      box.checked = reviewed.has(ref);
      sync();
      label.appendChild(box);
      label.appendChild(text);
      label.addEventListener("click", function (e) {
        /* A control nested inside a <summary> still bubbles its click up
           to the <summary>'s default action (toggling the parent
           <details> open/closed) unless stopped here -- marking reviewed
           should not also collapse or expand the phase. On the label, so
           it covers clicks on the text as well as the box. */
        e.stopPropagation();
      });
      box.addEventListener("change", function () {
        if (box.checked) {
          reviewed.add(ref);
        } else {
          reviewed.delete(ref);
        }
        sync();
        persistReviewed();
        renderReviewedCount();
      });
      return label;
    }

    /* Both toggles land on their block's heading ROW (.phase-head-line,
       .task-head) rather than inside the heading element itself: the row is
       already a baseline-aligned flex line built to carry the title plus its
       badges, so a control added to it lines up with them for free. */
    document.querySelectorAll("details.phase").forEach(function (details) {
      const ref = details.getAttribute("data-plan-ref");
      const line = details.querySelector("summary .phase-head-line");
      if (!ref || !line) return;
      line.appendChild(addReviewedBox(details, ref));
    });

    taskEls.forEach(function (el) {
      const ref = el.getAttribute("data-plan-ref");
      if (!ref) return;
      const line = el.querySelector(".task-head") || el;
      line.appendChild(addReviewedBox(el, ref));
    });

    renderReviewedCount();
  }

  function run() {
    /* The window.name trigger exists for the CI harness, which embeds this
       page via a sandboxed iframe's srcdoc (an about:srcdoc document has no
       URL fragment to carry #selftest). Inert otherwise: the selftest only
       appends a result marker. */
    if (location.hash === "#selftest" || window.name === "loadout-selftest") selftest(); else init();
  }
  if (document.readyState !== "loading") {
    run();
  } else {
    document.addEventListener("DOMContentLoaded", run);
  }
})();
