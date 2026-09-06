// Interactive 3D neighborhood graph for the concept page.
//
// Progressive enhancement over the server-rendered SVG: when WebGL and the
// vendored 3d-force-graph bundle are available, the graph JSON endpoint is
// drawn as a force-directed scene (d3 engine; no dynamic code, so it runs
// under the page's Content-Security-Policy). Labels are ordinary HTML
// positioned over the canvas each frame, so they stay crisp, theme-aware,
// and clickable. Without any of this the SVG and the hops form still work.
(function () {
  'use strict';

  var root = document.getElementById('pgokf-graph');
  if (!root || typeof ForceGraph3D !== 'function') return;

  var canvasHost = root.querySelector('.graph3d-canvas');
  var labelHost = root.querySelector('.graph3d-labels');
  var card = root.querySelector('.graph3d-card');
  var tip = root.querySelector('.graph3d-tip');
  var status = root.querySelector('.graph3d-status');
  var fallback = root.parentElement.querySelector('.graph-fallback');
  var form = root.parentElement.querySelector('form.graph-toolbar');
  var hopsSelect = form ? form.querySelector('select[name=hops]') : null;
  var fitButton = form ? form.querySelector('[data-graph-fit]') : null;
  var dimsButton = form ? form.querySelector('[data-graph-dims]') : null;
  var resetButton = form ? form.querySelector('[data-graph-reset]') : null;
  var hint = form ? form.querySelector('.graph-hint') : null;

  var pageSeedUrl = root.getAttribute('data-graph-url');
  var seedUrl = pageSeedUrl;
  var hops = parseInt(root.getAttribute('data-hops'), 10) || 2;
  var graph = null;
  var data = null;
  var labels = new Map();
  var dims = 3;
  var frame = null;
  var started = false;
  var fitPending = false;

  function fit() {
    if (graph) graph.zoomToFit(500, 48);
  }

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
    return colors.hops[Math.min(node.hops, colors.hops.length - 1)];
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
    [fitButton, dimsButton, hint].forEach(function (el) { if (el) el.hidden = !on; });
    if (resetButton) resetButton.hidden = !on || seedUrl === pageSeedUrl;
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
    return {
      x: (cx / cw * 0.5 + 0.5) * width,
      y: (-cy / cw * 0.5 + 0.5) * height,
      depth: cz / cw,
    };
  }

  function labelledNodes(nodes) {
    // Every node when the picture is small; otherwise the seed and its
    // direct neighbors (the rest are reachable by hovering).
    return nodes.length <= 36 ? nodes : nodes.filter(function (n) { return n.hops <= 1; });
  }

  function rebuildLabels() {
    labelHost.textContent = '';
    labels = new Map();
    if (!data) return;
    labelledNodes(data.nodes).forEach(function (node) {
      var el = document.createElement('a');
      el.className = 'graph3d-label' + (node.hops === 0 ? ' seed' : '');
      el.href = node.href;
      el.textContent = node.title;
      el.title = node.path;
      el.addEventListener('click', function (event) {
        event.preventDefault();
        select(node, true);
      });
      labelHost.appendChild(el);
      labels.set(node.id, el);
    });
  }

  function placeLabels() {
    frame = null;
    if (!graph || root.hidden || root.offsetParent === null) { frame = requestAnimationFrame(placeLabels); return; }
    var camera = graph.camera();
    var width = canvasHost.clientWidth;
    var height = canvasHost.clientHeight;
    labels.forEach(function (el, id) {
      var node = nodeById(id);
      if (!node || node.x === undefined) { el.style.display = 'none'; return; }
      var pos = project(camera, node.x, node.y, dims === 3 ? node.z : 0, width, height);
      if (!pos || pos.x < -80 || pos.x > width + 80 || pos.y < -40 || pos.y > height + 40) {
        el.style.display = 'none';
        return;
      }
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
    var width = root.clientWidth;
    var flip = pointer.x > width * 0.6;
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
  function endId(end) { return typeof end === 'object' ? end.id : end; }
  function nodeTip(n) {
    return '<strong>' + escapeHtml(n.title) + '</strong>' +
      (n.type ? ' <span class="pill type">' + escapeHtml(n.type) + '</span>' : '') +
      '<div class="muted small">' + escapeHtml(n.path) + '</div>';
  }
  function linkTip(l) {
    var from = nodeById(endId(l.source));
    var to = nodeById(endId(l.target));
    var rel = (l.relations || []).filter(function (r) { return r !== 'reference'; });
    return '<div class="small">' + escapeHtml(from ? from.title : endId(l.source)) + ' → ' +
      escapeHtml(to ? to.title : endId(l.target)) + '</div>' +
      '<div class="muted small">' + escapeHtml((rel.length ? rel.join(', ') + ' · ' : '') +
      l.count + ' link' + (l.count === 1 ? '' : 's')) + '</div>';
  }

  // ---- selection card ------------------------------------------------------
  function select(node, focus) {
    if (!node) { card.hidden = true; return; }
    var degree = data.links.filter(function (l) {
      return endId(l.source) === node.id || endId(l.target) === node.id;
    }).length;
    card.innerHTML =
      '<strong>' + escapeHtml(node.title) + '</strong>' +
      (node.type ? ' <span class="pill type">' + escapeHtml(node.type) + '</span>' : '') +
      '<div class="muted small">' + escapeHtml(node.path) + ' · ' +
      (node.hops === 0 ? 'this concept' : node.hops + ' hop' + (node.hops === 1 ? '' : 's') + ' away') +
      ' · ' + degree + ' link' + (degree === 1 ? '' : 's') + '</div>' +
      '<div class="graph3d-actions">' +
      '<a class="btn small" href="' + escapeHtml(node.href) + '">Open</a>' +
      (node.hops === 0 ? '' : '<button type="button" class="btn small ghost" data-explore="' + escapeHtml(node.graph_href) + '">Explore from here</button>') +
      '</div>';
    card.hidden = false;
    if (focus && graph && node.x !== undefined) {
      var distance = 140;
      var length = Math.hypot(node.x, node.y, node.z || 0) || 1;
      var ratio = 1 + distance / length;
      graph.cameraPosition(
        dims === 3 ? { x: node.x * ratio, y: node.y * ratio, z: node.z * ratio } : { x: node.x, y: node.y, z: 400 },
        node,
        700
      );
    }
  }

  card.addEventListener('click', function (event) {
    var explore = event.target.closest('[data-explore]');
    if (!explore) return;
    seedUrl = explore.getAttribute('data-explore');
    load();
  });

  // ---- data ----------------------------------------------------------------
  function load() {
    showStatus('Loading the graph…');
    card.hidden = true;
    return fetch(seedUrl + '?hops=' + hops, { headers: { Accept: 'application/json' } })
      .then(function (response) {
        if (!response.ok) throw new Error('HTTP ' + response.status);
        return response.json();
      })
      .then(function (json) {
        data = json;
        if (!graph) build();
        graph.graphData({ nodes: data.nodes, links: data.links });
        rebuildLabels();
        setLive(true);
        showStatus(data.nodes.length <= 1 ? 'No resolved links within ' + hops + ' hop' + (hops === 1 ? '' : 's') + '.' : '');
        if (!frame) frame = requestAnimationFrame(placeLabels);
        fitPending = true;
        // Small pictures settle before the engine reports it; fit early too.
        setTimeout(fit, 700);
      })
      .catch(function (error) {
        if (!graph) {
          // Nothing drawn yet: leave the server-rendered picture in place.
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
      .onLinkHover(function (link) { showTip(link ? linkTip(link) : ''); })
      .nodeColor(function (n) { return colorFor(n, colors); })
      .nodeVal(function (n) { return n.hops === 0 ? 12 : n.hops === 1 ? 5 : 2.5; })
      .nodeResolution(16)
      .nodeOpacity(0.95)
      .linkColor(function () { return colors.link; })
      .linkOpacity(0.55)
      .linkWidth(function (l) { return Math.min(3, 0.5 + l.count * 0.5); })
      .linkDirectionalArrowLength(4)
      .linkDirectionalArrowRelPos(1)
      .linkLabel(function () { return ''; })
      .onNodeClick(function (node) { select(node, true); })
      .onNodeRightClick(function (node) { window.location.href = node.href; })
      .onBackgroundClick(function () { card.hidden = true; })
      .warmupTicks(60)
      .cooldownTicks(200)
      .onEngineStop(function () {
        if (fitPending) { fitPending = false; fit(); }
      });
    graph.d3Force('charge').strength(-140);
    graph.d3Force('link').distance(function (l) { return 40 + Math.min(40, l.count * 4); });
    canvasHost.addEventListener('dblclick', function () {
      // The last clicked node is the card's subject; double-click opens it.
      var open = card.querySelector('a.btn');
      if (open && !card.hidden) window.location.href = open.getAttribute('href');
    });

    new ResizeObserver(function () {
      if (!graph) return;
      graph.width(canvasHost.clientWidth).height(canvasHost.clientHeight);
    }).observe(canvasHost);

    new MutationObserver(function () {
      var next = palette();
      graph.backgroundColor(next.background);
      graph.nodeColor(function (n) { return colorFor(n, next); });
      graph.linkColor(function () { return next.link; });
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
  if (fitButton) fitButton.addEventListener('click', fit);
  if (dimsButton) dimsButton.addEventListener('click', function () {
    if (!graph) return;
    dims = dims === 3 ? 2 : 3;
    graph.numDimensions(dims);
    dimsButton.textContent = dims === 3 ? '2D' : '3D';
    fitPending = true;
    setTimeout(fit, 700);
  });
  if (resetButton) resetButton.addEventListener('click', function () { seedUrl = pageSeedUrl; load(); });

  // ---- start when the panel is first visible --------------------------------
  function visible() { return root.parentElement.offsetParent !== null; }
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
