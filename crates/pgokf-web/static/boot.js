// Runs before first paint: marks that scripting is available (the stylesheet
// hides the tab panels only then, so every panel is readable without it)
// and applies the saved theme so the page never flashes the wrong one.
(function () {
  'use strict';
  var root = document.documentElement;
  root.setAttribute('data-js', '1');
  try {
    var KEY = 'pgokf-theme';
    var fromUrl = new URLSearchParams(location.search).get('theme');
    if (fromUrl === 'light' || fromUrl === 'dark') { localStorage.setItem(KEY, fromUrl); }
    var saved = localStorage.getItem(KEY);
    if (saved === 'light' || saved === 'dark') { root.setAttribute('data-theme', saved); }
  } catch (_) { /* storage unavailable */ }
})();
