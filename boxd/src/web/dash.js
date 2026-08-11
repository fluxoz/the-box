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
})();
