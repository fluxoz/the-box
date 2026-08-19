// The Box Console — clock, theme toggle, and the navigation layer that makes
// the console behave like an app instead of a stack of documents.
//
// No framework, and nothing fetched from anywhere: the console is a
// self-contained artifact that must work on a Box with no Internet (see the
// design system). The whole dynamic layer is progressive enhancement over the
// server-rendered pages — with JS off, every link and form still works as a
// plain request, which is also why there is no client-side router or template
// engine here. The server remains the only thing that renders HTML.
(function () {
  "use strict";
  var root = document.documentElement;

  // ---- theme ------------------------------------------------------------
  var t = document.getElementById("theme");
  if (t) {
    t.addEventListener("click", function () {
      var cur = root.getAttribute("data-theme")
        || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
      root.setAttribute("data-theme", cur === "dark" ? "light" : "dark");
    });
  }

  // ---- clock ------------------------------------------------------------
  var c = document.getElementById("clock");
  function p(n) { return String(n).padStart(2, "0"); }
  var days = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
  function tick() {
    if (!c) return;
    var d = new Date();
    c.textContent = p(d.getHours()) + ":" + p(d.getMinutes()) + ":" + p(d.getSeconds()) + " • " + days[d.getDay()];
  }
  tick();
  setInterval(tick, 1000);

  // ---- navigation -------------------------------------------------------
  if (!window.fetch || !window.history || !window.DOMParser) return;

  var main = document.querySelector("main");
  var nav = document.querySelector("nav.tabs");
  if (!main) return;

  var bar = document.createElement("div");
  bar.className = "loadbar";
  document.body.appendChild(bar);

  var busy = false;
  function setBusy(on) {
    busy = on;
    bar.classList.toggle("on", on);
    root.classList.toggle("busy", on);
  }

  // Same-origin document navigation only. Anything that leaves the console —
  // a deployed site, a new tab, a download, an external host — is left to the
  // browser.
  function isLocalDoc(url, a) {
    if (!url || url.origin !== location.origin) return false;
    if (a && (a.target || a.hasAttribute("download"))) return false;
    if (url.pathname.indexOf("/sites/") === 0) return false;
    if (url.pathname.indexOf("/api/") === 0) return false;
    return true;
  }

  // Copy each list-table's header text onto its cells (data-l), so the
  // phone-width stylesheet can stack rows into labeled cards. Idempotent;
  // key-value tables (no thead) are left alone.
  function labelTables(root) {
    (root || document).querySelectorAll("table").forEach(function (t) {
      var ths = t.querySelectorAll("thead th");
      if (!ths.length) return;
      var labels = [];
      ths.forEach(function (th) { labels.push(th.textContent.trim()); });
      t.querySelectorAll("tbody tr").forEach(function (tr) {
        var i = 0;
        tr.querySelectorAll(":scope > td").forEach(function (td) {
          var l = labels[i++];
          if (l && !td.hasAttribute("data-l")) td.setAttribute("data-l", l);
        });
      });
    });
  }
  labelTables();

  function swap(html, url) {
    var doc = new DOMParser().parseFromString(html, "text/html");
    var fresh = doc.querySelector("main");
    if (!fresh) { location.href = url; return; }
    main.replaceWith(fresh);
    main = fresh;
    if (doc.title) document.title = doc.title;
    // The server already marked the active tab; copy its answer rather than
    // re-deriving it here.
    var freshNav = doc.querySelector("nav.tabs");
    if (nav && freshNav) nav.innerHTML = freshNav.innerHTML;
    main.scrollIntoView({ block: "start" });
    // Move focus so a screen reader announces the new view.
    var h = main.querySelector("h2");
    if (h) { h.setAttribute("tabindex", "-1"); h.focus({ preventScroll: true }); }
    labelTables(main);
    watchJob(); // the new view may itself be a running job
    watchLive();
  }

  function go(url, opts, push, btn) {
    if (busy) return;
    setBusy(true);
    if (btn) btn.disabled = true;
    fetch(url, opts || { headers: { "Accept": "text/html" } })
      .then(function (r) {
        // fetch follows the POST-redirect-GET itself, so r.url is where we
        // actually ended up (including any ?ok=/?err= flash).
        return r.text().then(function (html) { return { html: html, url: r.url || url }; });
      })
      .then(function (res) {
        if (push) history.pushState({}, "", res.url);
        swap(res.html, res.url);
      })
      .catch(function () { location.href = url; })
      .finally(function () {
        setBusy(false);
        // Re-enable only once the action has actually finished, so a slow
        // deploy cannot be fired twice and the button reflects reality.
        if (btn) btn.disabled = false;
      });
  }

  document.addEventListener("click", function (e) {
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    var a = e.target.closest && e.target.closest("a[href]");
    if (!a) return;
    var url;
    try { url = new URL(a.getAttribute("href"), location.href); } catch (_) { return; }
    if (!isLocalDoc(url, a)) return;
    e.preventDefault();
    go(url.href, null, true);
  });

  document.addEventListener("submit", function (e) {
    if (e.defaultPrevented) return;
    var form = e.target;
    if (!form || form.method.toLowerCase() !== "post") return;
    var url;
    try { url = new URL(form.getAttribute("action") || location.href, location.href); } catch (_) { return; }
    if (!isLocalDoc(url, null)) return;
    e.preventDefault();

    go(url.href, {
      method: "POST",
      headers: { "Accept": "text/html" },
      body: new URLSearchParams(new FormData(form))
    }, true, e.submitter);
  });

  window.addEventListener("popstate", function () {
    go(location.href, null, false);
  });

  // ---- live progress ----------------------------------------------------
  // A long operation (deploy, rollback, recreate, backup, platform update)
  // redirects here as a job. Poll it and repaint in place, so the phase,
  // elapsed time and log advance while it runs instead of the page appearing
  // to hang. When it finishes, go where the job said it was going.
  var poll = null;
  var jobTicker = null;
  function stopPolling() {
    if (poll) { clearTimeout(poll); poll = null; }
    if (jobTicker) { clearInterval(jobTicker); jobTicker = null; }
  }

  function watchJob() {
    stopPolling();
    var el = main.querySelector(".job[data-job]");
    if (!el || el.getAttribute("data-state") !== "running") return;
    var id = el.getAttribute("data-job");
    // Anchor elapsed to the SERVER's count, not page load: reopening a job
    // that has run for a minute must not restart its clock at 0.
    var elapsed0 = parseInt(el.getAttribute("data-elapsed"), 10) || 0;
    var expected = parseInt(el.getAttribute("data-expected"), 10) || 0;
    var t0 = Date.now() - elapsed0 * 1000;
    var last = null; // the latest job JSON, for the between-polls repaint

    function elapsedSecs() { return (Date.now() - t0) / 1000; }

    // Steps with real durations: finished phases from their timestamps
    // (server clock only, so client skew cannot warp them), the running one
    // from the live elapsed count.
    function renderPhases(job) {
      var list = main.querySelector(".job .phases");
      if (!list || !job.phase_history || !job.phase_history.length) return;
      list.textContent = "";
      for (var i = 0; i < job.phase_history.length; i++) {
        var m = job.phase_history[i];
        var isLast = i === job.phase_history.length - 1;
        var li = document.createElement("li");
        var dur;
        if (!isLast) dur = job.phase_history[i + 1].started_unix - m.started_unix;
        else if (job.finished_unix) dur = job.finished_unix - m.started_unix;
        else dur = Math.round(elapsedSecs() - (m.started_unix - job.started_unix));
        li.className = (isLast && job.state === "running") ? "now" : "done";
        li.textContent = m.name;
        var t = document.createElement("span");
        t.className = "t";
        t.textContent = Math.max(0, dur) + "s";
        li.appendChild(t);
        list.appendChild(li);
      }
    }

    // The smooth half of the bar. Real counts from the build itself (nix's
    // done/expected, in the job JSON) win; the clock estimate only fills the
    // gaps before the first count and for kinds nothing measures.
    function paint() {
      var e = elapsedSecs();
      var units = last && last.units_total > 0
        ? [last.units_done || 0, last.units_total] : null;
      var secs = main.querySelector(".job .section-head .muted");
      if (secs) {
        secs.textContent = Math.round(e) + "s" +
          (units ? " · " + units[0] + " / " + units[1]
                 : (expected ? " · ~" + expected + "s typical" : ""));
      }
      var bar = main.querySelector(".job .bar");
      var fill = bar && bar.querySelector(".fill");
      if (fill && units) {
        // First real count: retire the sweep, this bar is measured now.
        bar.classList.add("det");
        fill.style.width = Math.max(1, Math.min(99, (units[0] / units[1]) * 100)) + "%";
      } else if (fill && bar.classList.contains("det") && expected) {
        fill.style.width = Math.max(2, Math.min(92, (e / expected) * 100)) + "%";
      }
      if (last) renderPhases(last);
    }
    jobTicker = setInterval(paint, 500);

    (function tick() {
      poll = setTimeout(function () {
        fetch("/api/v1/jobs/" + encodeURIComponent(id), { headers: { "Accept": "application/json" } })
          .then(function (r) { return r.ok ? r.json() : null; })
          .then(function (job) {
            if (!job) return; // daemon restarted; leave the last state on screen
            last = job;
            var phase = main.querySelector(".job .phase");
            if (phase && job.phase) phase.textContent = job.phase;
            var log = main.querySelector(".joblog");
            if (log && job.log) {
              var atBottom = log.scrollTop + log.clientHeight >= log.scrollHeight - 8;
              log.textContent = job.log.join("\n") + "\n";
              if (atBottom) log.scrollTop = log.scrollHeight;
            }
            paint();
            if (job.state !== "running") {
              // Land the bar before leaving: a finished job reads as 100%,
              // then the view moves on to where the job said it was going.
              var fill = main.querySelector(".job .bar .fill");
              if (fill) fill.style.width = "100%";
              stopPolling();
              go(location.href, null, false);
              setTimeout(function () { go(job.target || "/", null, true); }, 900);
              return;
            }
            tick();
          })
          .catch(function () { tick(); });
      }, 700);
    })();
  }

  // Live sections: any element with data-poll="<seconds>" keeps its page
  // fresh by re-fetching and swapping in place — logs advance, sync states
  // settle, deploys appear, with no reload button and no work when the tab
  // is hidden. The job watcher takes precedence (it polls faster and knows
  // where to go when the job ends).
  function watchLive() {
    if (poll) return; // a job is already driving the refresh
    var el = main.querySelector("[data-poll]");
    if (!el) return;
    var secs = parseInt(el.getAttribute("data-poll"), 10) || 4;
    poll = setTimeout(function () {
      poll = null;
      if (document.hidden) { watchLive(); return; }
      go(location.href, null, false);
    }, secs * 1000);
  }

  // Arm for the page we loaded on; swap() re-arms after every view change.
  watchJob();
  watchLive();
})();

// ---- security keys (WebAuthn) ---------------------------------------------
// The browser speaks ArrayBuffers and the server speaks base64url, so the only
// real work here is the translation either side of navigator.credentials.
(function () {
  "use strict";
  function b64urlToBuf(s) {
    s = String(s).replace(/-/g, "+").replace(/_/g, "/");
    while (s.length % 4) s += "=";
    var bin = atob(s), out = new Uint8Array(bin.length);
    for (var i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out.buffer;
  }
  function bufToB64url(buf) {
    var bytes = new Uint8Array(buf), s = "";
    for (var i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
    return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }
  // Walk the challenge the server sent, turning the fields WebAuthn requires as
  // BufferSource back into buffers.
  function reviveRequest(o) {
    if (o.publicKey) o = o.publicKey;
    var p = Object.assign({}, o);
    if (p.challenge) p.challenge = b64urlToBuf(p.challenge);
    if (p.user && p.user.id) p.user = Object.assign({}, p.user, { id: b64urlToBuf(p.user.id) });
    ["excludeCredentials", "allowCredentials"].forEach(function (k) {
      if (Array.isArray(p[k])) {
        p[k] = p[k].map(function (c) {
          return Object.assign({}, c, { id: b64urlToBuf(c.id) });
        });
      }
    });
    return p;
  }
  function serialize(cred) {
    var r = cred.response, out = {
      id: cred.id, rawId: bufToB64url(cred.rawId), type: cred.type,
      extensions: cred.getClientExtensionResults ? cred.getClientExtensionResults() : {},
      response: { clientDataJSON: bufToB64url(r.clientDataJSON) }
    };
    if (r.attestationObject) out.response.attestationObject = bufToB64url(r.attestationObject);
    if (r.authenticatorData) {
      out.response.authenticatorData = bufToB64url(r.authenticatorData);
      out.response.signature = bufToB64url(r.signature);
      out.response.userHandle = r.userHandle ? bufToB64url(r.userHandle) : null;
    }
    return out;
  }
  function post(url, body, asJson) {
    return fetch(url, {
      method: "POST",
      headers: asJson
        ? { "Content-Type": "application/json", "Accept": "application/json" }
        : { "Content-Type": "application/x-www-form-urlencoded", "Accept": "application/json" },
      body: asJson ? JSON.stringify(body) : new URLSearchParams(body)
    }).then(function (r) {
      return r.json().then(function (j) {
        if (!r.ok || j.error) throw new Error(j.error || "that did not work");
        return j;
      });
    });
  }

  function say(el, text, bad) {
    if (!el) return;
    el.textContent = text;
    el.style.color = bad ? "var(--critical)" : "var(--ok)";
  }

  // Enroll, from the Devices page. Two doors, one handler: the button says
  // which authenticator family the server should ask for.
  document.addEventListener("click", function (e) {
    var btn = e.target.closest && e.target.closest("#keyadd, #keyaddplatform");
    if (!btn) return;
    e.preventDefault();
    var msg = document.getElementById("keymsg");
    var label = (document.getElementById("keylabel") || {}).value || "Security key";
    if (!window.PublicKeyCredential) {
      say(msg, "This browser has no security-key support on this address.", true);
      return;
    }
    btn.disabled = true;
    say(msg, btn.dataset.kind === "platform" ? "Follow the Face ID or fingerprint prompt…" : "Touch your key…");
    post("/devices/keys/start", { label: label, kind: btn.dataset.kind || "any" }, false)
      .then(function (j) {
        return navigator.credentials
          .create({ publicKey: reviveRequest(j.challenge) })
          .then(function (cred) {
            return post("/devices/keys/finish",
              { handle: j.handle, label: label, response: serialize(cred) }, true);
          });
      })
      .then(function () { location.href = "/devices?ok=Security+key+enrolled"; })
      .catch(function (err) { say(msg, err.message || String(err), true); btn.disabled = false; });
  });

  // Sign in, from the pair page. One code path serves both doors: the
  // explicit button, and conditional UI (the passkey suggestion the platform
  // shows on its own, which is what makes Face ID appear with zero taps).
  var conditionalAbort = null;
  var conditionalDone = Promise.resolve();
  function keySignIn(mediation, btn) {
    var msg = document.getElementById("keysigninmsg");
    return post("/pair/key/start", {}, false)
      .then(function (j) {
        var req = { publicKey: reviveRequest(j.challenge) };
        if (mediation === "conditional") {
          conditionalAbort = new AbortController();
          req.mediation = "conditional";
          req.signal = conditionalAbort.signal;
        }
        return navigator.credentials.get(req).then(function (cred) {
          return post("/pair/key/finish", { handle: j.handle, response: serialize(cred) }, true);
        });
      })
      .then(function (j) { location.href = j.next || "/"; })
      .catch(function (err) {
        if (err && err.name === "AbortError") return; // superseded, not failed
        if (mediation === "conditional") return;
        // Safari sometimes refuses the first modal request right after the
        // ambient one is torn down; one quiet retry before showing anything.
        if (!keySignIn.retried && err && err.name === "NotAllowedError") {
          keySignIn.retried = true;
          say(msg, "One more try…");
          return new Promise(function (r) { setTimeout(r, 400); })
            .then(function () { return keySignIn(undefined, btn); });
        }
        say(msg, err.message || String(err), true);
        if (btn) btn.disabled = false;
      });
  }
  // Offer the platform's own passkey prompt as soon as the page can.
  if (document.getElementById("keysignin") && window.PublicKeyCredential
      && PublicKeyCredential.isConditionalMediationAvailable) {
    PublicKeyCredential.isConditionalMediationAvailable().then(function (yes) {
      if (yes) conditionalDone = keySignIn("conditional", null);
    });
  }
  document.addEventListener("click", function (e) {
    var btn = e.target.closest && e.target.closest("#keysignin");
    if (!btn) return;
    e.preventDefault();
    var msg = document.getElementById("keysigninmsg");
    if (!window.PublicKeyCredential) {
      say(msg, "This browser has no security-key support on this address.", true);
      return;
    }
    btn.disabled = true;
    say(msg, "Use Face ID, a fingerprint, or touch your key…");
    // Tear the ambient request down FULLY before the modal one starts; the
    // race between them is exactly what Safari refuses.
    if (conditionalAbort) conditionalAbort.abort();
    conditionalDone.then(function () { keySignIn(undefined, btn); });
  });
})();
