(() => {
  'use strict';

  const TOKEN_KEY = 'cliproxy_management_token';
  const PROVIDERS = ['openai', 'claude', 'gemini', 'codex', 'openai_compatible'];
  const NAMES = {openai: 'OpenAI', claude: 'Claude', gemini: 'Gemini', codex: 'Codex', openai_compatible: 'OpenAI compatible'};
  const TITLES = {
    overview: ['OPERATIONS / OVERVIEW', 'Your proxy,', 'at a glance.', 'Live activity across every connected provider and account.'],
    accounts: ['CONFIGURATION / ACCOUNTS', 'Every credential,', 'one place.', 'Connect multiple accounts and control which ones can serve requests.'],
    usage: ['OBSERVABILITY / USAGE', 'Every attempt,', 'accounted for.', 'Each row is one upstream attempt; retries can create multiple rows for a client request. Inspect reported tokens, latency, and errors.'],
    models: ['ROUTING / MODELS', 'One endpoint,', 'every model.', 'See configured routes and connect your favorite clients.'],
    settings: ['SYSTEM / SETTINGS', 'Your local', 'connection.', 'Runtime information and browser session controls.']
  };
  const state = {
    token: sessionStorage.getItem(TOKEN_KEY) || '', route: 'overview',
    accounts: [], usage: [], summary: null, providers: [], settings: null,
    loading: false, error: '', lastUpdated: null, usagePage: 0,
    filters: {provider: '', account: '', model: '', status: ''},
    integration: 'curl', selectedProvider: '', accountMode: 'key', editing: null,
    oauthSession: null, refreshTimer: null
  };
  const $ = id => document.getElementById(id);
  const number = value => Number.isFinite(Number(value)) ? Number(value) : 0;
  const fmt = value => number(value).toLocaleString();
  const fmtToken = value => value == null ? '—' : fmt(value);
  const compact = value => new Intl.NumberFormat(undefined, {notation: 'compact', maximumFractionDigits: 1}).format(number(value));
  const name = id => NAMES[id] || (id ? String(id).replaceAll('_', ' ') : 'Unknown');
  const dateTime = value => { const date = new Date(value); return Number.isNaN(date.getTime()) ? 'Unknown time' : date.toLocaleString(); };
  const timeOnly = value => { const date = new Date(value); return Number.isNaN(date.getTime()) ? '—' : date.toLocaleTimeString([], {hour: '2-digit', minute: '2-digit'}); };

  function node(tag, className, content) {
    const element = document.createElement(tag);
    if (className) element.className = className;
    if (content !== undefined && content !== null) element.textContent = String(content);
    return element;
  }
  function add(parent, ...children) { for (const child of children) if (child) parent.append(child); return parent; }
  function clear(element) { element.replaceChildren(); return element; }
  function setConnected(connected) {
    document.querySelector('.shell').inert = !connected;
    document.querySelector('.skip-link').inert = !connected;
    $('connect-screen').hidden = connected;
  }
  function closeNavigation() {
    $('sidebar').classList.remove('open');
    $('menu-button').setAttribute('aria-expanded', 'false');
    $('menu-button').setAttribute('aria-label', 'Open navigation');
  }
  function button(label, className, click, type = 'button') {
    const element = node('button', className, label);
    element.type = type;
    if (click) element.addEventListener('click', click);
    return element;
  }
  function heading(title, trailing) {
    const wrap = node('div', 'panel-head');
    add(wrap, node('h2', '', title), node('small', '', trailing || ''));
    return wrap;
  }
  function empty(title, detail, action) {
    const wrap = node('div', 'empty');
    add(wrap, node('span', 'empty-mark', '◇'), node('strong', '', title), node('p', '', detail));
    if (action) wrap.append(action);
    return wrap;
  }
  function panel(title, trailing) { const wrap = node('section', 'panel'); wrap.append(heading(title, trailing)); return wrap; }
  function notice(message, isError = false) { return node('div', `form-notice${isError ? ' error' : ''}`, message); }
  function toast(message, isError = false) {
    const element = node('div', `toast${isError ? ' error' : ''}`, message);
    $('toast-region').append(element);
    setTimeout(() => element.remove(), 4500);
  }
  function apiError(error) { return error instanceof Error ? error.message : String(error); }
  async function request(path, options = {}) {
    const headers = new Headers(options.headers || {});
    headers.set('Authorization', `Bearer ${state.token}`);
    if (options.body !== undefined) headers.set('Content-Type', 'application/json');
    let response;
    try { response = await fetch(`/api${path}`, {...options, headers, cache: 'no-store'}); }
    catch { throw new Error('Cannot reach the local proxy. Check that the server is running.'); }
    if (response.status === 401) {
      disconnect('The management token was rejected. Enter the current token to reconnect.');
      throw new Error('Management token rejected.');
    }
    if (!response.ok) {
      let message = `Request failed (${response.status}).`;
      try { const body = await response.json(); message = body?.error?.message || body?.message || message; } catch {}
      throw new Error(message);
    }
    if (response.status === 204) return null;
    return response.json();
  }
  async function loadData() {
    if (!state.token || state.loading) return;
    state.loading = true;
    $('refresh-button').disabled = true;
    if (!state.summary) render();
    try {
      const [accounts, usage, summary, providers, settings] = await Promise.all([
        request('/accounts'), request('/usage'), request('/summary'), request('/providers'), request('/settings')
      ]);
      if (!Array.isArray(accounts) || !Array.isArray(usage) || !Array.isArray(providers) || !summary || typeof summary !== 'object') throw new Error('The server returned an unexpected dashboard response.');
      Object.assign(state, {accounts, usage, summary, providers, settings, error: '', lastUpdated: new Date()});
      setConnected(true);
      $('management-token').value = '';
      $('connection-indicator').className = 'connection online';
      $('connection-text').textContent = 'Connected';
      $('last-updated').textContent = `Updated ${timeOnly(state.lastUpdated)}`;
      $('demo-badge').hidden = settings?.demo !== true;
    } catch (error) {
      state.error = apiError(error);
      $('connection-indicator').className = 'connection offline';
      $('connection-text').textContent = 'Connection error';
      if (!state.summary) {
        setConnected(false);
        $('connect-error').textContent = state.error;
      }
      if (state.token) toast(state.error, true);
    } finally {
      state.loading = false;
      $('refresh-button').disabled = false;
      render();
    }
  }
  function disconnect(message = '') {
    sessionStorage.removeItem(TOKEN_KEY);
    state.token = '';
    state.accounts = []; state.usage = []; state.summary = null; state.settings = null; state.providers = [];
    setConnected(false);
    $('connection-indicator').className = 'connection offline';
    $('connection-text').textContent = 'Disconnected';
    $('demo-badge').hidden = true;
    $('connect-error').textContent = message;
    $('management-token').value = '';
    $('management-token').focus();
  }
  function routeFromHash() {
    const route = location.hash.replace(/^#\/?/, '').split('/')[0];
    return Object.hasOwn(TITLES, route) ? route : 'overview';
  }
  function render() {
    state.route = routeFromHash();
    const [eyebrow, title, accent, subtitle] = TITLES[state.route];
    $('page-eyebrow').textContent = eyebrow;
    $('page-title').replaceChildren(document.createTextNode(title), node('br'), node('em', '', accent));
    $('page-subtitle').textContent = subtitle;
    $('breadcrumb-current').textContent = {overview: 'Overview', accounts: 'Accounts', usage: 'Usage log', models: 'Models & routes', settings: 'Settings'}[state.route];
    for (const link of document.querySelectorAll('[data-route]')) {
      const active = link.dataset.route === state.route;
      link.classList.toggle('active', active);
      if (active) link.setAttribute('aria-current', 'page'); else link.removeAttribute('aria-current');
    }
    const content = clear($('page-content'));
    const actions = clear($('page-head-actions'));
    if (state.error) content.append(notice(state.error, true));
    if (state.loading && !state.summary) { content.append(node('div', 'loading', 'Loading control room data')); return; }
    if (!state.summary) { content.append(empty('No dashboard data yet', 'Connect with your management token to view the proxy.')); return; }
    if (state.route === 'overview') renderOverview(content, actions);
    if (state.route === 'accounts') renderAccounts(content, actions);
    if (state.route === 'usage') renderUsage(content, actions);
    if (state.route === 'models') renderModels(content, actions);
    if (state.route === 'settings') renderSettings(content, actions);
  }
  function metric(label, value, foot, glyph) {
    const card = node('div', 'metric-card');
    add(card, add(node('div', 'metric-top'), node('span', 'metric-label', label), node('span', 'metric-glyph', glyph)), node('div', 'metric-value', value), node('div', 'metric-foot', foot));
    return card;
  }
  function renderOverview(content, actions) {
    actions.append(button('View usage →', 'button button-secondary', () => { location.hash = '#/usage'; }));
    const summary = state.summary;
    const totalTokens = number(summary.input_tokens) + number(summary.output_tokens);
    const success = number(summary.requests) ? `${Math.round(100 * number(summary.successful_requests) / number(summary.requests))}% success` : 'No attempts yet';
    add(content, add(node('div', 'grid metrics'),
      metric('Upstream attempts', fmt(summary.requests), success, '↗'),
      metric('Reported tokens', compact(totalTokens), `${fmt(summary.input_tokens)} in · ${fmt(summary.output_tokens)} out`, '◎'),
      metric('Reported cache tokens', compact(summary.cached_tokens), 'Provider-reported cached input tokens', '◫'),
      metric('Avg. latency', `${fmt(Math.round(number(summary.average_latency_ms)))} ms`, `${fmt(summary.reasoning_tokens)} reported reasoning tokens`, '◷')));

    const top = node('div', 'grid overview-grid');
    const chartPanel = panel('Upstream attempt history', 'ACTUAL DAILY TOTALS');
    const days = Array.isArray(summary.daily) ? summary.daily.slice(-30) : [];
    if (days.length) {
      const chart = node('div', 'chart');
      chart.setAttribute('role', 'img');
      chart.setAttribute('aria-label', `Daily upstream attempts: ${days.map(day => `${day.date}: ${fmt(day.requests)}`).join(', ')}`);
      const max = Math.max(1, ...days.map(day => number(day.requests)));
      days.forEach((day, index) => {
        const wrap = node('div', 'bar-wrap');
        const bar = node('div', 'bar');
        bar.style.height = `${Math.max(2, number(day.requests) / max * 100)}%`;
        bar.title = `${day.date}: ${fmt(day.requests)} attempts`;
        wrap.append(bar);
        if (index === 0 || index === days.length - 1 || index % Math.max(1, Math.ceil(days.length / 6)) === 0) {
          wrap.append(node('span', 'bar-label', String(day.date).slice(5)));
        }
        chart.append(wrap);
      });
      chartPanel.append(chart, add(node('div', 'chart-key'), node('i'), node('span', '', 'Upstream attempts per day · latest 30 days')));
    } else chartPanel.append(empty('History starts with your first attempt', 'Daily activity will appear here when upstream attempts are recorded.'));
    top.append(chartPanel);
    const providersPanel = panel('Provider activity', 'ATTEMPT SHARE');
    const providerStats = Array.isArray(summary.providers) ? summary.providers : [];
    if (providerStats.some(item => number(item.requests) > 0)) {
      const list = node('div', 'provider-list');
      const max = Math.max(1, ...providerStats.map(item => number(item.requests)));
      providerStats.forEach(item => {
        const line = node('div', 'provider-line');
        const track = node('div', 'progress-track');
        const fill = node('span'); fill.style.width = `${number(item.requests) / max * 100}%`;
        add(track, fill); add(line, node('strong', '', name(item.provider || item.id)), track, node('span', '', fmt(item.requests))); list.append(line);
      });
      providersPanel.append(list);
    } else providersPanel.append(empty('No provider traffic', 'Provider attempt totals appear after the first attempt.'));
    add(top, providersPanel); content.append(top);

    const lower = node('div', 'grid overview-lower');
    const activityPanel = panel('Recent attempts', 'LATEST ACTIVITY');
    if (state.usage.length) {
      const list = node('div', 'activity-list');
      state.usage.slice(0, 6).forEach(record => {
        const item = node('div', 'activity-item');
        const status = number(record.status);
        const dot = node('span', `event-dot${status >= 400 || status === 0 ? ' error' : ''}`);
        const main = add(node('div', 'activity-main'), node('strong', '', `${record.model || 'Unknown model'} · ${name(record.provider)}`), node('small', '', `${record.account_label || record.account_id || 'Unknown account'} · ${status || '—'} · ${fmt(record.duration_ms)} ms`));
        add(item, dot, main, node('time', '', timeOnly(record.timestamp)));
        item.title = 'Open usage log for attempt details';
        list.append(item);
      });
      activityPanel.append(list);
    } else activityPanel.append(empty('The log is quiet', 'Upstream attempts appear here.'));
    lower.append(activityPanel);
    const health = panel('At a glance', 'CURRENT STATE');
    const stack = node('div', 'stat-stack');
    [['Connected accounts', summary.accounts], ['Enabled accounts', summary.enabled_accounts], ['Failed attempts', summary.failed_requests], ['Reported reasoning tokens', summary.reasoning_tokens]].forEach(([label, value]) => add(stack, add(node('div', 'stat-row'), node('span', '', label), node('strong', '', fmt(value)))));
    health.append(stack);
    const go = button('Manage accounts →', 'text-button', () => { location.hash = '#/accounts'; });
    health.append(node('div', 'subtle-rule'), go);
    lower.append(health); content.append(lower);
  }
  function statusOf(account) {
    if (!account.enabled) return ['Disabled', 'disabled'];
    if (account.cooldown_until && number(account.cooldown_until) * 1000 > Date.now()) return ['Cooling down', 'cooldown'];
    if (account.status && !['ready', 'active', 'enabled', 'healthy'].includes(String(account.status).toLowerCase())) return [String(account.status), 'error'];
    return ['Ready', ''];
  }
  function renderAccounts(content, actions) {
    actions.append(button('+ Add account', 'button button-primary', () => openAccountDialog('key')));
    const accounts = state.accounts;
    add(content, add(node('div', 'account-summary'),
      miniStat('Total accounts', accounts.length),
      miniStat('Ready to route', accounts.filter(a => a.enabled && statusOf(a)[1] === '').length),
      miniStat('Providers connected', new Set(accounts.map(a => a.provider)).size)));
    if (!accounts.length) { content.append(empty('No accounts connected', 'Add an API key, start OAuth, or import an existing credential file to begin routing.', button('Add your first account', 'button button-primary', () => openAccountDialog('key')))); return; }
    const grouped = new Map();
    for (const account of accounts) { const key = account.provider || 'unknown'; if (!grouped.has(key)) grouped.set(key, []); grouped.get(key).push(account); }
    for (const [provider, items] of grouped) {
      const section = node('section', 'provider-group');
      add(section, add(node('div', 'group-head'), node('h2', '', name(provider)), node('small', '', `${items.length} ACCOUNT${items.length === 1 ? '' : 'S'}`)));
      const list = node('div', 'account-list'); items.forEach(account => list.append(accountCard(account)));
      section.append(list); content.append(section);
    }
  }
  function miniStat(label, value) { return add(node('div', 'mini-stat'), node('span', '', label), node('strong', '', fmt(value))); }
  function accountCard(account) {
    const card = node('article', 'account-card');
    const top = node('div', 'account-card-top');
    const avatar = node('div', 'account-avatar', name(account.provider).slice(0, 1));
    const main = add(node('div', 'account-main'), node('strong', '', account.label || 'Unnamed account'), node('small', '', account.id));
    const [status, statusClass] = statusOf(account);
    add(top, avatar, main, node('span', `pill ${statusClass}`, status)); card.append(top);
    const meta = node('div', 'account-meta');
    add(meta, add(node('div'), node('span', '', 'Auth method'), document.createTextNode(account.auth_kind === 'oauth' ? 'OAuth' : 'API key')),
      add(node('div'), node('span', '', 'Credential'), document.createTextNode(account.has_secret ? 'Saved locally' : 'Missing')));
    card.append(meta);
    const models = node('div', 'model-chips');
    if (Array.isArray(account.models) && account.models.length) account.models.slice(0, 5).forEach(model => models.append(node('span', 'chip', model)));
    else models.append(node('span', 'chip', 'Any model'));
    if (Array.isArray(account.models) && account.models.length > 5) models.append(node('span', 'chip', `+${account.models.length - 5}`));
    card.append(models);
    const actions = node('div', 'account-actions');
    actions.append(button('Edit', 'button button-secondary button-small', () => openAccountDialog('edit', account)));
    const toggle = button(account.enabled ? 'Disable' : 'Enable', 'button button-quiet button-small', async () => {
      toggle.disabled = true;
      try { await request(`/accounts/${encodeURIComponent(account.id)}`, {method: 'PATCH', body: JSON.stringify({enabled: !account.enabled})}); toast(`${account.label || 'Account'} ${account.enabled ? 'disabled' : 'enabled'}.`); await loadData(); }
      catch (error) { toast(apiError(error), true); toggle.disabled = false; }
    });
    actions.append(toggle, button('Remove', 'button button-quiet button-small', async () => {
      if (!window.confirm(`Remove ${account.label || 'this account'}? This credential will no longer be available to the proxy.`)) return;
      try { await request(`/accounts/${encodeURIComponent(account.id)}`, {method: 'DELETE'}); toast('Account removed.'); await loadData(); }
      catch (error) { toast(apiError(error), true); }
    }));
    card.append(actions); return card;
  }
  function option(value, label) { const item = node('option', '', label); item.value = value; return item; }
  function field(label, name, value = '', options = {}) {
    const wrap = node('div', `form-field${options.full ? ' full' : ''}`);
    const id = `account-${name}`;
    const caption = node('label', 'field-label', label); caption.htmlFor = id;
    let input;
    if (options.type === 'select') {
      input = node('select', 'select');
      options.choices.forEach(([itemValue, text]) => input.append(option(itemValue, text)));
      input.value = value;
    } else if (options.type === 'textarea') {
      input = node('textarea', 'textarea'); input.rows = options.rows || 3; input.value = value;
    } else {
      input = node('input', 'field'); input.type = options.type || 'text'; input.value = value; input.placeholder = options.placeholder || '';
    }
    input.id = id; input.name = name;
    if (options.required) input.required = true;
    if (options.autocomplete) input.autocomplete = options.autocomplete;
    if (options.spellcheck === false) input.spellcheck = false;
    wrap.append(caption, input);
    if (options.help) wrap.append(node('small', 'form-help', options.help));
    return wrap;
  }
  function providerSelect(value = 'openai') { return field('Provider', 'provider', value, {type: 'select', choices: PROVIDERS.map(id => [id, name(id)])}); }
  function accountFormFields(account) {
    const grid = node('div', 'form-grid');
    grid.append(field('Account name', 'label', account?.label || '', {required: true, placeholder: 'e.g. Primary workspace'}));
    grid.append(providerSelect(account?.provider || 'openai'));
    const providerInput = grid.querySelector('[name=provider]');
    if (account) providerInput.disabled = true;
    const base = field('Base URL', 'base_url', account?.base_url || '', {full: true, placeholder: 'Uses provider default when blank', help: 'Change this only for a custom endpoint.'});
    grid.append(base);
    grid.append(field(account ? 'Replace secret (optional)' : 'API key / secret', 'secret', '', {type: 'password', full: true, required: !account, autocomplete: 'new-password', help: account ? 'Leave blank to preserve the current credential. Secrets are write-only.' : 'Stored locally by the server. The dashboard never reads it back.'}));
    grid.append(field('Models', 'models', Array.isArray(account?.models) ? account.models.join(', ') : '', {type: 'textarea', full: true, rows: 2, placeholder: 'model-a, model-b', help: 'Comma or line separated. Blank allows any model for this account.'}));
    const enabledWrap = node('div', 'form-field full');
    const enabled = node('input'); enabled.type = 'checkbox'; enabled.name = 'enabled'; enabled.id = 'account-enabled'; enabled.checked = account?.enabled ?? true;
    add(enabledWrap, add(node('label', 'checkbox-row'), enabled, document.createTextNode(' Enabled for routing')));
    grid.append(enabledWrap);
    return grid;
  }
  function openAccountDialog(mode, account = null) {
    state.accountMode = mode; state.editing = account; state.oauthSession = null;
    const dialog = $('account-dialog');
    $('account-dialog-kicker').textContent = account ? 'ACCOUNT / EDIT' : 'ACCOUNT / NEW';
    $('account-dialog-title').textContent = account ? `Edit ${account.label || 'account'}` : 'Add an account';
    renderAccountDialog();
    dialog.showModal();
  }
  function renderAccountDialog() {
    const body = clear($('account-dialog-body'));
    const actions = clear($('account-dialog-actions'));
    const container = node('div', 'dialog-body'); body.append(container);
    if (!state.editing) {
      const tabs = node('div', 'dialog-tabs');
      [['key', 'API key'], ['oauth', 'OAuth'], ['import', 'Import JSON']].forEach(([mode, label]) => {
        const tab = button(label, state.accountMode === mode ? 'active' : '', () => { state.accountMode = mode; state.oauthSession = null; renderAccountDialog(); });
        tab.setAttribute('aria-pressed', String(state.accountMode === mode)); tabs.append(tab);
      });
      container.append(tabs);
    }
    const error = node('div', 'form-error'); error.id = 'account-error';
    if (state.accountMode === 'key' || state.accountMode === 'edit') {
      container.append(accountFormFields(state.editing), error);
      actions.append(button('Cancel', 'button button-secondary', () => $('account-dialog').close()), button(state.editing ? 'Save changes' : 'Add account', 'button button-primary', () => saveAccount()));
    } else if (state.accountMode === 'oauth') {
      renderOAuthForm(container, actions, error);
    } else {
      renderImportForm(container, actions, error);
    }
  }
  function modelsInput(text) { return String(text || '').split(/[\n,]/).map(part => part.trim()).filter(Boolean); }
  async function saveAccount() {
    const form = $('account-form');
    if (!form.reportValidity()) return;
    const label = form.elements.namedItem('label').value.trim();
    const base_url = form.elements.namedItem('base_url').value.trim();
    const secret = form.elements.namedItem('secret').value;
    const models = modelsInput(form.elements.namedItem('models').value);
    const enabled = form.elements.namedItem('enabled').checked;
    const payload = {label, models, enabled};
    if (base_url) payload.base_url = base_url;
    else if (state.editing) payload.base_url = state.providers.find(provider => provider.id === state.editing.provider)?.base_url || state.editing.base_url;
    if (!state.editing) { payload.provider = form.elements.namedItem('provider').value; payload.auth_kind = 'api_key'; payload.secret = secret; }
    else if (secret) payload.secret = secret;
    const save = $('account-dialog-actions').querySelector('.button-primary'); save.disabled = true;
    try {
      await request(state.editing ? `/accounts/${encodeURIComponent(state.editing.id)}` : '/accounts', {method: state.editing ? 'PATCH' : 'POST', body: JSON.stringify(payload)});
      $('account-dialog').close(); toast(state.editing ? 'Account updated.' : 'Account added.'); await loadData();
    } catch (error) { $('account-error').textContent = apiError(error); save.disabled = false; }
  }
  function renderOAuthForm(container, actions, error) {
    container.append(notice('OAuth is experimental for Codex and Claude until live inference is verified. Follow the provider authorization page, then paste the complete callback URL below.'));
    const fields = node('div', 'form-grid'); fields.style.marginTop = '16px';
    fields.append(field('Provider', 'provider', 'codex', {type: 'select', choices: [['codex', 'Codex (experimental)'], ['claude', 'Claude (experimental)']]}), field('Account name', 'label', '', {required: true, placeholder: 'e.g. Personal OAuth'}));
    container.append(fields);
    const startButton = button('Start OAuth ↗', 'button button-primary', async () => {
      if (!fields.querySelector('[name=label]').value.trim()) { fields.querySelector('[name=label]').reportValidity(); return; }
      startButton.disabled = true; error.textContent = '';
      try {
        const result = await request('/oauth/start', {method: 'POST', body: JSON.stringify({provider: fields.querySelector('[name=provider]').value, label: fields.querySelector('[name=label]').value.trim()})});
        if (!result?.state || !result?.authorization_url || !/^https?:\/\//.test(result.authorization_url)) throw new Error('The server returned an invalid authorization URL.');
        state.oauthSession = result;
        renderOAuthStep(container, result);
      } catch (startError) { error.textContent = apiError(startError); startButton.disabled = false; }
    });
    container.append(error);
    actions.append(button('Cancel', 'button button-secondary', () => $('account-dialog').close()), startButton);
  }
  function renderOAuthStep(container, session) {
    const previous = container.querySelector('.oauth-step'); if (previous) previous.remove();
    const wrap = node('div', 'oauth-step'); wrap.style.marginTop = '18px';
    const instruction = notice(session.callback_listener ? `Authorization is ready. Open the provider page, complete sign-in, then click Complete OAuth. If the local callback cannot be received, paste the full callback URL. Expected redirect: ${session.redirect_uri || 'See provider page'}` : `Authorization is ready. Open the provider page, complete sign-in, and paste the full callback URL. Expected redirect: ${session.redirect_uri || 'See provider page'}`);
    wrap.append(instruction);
    const link = node('a', 'button button-secondary', 'Open authorization page ↗');
    link.href = session.authorization_url; link.target = '_blank'; link.rel = 'noopener noreferrer'; link.style.margin = '13px 0'; wrap.append(link);
    wrap.append(field('Full callback URL', 'callback_url', '', {type: 'textarea', full: true, rows: 3, placeholder: 'http://localhost/...?...', help: 'Copy the complete URL from the browser address bar after authorization.'}));
    wrap.append(node('div', 'form-error')); container.append(wrap);
    const actions = clear($('account-dialog-actions'));
    actions.append(button('Cancel', 'button button-secondary', () => $('account-dialog').close()), button('Complete OAuth', 'button button-primary', async event => {
      const callback_url = wrap.querySelector('[name=callback_url]').value.trim();
      if (!callback_url && !session.callback_listener) { wrap.querySelector('.form-error').textContent = 'Paste the callback URL to continue.'; wrap.querySelector('[name=callback_url]').focus(); return; }
      event.currentTarget.disabled = true;
      try {
        await request('/oauth/complete', {method: 'POST', body: JSON.stringify({state: session.state, callback_url})});
        $('account-dialog').close(); toast('OAuth account connected.'); await loadData();
      } catch (error) { wrap.querySelector('.form-error').textContent = apiError(error); event.currentTarget.disabled = false; }
    }));
  }
  function renderImportForm(container, actions, error) {
    container.append(notice('Import a CLIProxyAPI or Codex credential JSON file. Its contents are sent to this local server and are not displayed after import.'));
    const grid = node('div', 'form-grid'); grid.style.marginTop = '16px';
    grid.append(field('Provider (optional)', 'provider', '', {type: 'select', choices: [['', 'Detect from credentials'], ...PROVIDERS.map(id => [id, name(id)])]}));
    grid.append(field('Account name (optional)', 'label', '', {placeholder: 'Uses credential name if available'}));
    const fileWrap = node('div', 'form-field full');
    const fileLabel = node('label', 'field-label', 'Choose JSON file'); fileLabel.htmlFor = 'import-file';
    const file = node('input', 'field'); file.type = 'file'; file.accept = '.json,application/json'; file.id = 'import-file'; file.style.width = '100%';
    file.addEventListener('change', async () => { if (file.files?.[0]) grid.querySelector('[name=credentials]').value = await file.files[0].text(); });
    fileWrap.append(fileLabel, file); grid.append(fileWrap);
    grid.append(field('Credential JSON', 'credentials', '', {type: 'textarea', full: true, rows: 6, placeholder: '{ "type": "...", "access_token": "..." }', help: 'Paste JSON or choose a file above.'}));
    container.append(grid, error);
    actions.append(button('Cancel', 'button button-secondary', () => $('account-dialog').close()), button('Import account', 'button button-primary', async event => {
      let credentials;
      try { credentials = JSON.parse(grid.querySelector('[name=credentials]').value); if (!credentials || typeof credentials !== 'object' || Array.isArray(credentials)) throw new Error('Credential JSON must be an object.'); }
      catch { error.textContent = 'Enter a valid credential JSON object.'; return; }
      const payload = {credentials};
      const provider = grid.querySelector('[name=provider]').value;
      const label = grid.querySelector('[name=label]').value.trim();
      if (provider) payload.provider = provider;
      if (label) payload.label = label;
      event.currentTarget.disabled = true;
      try { await request('/accounts/import', {method: 'POST', body: JSON.stringify(payload)}); $('account-dialog').close(); toast('Credential imported.'); await loadData(); }
      catch (importError) { error.textContent = apiError(importError); event.currentTarget.disabled = false; }
    }));
  }
  function filterSelect(label, current, values, changed) {
    const select = node('select', 'select'); select.setAttribute('aria-label', label);
    select.append(option('', label));
    values.forEach(([value, text]) => select.append(option(value, text)));
    select.value = current;
    select.addEventListener('change', () => { changed(select.value); state.usagePage = 0; render(); });
    return select;
  }
  function renderUsage(content, actions) {
    actions.append(button('Refresh log ↻', 'button button-secondary', loadData));
    const toolbar = node('div', 'toolbar');
    const providers = [...new Set(state.usage.map(record => record.provider).filter(Boolean))].sort();
    const accounts = [...new Map(state.usage.map(record => [record.account_id, record.account_label || record.account_id])).entries()].filter(([id]) => id);
    const models = [...new Set(state.usage.map(record => record.model).filter(Boolean))].sort();
    toolbar.append(
      filterSelect('All providers', state.filters.provider, providers.map(id => [id, name(id)]), value => { state.filters.provider = value; state.filters.account = ''; }),
      filterSelect('All accounts', state.filters.account, accounts, value => { state.filters.account = value; }),
      filterSelect('All models', state.filters.model, models.map(id => [id, id]), value => { state.filters.model = value; }),
      filterSelect('All statuses', state.filters.status, [['success', 'Success'], ['failure', 'Failed']], value => { state.filters.status = value; })
    );
    if (Object.values(state.filters).some(Boolean)) toolbar.append(button('Clear filters', 'button button-quiet button-small', () => { state.filters = {provider: '', account: '', model: '', status: ''}; state.usagePage = 0; render(); }));
    content.append(toolbar);
    const records = state.usage.filter(record => {
      const status = number(record.status);
      return (!state.filters.provider || record.provider === state.filters.provider) &&
        (!state.filters.account || record.account_id === state.filters.account) &&
        (!state.filters.model || record.model === state.filters.model) &&
        (!state.filters.status || (state.filters.status === 'success' ? status >= 200 && status < 400 : status < 200 || status >= 400));
    });
    const stats = node('div', 'account-summary');
    stats.append(miniStat('Matching attempts', records.length), miniStat('Reported input tokens', records.reduce((total, record) => total + number(record.input_tokens), 0)), miniStat('Reported output tokens', records.reduce((total, record) => total + number(record.output_tokens), 0)));
    content.append(stats);
    const logPanel = panel('Upstream attempt log', `${records.length} MATCHING`);
    if (!records.length) logPanel.append(empty(state.usage.length ? 'No attempts match' : 'No attempts recorded', state.usage.length ? 'Adjust your filters to see more results.' : 'Send a request through the proxy to populate this log.'));
    else {
      const tableWrap = node('div', 'table-wrap'); const table = node('table', 'data-table');
      table.append(tableHead(['Time', 'Provider', 'Account', 'Model', 'Status', 'Tokens in / out', 'Latency', '']));
      const body = node('tbody'); const start = state.usagePage * 25;
      records.slice(start, start + 25).forEach(record => {
        const row = node('tr');
        row.append(cell(dateTime(record.timestamp), 'mono'), cell(name(record.provider)), cell(record.account_label || record.account_id || '—'), cell(record.model || '—', '', true));
        const code = node('span', `status-code${number(record.status) < 200 || number(record.status) >= 400 ? ' bad' : ''}`, record.status || '—');
        add(row, add(node('td'), code), cell(`${fmtToken(record.input_tokens)} / ${fmtToken(record.output_tokens)}`, 'mono'), cell(`${fmt(record.duration_ms)} ms`, 'mono'));
        const detail = button('Details →', 'row-button', () => showRequest(record)); row.append(add(node('td'), detail)); body.append(row);
      });
      table.append(body); tableWrap.append(table); logPanel.append(tableWrap);
      const footer = node('div', 'table-footer');
      footer.append(node('span', '', `Showing ${start + 1}–${Math.min(start + 25, records.length)} of ${records.length}`));
      if (records.length > 25) {
        const nav = node('div');
        const previous = button('← Previous', 'button button-quiet button-small', () => { state.usagePage--; render(); }); previous.disabled = state.usagePage === 0;
        const next = button('Next →', 'button button-quiet button-small', () => { state.usagePage++; render(); }); next.disabled = start + 25 >= records.length;
        add(nav, previous, next); footer.append(nav);
      }
      logPanel.append(footer);
    }
    content.append(logPanel);
    const totals = Array.isArray(state.summary.by_account) ? state.summary.by_account : [];
    const totalsPanel = panel('Account totals', 'ALL RECORDED ATTEMPTS');
    if (!totals.length) totalsPanel.append(empty('No account totals yet', 'Traffic will be attributed to each account attempt.'));
    else {
      const wrap = node('div', 'table-wrap'); const table = node('table', 'data-table');
      table.append(tableHead(['Account', 'Provider', 'Attempts', 'Failures', 'Reported input tokens', 'Reported output tokens']));
      const body = node('tbody');
      totals.forEach(item => { const row = node('tr'); add(row, cell(item.account_label || item.account_id || '—', '', true), cell(name(item.provider)), cell(fmt(item.requests), 'mono'), cell(fmt(item.failures), 'mono'), cell(fmt(item.input_tokens), 'mono'), cell(fmt(item.output_tokens), 'mono')); body.append(row); });
      table.append(body); wrap.append(table); totalsPanel.append(wrap);
    }
    totalsPanel.style.marginTop = '16px'; content.append(totalsPanel);
  }
  function tableHead(labels) { const head = node('thead'); const row = node('tr'); labels.forEach(label => row.append(node('th', '', label))); head.append(row); return head; }
  function cell(value, className = '', strong = false) { const td = node('td', className); td.append(node(strong ? 'strong' : 'span', '', value)); return td; }
  function showRequest(record) {
    const body = clear($('detail-body')); const list = node('dl', 'detail-list');
    const dialog = $('detail-dialog');
    dialog.querySelector('.dialog-header h2').textContent = 'Attempt detail';
    dialog.querySelector('.dialog-header .eyebrow').textContent = 'USAGE / ATTEMPT DETAIL';
    const fields = [['Attempt ID', record.id], ['Time', dateTime(record.timestamp)], ['Provider', name(record.provider)], ['Account', record.account_label || record.account_id], ['Model', record.model], ['Endpoint', record.endpoint], ['Status', record.status], ['Duration', `${fmt(record.duration_ms)} ms`], ['Input tokens', record.input_tokens == null ? 'Not reported' : fmtToken(record.input_tokens)], ['Output tokens', record.output_tokens == null ? 'Not reported' : fmtToken(record.output_tokens)], ['Cached tokens', record.cached_tokens == null ? 'Not reported' : fmtToken(record.cached_tokens)], ['Reasoning tokens', record.reasoning_tokens == null ? 'Not reported' : fmtToken(record.reasoning_tokens)], ['Streaming', record.stream ? 'Yes' : 'No']];
    fields.forEach(([label, value]) => { const pair = node('div'); add(pair, node('dt', '', label), node('dd', '', value ?? '—')); list.append(pair); });
    body.append(add(node('div', 'dialog-body'), list));
    if (record.error) body.querySelector('.dialog-body').append(node('div', 'detail-error', record.error));
    $('detail-dialog').showModal();
  }
  function renderModels(content, actions) {
    actions.append(button('+ Add route account', 'button button-primary', () => openAccountDialog('key')));
    if (state.settings?.demo === true) content.append(notice('Demo mode shows example accounts and usage. Inference requests are disabled until you restart without --demo.'));
    const metadata = state.providers;
    if (!metadata.length) { content.append(empty('No provider metadata available', 'Connect a provider account to set up model routes.')); return; }
    const grid = node('div', 'route-grid');
    metadata.forEach(provider => {
      const id = provider.id || provider.provider;
      const accounts = state.accounts.filter(account => account.provider === id);
      const configured = [...new Set(accounts.flatMap(account => Array.isArray(account.models) ? account.models : []))];
      const card = node('article', 'route-card');
      add(card, add(node('div', 'route-card-head'), node('h2', '', provider.name || name(id)), node('span', `pill${accounts.some(account => account.enabled) ? '' : ' disabled'}`, accounts.some(account => account.enabled) ? 'Configured' : 'No active account')),
        node('p', '', provider.description || 'Connect an account to route requests through this provider.'),
        node('code', '', `${id}/model`));
      const details = node('div', 'route-details');
      add(details, detailRow('Accounts', `${accounts.length} total · ${accounts.filter(account => account.enabled).length} enabled`), detailRow('Models', configured.length ? configured.slice(0, 3).join(', ') + (configured.length > 3 ? ` +${configured.length - 3}` : '') : 'Any model when account allows it'), detailRow('Base URL', provider.base_url || 'Provider default'));
      card.append(details);
      card.append(button('Use in quickstart →', 'text-button', () => { state.selectedProvider = id; render(); $('integration-heading')?.scrollIntoView({behavior: 'smooth', block: 'start'}); }));
      grid.append(card);
    });
    content.append(grid);
    const integration = node('section', 'integration-grid');
    const tabs = node('div', 'integration-tabs');
    [['curl', 'curl'], ['python', 'Python'], ['claude', 'Claude Code'], ['codex', 'Codex CLI']].forEach(([id, label]) => {
      const tab = button(label, state.integration === id ? 'active' : '', () => { state.integration = id; render(); $('integration-heading')?.scrollIntoView({behavior: 'smooth', block: 'start'}); });
      tab.setAttribute('aria-pressed', String(state.integration === id)); tabs.append(tab);
    });
    const codePanel = panel('Integration quickstart', 'COPY · CONFIGURE · RUN'); codePanel.id = 'integration-heading';
    const defaultProvider = state.accounts.find(item => item.enabled)?.provider || metadata[0]?.id || 'openai';
    const selector = filterSelect('Choose provider', state.selectedProvider || defaultProvider, metadata.map(item => [item.id, item.name || name(item.id)]), value => { state.selectedProvider = value; render(); });
    selector.style.marginBottom = '11px'; codePanel.append(selector);
    const selected = state.selectedProvider || defaultProvider;
    const account = state.accounts.find(item => item.provider === selected && item.enabled && Array.isArray(item.models) && item.models.length);
    const model = account?.models?.[0] || '<model>';
    const snippet = quickstart(state.integration, selected, model);
    const box = node('div', 'code-box');
    const copy = button('Copy', 'button button-secondary button-small copy-button', async () => {
      try { await navigator.clipboard.writeText(snippet); copy.textContent = 'Copied'; setTimeout(() => { copy.textContent = 'Copy'; }, 1600); }
      catch { toast('Clipboard unavailable. Select the snippet to copy it.', true); }
    });
    add(box, copy, add(node('pre'), node('code', '', snippet))); codePanel.append(box);
    codePanel.append(node('p', 'section-intro', 'Replace <model> with a supported model and YOUR_API_KEY with your inference key when required. The management token is for this dashboard only.'));
    const endpoints = node('div', 'endpoint-list');
    const selectedMeta = metadata.find(item => item.id === selected);
    (Array.isArray(selectedMeta?.endpoints) ? selectedMeta.endpoints : []).forEach(endpoint => endpoints.append(node('code', '', endpoint)));
    if (endpoints.childElementCount) codePanel.append(endpoints);
    add(integration, tabs, codePanel); content.append(integration);
  }
  function detailRow(label, value) { return add(node('div'), node('span', '', label), node('strong', '', value)); }
  function quickstart(kind, provider, model) {
    const origin = location.origin;
    const routed = `${provider}/${model}`;
    if (kind === 'python') return `from openai import OpenAI\n\nclient = OpenAI(base_url="${origin}/v1", api_key="YOUR_API_KEY")\nreply = client.chat.completions.create(\n    model="${routed}",\n    messages=[{"role": "user", "content": "Hello"}],\n)\nprint(reply.choices[0].message.content)`;
    if (kind === 'claude') return `ANTHROPIC_BASE_URL="${origin}" \\\nANTHROPIC_AUTH_TOKEN="YOUR_API_KEY" \\\nANTHROPIC_MODEL="${routed}" \\\nclaude`;
    if (kind === 'codex') return `# In ~/.codex/config.toml\nmodel = "${routed}"\nmodel_provider = "cliproxy"\n\n[model_providers.cliproxy]\nname = "CLIProxy"\nbase_url = "${origin}/v1"\nwire_api = "responses"\nenv_key = "CLIPROXY_API_KEY"\n\n# Then export CLIPROXY_API_KEY=YOUR_API_KEY and run codex`;
    return `curl ${origin}/v1/chat/completions \\\n  -H 'Authorization: Bearer YOUR_API_KEY' \\\n  -H 'Content-Type: application/json' \\\n  -d '{"model":"${routed}","messages":[{"role":"user","content":"Hello"}]}'`;
  }
  function renderSettings(content, actions) {
    actions.append(button('Disconnect session', 'button button-secondary', () => { disconnect(); render(); }));
    const grid = node('div', 'settings-grid');
    const connection = panel('Connection', 'LOCAL INSTANCE');
    const rows = node('div');
    [['Dashboard origin', location.origin], ['Management API', `${location.origin}/api`], ['Connection', state.error ? 'Error' : 'Connected'], ['Demo mode', state.settings?.demo === true ? 'On · sample data' : 'Off'], ['Inference API key', state.settings?.inference_key_required === true ? 'Required' : state.settings?.inference_key_required === false ? 'Optional on loopback' : 'Unknown']].forEach(([label, value]) => rows.append(detailRow(label, value)));
    rows.className = 'settings-rows';
    for (const row of rows.children) row.className = 'settings-row';
    connection.append(rows); grid.append(connection);
    const security = panel('Session & security', 'BROWSER TAB');
    security.append(node('p', 'security-note', 'The management token is held only in this tab’s session storage. Disconnect clears it. Account secrets are write-only in the dashboard; stored credentials are never returned by the management API.'));
    security.append(node('div', 'subtle-rule'));
    security.append(button('Clear token & disconnect', 'button button-secondary', () => { disconnect(); render(); }));
    grid.append(security); content.append(grid);
  }

  $('connect-form').addEventListener('submit', async event => {
    event.preventDefault();
    const token = $('management-token').value.trim();
    if (!token) return;
    state.token = token;
    sessionStorage.setItem(TOKEN_KEY, token);
    $('connect-error').textContent = '';
    await loadData();
    if (state.error) $('connect-error').textContent = state.error;
  });
  $('refresh-button').addEventListener('click', loadData);
  $('menu-button').addEventListener('click', () => {
    const open = $('sidebar').classList.toggle('open');
    $('menu-button').setAttribute('aria-expanded', String(open));
    $('menu-button').setAttribute('aria-label', open ? 'Close navigation' : 'Open navigation');
  });
  window.addEventListener('hashchange', () => { closeNavigation(); render(); });
  document.addEventListener('click', event => {
    const closer = event.target.closest('[data-close-dialog]');
    if (closer) closer.closest('dialog')?.close();
    if (event.target.closest('.sidebar a')) closeNavigation();
  });
  $('account-form').addEventListener('submit', event => event.preventDefault());
  $('account-dialog').addEventListener('close', () => { clear($('account-dialog-body')); clear($('account-dialog-actions')); state.oauthSession = null; });
  for (const dialog of document.querySelectorAll('dialog')) dialog.addEventListener('click', event => { if (event.target === dialog) dialog.close(); });
  setConnected(false);
  if (state.token) loadData(); else $('management-token').focus();
  render();
  state.refreshTimer = setInterval(() => { if (state.token && !document.hidden && !document.querySelector('dialog[open]')) loadData(); }, 30000);
})();
