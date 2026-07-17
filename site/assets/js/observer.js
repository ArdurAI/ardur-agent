// Lightweight progressive enhancement. No framework, no dependencies.
// Honors prefers-reduced-motion and degrades cleanly when APIs are missing.
(function () {
  "use strict";

  var reduce =
    window.matchMedia &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  // 1. Reveal-on-scroll fallback for browsers without animation-timeline.
  var supportsTimeline =
    window.CSS && CSS.supports && CSS.supports("animation-timeline: view()");
  var reveals = document.querySelectorAll(".reveal");
  if (reduce || !("IntersectionObserver" in window)) {
    reveals.forEach(function (el) {
      el.classList.add("is-visible");
    });
  } else if (!supportsTimeline) {
    var io = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (entry) {
          if (entry.isIntersecting) {
            entry.target.classList.add("is-visible");
            io.unobserve(entry.target);
          }
        });
      },
      { threshold: 0.15, rootMargin: "0px 0px -8% 0px" }
    );
    reveals.forEach(function (el) {
      io.observe(el);
    });
  }

  // 2. Sticky nav backdrop once scrolled past the hero.
  var nav = document.querySelector(".site-nav");
  if (nav) {
    var onScroll = function () {
      if (window.scrollY > 80) {
        nav.classList.add("nav-scrolled");
      } else {
        nav.classList.remove("nav-scrolled");
      }
    };
    window.addEventListener("scroll", onScroll, { passive: true });
    onScroll();
  }

  // 3. Stat counters animate 0 -> target when scrolled into view.
  var counters = document.querySelectorAll("[data-count-to]");
  var runCounter = function (el) {
    var target = parseFloat(el.getAttribute("data-count-to"));
    var suffix = el.getAttribute("data-count-suffix") || "";
    if (reduce || isNaN(target)) {
      el.textContent = target + suffix;
      return;
    }
    var start = null;
    var dur = 1400;
    var step = function (ts) {
      if (start === null) start = ts;
      var p = Math.min((ts - start) / dur, 1);
      var eased = 1 - Math.pow(1 - p, 3);
      el.textContent = Math.round(target * eased) + suffix;
      if (p < 1) requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  };
  if (counters.length) {
    if (!("IntersectionObserver" in window)) {
      counters.forEach(runCounter);
    } else {
      var cio = new IntersectionObserver(
        function (entries) {
          entries.forEach(function (entry) {
            if (entry.isIntersecting) {
              runCounter(entry.target);
              cio.unobserve(entry.target);
            }
          });
        },
        { threshold: 0.6 }
      );
      counters.forEach(function (el) {
        cio.observe(el);
      });
    }
  }

  // 4. Theme toggle (dark default). Persists choice in localStorage.
  var root = document.documentElement;
  var stored = null;
  try {
    stored = localStorage.getItem("ardur-theme");
  } catch (e) {}
  if (stored) root.setAttribute("data-theme", stored);
  var toggle = document.querySelector("[data-theme-toggle]");
  if (toggle) {
    toggle.addEventListener("click", function () {
      var next =
        root.getAttribute("data-theme") === "light" ? "dark" : "light";
      root.setAttribute("data-theme", next);
      try {
        localStorage.setItem("ardur-theme", next);
      } catch (e) {}
    });
  }

  // 5. Animated demo terminal: types a short sample exchange line by line.
  var term = document.querySelector("[data-term]");
  if (term) {
    var lines = JSON.parse(term.getAttribute("data-term"));
    var out = term.querySelector("[data-term-out]");
    var caret = term.querySelector(".term-caret");
    if (reduce) {
      // Render the whole exchange immediately, no typing.
      out.innerHTML = lines
        .map(function (l) {
          return '<div class="' + l.cls + '">' + l.text + "</div>";
        })
        .join("");
      if (caret) caret.style.display = "none";
    } else {
      var li = 0;
      var ci = 0;
      var cur = null;
      var typeNext = function () {
        if (li >= lines.length) {
          if (caret) caret.style.display = "none";
          return;
        }
        var line = lines[li];
        if (!cur) {
          cur = document.createElement("div");
          cur.className = line.cls;
          out.appendChild(cur);
        }
        if (ci <= line.text.length) {
          cur.textContent = line.text.slice(0, ci);
          ci++;
          setTimeout(typeNext, line.cls.indexOf("prompt") > -1 ? 38 : 12);
        } else {
          li++;
          ci = 0;
          cur = null;
          setTimeout(typeNext, 320);
        }
      };
      var startKick = new IntersectionObserver(function (entries) {
        entries.forEach(function (entry) {
          if (entry.isIntersecting) {
            typeNext();
            startKick.disconnect();
          }
        });
      });
      startKick.observe(term);
    }
  }
})();
