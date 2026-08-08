// The Box Configurator — clock + theme toggle (matches the pre-install configurator).
(function () {
  "use strict";
  var root = document.documentElement;
  var t = document.getElementById("theme");
  if (t) {
    t.addEventListener("click", function () {
      var cur = root.getAttribute("data-theme")
        || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
      root.setAttribute("data-theme", cur === "dark" ? "light" : "dark");
    });
  }
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
})();
