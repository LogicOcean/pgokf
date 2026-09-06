// pgokf-web client behaviour: theme toggle, keyboard focus, copy buttons,
// tab panels, list filtering, auto-submitting selects, and "load more" list
// merging. No framework; htmx handles the partial swaps declared in the
// templates, and every page works without this file (see boot.js).
(function () {
  'use strict';

  var root = document.documentElement;
  var THEME_KEY = 'pgokf-theme';

  // ---- theme toggle -------------------------------------------------------
  var toggle = document.getElementById('pgokf-theme');
  if (toggle) {
    toggle.addEventListener('click', function () {
      var current = root.getAttribute('data-theme');
      var dark = current === 'dark' || (current !== 'light' && window.matchMedia('(prefers-color-scheme: dark)').matches);
      var next = dark ? 'light' : 'dark';
      root.setAttribute('data-theme', next);
      try { localStorage.setItem(THEME_KEY, next); } catch (_) { /* ignore */ }
    });
  }

  // ---- keyboard: "/" focuses the search box -------------------------------
  document.addEventListener('keydown', function (event) {
    if (event.key !== '/' || event.metaKey || event.ctrlKey || event.altKey) return;
    var target = event.target;
    var tag = target && target.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || (target && target.isContentEditable)) return;
    var box = document.querySelector('.topsearch input[type=search]');
    if (box) { event.preventDefault(); box.focus(); box.select(); }
  });

  // ---- copy buttons -------------------------------------------------------
  document.addEventListener('click', function (event) {
    var button = event.target.closest('[data-copy]');
    if (!button) return;
    var text = button.getAttribute('data-copy');
    var done = function () {
      var label = button.textContent;
      button.textContent = 'Copied';
      button.classList.add('copied');
      setTimeout(function () { button.textContent = label; button.classList.remove('copied'); }, 1200);
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, function () { window.prompt('Copy:', text); });
    } else {
      window.prompt('Copy:', text);
    }
  });

  // ---- selects that submit their form on change (a button stays for no-JS)
  document.querySelectorAll('select[data-autosubmit]').forEach(function (select) {
    select.addEventListener('change', function () {
      // A form another script drives live (the 3D graph) is left to it.
      if (select.form && !select.form.hasAttribute('data-live')) select.form.requestSubmit();
    });
  });

  // ---- plugin builder: the chosen target drives which fields show -------
  var builder = document.getElementById('pgokf-plugin-form');
  if (builder) {
    var picker = builder.querySelector('details.targets-picker');
    var pickerLabel = builder.querySelector('[data-target-label]');
    var syncTarget = function () {
      var picked = builder.querySelector('input[name=target]:checked');
      builder.setAttribute('data-target', picked ? picked.value : '');
      builder.querySelectorAll('.target-card').forEach(function (cardEl) {
        cardEl.classList.toggle('selected', cardEl.contains(picked));
      });
      if (pickerLabel && picked) {
        var label = picked.closest('.target-card').querySelector('.target-label');
        pickerLabel.textContent = label ? label.textContent : picked.value;
      }
    };
    builder.addEventListener('change', syncTarget);
    syncTarget();
    // On a phone the ten target cards fold behind their summary.
    if (picker) {
      var narrow = window.matchMedia('(max-width: 900px)');
      var foldPicker = function () { picker.open = !narrow.matches; };
      foldPicker();
      narrow.addEventListener('change', foldPicker);
      builder.addEventListener('change', function (event) {
        if (event.target.name === 'target' && narrow.matches) picker.open = false;
      });
    }
  }

  // ---- tabs (ARIA tabs pattern; the hash names the tab as #tab-<name>) ----
  var TAB_PREFIX = 'tab-';
  function initTabs(container) {
    var tabs = Array.prototype.slice.call(container.querySelectorAll('[role=tab]'));
    var panels = container.querySelectorAll('[role=tabpanel]');
    function select(name, focus) {
      tabs.forEach(function (t) {
        var on = t.getAttribute('data-tab') === name;
        t.setAttribute('aria-selected', on ? 'true' : 'false');
        t.tabIndex = on ? 0 : -1;
        if (on && focus) t.focus();
      });
      panels.forEach(function (p) { p.classList.toggle('active', p.getAttribute('data-panel') === name); });
      try { history.replaceState(null, '', '#' + TAB_PREFIX + name); } catch (_) { /* ignore */ }
    }
    tabs.forEach(function (t, i) {
      t.addEventListener('click', function () { select(t.getAttribute('data-tab'), false); });
      t.addEventListener('keydown', function (event) {
        var delta = event.key === 'ArrowRight' ? 1 : event.key === 'ArrowLeft' ? -1 : 0;
        if (!delta) return;
        event.preventDefault();
        var next = tabs[(i + delta + tabs.length) % tabs.length];
        select(next.getAttribute('data-tab'), true);
      });
    });
    var hash = (location.hash || '').slice(1);
    if (hash.indexOf(TAB_PREFIX) === 0) {
      var wanted = hash.slice(TAB_PREFIX.length);
      var known = tabs.some(function (t) { return t.getAttribute('data-tab') === wanted; });
      if (known) select(wanted, false);
    }
  }
  document.querySelectorAll('[data-tabs]').forEach(initTabs);

  // ---- list filtering -----------------------------------------------------
  document.querySelectorAll('input[data-filter]').forEach(function (input) {
    var list = document.querySelector(input.getAttribute('data-filter'));
    if (!list) return;
    input.addEventListener('input', function () {
      var needle = input.value.trim().toLowerCase();
      list.querySelectorAll('[data-filter-text]').forEach(function (item) {
        var hay = item.getAttribute('data-filter-text').toLowerCase();
        item.hidden = needle !== '' && hay.indexOf(needle) === -1;
      });
      list.querySelectorAll('details').forEach(function (group) {
        var visible = group.querySelectorAll('li:not([hidden])').length;
        group.hidden = needle !== '' && visible === 0;
        if (needle !== '') group.open = true;
      });
    });
  });

  // ---- "load more": merge appended hits into the existing list ----------
  document.body.addEventListener('htmx:afterSwap', function (event) {
    var results = document.getElementById('pgokf-results');
    if (!results || event.target !== results) return;
    var lists = results.querySelectorAll('ol.hits');
    if (lists.length > 1) {
      var first = lists[0];
      for (var i = 1; i < lists.length; i++) {
        while (lists[i].firstChild) first.appendChild(lists[i].firstChild);
        lists[i].remove();
      }
    }
    // Every appended page carries a .more (empty on the last page), so the
    // newest one always replaces the button that fetched it.
    var more = results.querySelectorAll('.more');
    for (var j = 0; j < more.length - 1; j++) more[j].remove();
    var last = more[more.length - 1];
    if (last && !last.firstElementChild) last.remove();
    // Orphan <li> elements appended by hx-select land after the list; fold them in.
    var strays = Array.prototype.filter.call(results.children, function (el) { return el.tagName === 'LI'; });
    if (strays.length && lists[0]) strays.forEach(function (li) { lists[0].appendChild(li); });
  });
})();
