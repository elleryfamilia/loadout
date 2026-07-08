/* loadout plan viewer — progressive enhancement over server-rendered HTML.
   Pure core first (no DOM), DOM layer second, #selftest harness last. */
(function () {
  "use strict";

  const core = {
    parseIsland(text) { return JSON.parse(text); },
    makeComment(ref, type, quote, text) {
      return { ref: ref, type: type, quote: quote || null, text: text };
    },
    buildFeedback(plan, fingerprint, comments) {
      const doc = {
        format: "loadout.plan-feedback/1",
        plan_id: plan.meta.id,
        plan_hash: fingerprint,
        verdict: comments.some(c => c.type === "blocker") ? "request_changes" : "comment",
        comments: comments.map((c, i) => ({
          id: "c-" + (i + 1),
          ref: c.ref, type: c.type, quote: c.quote, text: c.text,
        })),
      };
      const lines = ["## Plan feedback — " + plan.meta.id, ""];
      for (const c of doc.comments) {
        lines.push("### " + c.ref + " — " + c.type);
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
      return { json: json, markdown: markdown,
               combined: "```json\n" + json + "\n```\n\n" + markdown };
    },
  };
  window.loadoutPlan = core;

  function selftest() {
    const results = [];
    function check(name, fn) {
      try { fn(); results.push("PASS " + name); }
      catch (e) { results.push("FAIL " + name + ": " + e.message); }
    }
    check("island parses", function () {
      const plan = core.parseIsland(document.getElementById("plan-data").textContent);
      if (!plan.meta || !plan.meta.id) throw new Error("no meta.id");
    });
    check("feedback round-trips", function () {
      const plan = core.parseIsland(document.getElementById("plan-data").textContent);
      const fp = document.body.getAttribute("data-plan-fingerprint");
      const fb = core.buildFeedback(plan, fp,
        [core.makeComment("task:t-session-store", "blocker", "q", "needs work")]);
      const parsed = JSON.parse(fb.json);
      if (parsed.verdict !== "request_changes") throw new Error("verdict");
      if (parsed.plan_hash !== fp) throw new Error("hash");
      if (fb.combined.indexOf("```json") !== 0) throw new Error("combined shape");
    });
    check("refs exist in dom", function () {
      if (!document.querySelector('[data-plan-ref="task:t-session-store"]'))
        throw new Error("missing data-plan-ref");
    });
    const failed = results.some(r => r.indexOf("FAIL") === 0);
    const marker = document.createElement("pre");
    marker.id = "selftest-result";
    marker.textContent =
      (failed ? "LOADOUT_SELFTEST_FAIL" : "LOADOUT_SELFTEST_PASS") + "\n" + results.join("\n");
    document.body.appendChild(marker);
  }

  /* ---- DOM layer -------------------------------------------------- */

  const COMMENT_TYPES = ["blocker", "question", "suggestion", "change_request"];
  const BANNER_TEXT = "comments live in this page — copy feedback before closing";

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
      if (copied) done();
    }
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, fallback);
    } else {
      fallback();
    }
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

      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "comment-btn";
      btn.textContent = "💬";
      btn.setAttribute("aria-label", "Add comment");

      const box = document.createElement("div");
      box.className = "comment-box";
      box.hidden = true;

      const select = document.createElement("select");
      COMMENT_TYPES.forEach(function (type) {
        const opt = document.createElement("option");
        opt.value = type;
        opt.textContent = type.replace("_", " ");
        select.appendChild(opt);
      });

      const textarea = document.createElement("textarea");
      textarea.placeholder = "Add a comment…";
      textarea.rows = 3;

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
      box.appendChild(select);
      box.appendChild(textarea);
      box.appendChild(actions);

      btn.addEventListener("click", function () {
        box.hidden = !box.hidden;
        if (!box.hidden) textarea.focus();
      });

      cancelBtn.addEventListener("click", function () {
        textarea.value = "";
        box.hidden = true;
      });

      addBtn.addEventListener("click", function () {
        const text = textarea.value.trim();
        if (!text) return;
        comments.push(core.makeComment(ref, select.value, quote, text));
        textarea.value = "";
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
      const box = document.createElement("input");
      box.type = "checkbox";
      box.className = "reviewed-box";
      box.setAttribute("aria-label", "mark reviewed");
      box.checked = reviewed.has(ref);
      if (box.checked) container.classList.add("is-reviewed");
      box.addEventListener("click", function (e) {
        /* A checkbox nested inside a <summary> still bubbles its click up
           to the <summary>'s default action (toggling the parent
           <details> open/closed) unless stopped here -- checking the box
           should not also collapse or expand the phase. */
        e.stopPropagation();
      });
      box.addEventListener("change", function () {
        if (box.checked) {
          reviewed.add(ref);
          container.classList.add("is-reviewed");
        } else {
          reviewed.delete(ref);
          container.classList.remove("is-reviewed");
        }
        persistReviewed();
        renderReviewedCount();
      });
      return box;
    }

    document.querySelectorAll("details.phase").forEach(function (details) {
      const ref = details.getAttribute("data-plan-ref");
      const heading = details.querySelector("summary h2");
      if (!ref || !heading) return;
      heading.appendChild(addReviewedBox(details, ref));
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
    if (location.hash === "#selftest") selftest(); else init();
  }
  if (document.readyState !== "loading") {
    run();
  } else {
    document.addEventListener("DOMContentLoaded", run);
  }
})();
