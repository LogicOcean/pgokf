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

  // ---- agent plugin builder ------------------------------------------------
  var builder = document.getElementById('pgokf-plugin-form');
  if (builder) {
    // The chosen target drives the note under the grid and target-only fields.
    var syncTarget = function () {
      var picked = builder.querySelector('input[name=target]:checked');
      var id = picked ? picked.value : '';
      builder.setAttribute('data-target', id);
      builder.querySelectorAll('.target-card').forEach(function (cardEl) {
        cardEl.classList.toggle('selected', cardEl.contains(picked));
      });
      builder.querySelectorAll('[data-target-note]').forEach(function (note) {
        note.hidden = note.getAttribute('data-target-note') !== id;
      });
    };
    // An extra's own fields show only while that extra is ticked.
    var syncExtras = function () {
      builder.querySelectorAll('[data-needs]').forEach(function (field) {
        var needs = field.getAttribute('data-needs').split(/\s+/);
        var on = needs.some(function (name) {
          var box = builder.querySelector('input[name="' + name + '"]');
          return box && box.checked;
        });
        field.hidden = !on;
      });
    };
    // Chips toggle a value in the comma-separated text field beside them;
    // typing in the field keeps the chips in step.
    var splitList = function (value) {
      return value.split(',').map(function (v) { return v.trim(); }).filter(Boolean);
    };
    builder.querySelectorAll('[data-chips-for]').forEach(function (group) {
      var input = document.getElementById(group.getAttribute('data-chips-for'));
      if (!input) return;
      var syncChips = function () {
        var picked = splitList(input.value).map(function (v) { return v.toLowerCase(); });
        group.querySelectorAll('[data-chip]').forEach(function (chip) {
          var on = picked.indexOf(chip.getAttribute('data-chip').toLowerCase()) !== -1;
          chip.setAttribute('aria-pressed', on ? 'true' : 'false');
        });
      };
      group.addEventListener('click', function (event) {
        var chip = event.target.closest('[data-chip]');
        if (!chip) return;
        var value = chip.getAttribute('data-chip');
        var list = splitList(input.value);
        var index = list.map(function (v) { return v.toLowerCase(); }).indexOf(value.toLowerCase());
        if (index === -1) list.push(value); else list.splice(index, 1);
        input.value = list.join(', ');
        syncChips();
        input.dispatchEvent(new Event('change', { bubbles: true }));
      });
      input.addEventListener('input', syncChips);
      syncChips();
    });
    builder.addEventListener('change', function () { syncTarget(); syncExtras(); });
    syncTarget();
    syncExtras();
    initPicker(builder);
  }

  // ---- file picker: browse a bundle, tick files, feed the picks field -------
  function initPicker(form) {
    var picker = form.querySelector('[data-picker]');
    var picks = document.getElementById('p-picks');
    if (!picker || !picks) return;
    var bundleSelect = picker.querySelector('[data-picker-bundle]');
    var filter = picker.querySelector('[data-picker-filter]');
    var status = picker.querySelector('[data-picker-status]');
    var tree = picker.querySelector('[data-picker-tree]');
    var loaded = null;   // { bundleId, entries }
    var openDirs = {};   // dir -> true/false once the user toggled it
    var pickedSet = function () {
      var set = {};
      picks.value.split('\n').forEach(function (line) {
        line = line.trim();
        if (line) set[line] = true;
      });
      return set;
    };
    var writePicks = function (set) {
      picks.value = Object.keys(set).join('\n');
      picks.dispatchEvent(new Event('change', { bubbles: true }));
    };
    var refOf = function (entry) { return loaded.bundleId + ':' + entry.id; };
    var render = function () {
      tree.textContent = '';
      if (!loaded) return;
      var set = pickedSet();
      var needle = (filter.value || '').trim().toLowerCase();
      // Group by directory (the part of the path before the file name).
      var dirs = {};
      var order = [];
      loaded.entries.forEach(function (entry) {
        if (needle && (entry.path + ' ' + (entry.title || '')).toLowerCase().indexOf(needle) === -1) return;
        var slash = entry.path.lastIndexOf('/');
        var dir = slash === -1 ? '' : entry.path.slice(0, slash);
        if (!dirs[dir]) { dirs[dir] = []; order.push(dir); }
        dirs[dir].push(entry);
      });
      // Code-unit order puts a package directory before its subdirectories.
      order.sort();
      var root = document.createElement('ul');
      order.forEach(function (dir) {
        var li = document.createElement('li');
        var details = document.createElement('details');
        details.setAttribute('data-dir', dir);
        // A directory the user toggled keeps its state; otherwise it opens
        // when filtering, when the tree is short, or when it holds a pick,
        // so a ticked file is never hidden behind a closed directory.
        var holdsPick = dirs[dir].some(function (entry) { return !!set[refOf(entry)]; });
        details.open = Object.prototype.hasOwnProperty.call(openDirs, dir)
          ? openDirs[dir]
          : (!!needle || order.length <= 6 || holdsPick);
        var summary = document.createElement('summary');
        summary.textContent = (dir || '(bundle root)') + ' ';
        var count = document.createElement('span');
        count.className = 'muted small';
        count.textContent = dirs[dir].length;
        summary.appendChild(count);
        details.appendChild(summary);
        var list = document.createElement('ul');
        dirs[dir].forEach(function (entry) {
          var item = document.createElement('li');
          var label = document.createElement('label');
          var box = document.createElement('input');
          box.type = 'checkbox';
          box.setAttribute('data-pick', refOf(entry));
          box.checked = !!set[refOf(entry)];
          var path = document.createElement('span');
          path.className = 'pick-path';
          path.textContent = entry.path.slice(dir ? dir.length + 1 : 0);
          label.appendChild(box);
          label.appendChild(path);
          if (entry.package) {
            var pill = document.createElement('span');
            pill.className = 'pill type';
            pill.textContent = 'skill package';
            label.appendChild(pill);
          } else if (entry.package_of) {
            var member = document.createElement('span');
            member.className = 'pill muted';
            member.textContent = entry.type === 'Script' ? 'script' : 'reference';
            label.appendChild(member);
          } else if (entry.type) {
            var type = document.createElement('span');
            type.className = 'pill muted';
            type.textContent = entry.type;
            label.appendChild(type);
          }
          var fileName = entry.path.slice(dir ? dir.length + 1 : 0);
          if (entry.title && entry.title !== entry.id && entry.title !== fileName) {
            var title = document.createElement('span');
            title.className = 'pick-title';
            title.textContent = entry.title;
            label.appendChild(title);
          }
          item.appendChild(label);
          list.appendChild(item);
        });
        details.appendChild(list);
        li.appendChild(details);
        root.appendChild(li);
      });
      tree.appendChild(root);
      var picked = Object.keys(set).length;
      status.textContent = (loaded.truncated ? 'first ' + loaded.entries.length + ' files shown; ' : loaded.entries.length + ' files; ')
        + picked + ' picked';
    };
    var load = function () {
      var bundleId = bundleSelect.value;
      if (!bundleId) { loaded = null; render(); return; }
      status.textContent = 'loading…';
      fetch('/api/bundles/' + encodeURIComponent(bundleId) + '/tree', { headers: { Accept: 'application/json' } })
        .then(function (response) { if (!response.ok) throw new Error(response.status); return response.json(); })
        .then(function (data) {
          loaded = { bundleId: bundleId, entries: data.entries || [], truncated: !!data.truncated };
          render();
        })
        .catch(function () { loaded = null; tree.textContent = ''; status.textContent = 'could not load this bundle'; });
    };
    tree.addEventListener('toggle', function (event) {
      var details = event.target;
      if (details && details.hasAttribute && details.hasAttribute('data-dir')) {
        openDirs[details.getAttribute('data-dir')] = details.open;
      }
    }, true);
    tree.addEventListener('change', function (event) {
      var box = event.target.closest('[data-pick]');
      if (!box) return;
      var set = pickedSet();
      if (box.checked) set[box.getAttribute('data-pick')] = true; else delete set[box.getAttribute('data-pick')];
      writePicks(set);
      render();
    });
    // The picks field is the source of truth; typing in it re-marks the tree.
    picks.addEventListener('input', render);
    filter.addEventListener('input', render);
    bundleSelect.addEventListener('change', function () { openDirs = {}; load(); });
    picker.addEventListener('toggle', function () { if (picker.open && !loaded) load(); });
    if (picker.open) load();
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
