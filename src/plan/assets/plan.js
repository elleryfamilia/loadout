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
        window.setTimeout(function () { copyBtn.textContent = original; }, 2000);
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

      btn.addEventListener("click", function () {
        box.hidden = !box.hidden;
        if (!box.hidden) textarea.focus();
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

      /* Append rather than chase the heading's exact DOM position: a
         heading found by querySelector may be nested several levels
         below el (e.g. inside a <summary>), and el.insertBefore requires
         a direct child as the reference node. Appending is always valid
         regardless of el's internal structure; plan.css positions
         .comment-btn absolutely so it still reads as "next to" the
         heading visually. */
      el.appendChild(btn);
      el.appendChild(box);
    });

    /* ---- expand/collapse all ---- */
    const firstPhase = document.querySelector("details.phase");
    if (firstPhase) {
      function collapsibles() {
        return document.querySelectorAll("details.phase, details.graph");
      }

      const ctl = document.createElement("div");
      ctl.className = "expand-ctl";

      const expandBtn = document.createElement("button");
      expandBtn.type = "button";
      expandBtn.textContent = "Expand all";
      expandBtn.addEventListener("click", function () {
        collapsibles().forEach(function (d) { d.open = true; });
      });

      const collapseBtn = document.createElement("button");
      collapseBtn.type = "button";
      collapseBtn.textContent = "Collapse all";
      collapseBtn.addEventListener("click", function () {
        collapsibles().forEach(function (d) { d.open = false; });
      });

      ctl.appendChild(expandBtn);
      ctl.appendChild(collapseBtn);
      firstPhase.parentNode.insertBefore(ctl, firstPhase);

      /* Printing (or a reader stepping through word-by-word with find-in-
         page) needs every phase/graph visible -- expand everything just
         before print, then restore whatever state the user had before. */
      let preprintState = null;
      window.addEventListener("beforeprint", function () {
        preprintState = Array.from(collapsibles()).map(function (d) { return d.open; });
        collapsibles().forEach(function (d) { d.open = true; });
      });
      window.addEventListener("afterprint", function () {
        if (!preprintState) return;
        collapsibles().forEach(function (d, i) { d.open = preprintState[i]; });
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

    document.querySelectorAll("details.phase").forEach(function (details) {
      const ref = details.getAttribute("data-plan-ref");
      const heading = details.querySelector("summary h2");
      if (!ref || !heading) return;
      /* Before the teaser (a block-level span), so the toggle stays on the
         title line instead of wrapping under the description. */
      const teaser = heading.querySelector(".phase-teaser");
      const toggle = addReviewedBox(details, ref);
      if (teaser) {
        heading.insertBefore(toggle, teaser);
      } else {
        heading.appendChild(toggle);
      }
    });

    taskEls.forEach(function (el) {
      const ref = el.getAttribute("data-plan-ref");
      if (!ref) return;
      const heading = el.querySelector("h3") || el;
      heading.appendChild(addReviewedBox(el, ref));
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
