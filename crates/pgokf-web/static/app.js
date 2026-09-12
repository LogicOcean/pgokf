// SPDX-License-Identifier: AGPL-3.0-only
// pgokf-web client behaviour: theme toggle, keyboard focus, copy buttons,
// tab panels, list filtering, auto-submitting selects, and the top nav
// strip. No framework; htmx handles the partial swaps declared in the
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
    var splitList = function (value) {
      return value.split(',').map(function (v) { return v.trim(); }).filter(Boolean);
    };
    var fire = function (el) { el.dispatchEvent(new Event('change', { bubbles: true })); };

    // Step 1: the kind decides which agents are listed; a name outside the
    // list adds an agent (a skills agent also says where it reads skills).
    var kindSelect = builder.querySelector('[data-kind-select]');
    var agentInput = builder.querySelector('[data-agent-input]');
    var agentNote = builder.querySelector('[data-agent-note]');
    var kindNote = builder.querySelector('[data-kind-note]');
    var customSkills = builder.querySelector('[data-custom-skills]');
    var agentOptions = function (kind) {
      var list = document.getElementById('pgokf-agents-' + kind);
      return list ? Array.prototype.slice.call(list.options) : [];
    };
    var findAgent = function (kind, value) {
      var wanted = value.trim().toLowerCase();
      if (!wanted) return null;
      return agentOptions(kind).filter(function (option) {
        return option.value.toLowerCase() === wanted
          || (option.getAttribute('data-id') || '').toLowerCase() === wanted;
      })[0] || null;
    };
    var syncAgent = function () {
      if (!kindSelect || !agentInput) return;
      var kind = kindSelect.value;
      var name = agentInput.value.trim();
      var kindOption = kindSelect.options[kindSelect.selectedIndex];
      builder.setAttribute('data-kind', kind);
      agentInput.setAttribute('list', 'pgokf-agents-' + kind);
      if (kindNote && kindOption) kindNote.textContent = kindOption.getAttribute('data-description') || '';
      var match = findAgent(kind, name);
      var custom = !match && name !== '';
      builder.setAttribute('data-custom', custom ? 'true' : 'false');
      if (customSkills) customSkills.hidden = !(custom && kind === 'skills');
      builder.querySelectorAll('[data-agent-name]').forEach(function (el) { el.textContent = name; });
      if (!agentNote) return;
      if (match) {
        agentNote.textContent = match.getAttribute('data-notes') || '';
      } else if (custom) {
        agentNote.textContent = kind === 'skills'
          ? name + ' is not in the list: it is added as a new agent, with the skill package under the directory you give below.'
          : name + ' is not in the list: it is added as a new agent and gets the standard '
            + (kindOption ? kindOption.textContent.toLowerCase() : kind) + ' layout.';
      } else {
        agentNote.textContent = 'Choose an agent from the list, or type a new name to add one.';
      }
    };
    if (kindSelect && agentInput) {
      kindSelect.addEventListener('change', function () {
        // Keep the agent when the new kind lists it; otherwise take the first.
        var first = agentOptions(kindSelect.value)[0];
        if (!findAgent(kindSelect.value, agentInput.value) && first) agentInput.value = first.value;
        syncAgent();
      });
      agentInput.addEventListener('input', syncAgent);
      var opener = builder.querySelector('[data-agent-open]');
      if (opener) {
        opener.addEventListener('click', function () {
          // An empty field shows the whole list; the kind's first agent
          // applies until one is picked.
          agentInput.value = '';
          syncAgent();
          agentInput.focus();
        });
      }
    }

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

    // Two fields that are alternatives: filling one greys the other, so a
    // combination the builder refuses cannot be typed in the first place.
    // A link that arrives with both filled must not grey both - nothing
    // would then be editable - so on load the pair is left alone and the
    // server's refusal stands until one is cleared.
    builder.querySelectorAll('[data-alternative-to]').forEach(function (input) {
      var other = document.getElementById(input.getAttribute('data-alternative-to'));
      if (!other) return;
      var sync = function () {
        var mute = input.value.trim() !== '' && other.value.trim() === '';
        other.disabled = mute;
        other.closest('.field').classList.toggle('is-muted', mute);
      };
      input.addEventListener('input', sync);
      sync();
    });

    // Chips toggle a value in the comma-separated text field beside them;
    // typing in the field keeps the chips in step.
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
        fire(input);
      });
      input.addEventListener('input', syncChips);
      builder.addEventListener('change', syncChips);
      syncChips();
    });

    builder.addEventListener('change', function () { syncAgent(); syncExtras(); });
    syncAgent();
    syncExtras();
    initBrowser(builder, splitList, fire);
  }

  // ---- content browser: browse bundles, tick files, or take everything -----
  function initBrowser(form, splitList, fire) {
    var picks = document.getElementById('p-picks');
    var tree = form.querySelector('[data-browse-tree]');
    if (!picks || !tree) return;
    var bundleSelect = form.querySelector('[data-browse-bundle]');
    var find = form.querySelector('[data-browse-find]');
    var status = form.querySelector('[data-browse-status]');
    var scopeAll = form.querySelector('[data-scope-all]');
    var scopeName = form.querySelector('[data-scope-name]');
    var narrow = form.querySelector('[data-narrow]');
    var chips = form.querySelector('[data-selection-chips]');
    var clearButton = form.querySelector('[data-clear-selection]');
    var typesInput = document.getElementById('p-types');
    var tagsInput = document.getElementById('p-tags');
    var qInput = document.getElementById('p-q');
    var idsInput = document.getElementById('p-ids');
    var verifiedBox = form.querySelector('input[name=verified]');
    var loaded = [];      // entries of the browsed bundles, each with bundleId/bundleName
    var truncated = false;
    var loadToken = 0;
    var openDirs = {};    // group -> true/false once the user toggled it
    var lower = function (v) { return String(v || '').toLowerCase(); };
    var valueOf = function (input) { return input ? input.value : ''; };

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
      fire(picks);
    };
    var refOf = function (entry) { return entry.bundleId + ':' + entry.id; };
    var scopeLabel = function () {
      var option = bundleSelect && bundleSelect.options[bundleSelect.selectedIndex];
      return option && option.value ? option.textContent : 'all bundles';
    };
    var pathOf = function (ref) {
      for (var i = 0; i < loaded.length; i++) {
        if (refOf(loaded[i]) === ref) {
          return (bundleSelect && bundleSelect.value ? '' : loaded[i].bundleName + ' / ') + loaded[i].path;
        }
      }
      return ref;
    };
    // The client-side reading of the rule (types and tags; a search query
    // is ranked by the server and shows only in the preview).
    var inRule = function (entry) {
      if (!scopeAll || !scopeAll.checked) return false;
      var types = splitList(valueOf(typesInput)).map(lower);
      var tags = splitList(valueOf(tagsInput)).map(lower);
      if (types.length && types.indexOf(lower(entry.type)) === -1) return false;
      var have = (entry.tags || []).map(lower);
      return tags.every(function (t) { return have.indexOf(t) !== -1; });
    };

    var render = function () {
      tree.textContent = '';
      var set = pickedSet();
      var needle = lower(valueOf(find)).trim();
      var byBundle = !(bundleSelect && bundleSelect.value);
      var groups = {};
      var order = [];
      loaded.forEach(function (entry) {
        var hay = entry.path + ' ' + (entry.title || '') + ' ' + (entry.type || '') + ' ' + (entry.tags || []).join(' ');
        if (needle && lower(hay).indexOf(needle) === -1) return;
        var slash = entry.path.lastIndexOf('/');
        var dir = slash === -1 ? '' : entry.path.slice(0, slash);
        var key = (byBundle ? entry.bundleName + ' / ' : '') + dir;
        if (!groups[key]) { groups[key] = []; order.push(key); }
        groups[key].push(entry);
      });
      order.sort();
      var shown = 0;
      var root = document.createElement('ul');
      order.forEach(function (key) {
        var entries = groups[key];
        shown += entries.length;
        var picked = entries.filter(function (e) { return !!set[refOf(e)]; }).length;
        var li = document.createElement('li');
        var row = document.createElement('div');
        row.className = 'dir-row';
        var all = document.createElement('input');
        all.type = 'checkbox';
        all.title = 'Tick every file in this directory';
        all.checked = picked === entries.length;
        all.indeterminate = picked > 0 && picked < entries.length;
        all.addEventListener('change', function () {
          var next = pickedSet();
          entries.forEach(function (e) {
            if (all.checked) next[refOf(e)] = true; else delete next[refOf(e)];
          });
          writePicks(next);
        });
        var details = document.createElement('details');
        details.setAttribute('data-dir', key);
        details.open = Object.prototype.hasOwnProperty.call(openDirs, key)
          ? openDirs[key]
          : (!!needle || order.length <= 6 || picked > 0);
        var summary = document.createElement('summary');
        var name = document.createElement('span');
        name.className = 'dir-name';
        name.textContent = key.replace(/ \/ $/, '') || '(bundle root)';
        summary.appendChild(name);
        var count = document.createElement('span');
        count.className = 'muted small';
        count.textContent = picked ? picked + ' of ' + entries.length + ' ticked' : entries.length + ' file' + (entries.length === 1 ? '' : 's');
        summary.appendChild(count);
        details.appendChild(summary);
        var list = document.createElement('ul');
        entries.forEach(function (entry) {
          var item = document.createElement('li');
          var label = document.createElement('label');
          var box = document.createElement('input');
          box.type = 'checkbox';
          box.setAttribute('data-pick', refOf(entry));
          box.checked = !!set[refOf(entry)];
          var path = document.createElement('span');
          path.className = 'pick-path';
          var slash = entry.path.lastIndexOf('/');
          path.textContent = slash === -1 ? entry.path : entry.path.slice(slash + 1);
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
          if (entry.title && entry.title !== path.textContent) {
            var title = document.createElement('span');
            title.className = 'pick-title';
            title.textContent = entry.title;
            label.appendChild(title);
          }
          if (inRule(entry) && !box.checked) {
            var rule = document.createElement('span');
            rule.className = 'in-rule';
            rule.textContent = 'in the rule';
            label.appendChild(rule);
          }
          item.appendChild(label);
          list.appendChild(item);
        });
        details.appendChild(list);
        row.appendChild(all);
        row.appendChild(details);
        li.appendChild(row);
        root.appendChild(li);
      });
      if (order.length) {
        tree.appendChild(root);
      } else if (loaded.length) {
        var none = document.createElement('p');
        none.className = 'browser-empty';
        none.textContent = 'No file matches the filter.';
        tree.appendChild(none);
      }
      if (status) {
        var ticked = Object.keys(set).length;
        status.textContent = (needle ? shown + ' of ' + loaded.length : loaded.length) + ' file'
          + (loaded.length === 1 ? '' : 's') + (ticked ? '; ' + ticked + ' ticked' : '')
          + (truncated ? '; list cut at ' + loaded.length : '');
      }
    };

    var chip = function (kind, label, text, remove) {
      var el = document.createElement('span');
      el.className = 'sel-chip ' + kind;
      if (label) {
        var k = document.createElement('span');
        k.className = 'sel-kind';
        k.textContent = label;
        el.appendChild(k);
      }
      var t = document.createElement('span');
      t.className = 'sel-text';
      t.textContent = text;
      t.title = text;
      el.appendChild(t);
      var x = document.createElement('button');
      x.type = 'button';
      x.className = 'sel-x';
      x.setAttribute('aria-label', 'Remove ' + (label ? label + ' ' : '') + text);
      x.textContent = '×';
      x.addEventListener('click', remove);
      el.appendChild(x);
      return el;
    };
    var removeFromList = function (input, value) {
      input.value = splitList(input.value).filter(function (v) { return v.toLowerCase() !== value.toLowerCase(); }).join(', ');
      fire(input);
    };
    var renderChips = function () {
      if (!chips) return;
      chips.textContent = '';
      var items = [];
      if (scopeAll && scopeAll.checked) {
        items.push(chip('rule', 'everything in', scopeLabel(), function () { scopeAll.checked = false; fire(scopeAll); }));
        splitList(valueOf(typesInput)).forEach(function (t) {
          items.push(chip('rule', 'type', t, function () { removeFromList(typesInput, t); }));
        });
        splitList(valueOf(tagsInput)).forEach(function (t) {
          items.push(chip('rule', 'tag', t, function () { removeFromList(tagsInput, t); }));
        });
        if (valueOf(qInput).trim()) {
          items.push(chip('rule', 'search', qInput.value.trim(), function () { qInput.value = ''; fire(qInput); }));
        }
        if (valueOf(idsInput).trim()) {
          items.push(chip('rule', 'ids', valueOf(idsInput).split('\n').filter(function (v) { return v.trim(); }).length + ' concept id(s)', function () { idsInput.value = ''; fire(idsInput); }));
        }
      }
      if (verifiedBox && verifiedBox.checked) {
        items.push(chip('rule', '', 'verified only', function () { verifiedBox.checked = false; fire(verifiedBox); }));
      }
      Object.keys(pickedSet()).forEach(function (ref) {
        items.push(chip('pick', '', pathOf(ref), function () {
          var next = pickedSet();
          delete next[ref];
          writePicks(next);
        }));
      });
      if (!items.length) {
        var none = document.createElement('span');
        none.className = 'muted';
        none.textContent = 'nothing yet: tick files below, or take everything in a bundle';
        chips.appendChild(none);
      }
      items.forEach(function (item) { chips.appendChild(item); });
      if (clearButton) clearButton.hidden = !items.length;
    };

    var load = function () {
      var token = ++loadToken;
      var ids = [];
      if (bundleSelect) {
        if (bundleSelect.value) {
          ids.push({ id: bundleSelect.value, name: scopeLabel() });
        } else {
          Array.prototype.forEach.call(bundleSelect.options, function (option) {
            if (option.value) ids.push({ id: option.value, name: option.textContent });
          });
        }
      }
      if (status) status.textContent = 'Loading…';
      Promise.all(ids.map(function (bundle) {
        return fetch('/api/bundles/' + encodeURIComponent(bundle.id) + '/tree', { headers: { Accept: 'application/json' } })
          .then(function (response) { return response.ok ? response.json() : { entries: [], truncated: false }; })
          .then(function (data) {
            return { bundle: bundle, entries: data.entries || [], truncated: !!data.truncated };
          });
      })).then(function (results) {
        if (token !== loadToken) return;
        loaded = [];
        truncated = false;
        results.forEach(function (result) {
          truncated = truncated || result.truncated;
          result.entries.forEach(function (entry) {
            entry.bundleId = result.bundle.id;
            entry.bundleName = result.bundle.name;
            loaded.push(entry);
          });
        });
        render();
        renderChips();
      }).catch(function () {
        if (token === loadToken && status) status.textContent = 'The file list could not be loaded.';
      });
    };

    var syncScope = function () {
      if (scopeName) scopeName.textContent = scopeLabel();
      if (narrow && scopeAll) narrow.hidden = !scopeAll.checked;
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
      var next = pickedSet();
      if (box.checked) next[box.getAttribute('data-pick')] = true; else delete next[box.getAttribute('data-pick')];
      writePicks(next);
    });
    if (find) find.addEventListener('input', render);
    if (bundleSelect) bundleSelect.addEventListener('change', function () { openDirs = {}; syncScope(); load(); });
    if (scopeAll) scopeAll.addEventListener('change', function () { syncScope(); render(); renderChips(); });
    if (clearButton) {
      clearButton.addEventListener('click', function () {
        if (scopeAll) scopeAll.checked = false;
        [typesInput, tagsInput, qInput, idsInput].forEach(function (input) { if (input) input.value = ''; });
        if (verifiedBox) verifiedBox.checked = false;
        picks.value = '';
        syncScope();
        fire(picks);
      });
    }
    // Any change to the selection fields redraws the chips and the tree's
    // ticks (the picks field fires change itself when written).
    form.addEventListener('change', function () { render(); renderChips(); });
    form.addEventListener('input', function (event) {
      if (event.target === find) return;
      renderChips();
    });
    syncScope();
    load();
  }

  // ---- top nav: keep the current section in view ---------------------------
  // The strip scrolls horizontally on narrow screens, and every navigation
  // is a full page load that restarts it at its left edge - so the tab the
  // user just tapped seemed to "jump to the front" (it sat off-screen right
  // and had to be scrolled to again). Reveal the active tab by adjusting
  // only the strip's own scrollLeft; scrollIntoView would also scroll
  // ancestor viewports, moving the page itself.
  var topnav = document.querySelector('.topnav');
  if (topnav) {
    var currentSection = topnav.querySelector('a[aria-current=page]');
    if (currentSection) {
      var stripBox = topnav.getBoundingClientRect();
      var tabBox = currentSection.getBoundingClientRect();
      topnav.scrollLeft += tabBox.left - stripBox.left - (stripBox.width - tabBox.width) / 2;
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

  // ---- confirmations and small form helpers --------------------------------
  // A form that must not be sent twice (minting a token, adding a person):
  // its submit buttons are disabled once it is on its way. The page that
  // answers replaces this one, so nothing has to re-enable them.
  document.addEventListener('submit', function (event) {
    var form = event.target.closest('form[data-once]');
    if (!form || event.defaultPrevented) return;
    form.querySelectorAll('button[type=submit]').forEach(function (button) { button.disabled = true; });
  });
  // ...except when the browser brings the very page back from its cache
  // (Back after minting), which restores the disabled state with it.
  window.addEventListener('pageshow', function (event) {
    if (!event.persisted) return;
    document.querySelectorAll('form[data-once] button[type=submit]').forEach(function (button) { button.disabled = false; });
  });
  document.addEventListener('click', function (event) {
    var button = event.target.closest('[data-confirm]');
    if (button && !window.confirm(button.getAttribute('data-confirm'))) event.preventDefault();
    var opener = event.target.closest('[data-open-tab]');
    if (opener) {
      var tab = document.querySelector('[role=tab][data-tab="' + opener.getAttribute('data-open-tab') + '"]');
      if (tab) { event.preventDefault(); tab.click(); tab.scrollIntoView({ block: 'start', behavior: 'smooth' }); }
    }
  });
  var uploadBundle = document.querySelector('[data-upload-bundle]');
  if (uploadBundle) {
    var newBundle = document.querySelector('[data-upload-new]');
    var syncUpload = function () { if (newBundle) newBundle.hidden = uploadBundle.value !== ''; };
    uploadBundle.addEventListener('change', syncUpload);
    syncUpload();
  }

  // ---- sortable tables: click a heading to sort by its column --------------
  document.querySelectorAll('table[data-sortable]').forEach(function (table) {
    var headers = Array.prototype.slice.call(table.querySelectorAll('th[data-sort]'));
    var sortBy = function (th) {
      var index = th.cellIndex;
      var numeric = th.getAttribute('data-sort') === 'num';
      var direction = th.getAttribute('aria-sort') === 'ascending' ? 'descending' : 'ascending';
      headers.forEach(function (other) { other.removeAttribute('aria-sort'); });
      th.setAttribute('aria-sort', direction);
      var body = table.tBodies[0];
      if (!body) return;
      var keyOf = function (row) {
        var cell = row.cells[index];
        if (!cell) return numeric ? 0 : '';
        var raw = cell.hasAttribute('data-value') ? cell.getAttribute('data-value') : cell.textContent;
        return numeric ? (parseFloat(raw) || 0) : raw.trim().toLowerCase();
      };
      var rows = Array.prototype.slice.call(body.rows);
      rows.sort(function (a, b) {
        var ka = keyOf(a), kb = keyOf(b);
        var order = ka < kb ? -1 : (ka > kb ? 1 : 0);
        return direction === 'ascending' ? order : -order;
      });
      rows.forEach(function (row) { body.appendChild(row); });
    };
    headers.forEach(function (th) {
      th.setAttribute('tabindex', '0');
      th.setAttribute('role', 'button');
      th.addEventListener('click', function () { sortBy(th); });
      th.addEventListener('keydown', function (event) {
        if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); sortBy(th); }
      });
    });
  });

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
})();
