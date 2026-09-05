/*
 * MCP / tool governance — portal extension.
 *
 * This file is a self-injecting overlay for the CCAG admin SPA. It adds its own nav
 * entry, page, and modal at runtime instead of being written into static/index.html,
 * so the upstream portal file needs exactly one line (the <script> tag that loads
 * this) and rebasing onto a new CCAG release stays trivial.
 *
 * It reuses the SPA's existing globals rather than shipping its own: api(), toast(),
 * esc(), showModal(), closeModal(), fmtDate(), and navigate().
 */
(function () {
  'use strict';

  var PAGE = 'mcpgov';
  var state = {
    tab: 'summary',
    summary: null,
    servers: [],
    tools: [],
    policies: [],
    events: [],
    expanded: {},      // server_name -> bool
    editing: null,     // policy being edited
    sim: null,         // last simulation result
    loaded: false
  };

  // ---------------------------------------------------------------------------
  // Small helpers. Defined defensively: if the host SPA ever renames one of its
  // globals, the extension degrades instead of throwing.
  // ---------------------------------------------------------------------------

  function esc(s) {
    if (window.esc) return window.esc(s);
    var d = document.createElement('div');
    d.textContent = s == null ? '' : String(s);
    return d.innerHTML;
  }

  /**
   * Attribute-safe escaping.
   *
   * The host SPA's esc() goes through textContent -> innerHTML, which escapes &, <
   * and > but NOT quotes, because quotes need no escaping in element content. That
   * makes it unsafe for attribute values: a name containing a double quote would end
   * the attribute early and could inject markup. Since MCP server and tool names
   * arrive from client-supplied tool definitions, they are untrusted input.
   */
  function attr(s) {
    return String(s == null ? '' : s)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  function toast(msg, type) {
    if (window.toast) return window.toast(msg, type);
    console.log('[mcpgov]', type || 'info', msg);
  }

  function api(method, path, body) {
    if (!window.api) return Promise.resolve({ error: 'portal api() unavailable' });
    return window.api(method, path, body);
  }

  function fmtDate(s) {
    if (!s) return '-';
    if (window.fmtDate) return window.fmtDate(s);
    try { return new Date(s).toLocaleString(); } catch (e) { return String(s); }
  }

  function fmtNum(n) {
    if (n == null) return '0';
    if (window.fmtNum) return window.fmtNum(n);
    return String(n);
  }

  function statusBadge(status) {
    var cls = 'badge-muted';
    if (status === 'approved') cls = 'badge-green';
    else if (status === 'denied') cls = 'badge-red';
    else if (status === 'pending' || status === 'inherit') cls = 'badge-yellow';
    return '<span class="badge ' + cls + '">' + esc(status) + '</span>';
  }

  function modeBadge(mode) {
    var cls = mode === 'enforce' ? 'badge-red' : (mode === 'warn' ? 'badge-yellow' : 'badge-blue');
    return '<span class="badge ' + cls + '">' + esc(mode) + '</span>';
  }

  // ---------------------------------------------------------------------------
  // Markup injection
  // ---------------------------------------------------------------------------

  var NAV_HTML =
    '<div class="nav-item" data-page="' + PAGE + '" onclick="navigate(\'' + PAGE + '\')">' +
      '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">' +
        '<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"></path>' +
        '<path d="M9 12l2 2 4-4"></path>' +
      '</svg>' +
      ' MCP &amp; Tools' +
    '</div>';

  var PAGE_HTML =
    '<div id="page-' + PAGE + '" class="page">' +
      '<div class="main-header">' +
        '<h2>MCP &amp; Tools Governance</h2>' +
        '<div style="display:flex;gap:8px">' +
          '<button class="btn btn-secondary btn-sm" onclick="mcpgovRefresh()">Refresh</button>' +
          '<button class="btn btn-primary btn-sm" onclick="mcpgovNewPolicy()">New Policy</button>' +
        '</div>' +
      '</div>' +
      '<div class="main-content">' +
        '<div class="info-callout" style="margin-bottom:16px">' +
          '<div class="info-callout-body">' +
            "<strong>How this works.</strong> Claude Code's own MCP controls are client-side, and " +
            'Anthropic\u2019s server-managed settings are skipped for any session using a custom ' +
            '<code>ANTHROPIC_BASE_URL</code> \u2014 which is how CCAG is used. So policy is applied here, ' +
            'on the request path: denied tool definitions are stripped before the request reaches ' +
            'Bedrock, with no client configuration and no way to bypass it locally.' +
            '<br><br>' +
            'Start in <strong>observe</strong> mode to build the catalog from real traffic, ' +
            'approve what belongs, then switch to <strong>enforce</strong>.' +
          '</div>' +
        '</div>' +

        '<div class="oa-tab-bar" id="mcpgov-tabs">' +
          '<button class="oa-tab-btn" data-tab="summary" onclick="mcpgovTab(\'summary\')">Overview</button>' +
          '<button class="oa-tab-btn" data-tab="catalog" onclick="mcpgovTab(\'catalog\')">Catalog</button>' +
          '<button class="oa-tab-btn" data-tab="policies" onclick="mcpgovTab(\'policies\')">Policies</button>' +
          '<button class="oa-tab-btn" data-tab="simulate" onclick="mcpgovTab(\'simulate\')">Simulator</button>' +
          '<button class="oa-tab-btn" data-tab="audit" onclick="mcpgovTab(\'audit\')">Audit</button>' +
        '</div>' +

        '<div id="mcpgov-body" style="margin-top:16px"><div class="oa-loading">Loading\u2026</div></div>' +
      '</div>' +
    '</div>';

  var MODAL_HTML =
    '<div id="modal-mcpgov-policy" class="modal-overlay" style="display:none" onclick="if(event.target===this)closeModal()">' +
      '<div class="modal">' +
        '<div class="modal-header"><h3 id="mcpgov-modal-title">New Policy</h3></div>' +
        '<div class="modal-body">' +
          '<div class="form-row">' +
            '<div class="form-group">' +
              '<label class="form-label">Scope</label>' +
              '<select class="form-input" id="mcpgov-p-scope" onchange="mcpgovScopeChanged()">' +
                '<option value="global">Global (applies to everyone)</option>' +
                '<option value="team">Team</option>' +
                '<option value="user">User</option>' +
              '</select>' +
            '</div>' +
            '<div class="form-group" id="mcpgov-p-ref-group" style="display:none">' +
              '<label class="form-label" id="mcpgov-p-ref-label">Team</label>' +
              '<input class="form-input" id="mcpgov-p-ref" placeholder="team UUID or user email">' +
              '<div class="form-hint" id="mcpgov-p-ref-hint"></div>' +
            '</div>' +
          '</div>' +

          '<div class="form-row">' +
            '<div class="form-group">' +
              '<label class="form-label">Mode</label>' +
              '<select class="form-input" id="mcpgov-p-mode">' +
                '<option value="observe">observe \u2014 record only, never modify requests</option>' +
                '<option value="warn">warn \u2014 allow, but record as a violation</option>' +
                '<option value="enforce">enforce \u2014 strip denied tools</option>' +
              '</select>' +
            '</div>' +
            '<div class="form-group">' +
              '<label class="form-label">Default for unknown tools</label>' +
              '<select class="form-input" id="mcpgov-p-default">' +
                '<option value="allow">allow \u2014 permissive (good for rollout)</option>' +
                '<option value="deny">deny \u2014 allowlist posture</option>' +
              '</select>' +
            '</div>' +
          '</div>' +

          '<div class="form-group">' +
            '<label class="form-label">Applies to</label>' +
            '<select class="form-input" id="mcpgov-p-applies">' +
              '<option value="mcp_only">MCP tools only \u2014 never touches Read/Write/Bash</option>' +
              '<option value="all_tools">All tools \u2014 includes built-ins</option>' +
            '</select>' +
            '<div class="form-hint">Leave on <code>mcp_only</code> unless you specifically need to govern built-in tools. Governing all tools can break basic Claude Code use if misconfigured.</div>' +
          '</div>' +

          '<div class="form-group">' +
            '<label class="form-label">Deny patterns</label>' +
            '<textarea class="form-input" id="mcpgov-p-deny" rows="4" placeholder="mcp__github__delete_*&#10;mcp__risky__*"></textarea>' +
            '<div class="form-hint">One glob per line. Deny always wins \u2014 nothing overrides a match. Denies from broader scopes are <strong>unioned</strong> in, so a global deny cannot be dropped by a narrower policy.</div>' +
          '</div>' +

          '<div class="form-group">' +
            '<label class="form-label">Allow patterns</label>' +
            '<textarea class="form-input" id="mcpgov-p-allow" rows="4" placeholder="mcp__github__*&#10;mcp__jira__*"></textarea>' +
            '<div class="form-hint">One glob per line. If non-empty this becomes an <strong>exclusive allowlist</strong>: anything not matching is denied. A narrower scope\u2019s allowlist <strong>replaces</strong> a broader one. Leave empty to fall back to the catalog.</div>' +
          '</div>' +

          '<div class="form-group">' +
            '<label class="form-label"><input type="checkbox" id="mcpgov-p-enabled" checked> Enabled</label>' +
          '</div>' +
        '</div>' +
        '<div class="modal-footer">' +
          '<button class="btn btn-secondary" onclick="closeModal()">Cancel</button>' +
          '<button class="btn btn-primary" onclick="mcpgovSavePolicy()">Save</button>' +
        '</div>' +
      '</div>' +
    '</div>';

  /**
   * Delegated click routing for rows rendered from untrusted data.
   *
   * Row actions carry `data-act` plus their arguments as data attributes instead of
   * inline `onclick="fn('value')"`. Interpolating a name into an inline handler means
   * escaping it for two nested contexts (HTML attribute, then JavaScript string), which
   * is easy to get wrong and was in fact wrong here: quotes in a name broke the
   * attribute. Data attributes need only attribute escaping, and the value reaches the
   * handler as a plain string that is never parsed as code.
   */
  function onDelegatedClick(ev) {
    var target = ev.target;
    if (!target || typeof target.closest !== 'function') return;
    var el = target.closest('[data-act]');
    if (!el) return;

    var act = el.getAttribute('data-act');
    var name = el.getAttribute('data-name');
    var status = el.getAttribute('data-status');
    var id = el.getAttribute('data-id');

    if (act === 'toggle') window.mcpgovToggle(name);
    else if (act === 'server-status') window.mcpgovSetServer(name, status);
    else if (act === 'tool-status') window.mcpgovSetTool(name, status);
    else if (act === 'bulk') window.mcpgovBulk(name, status);
    else if (act === 'enable-enforce') window.mcpgovEnableEnforce();
    else if (act === 'policy-edit') window.mcpgovEditPolicy(id);
    else if (act === 'policy-delete') {
      window.mcpgovDeletePolicy(id, el.getAttribute('data-label') || id);
    }
  }

  function inject() {
    if (document.getElementById('page-' + PAGE)) return true;

    var navSection = document.getElementById('nav-admin-section');
    var main = document.querySelector('.main');
    if (!navSection || !main) return false;

    navSection.insertAdjacentHTML('beforeend', NAV_HTML);
    main.insertAdjacentHTML('beforeend', PAGE_HTML);
    document.body.insertAdjacentHTML('beforeend', MODAL_HTML);

    var pageEl = document.getElementById('page-' + PAGE);
    if (pageEl && pageEl.addEventListener) {
      pageEl.addEventListener('click', onDelegatedClick);
    }
    return true;
  }

  // ---------------------------------------------------------------------------
  // Rendering
  // ---------------------------------------------------------------------------

  function setTabActive() {
    var btns = document.querySelectorAll('#mcpgov-tabs .oa-tab-btn');
    for (var i = 0; i < btns.length; i++) {
      var active = btns[i].getAttribute('data-tab') === state.tab;
      btns[i].classList.toggle('active', active);
    }
  }

  function render() {
    var el = document.getElementById('mcpgov-body');
    if (!el) return;
    setTabActive();

    if (state.tab === 'summary') el.innerHTML = renderSummary();
    else if (state.tab === 'catalog') el.innerHTML = renderCatalog();
    else if (state.tab === 'policies') el.innerHTML = renderPolicies();
    else if (state.tab === 'simulate') el.innerHTML = renderSimulator();
    else if (state.tab === 'audit') el.innerHTML = renderAudit();
  }

  /**
   * Warns when the catalog says "denied" but the mode says "do nothing".
   *
   * Denying a server or tool writes a catalog row; it does not by itself change the
   * enforcement mode. In any mode other than `enforce` those rows are inert, so the
   * portal reports blocks that never happen and the client keeps calling the tool.
   * That mismatch is silent and looks exactly like a broken filter, so it gets a loud
   * banner and a one-click fix rather than a footnote.
   *
   * Returns '' when there is nothing to warn about.
   */
  function enforcementGapBanner() {
    var s = state.summary;
    if (!s) return '';
    var sum = s.summary || {};
    var gp = s.global_policy || {};
    var mode = gp.mode || 'observe';
    if (mode === 'enforce') return '';

    var denials = (sum.tools_denied || 0) + (sum.servers_denied || 0) +
      ((gp.deny_patterns || []).length);
    if (denials === 0) return '';

    var what = mode === 'observe'
      ? 'recorded as "would block" and then sent through untouched'
      : 'flagged in the audit trail and then sent through untouched';

    return '<div style="display:flex;margin-bottom:16px;padding:10px 14px;' +
      'background:var(--yellow-subtle);border:1px solid var(--yellow);' +
      'border-radius:var(--radius);align-items:center;gap:10px;font-size:13px">' +
      '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="var(--yellow)" ' +
        'stroke-width="2" style="flex-shrink:0"><path d="M10.29 3.86L1.82 18a2 2 0 0 0 ' +
        '1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/>' +
        '<line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>' +
      '<span style="flex:1;color:var(--yellow)">' +
        '<strong>Nothing is being blocked.</strong> You have ' + fmtNum(denials) +
        ' denial' + (denials === 1 ? '' : 's') + ' configured, but the mode is <strong>' +
        esc(mode) + '</strong> \u2014 denied tools are ' + what + '.' +
      '</span>' +
      '<button class="btn btn-sm" data-act="enable-enforce" ' +
        'style="background:var(--yellow-subtle);color:var(--yellow);' +
        'border:1px solid var(--yellow);font-weight:600;white-space:nowrap">' +
        'Switch to enforce' +
      '</button>' +
    '</div>';
  }

  function renderSummary() {
    var s = state.summary;
    if (!s) return '<div class="oa-loading">Loading\u2026</div>';
    var sum = s.summary || {};
    var gp = s.global_policy || {};
    var drops = s.buffer_drops || {};

    var h = enforcementGapBanner();

    h += '<div class="card" style="margin-bottom:16px">' +
      '<div class="card-header"><strong>Current posture</strong></div>' +
      '<div style="padding:12px 16px;display:flex;gap:24px;flex-wrap:wrap;align-items:center">' +
        '<div>Mode: ' + modeBadge(gp.mode || 'observe') + '</div>' +
        '<div>Unknown tools: <span class="badge badge-outline">' + esc(gp.default_action || 'allow') + '</span></div>' +
        '<div>Scope: <span class="badge badge-outline">' + esc(gp.applies_to || 'mcp_only') + '</span></div>' +
        '<div>Deny patterns: <strong>' + fmtNum((gp.deny_patterns || []).length) + '</strong></div>' +
        '<div>Allow patterns: <strong>' + fmtNum((gp.allow_patterns || []).length) + '</strong></div>' +
      '</div>' +
      (gp.mode === 'observe'
        ? '<div style="padding:0 16px 12px;color:var(--text-muted,#888);font-size:13px">' +
          'Observe mode: nothing is being stripped. The numbers below are what <em>would</em> be blocked.' +
          '</div>'
        : '') +
    '</div>';

    h += '<div class="stats-grid" style="grid-template-columns:repeat(4,1fr);margin-bottom:16px">' +
      statCard('MCP servers', sum.servers_total, sum.servers_pending + ' pending') +
      statCard('Tools discovered', sum.tools_total, sum.tools_pending + ' undecided') +
      statCard('Approved tools', sum.tools_approved, sum.servers_approved + ' approved servers') +
      statCard('Denied tools', sum.tools_denied, sum.servers_denied + ' denied servers') +
    '</div>';

    h += '<div class="stats-grid" style="grid-template-columns:repeat(3,1fr);margin-bottom:16px">' +
      statCard('Blocked (24h)', sum.events_24h_blocked, 'actually stripped') +
      statCard('Would block (24h)', sum.events_24h_would_block, 'observe mode dry run') +
      statCard('Warned (24h)', sum.events_24h_warned, 'allowed but flagged') +
    '</div>';

    if ((drops.discovery || 0) > 0 || (drops.events || 0) > 0) {
      h += '<div class="info-callout"><div class="info-callout-body">' +
        '<strong>Buffer overflow.</strong> ' + fmtNum(drops.discovery) + ' discovery and ' +
        fmtNum(drops.events) + ' audit items were dropped because the write buffers filled. ' +
        'Enforcement is unaffected \u2014 only recording is lossy under extreme load.' +
        '</div></div>';
    }

    if (sum.tools_pending > 0) {
      h += '<div class="card"><div style="padding:16px">' +
        '<strong>' + fmtNum(sum.tools_pending) + ' tools are awaiting a decision.</strong> ' +
        '<button class="btn btn-primary btn-sm" style="margin-left:8px" onclick="mcpgovTab(\'catalog\')">Review catalog</button>' +
        '</div></div>';
    }

    return h;
  }

  function statCard(label, value, sub) {
    return '<div class="stat-card">' +
      '<div class="stat-label">' + esc(label) + '</div>' +
      '<div class="stat-value">' + fmtNum(value) + '</div>' +
      '<div class="stat-sub">' + esc(sub == null ? '' : sub) + '</div>' +
    '</div>';
  }

  function renderCatalog() {
    if (!state.servers.length && !state.tools.length) {
      return '<div class="empty-state">' +
        '<p>Nothing discovered yet.</p>' +
        '<p style="font-size:13px;color:var(--text-muted,#888)">' +
        'The catalog fills in automatically as developers send requests. Every request carries its ' +
        'full tool list, so this becomes a complete inventory of the MCP servers actually in use.' +
        '</p></div>';
    }

    // Group tools by server so the catalog reads as "server -> its tools".
    var byServer = {};
    var builtins = [];
    for (var i = 0; i < state.tools.length; i++) {
      var t = state.tools[i];
      if (!t.server_name) { builtins.push(t); continue; }
      if (!byServer[t.server_name]) byServer[t.server_name] = [];
      byServer[t.server_name].push(t);
    }

    var h = enforcementGapBanner();
    for (var j = 0; j < state.servers.length; j++) {
      var srv = state.servers[j];
      var name = srv.server_name;
      var tools = byServer[name] || [];
      var open = !!state.expanded[name];
      var an = attr(name);

      h += '<div class="card" style="margin-bottom:12px">' +
        '<div class="card-header" style="display:flex;justify-content:space-between;align-items:center;gap:12px">' +
          '<div style="display:flex;align-items:center;gap:10px;cursor:pointer" data-act="toggle" data-name="' + an + '">' +
            '<span style="font-family:monospace">' + (open ? '\u25be' : '\u25b8') + '</span>' +
            '<strong style="font-family:monospace">' + esc(name) + '</strong>' +
            statusBadge(srv.status) +
            '<span style="color:var(--text-muted,#888);font-size:12px">' + tools.length + ' tools</span>' +
          '</div>' +
          '<div style="display:flex;gap:6px">' +
            '<button class="btn btn-sm btn-secondary" data-act="server-status" data-name="' + an + '" data-status="approved">Approve</button>' +
            '<button class="btn btn-sm btn-danger" data-act="server-status" data-name="' + an + '" data-status="denied">Deny</button>' +
            '<button class="btn btn-sm btn-ghost" data-act="server-status" data-name="' + an + '" data-status="pending">Reset</button>' +
          '</div>' +
        '</div>' +
        '<div style="padding:8px 16px;font-size:12px;color:var(--text-muted,#888)">' +
          'First seen ' + esc(fmtDate(srv.first_seen)) + ' \u00b7 last seen ' + esc(fmtDate(srv.last_seen)) +
          (srv.notes ? ' \u00b7 ' + esc(srv.notes) : '') +
        '</div>';

      if (open) {
        h += '<div style="padding:0 16px 12px">';
        if (!tools.length) {
          h += '<div style="font-size:13px;color:var(--text-muted,#888)">No tools recorded for this server yet.</div>';
        } else {
          h += '<div style="margin-bottom:8px;display:flex;gap:6px">' +
            '<button class="btn btn-sm btn-secondary" data-act="bulk" data-name="' + an + '" data-status="approved">Approve all</button>' +
            '<button class="btn btn-sm btn-danger" data-act="bulk" data-name="' + an + '" data-status="denied">Deny all</button>' +
            '<button class="btn btn-sm btn-ghost" data-act="bulk" data-name="' + an + '" data-status="inherit">Reset all to inherit</button>' +
            '</div>';
          h += '<table class="oa-data-table"><thead><tr>' +
            '<th>Tool</th><th>Status</th><th>Seen</th><th>Last seen</th><th></th>' +
            '</tr></thead><tbody>';
          for (var k = 0; k < tools.length; k++) {
            h += toolRow(tools[k]);
          }
          h += '</tbody></table>';
        }
        h += '</div>';
      }

      h += '</div>';
    }

    if (builtins.length) {
      h += '<div class="card" style="margin-bottom:12px">' +
        '<div class="card-header"><strong>Built-in tools</strong> ' +
        '<span style="color:var(--text-muted,#888);font-size:12px">' +
        'not governed unless a policy sets <code>applies_to = all_tools</code></span></div>' +
        '<div style="padding:0 16px 12px"><table class="oa-data-table"><thead><tr>' +
        '<th>Tool</th><th>Status</th><th>Seen</th><th>Last seen</th><th></th>' +
        '</tr></thead><tbody>';
      for (var m = 0; m < builtins.length; m++) h += toolRow(builtins[m]);
      h += '</tbody></table></div></div>';
    }

    return h;
  }

  function toolRow(t) {
    var an = attr(t.tool_name);
    return '<tr>' +
      '<td style="font-family:monospace;font-size:12px">' + esc(t.tool_name) + '</td>' +
      '<td>' + statusBadge(t.status) + '</td>' +
      '<td>' + fmtNum(t.seen_count) + '</td>' +
      '<td style="font-size:12px">' + esc(fmtDate(t.last_seen)) + '</td>' +
      '<td style="text-align:right;white-space:nowrap">' +
        '<button class="btn btn-sm btn-ghost" data-act="tool-status" data-name="' + an + '" data-status="approved">Approve</button> ' +
        '<button class="btn btn-sm btn-ghost" data-act="tool-status" data-name="' + an + '" data-status="denied">Deny</button> ' +
        '<button class="btn btn-sm btn-ghost" data-act="tool-status" data-name="' + an + '" data-status="inherit">Inherit</button>' +
      '</td>' +
    '</tr>';
  }

  function renderPolicies() {
    if (!state.policies.length) {
      return '<div class="empty-state"><p>No policies defined.</p></div>';
    }

    var h = '<table class="oa-data-table"><thead><tr>' +
      '<th>Scope</th><th>Applies to</th><th>Mode</th><th>Unknown tools</th>' +
      '<th>Allow</th><th>Deny</th><th>Enabled</th><th>Updated</th><th></th>' +
      '</tr></thead><tbody>';

    for (var i = 0; i < state.policies.length; i++) {
      var p = state.policies[i];
      var label = p.scope === 'global' ? 'global' : p.scope + ': ' + (p.scope_ref || '');
      var allow = (p.allow_patterns || []);
      var deny = (p.deny_patterns || []);
      h += '<tr>' +
        '<td><strong>' + esc(label) + '</strong></td>' +
        '<td><span class="badge badge-outline">' + esc(p.applies_to) + '</span></td>' +
        '<td>' + modeBadge(p.mode) + '</td>' +
        '<td>' + esc(p.default_action) + '</td>' +
        '<td title="' + esc(allow.join('\n')) + '">' + allow.length + '</td>' +
        '<td title="' + esc(deny.join('\n')) + '">' + deny.length + '</td>' +
        '<td>' + (p.enabled ? '<span class="badge badge-green">yes</span>' : '<span class="badge badge-muted">no</span>') + '</td>' +
        '<td style="font-size:12px">' + esc(fmtDate(p.updated_at)) +
          (p.updated_by ? '<br><span style="color:var(--text-muted,#888)">' + esc(p.updated_by) + '</span>' : '') +
        '</td>' +
        '<td style="text-align:right;white-space:nowrap">' +
          '<button class="btn btn-sm btn-ghost" data-act="policy-edit" data-id="' + attr(p.id) + '">Edit</button>' +
          (p.scope === 'global'
            ? ''
            : ' <button class="btn btn-sm btn-ghost" data-act="policy-delete" data-id="' +
              attr(p.id) + '" data-label="' + attr(label) + '">Delete</button>') +
        '</td>' +
      '</tr>';
    }
    h += '</tbody></table>';

    h += '<div class="info-callout" style="margin-top:16px"><div class="info-callout-body">' +
      '<strong>Resolution order.</strong> Policies merge global \u2192 team \u2192 user. ' +
      'Scalars (mode, defaults) take the most specific value. Deny lists are <em>unioned</em> across ' +
      'scopes. Allow lists are <em>replaced</em> by the most specific non-empty one. ' +
      'Per tool the order is: deny pattern \u2192 catalog denial \u2192 allowlist \u2192 catalog approval \u2192 default.' +
      '</div></div>';

    return h;
  }

  function renderSimulator() {
    var h = '<div class="card" style="margin-bottom:16px"><div style="padding:16px">' +
      '<div class="form-row">' +
        '<div class="form-group">' +
          '<label class="form-label">User identity (email)</label>' +
          '<input class="form-input" id="mcpgov-sim-user" placeholder="dev@example.com">' +
        '</div>' +
        '<div class="form-group">' +
          '<label class="form-label">Team UUID (optional)</label>' +
          '<input class="form-input" id="mcpgov-sim-team" placeholder="leave blank for none">' +
        '</div>' +
      '</div>' +
      '<div class="form-group">' +
        '<label class="form-label">Tool names (optional)</label>' +
        '<textarea class="form-input" id="mcpgov-sim-tools" rows="3" placeholder="One per line. Leave empty to test the entire discovered catalog."></textarea>' +
      '</div>' +
      '<button class="btn btn-primary" onclick="mcpgovRunSim()">Simulate</button>' +
      '<div class="form-hint" style="margin-top:8px">Resolves the exact policy that identity would get and shows the verdict for each tool, with the reason.</div>' +
    '</div></div>';

    if (state.sim) {
      var eff = state.sim.effective_policy || {};
      var counts = state.sim.counts || {};
      h += '<div class="card" style="margin-bottom:16px">' +
        '<div class="card-header"><strong>Effective policy</strong></div>' +
        '<div style="padding:12px 16px;display:flex;gap:24px;flex-wrap:wrap">' +
          '<div>Mode: ' + modeBadge(eff.mode) + '</div>' +
          '<div>Decided by: <span class="badge badge-outline">' + esc(eff.decided_by) + '</span></div>' +
          '<div>Unknown tools: <span class="badge badge-outline">' + esc(eff.default_action) + '</span></div>' +
          '<div>Allowed: <strong style="color:#22c55e">' + fmtNum(counts.allowed) + '</strong></div>' +
          '<div>Denied: <strong style="color:#ef4444">' + fmtNum(counts.denied) + '</strong></div>' +
        '</div>' +
      '</div>';

      var results = state.sim.results || [];
      if (!results.length) {
        h += '<div class="empty-state"><p>No tools to evaluate. Discover some traffic first, or enter tool names above.</p></div>';
      } else {
        h += '<table class="oa-data-table"><thead><tr>' +
          '<th>Tool</th><th>Verdict</th><th>Why</th><th>Decided by</th>' +
          '</tr></thead><tbody>';
        for (var i = 0; i < results.length; i++) {
          var r = results[i];
          h += '<tr>' +
            '<td style="font-family:monospace;font-size:12px">' + esc(r.tool_name) + '</td>' +
            '<td>' + (r.allowed
              ? '<span class="badge badge-green">allowed</span>'
              : '<span class="badge badge-red">denied</span>') + '</td>' +
            '<td style="font-size:12px">' + esc(r.reason) + '</td>' +
            '<td><span class="badge badge-outline">' + esc(r.decided_by) + '</span></td>' +
          '</tr>';
        }
        h += '</tbody></table>';
      }
    }

    return h;
  }

  function renderAudit() {
    var h = '<div style="margin-bottom:12px;display:flex;gap:8px;align-items:center">' +
      '<label class="form-label" style="margin:0">Decision</label>' +
      '<select class="form-input" style="max-width:200px" id="mcpgov-audit-filter" onchange="mcpgovLoadEvents()">' +
        '<option value="">all</option>' +
        '<option value="blocked">blocked</option>' +
        '<option value="would_block">would_block</option>' +
        '<option value="warned">warned</option>' +
      '</select>' +
      '</div>';

    if (!state.events.length) {
      return h + '<div class="empty-state"><p>No policy decisions recorded yet.</p></div>';
    }

    h += '<table class="oa-data-table"><thead><tr>' +
      '<th>When</th><th>Decision</th><th>Tool</th><th>User</th><th>Mode</th><th>Reason</th>' +
      '</tr></thead><tbody>';
    for (var i = 0; i < state.events.length; i++) {
      var e = state.events[i];
      var badge = e.decision === 'blocked' ? 'badge-red'
        : (e.decision === 'warned' ? 'badge-yellow' : 'badge-blue');
      h += '<tr>' +
        '<td style="font-size:12px;white-space:nowrap">' + esc(fmtDate(e.created_at)) + '</td>' +
        '<td><span class="badge ' + badge + '">' + esc(e.decision) + '</span></td>' +
        '<td style="font-family:monospace;font-size:12px">' + esc(e.tool_name) + '</td>' +
        '<td style="font-size:12px">' + esc(e.user_identity || '-') + '</td>' +
        '<td style="font-size:12px">' + esc(e.mode) + '</td>' +
        '<td style="font-size:12px">' + esc(e.reason) + '</td>' +
      '</tr>';
    }
    h += '</tbody></table>';
    return h;
  }

  // ---------------------------------------------------------------------------
  // Data loading
  // ---------------------------------------------------------------------------

  async function loadAll() {
    var res = await Promise.all([
      api('GET', '/admin/mcpgov/summary'),
      api('GET', '/admin/mcpgov/servers'),
      api('GET', '/admin/mcpgov/tools?limit=2000'),
      api('GET', '/admin/mcpgov/policies')
    ]);

    if (res[0] && res[0].error) {
      var el = document.getElementById('mcpgov-body');
      if (el) {
        el.innerHTML = '<div class="empty-state"><p>Could not load governance data.</p>' +
          '<p style="font-size:13px;color:var(--text-muted,#888)">' +
          esc((res[0].error && res[0].error.message) || res[0].error) + '</p></div>';
      }
      return;
    }

    state.summary = res[0] || null;
    state.servers = (res[1] && res[1].servers) || [];
    state.tools = (res[2] && res[2].tools) || [];
    state.policies = (res[3] && res[3].policies) || [];
    state.loaded = true;
    render();
  }

  // ---------------------------------------------------------------------------
  // Global handlers (referenced from injected onclick attributes)
  // ---------------------------------------------------------------------------

  window.mcpgovTab = function (tab) {
    state.tab = tab;
    render();
    if (tab === 'audit') window.mcpgovLoadEvents();
  };

  window.mcpgovRefresh = function () {
    loadAll();
    toast('Refreshed', 'success');
  };

  window.mcpgovToggle = function (name) {
    state.expanded[name] = !state.expanded[name];
    render();
  };

  window.mcpgovSetServer = async function (name, status) {
    var res = await api('PUT', '/admin/mcpgov/servers/status', {
      server_name: name, status: status
    });
    if (res.updated) { toast('Server ' + status, 'success'); loadAll(); }
    else toast((res.error && res.error.message) || 'Failed to update server', 'error');
  };

  window.mcpgovSetTool = async function (name, status) {
    var res = await api('PUT', '/admin/mcpgov/tools/status', {
      tool_name: name, status: status
    });
    if (res.updated) { toast('Tool ' + status, 'success'); loadAll(); }
    else toast((res.error && res.error.message) || 'Failed to update tool', 'error');
  };

  window.mcpgovBulk = async function (server, status) {
    if (!confirm('Set every tool of "' + server + '" to ' + status + '?')) return;
    var res = await api('PUT', '/admin/mcpgov/servers/bulk-status', {
      server_name: server, status: status
    });
    if (res.updated != null) { toast(res.updated + ' tools set to ' + status, 'success'); loadAll(); }
    else toast((res.error && res.error.message) || 'Bulk update failed', 'error');
  };

  window.mcpgovScopeChanged = function () {
    var scope = document.getElementById('mcpgov-p-scope').value;
    var group = document.getElementById('mcpgov-p-ref-group');
    var label = document.getElementById('mcpgov-p-ref-label');
    var hint = document.getElementById('mcpgov-p-ref-hint');
    if (scope === 'global') {
      group.style.display = 'none';
      return;
    }
    group.style.display = '';
    if (scope === 'team') {
      label.textContent = 'Team UUID';
      hint.textContent = 'Must be an existing team UUID (see the Teams page).';
    } else {
      label.textContent = 'User email';
      hint.textContent = 'Matched case-insensitively against the request identity.';
    }
  };

  window.mcpgovNewPolicy = function () {
    state.editing = null;
    document.getElementById('mcpgov-modal-title').textContent = 'New Policy';
    document.getElementById('mcpgov-p-scope').value = 'global';
    document.getElementById('mcpgov-p-scope').disabled = false;
    document.getElementById('mcpgov-p-ref').value = '';
    document.getElementById('mcpgov-p-mode').value = 'observe';
    document.getElementById('mcpgov-p-default').value = 'allow';
    document.getElementById('mcpgov-p-applies').value = 'mcp_only';
    document.getElementById('mcpgov-p-allow').value = '';
    document.getElementById('mcpgov-p-deny').value = '';
    document.getElementById('mcpgov-p-enabled').checked = true;
    window.mcpgovScopeChanged();
    if (window.showModal) window.showModal('mcpgov-policy');
    else document.getElementById('modal-mcpgov-policy').style.display = 'flex';
  };

  window.mcpgovEditPolicy = function (id) {
    var p = null;
    for (var i = 0; i < state.policies.length; i++) {
      if (state.policies[i].id === id) { p = state.policies[i]; break; }
    }
    if (!p) return;

    state.editing = p;
    document.getElementById('mcpgov-modal-title').textContent = 'Edit Policy';
    document.getElementById('mcpgov-p-scope').value = p.scope;
    // Scope identity is the primary key; changing it would create a new policy
    // rather than edit this one, so it is locked during an edit.
    document.getElementById('mcpgov-p-scope').disabled = true;
    document.getElementById('mcpgov-p-ref').value = p.scope_ref || '';
    document.getElementById('mcpgov-p-mode').value = p.mode;
    document.getElementById('mcpgov-p-default').value = p.default_action;
    document.getElementById('mcpgov-p-applies').value = p.applies_to;
    document.getElementById('mcpgov-p-allow').value = (p.allow_patterns || []).join('\n');
    document.getElementById('mcpgov-p-deny').value = (p.deny_patterns || []).join('\n');
    document.getElementById('mcpgov-p-enabled').checked = !!p.enabled;
    window.mcpgovScopeChanged();
    if (window.showModal) window.showModal('mcpgov-policy');
    else document.getElementById('modal-mcpgov-policy').style.display = 'flex';
  };

  function lines(id) {
    var raw = document.getElementById(id).value || '';
    return raw.split('\n').map(function (s) { return s.trim(); })
      .filter(function (s) { return s.length > 0; });
  }

  window.mcpgovSavePolicy = async function () {
    var scope = document.getElementById('mcpgov-p-scope').value;
    var ref = (document.getElementById('mcpgov-p-ref').value || '').trim();
    if (scope !== 'global' && !ref) {
      toast('A team UUID or user email is required for this scope', 'error');
      return;
    }

    var mode = document.getElementById('mcpgov-p-mode').value;
    var allow = lines('mcpgov-p-allow');
    var deny = lines('mcpgov-p-deny');
    var applies = document.getElementById('mcpgov-p-applies').value;

    // Enforcing on all tools with a deny-by-default posture and no allowlist would
    // strip every built-in tool. Confirm rather than silently breaking Claude Code.
    if (mode === 'enforce' && applies === 'all_tools' &&
        document.getElementById('mcpgov-p-default').value === 'deny' && !allow.length) {
      if (!confirm('This will deny every built-in tool (Read, Write, Bash, ...) for the ' +
                   'matching identities, because you are enforcing all_tools with ' +
                   'deny-by-default and no allowlist.\n\nContinue?')) {
        return;
      }
    }

    var payload = {
      scope: scope,
      scope_ref: scope === 'global' ? null : ref,
      mode: mode,
      default_action: document.getElementById('mcpgov-p-default').value,
      applies_to: applies,
      allow_patterns: allow,
      deny_patterns: deny,
      enabled: document.getElementById('mcpgov-p-enabled').checked
    };

    var res = await api('PUT', '/admin/mcpgov/policies', payload);
    if (res.policy) {
      toast('Policy saved', 'success');
      if (window.closeModal) window.closeModal();
      loadAll();
    } else {
      toast((res.error && res.error.message) || 'Failed to save policy', 'error');
    }
  };

  /**
   * Flips the global policy to `enforce`, preserving every other field.
   *
   * The policies endpoint is a full upsert, so the current global policy is replayed
   * with only the mode changed. Anything the summary does not carry keeps the server's
   * default, which matches what the policy editor would send for a fresh global row.
   */
  window.mcpgovEnableEnforce = async function () {
    var gp = (state.summary && state.summary.global_policy) || {};
    if (!confirm('Switch the global policy to enforce?\n\n' +
                 'Denied tools will be removed from requests instead of only being ' +
                 'recorded. Clients lose access to them immediately.')) {
      return;
    }

    var res = await api('PUT', '/admin/mcpgov/policies', {
      scope: 'global',
      scope_ref: null,
      mode: 'enforce',
      default_action: gp.default_action || 'allow',
      applies_to: gp.applies_to || 'mcp_only',
      allow_patterns: gp.allow_patterns || [],
      deny_patterns: gp.deny_patterns || [],
      enabled: true
    });

    if (res.policy) { toast('Enforcement enabled', 'success'); loadAll(); }
    else toast((res.error && res.error.message) || 'Failed to enable enforcement', 'error');
  };

  window.mcpgovDeletePolicy = async function (id, label) {
    if (!confirm('Delete the policy for ' + label + '?')) return;
    var res = await api('DELETE', '/admin/mcpgov/policies/' + encodeURIComponent(id));
    if (res.deleted) { toast('Policy deleted', 'success'); loadAll(); }
    else toast((res.error && res.error.message) || 'Failed to delete policy', 'error');
  };

  window.mcpgovRunSim = async function () {
    var user = (document.getElementById('mcpgov-sim-user').value || '').trim();
    var team = (document.getElementById('mcpgov-sim-team').value || '').trim();
    var tools = lines('mcpgov-sim-tools');

    var payload = { tool_names: tools };
    if (user) payload.user_identity = user;
    if (team) payload.team_id = team;

    var res = await api('POST', '/admin/mcpgov/simulate', payload);
    if (res.results) {
      state.sim = res;
      render();
    } else {
      toast((res.error && res.error.message) || 'Simulation failed', 'error');
    }
  };

  window.mcpgovLoadEvents = async function () {
    var sel = document.getElementById('mcpgov-audit-filter');
    var filter = sel ? sel.value : '';
    var path = '/admin/mcpgov/events?limit=300' +
      (filter ? '&decision=' + encodeURIComponent(filter) : '');
    var res = await api('GET', path);
    state.events = (res && res.events) || [];
    if (state.tab === 'audit') {
      render();
      // Re-apply the filter selection lost to the re-render.
      var sel2 = document.getElementById('mcpgov-audit-filter');
      if (sel2 && filter) sel2.value = filter;
    }
  };

  // ---------------------------------------------------------------------------
  // Boot: inject markup, then hook navigate() for lazy loading.
  // ---------------------------------------------------------------------------

  function hookNavigate() {
    if (typeof window.navigate !== 'function' || window.navigate.__mcpgov) return false;
    var original = window.navigate;
    var wrapped = function (page) {
      original(page);
      if (page === PAGE && !state.loaded) loadAll();
      else if (page === PAGE) render();
    };
    wrapped.__mcpgov = true;
    window.navigate = wrapped;
    return true;
  }

  function boot() {
    if (!inject()) return false;
    hookNavigate();
    // Deep link support: the host SPA may have already resolved the hash before this
    // file finished loading, in which case its page lookup silently failed.
    if (window.location.hash === '#/' + PAGE) {
      if (typeof window.navigate === 'function') window.navigate(PAGE);
    }
    return true;
  }

  // The host SPA builds its shell synchronously in an inline <script>, but this file
  // may load before DOMContentLoaded. Retry briefly rather than assuming an order.
  if (!boot()) {
    var attempts = 0;
    var timer = setInterval(function () {
      attempts++;
      if (boot() || attempts > 100) clearInterval(timer);
    }, 100);
    document.addEventListener('DOMContentLoaded', function () { boot(); });
  }
})();
