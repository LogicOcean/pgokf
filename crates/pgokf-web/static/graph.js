// SPDX-License-Identifier: AGPL-3.0-only
// Interactive 3D graph: the concept page's neighborhood and the catalog-wide
// explorer share this script.
//
// Progressive enhancement over the server-rendered SVG (concept page) or a
// plain note (explorer): when WebGL and the vendored 3d-force-graph bundle
// are available, the graph JSON endpoint is drawn as a force-directed scene
// (d3 engine; no dynamic code, so it runs under the page's
// Content-Security-Policy). Labels and cards are ordinary HTML positioned
// over the canvas, so they stay crisp, theme-aware, and clickable.
(function () {
  'use strict';

  var root = document.getElementById('pgokf-graph');
  if (!root || typeof ForceGraph3D !== 'function') return;

  var canvasHost = root.querySelector('.graph3d-canvas');
  var labelHost = root.querySelector('.graph3d-labels');
  var card = root.querySelector('.graph3d-card');
  var tip = root.querySelector('.graph3d-tip');
  var status = root.querySelector('.graph3d-status');
  var legendBox = root.querySelector('.graph3d-legend');
  var explorer = root.hasAttribute('data-explorer');
  var section = root.parentElement;
  var fallback = section.querySelector('.graph-fallback');
  var form = section.querySelector('form.graph-toolbar');
  var hopsSelect = form ? form.querySelector('select[name=hops]') : null;
  var resetButton = form ? form.querySelector('[data-graph-reset]') : null;
  var hint = form ? form.querySelector('.graph-hint') : null;
  var legendList = document.querySelector('[data-graph-legend]');
  var stats = document.querySelector('[data-graph-stats]');
  var finder = document.querySelector('[data-graph-find]');
  var matches = document.querySelector('[data-graph-matches]');

  var pageSeedUrl = root.getAttribute('data-graph-url');
  var seedUrl = pageSeedUrl;
  var hops = parseInt(root.getAttribute('data-hops'), 10) || 2;
  var graph = null;
  var data = null;
  var labels = new Map();
  // Each label's measured box, by node id.
  var sizes = new Map();
  var dims = 3;
  var frame = null;
  var started = false;
  var fitPending = false;
  var groupColors = new Map();
  var selected = null;
  var highlightLink = null;
  var hoverLink = null;
  var touch = window.matchMedia('(hover: none), (pointer: coarse)').matches;

  var PALETTE = ['#2f6fed', '#e0762e', '#3aa76d', '#c2409a', '#8b6bd6', '#d7b12a', '#2fb5c9', '#b04a4a', '#6b8e23', '#9c7c5c'];

  function escapeHtml(text) {
    return String(text).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }

  function cssVar(name, fallbackValue) {
    var value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    return value || fallbackValue;
  }

  function palette() {
    return {
      background: cssVar('--surface-2', '#f1f3f7'),
      link: cssVar('--graph-link', '#9aa3b2'),
      hops: [
        cssVar('--graph-h0', '#2f6fed'),
        cssVar('--graph-h1', '#3b9ddd'),
        cssVar('--graph-h2', '#4fb8a5'),
        cssVar('--graph-h3', '#8b8fa8'),
      ],
    };
  }

  function colorFor(node, colors) {
    if (data && data.color_by !== 'hops') {
      return groupColors.get(node.group) || colors.hops[3];
    }
    return colors.hops[Math.min(node.hops, colors.hops.length - 1)];
  }

  function sameLink(a, b) {
    return a && b && endId(a.source) === endId(b.source) && endId(a.target) === endId(b.target);
  }
  function linkIsLit(link) { return sameLink(link, highlightLink) || sameLink(link, hoverLink); }
  function somethingIsLit() { return !!(highlightLink || hoverLink); }
  // The lit link's own colour; the rest fade back while one is lit.
  function linkColorFor(link, plain) {
    if (linkIsLit(link)) return cssVar('--graph-glow', '#5c93f5');
    return somethingIsLit() ? cssVar('--graph-link-dim', '#c9cfd8') : plain;
  }
  function nodeIsLit(node) {
    var l = highlightLink || hoverLink;
    return !!l && (endId(l.source) === node.id || endId(l.target) === node.id);
  }
  // Re-run the colour, width, and size accessors so a highlight change is
  // drawn.
  function repaint() {
    if (!graph) return;
    graph.linkColor(graph.linkColor());
    graph.linkWidth(graph.linkWidth());
    graph.nodeColor(graph.nodeColor());
    graph.nodeVal(graph.nodeVal());
    labels.forEach(function (el, id) {
      var node = nodeById(id);
      el.classList.toggle('lit', !!node && nodeIsLit(node));
      el.classList.toggle('dim', !!(highlightLink || hoverLink) && !!node && !nodeIsLit(node));
    });
  }

  function assignGroupColors() {
    groupColors = new Map();
    if (!data || data.color_by === 'hops') return;
    (data.legend || []).forEach(function (group, i) {
      groupColors.set(group, PALETTE[i % PALETTE.length]);
    });
  }

  function renderLegend() {
    if (!data) return;
    var byHops = data.color_by === 'hops';
    if (legendBox) legendBox.hidden = !byHops;
    if (!legendList) return;
    legendList.textContent = '';
    if (byHops) {
      var colors = palette();
      [['this concept', colors.hops[0]], ['1 hop', colors.hops[1]], ['2 hops', colors.hops[2]], ['3+ hops', colors.hops[3]]].forEach(function (entry) {
        legendList.appendChild(legendItem(entry[0], entry[1]));
      });
    } else {
      (data.legend || []).forEach(function (group) {
        legendList.appendChild(legendItem((data.color_by === 'bundle' ? 'bundle ' : '') + group, groupColors.get(group)));
      });
    }
  }

  function legendItem(label, color) {
    var li = document.createElement('li');
    var dot = document.createElement('i');
    dot.style.background = color;
    li.appendChild(dot);
    li.appendChild(document.createTextNode(label));
    return li;
  }

  function renderStats() {
    if (!stats || !data) return;
    var text = data.nodes.length + ' concept' + (data.nodes.length === 1 ? '' : 's') + ', ' + data.links.length + ' link' + (data.links.length === 1 ? '' : 's') + ' drawn';
    if (data.total > data.nodes.length) text += ' (of ' + data.total + ' visible; the best connected are shown)';
    stats.textContent = text + '.';
  }

  function showStatus(message) {
    if (!status) return;
    status.textContent = message;
    status.hidden = !message;
  }

  function setLive(on) {
    root.hidden = !on;
    if (fallback) fallback.hidden = on;
    if (form) form.toggleAttribute('data-live', on);
    if (hint) hint.hidden = !on;
    if (resetButton) resetButton.hidden = !on || seedUrl === pageSeedUrl;
  }

  function fit() {
    if (graph) graph.zoomToFit(500, 48);
  }

  // Tell the renderer how big it is. The camera's aspect - and so where a
  // label lands - follows from this, so it runs on every size change and on
  // the full-screen transition rather than only from the observer.
  function resize() {
    if (!graph) return;
    graph.width(canvasHost.clientWidth).height(canvasHost.clientHeight);
  }

  // ---- projection of world positions to overlay pixels --------------------
  function project(camera, x, y, z, width, height) {
    var v = camera.matrixWorldInverse.elements;
    var p = camera.projectionMatrix.elements;
    var vx = v[0] * x + v[4] * y + v[8] * z + v[12];
    var vy = v[1] * x + v[5] * y + v[9] * z + v[13];
    var vz = v[2] * x + v[6] * y + v[10] * z + v[14];
    var vw = v[3] * x + v[7] * y + v[11] * z + v[15];
    var cx = p[0] * vx + p[4] * vy + p[8] * vz + p[12] * vw;
    var cy = p[1] * vx + p[5] * vy + p[9] * vz + p[13] * vw;
    var cz = p[2] * vx + p[6] * vy + p[10] * vz + p[14] * vw;
    var cw = p[3] * vx + p[7] * vy + p[11] * vz + p[15] * vw;
    if (cw <= 0) return null;
    return { x: (cx / cw * 0.5 + 0.5) * width, y: (-cy / cw * 0.5 + 0.5) * height, depth: cz / cw };
  }

  function labelledNodes(nodes) {
    // Every node when the picture is small; otherwise the seed and its direct
    // neighbors, or in the explorer the best connected (the rest by hover).
    if (nodes.length <= 36) return nodes;
    if (data.color_by === 'hops') return nodes.filter(function (n) { return n.hops <= 1; });
    return nodes.slice().sort(function (a, b) { return b.degree - a.degree; }).slice(0, 36);
  }

  function rebuildLabels() {
    labelHost.textContent = '';
    labels = new Map();
    sizes = new Map();
    if (!data) return;
    labelledNodes(data.nodes).forEach(function (node) {
      var el = document.createElement('a');
      el.className = 'graph3d-label' + (data.seed === node.id ? ' seed' : '');
      el.href = node.href;
      el.textContent = node.title;
      el.title = node.path;
      el.addEventListener('click', function (event) { event.preventDefault(); selectNode(node, true); });
      labelHost.appendChild(el);
      // Measured once, after it is in the document: the text never changes,
      // and reading layout every frame would reflow the page.
      sizes.set(node.id, { w: el.offsetWidth, h: el.offsetHeight });
      labels.set(node.id, el);
    });
  }

  function placeLabels() {
    frame = null;
    if (!graph || root.hidden || root.offsetParent === null) { frame = requestAnimationFrame(placeLabels); return; }
    var camera = graph.camera();
    // The renderer's own dimensions, not the element's: the camera's
    // projection matrix was built from these, and reading the DOM here
    // instead would race the resize - which is what sent every label off
    // into the distance for a frame or more when the aspect changed on
    // entering full screen.
    var width = graph.width() || canvasHost.clientWidth;
    var height = graph.height() || canvasHost.clientHeight;
    var placed = [];
    labels.forEach(function (el, id) {
      var node = nodeById(id);
      if (!node || node.x === undefined) { el.style.display = 'none'; return; }
      var pos = project(camera, node.x, node.y, dims === 3 ? node.z : 0, width, height);
      if (!pos || pos.x < -80 || pos.x > width + 80 || pos.y < -40 || pos.y > height + 40) { el.style.display = 'none'; return; }
      // Two titles printed over each other read as neither, so a label that
      // would land on one already placed stands down. The lit one always
      // gets its place; the rest are in the order they were listed, which
      // is best-connected first.
      var size = sizes.get(id) || { w: 0, h: 0 };
      var box = { l: pos.x - size.w / 2, t: pos.y - 10 - size.h, r: pos.x + size.w / 2, b: pos.y - 10 };
      var lit = nodeIsLit(node);
      if (!lit && placed.some(function (o) { return box.l < o.r && box.r > o.l && box.t < o.b && box.b > o.t; })) {
        el.style.display = 'none';
        return;
      }
      placed.push(box);
      el.style.display = '';
      el.style.transform = 'translate(' + pos.x.toFixed(1) + 'px,' + (pos.y - 10).toFixed(1) + 'px) translate(-50%, -100%)';
      el.style.opacity = String(Math.max(0.35, Math.min(1, 1.6 - pos.depth)));
    });
    frame = requestAnimationFrame(placeLabels);
  }

  function nodeById(id) {
    if (!data) return null;
    for (var i = 0; i < data.nodes.length; i++) if (data.nodes[i].id === id) return data.nodes[i];
    return null;
  }
  function endId(end) { return typeof end === 'object' ? end.id : end; }

  // ---- hover tooltip (own element: the bundle's tooltip positions itself
  // with inline style text, which the page's Content-Security-Policy blocks)
  var pointer = { x: 0, y: 0 };
  canvasHost.addEventListener('mousemove', function (event) {
    var box = root.getBoundingClientRect();
    pointer.x = event.clientX - box.left;
    pointer.y = event.clientY - box.top;
    if (!tip.hidden) placeTip();
  });
  function placeTip() {
    var flip = pointer.x > root.clientWidth * 0.6;
    tip.style.left = (flip ? pointer.x - 12 : pointer.x + 12) + 'px';
    tip.style.top = (pointer.y + 14) + 'px';
    tip.style.transform = flip ? 'translateX(-100%)' : '';
  }
  function showTip(html) {
    if (!html) { tip.hidden = true; return; }
    tip.innerHTML = html;
    tip.hidden = false;
    placeTip();
  }
  function nodeTip(n) {
    return '<strong>' + escapeHtml(n.title) + '</strong>' +
      (n.type ? ' <span class="pill type">' + escapeHtml(n.type) + '</span>' : '') +
      '<div class="muted small">' + escapeHtml(n.bundle_name + ' / ' + n.path) + '</div>';
  }
  function linkTip(l) {
    var from = nodeById(endId(l.source));
    var to = nodeById(endId(l.target));
    var rel = (l.relations || []).filter(function (r) { return r !== 'reference'; });
    return '<div class="small">' + escapeHtml(from ? from.title : endId(l.source)) + ' → ' +
      escapeHtml(to ? to.title : endId(l.target)) + '</div>' +
      '<div class="muted small">' + escapeHtml((rel.length ? rel.join(', ') + ' · ' : '') + l.count + ' link' + (l.count === 1 ? '' : 's') + ' · click to inspect') + '</div>';
  }

  // ---- selection cards -----------------------------------------------------
  function linksOf(node) {
    return data.links.filter(function (l) { return endId(l.source) === node.id || endId(l.target) === node.id; });
  }

  // The node card lists its connections, so an edge can be reached by a
  // tap on a phone where a thin line is hard to hit.
  function connectionsHtml(node) {
    var links = linksOf(node);
    if (!links.length) return '';
    var items = links.slice(0, 12).map(function (l, i) {
      var outgoing = endId(l.source) === node.id;
      var other = nodeById(outgoing ? endId(l.target) : endId(l.source));
      return '<li><button type="button" class="linkish" data-edge="' + i + '">' +
        (outgoing ? '→ ' : '← ') + escapeHtml(other ? other.title : '?') + '</button>' +
        (l.count > 1 ? ' <span class="count">×' + l.count + '</span>' : '') + '</li>';
    }).join('');
    return '<div class="muted small">Connections' + (links.length > 12 ? ' (first 12 of ' + links.length + ')' : '') + '</div><ul class="edge-texts connections">' + items + '</ul>';
  }

  function actions(node) {
    return '<div class="graph3d-actions">' +
      '<a class="btn small" href="' + escapeHtml(node.href) + '">Open</a>' +
      (data.seed === node.id ? '' : '<button type="button" class="btn small ghost" data-explore="' + escapeHtml(node.graph_href) + '">Explore from here</button>') +
      '</div>';
  }

  function selectNode(node, focus) {
    if (!node) { card.hidden = true; return; }
    selected = node;
    highlightLink = null;
    repaint();
    var degree = linksOf(node).length;
    card.innerHTML =
      '<button type="button" class="card-close" aria-label="Close" data-close>×</button>' +
      '<strong>' + escapeHtml(node.title) + '</strong>' +
      (node.type ? ' <span class="pill type">' + escapeHtml(node.type) + '</span>' : '') +
      '<div class="muted small">' + escapeHtml(node.bundle_name + ' / ' + node.path) + ' · ' +
      (data.color_by === 'hops' ? (node.hops === 0 ? 'this concept' : node.hops + ' hop' + (node.hops === 1 ? '' : 's') + ' away') : node.degree + ' link' + (node.degree === 1 ? '' : 's') + ' in the catalog') +
      ' · ' + degree + ' drawn</div>' + actions(node) + connectionsHtml(node);
    card.hidden = false;
    if (focus) focusOn(node);
  }

  function selectLink(link) {
    if (!link) { card.hidden = true; return; }
    var from = nodeById(endId(link.source));
    var to = nodeById(endId(link.target));
    if (!from || !to) return;
    selected = null;
    highlightLink = link;
    repaint();
    var rel = (link.relations || []).filter(function (r) { return r !== 'reference'; });
    var texts = (link.texts || []).map(function (t) { return '<li>' + escapeHtml(t) + '</li>'; }).join('');
    card.innerHTML =
      '<button type="button" class="card-close" aria-label="Close" data-close>×</button>' +
      '<div class="muted small">Link</div>' +
      '<strong>' + escapeHtml(from.title) + '</strong> → <strong>' + escapeHtml(to.title) + '</strong>' +
      '<div class="muted small">' + escapeHtml(link.count + ' link' + (link.count === 1 ? '' : 's') + (rel.length ? ' · ' + rel.join(', ') : '')) + '</div>' +
      (texts ? '<div class="muted small">Link text:</div><ul class="edge-texts">' + texts + '</ul>' : '') +
      '<div class="graph3d-actions">' +
      '<a class="btn small" href="' + escapeHtml(from.href) + '#tab-links">Open source</a>' +
      '<a class="btn small" href="' + escapeHtml(to.href) + '">Open target</a>' +
      '<button type="button" class="btn small ghost" data-explore="' + escapeHtml(to.graph_href) + '">Explore</button>' +
      '</div>';
    card.hidden = false;
    var mid = { x: (from.x + to.x) / 2, y: (from.y + to.y) / 2, z: ((from.z || 0) + (to.z || 0)) / 2 };
    focusOn(mid);
  }

  function focusOn(point) {
    if (!graph || point.x === undefined) return;
    var distance = 140;
    var length = Math.hypot(point.x, point.y, point.z || 0) || 1;
    var ratio = 1 + distance / length;
    graph.cameraPosition(
      dims === 3 ? { x: point.x * ratio, y: point.y * ratio, z: (point.z || 0) * ratio } : { x: point.x, y: point.y, z: 400 },
      point,
      700
    );
  }

  function closeCard() {
    card.hidden = true;
    highlightLink = null;
    selected = null;
    repaint();
  }

  card.addEventListener('click', function (event) {
    if (event.target.closest('[data-close]')) { closeCard(); return; }
    var edge = event.target.closest('[data-edge]');
    if (edge && selected) {
      var link = linksOf(selected)[parseInt(edge.getAttribute('data-edge'), 10)];
      if (link) selectLink(link);
      return;
    }
    var explore = event.target.closest('[data-explore]');
    if (!explore) return;
    seedUrl = explore.getAttribute('data-explore');
    load();
  });

  // ---- zoom, center, full screen -------------------------------------------
  function zoom(factor) {
    if (!graph) return;
    var pos = graph.cameraPosition();
    var target = graph.controls().target;
    var next = {
      x: target.x + (pos.x - target.x) * factor,
      y: target.y + (pos.y - target.y) * factor,
      z: target.z + (pos.z - target.z) * factor,
    };
    graph.cameraPosition(next, target, 300);
  }
  root.querySelectorAll('[data-graph-zoom]').forEach(function (button) {
    button.addEventListener('click', function () { zoom(button.getAttribute('data-graph-zoom') === 'in' ? 0.7 : 1.45); });
  });
  root.querySelectorAll('[data-graph-fit]').forEach(function (button) { button.addEventListener('click', fit); });
  root.querySelectorAll('[data-graph-dims]').forEach(function (button) {
    button.addEventListener('click', function () {
      if (!graph) return;
      dims = dims === 3 ? 2 : 3;
      graph.numDimensions(dims);
      button.textContent = dims === 3 ? '2D' : '3D';
      fitPending = true;
      setTimeout(fit, 700);
    });
  });
  root.querySelectorAll('[data-graph-fullscreen]').forEach(function (button) {
    button.addEventListener('click', function () {
      // Refit whichever way the box changes shape: `fullscreenchange` does
      // not fire for the class-toggle fallback.
      var settle = function () { setTimeout(function () { resize(); fit(); }, 400); };
      if (document.fullscreenElement === root) { document.exitFullscreen(); settle(); return; }
      if (root.requestFullscreen) {
        root.requestFullscreen().catch(function () { root.classList.toggle('fullscreen'); });
      } else {
        root.classList.toggle('fullscreen');
      }
      settle();
    });
  });
  document.addEventListener('fullscreenchange', function () {
    // Size the renderer at once: the observer would get there, but a frame
    // drawn with the old aspect misplaces every label until it does.
    resize();
    setTimeout(function () { resize(); fit(); }, 400);
  });
  document.addEventListener('keydown', function (event) {
    if (root.hidden || root.offsetParent === null) return;
    var tag = event.target && event.target.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;
    if (event.key === '+' || event.key === '=') zoom(0.7);
    else if (event.key === '-') zoom(1.45);
    else if (event.key === 'Escape') { closeCard(); }
  });

  // ---- finder (explorer) ----------------------------------------------------
  if (finder && matches) {
    finder.addEventListener('input', function () {
      var needle = finder.value.trim().toLowerCase();
      matches.textContent = '';
      if (!data || needle.length < 2) return;
      data.nodes.filter(function (n) {
        return n.title.toLowerCase().indexOf(needle) !== -1 || n.concept_id.toLowerCase().indexOf(needle) !== -1;
      }).slice(0, 12).forEach(function (n) {
        var li = document.createElement('li');
        li.textContent = n.title;
        li.title = n.bundle_name + ' / ' + n.path;
        li.addEventListener('click', function () { selectNode(n, true); });
        matches.appendChild(li);
      });
    });
  }

  // ---- data ----------------------------------------------------------------
  function load() {
    showStatus('Loading the graph…');
    card.hidden = true;
    var url = seedUrl + (seedUrl.indexOf('?') === -1 ? '?' : '&') + 'hops=' + hops;
    return fetch(url, { headers: { Accept: 'application/json' } })
      .then(function (response) {
        if (!response.ok) throw new Error('HTTP ' + response.status);
        return response.json();
      })
      .then(function (json) {
        data = json;
        assignGroupColors();
        if (!graph) build();
        var colors = palette();
        highlightLink = null;
        hoverLink = null;
        graph.nodeColor(function (n) { return colorFor(n, colors); });
        graph.graphData({ nodes: data.nodes, links: data.links });
        rebuildLabels();
        renderLegend();
        renderStats();
        setLive(true);
        showStatus(data.nodes.length <= 1 ? 'No resolved links to draw.' : '');
        if (!frame) frame = requestAnimationFrame(placeLabels);
        fitPending = true;
        setTimeout(fit, 700);
        // Disconnected components keep drifting apart for a while.
        if (explorer) setTimeout(fit, 2500);
      })
      .catch(function (error) {
        if (!graph) {
          root.hidden = true;
          if (fallback) fallback.hidden = false;
        } else {
          showStatus('The graph could not be loaded (' + error.message + ').');
        }
      });
  }

  function build() {
    var colors = palette();
    graph = ForceGraph3D({ controlType: 'orbit' })(canvasHost)
      .forceEngine('d3')
      .backgroundColor(colors.background)
      .showNavInfo(false)
      .nodeId('id')
      .nodeLabel(function () { return ''; })
      .onNodeHover(function (node) { showTip(node ? nodeTip(node) : ''); })
      .onLinkHover(function (link) {
        hoverLink = link || null;
        repaint();
        showTip(link ? linkTip(link) : '');
      })
      .nodeColor(function (n) { return colorFor(n, colors); })
      .nodeVal(function (n) {
        // A lit node keeps its colour and grows instead.
        var boost = nodeIsLit(n) ? 1.8 : 1;
        if (data && data.color_by !== 'hops') return boost * (2 + Math.min(10, n.degree * 0.6));
        return boost * (n.hops === 0 ? 12 : n.hops === 1 ? 5 : 2.5);
      })
      .nodeResolution(16)
      .nodeOpacity(0.95)
      // A lit link glows: a wide halo, and every other link recedes, so the
      // highlight reads by contrast rather than by hue and never looks like
      // one of the node colours. Nothing moves.
      .linkColor(function (l) { return linkColorFor(l, colors.link); })
      .linkOpacity(0.75)
      .linkWidth(function (l) { return (linkIsLit(l) ? 6 : 0) + Math.min(3, (touch ? 1.2 : 0.5) + l.count * 0.5); })
      .linkHoverPrecision(touch ? 8 : 4)
      .linkDirectionalArrowLength(4)
      .linkDirectionalArrowRelPos(1)
      .linkLabel(function () { return ''; })
      .onNodeClick(function (node) { selectNode(node, true); })
      .onLinkClick(function (link) { selectLink(link); })
      .onNodeRightClick(function (node) { window.location.href = node.href; })
      .onBackgroundClick(closeCard)
      .warmupTicks(60)
      .cooldownTicks(200)
      .onEngineStop(function () { if (fitPending) { fitPending = false; fit(); } });
    // Repulsion with a reach limit keeps disconnected bundles from pushing
    // each other out of frame in the catalog-wide picture.
    graph.d3Force('charge').strength(explorer ? -140 : -210).distanceMax(explorer ? 300 : 520);
    graph.d3Force('link').distance(function (l) { return 60 + Math.min(45, l.count * 4.5); });
    canvasHost.addEventListener('dblclick', function () {
      if (selected && !card.hidden) window.location.href = selected.href;
    });

    new ResizeObserver(resize).observe(canvasHost);

    new MutationObserver(function () {
      var next = palette();
      graph.backgroundColor(next.background);
      graph.nodeColor(function (n) { return colorFor(n, next); });
      graph.linkColor(function (l) { return linkColorFor(l, next.link); });
      renderLegend();
    }).observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
  }

  // ---- controls ------------------------------------------------------------
  if (hopsSelect) {
    hopsSelect.addEventListener('change', function () {
      if (!form.hasAttribute('data-live')) return;
      hops = parseInt(hopsSelect.value, 10) || hops;
      load();
    });
  }
  if (resetButton) resetButton.addEventListener('click', function () { seedUrl = pageSeedUrl; load(); });

  // ---- start when the container is first visible ----------------------------
  function visible() { return section.offsetParent !== null; }
  function start() {
    if (started) return;
    started = true;
    try {
      root.hidden = false;
      load();
    } catch (error) {
      root.hidden = true;
      if (fallback) fallback.hidden = false;
    }
  }
  if (visible()) {
    start();
  } else {
    var tabs = document.querySelector('[data-tabs]');
    if (tabs) tabs.addEventListener('click', function () { if (visible()) setTimeout(start, 0); });
    window.addEventListener('hashchange', function () { if (visible()) start(); });
  }
})();

// Bundle picker: filter the explorer's checkbox list, keep a running count.
// Runs on its own so it still works when WebGL or the graph bundle is
// unavailable; without JavaScript every box stays visible and submits.
(function () {
  'use strict';

  var input = document.querySelector('[data-bundle-filter]');
  var list = document.querySelector(input ? input.getAttribute('data-bundle-filter') : null);
  if (!input || !list) return;

  var items = Array.prototype.slice.call(list.querySelectorAll('li[data-filter-text]'));
  var boxes = list.querySelectorAll('input[type=checkbox][name=bundle]');
  var empty = list.querySelector('[data-bundle-empty]');
  var count = document.querySelector('[data-bundle-count]');
  var clearButton = document.querySelector('[data-bundle-clear]');
  var visibleButton = document.querySelector('[data-bundle-visible]');

  function updateCount() {
    if (!count) return;
    var picked = list.querySelectorAll('input[type=checkbox][name=bundle]:checked').length;
    count.textContent = picked + ' of ' + boxes.length + ' bundles selected';
    if (clearButton) clearButton.disabled = picked === 0;
  }

  // The filter only hides rows; it never touches what is checked.
  function applyFilter() {
    var needle = input.value.trim().toLowerCase();
    var shown = 0;
    items.forEach(function (item) {
      var match = needle === '' || item.getAttribute('data-filter-text').toLowerCase().indexOf(needle) !== -1;
      item.hidden = !match;
      if (match) shown += 1;
    });
    if (empty) empty.hidden = shown !== 0;
    if (visibleButton) visibleButton.disabled = shown === 0;
  }

  input.addEventListener('input', applyFilter);
  list.addEventListener('change', updateCount);
  if (clearButton) {
    clearButton.addEventListener('click', function () {
      boxes.forEach(function (box) { box.checked = false; });
      updateCount();
    });
  }
  if (visibleButton) {
    visibleButton.addEventListener('click', function () {
      items.forEach(function (item) {
        if (item.hidden) return;
        var box = item.querySelector('input[type=checkbox]');
        if (box) box.checked = true;
      });
      updateCount();
    });
  }

  applyFilter();
  updateCount();
})();

// Type-group picker: the clear affordance, mirroring the bundle picker. The
// list is short enough to need no filter box; without JavaScript the boxes
// are still unchecked by hand and submit as before.
(function () {
  'use strict';

  var clear = document.querySelector('[data-type-clear]');
  if (!clear) return;
  clear.addEventListener('click', function () {
    document
      .querySelectorAll('#g-type-list input[type=checkbox]')
      .forEach(function (box) { box.checked = false; });
  });
})();
