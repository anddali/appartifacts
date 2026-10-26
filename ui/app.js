/* apphost portal UI (no build step; served by the Rust binary). */
'use strict';

const state = { me: null, scope: 'all', query: '' };
const view = document.getElementById('view');
const dialog = document.getElementById('dialog');

// ---------- helpers ----------

function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'class') el.className = v;
    else if (k === 'value') el.value = v;
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children.flat()) {
    if (c === undefined || c === null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

async function api(method, path, body) {
  const res = await fetch(path, {
    method,
    credentials: 'same-origin',
    headers: { 'content-type': 'application/json', 'x-apphost-csrf': '1' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (res.status === 401) {
    location.href = '/auth/login?return=' + encodeURIComponent('/' + location.hash);
    throw new Error('Signed out');
  }
  if (res.status === 204) return null;
  const json = await res.json().catch(() => null);
  if (!res.ok) throw new Error((json && json.error && json.error.message) || res.statusText);
  return json;
}

let toastTimer;
function toast(msg, isError) {
  const t = document.getElementById('toast');
  t.textContent = msg;
  t.className = 'show' + (isError ? ' error' : '');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.className = ''), isError ? 5000 : 2200);
}

async function attempt(fn, okMsg) {
  try {
    const r = await fn();
    if (okMsg) toast(okMsg);
    return r;
  } catch (e) {
    toast(e.message, true);
    throw e;
  }
}

function ago(iso) {
  if (!iso) return 'never';
  const s = (Date.now() - new Date(iso).getTime()) / 1000;
  if (s < 60) return 'just now';
  if (s < 3600) return Math.floor(s / 60) + ' min ago';
  if (s < 86400) return Math.floor(s / 3600) + ' h ago';
  if (s < 86400 * 30) return Math.floor(s / 86400) + ' d ago';
  return new Date(iso).toLocaleDateString();
}

function bytes(n) {
  if (n < 1024) return n + ' B';
  if (n < 1024 * 1024) return (n / 1024).toFixed(1) + ' KB';
  return (n / 1024 / 1024).toFixed(1) + ' MB';
}

function copyField(value, label) {
  const input = h('input', { readonly: true, value, 'aria-label': label || 'Value' });
  input.addEventListener('focus', () => input.select());
  return h('div', { class: 'copy' }, input,
    h('button', { class: 'secondary', type: 'button', onclick: () => copy(value) }, 'Copy'));
}

async function copy(text) {
  try {
    await navigator.clipboard.writeText(text);
    toast('Copied');
  } catch {
    toast('Copy failed — select and copy manually', true);
  }
}

function openDialog(content, wide) {
  dialog.replaceChildren(content);
  dialog.className = wide ? 'wide' : '';
  dialog.showModal();
}

function confirmDialog(title, message, action, danger) {
  return new Promise((resolve) => {
    const form = h('form', { method: 'dialog' },
      h('h2', {}, title),
      h('p', {}, message),
      h('div', { class: 'foot' },
        h('button', { class: 'secondary', value: 'cancel' }, 'Cancel'),
        h('button', { class: danger ? 'danger' : '', value: 'ok' }, action)));
    dialog.addEventListener('close', () => resolve(dialog.returnValue === 'ok'), { once: true });
    openDialog(form);
  });
}

function setNav(name) {
  document.querySelectorAll('[data-nav]').forEach((a) => a.classList.toggle('active', a.dataset.nav === name));
}

// ---------- router ----------

async function route() {
  const hash = location.hash.replace(/^#\/?/, '');
  const [page, arg] = hash.split('/');
  try {
    if (page === 'a' && arg) {
      setNav('artifacts');
      await renderArtifact(decodeURIComponent(arg));
    } else if (page === 'connect') {
      setNav('connect');
      await renderConnect();
    } else {
      setNav('artifacts');
      await renderList();
    }
  } catch (e) {
    view.replaceChildren(h('div', { class: 'card' }, h('h2', {}, 'Something went wrong'), h('p', { class: 'muted' }, e.message)));
  }
  window.scrollTo(0, 0);
}

// ---------- artifact list ----------

async function renderList() {
  const listBox = h('div', {}, h('p', { class: 'muted' }, 'Loading…'));
  const search = h('input', { type: 'search', placeholder: 'Search artifacts', value: state.query, 'aria-label': 'Search artifacts' });
  let timer;
  search.addEventListener('input', () => {
    clearTimeout(timer);
    timer = setTimeout(() => { state.query = search.value; load(); }, 200);
  });
  const seg = h('div', { class: 'segmented', role: 'tablist' },
    ...[['all', 'All'], ['mine', 'Mine'], ['shared', 'Shared with me']].map(([k, label]) =>
      h('button', { type: 'button', class: state.scope === k ? 'on' : '', onclick: () => { state.scope = k; renderList(); } }, label)));

  view.replaceChildren(
    h('div', { class: 'page-head' },
      h('div', {}, h('h1', {}, 'Artifacts'), h('p', { class: 'muted' }, 'Pages published by you and your colleagues through Claude.')),
      h('button', { onclick: () => uploadDialog() }, 'Upload')),
    h('div', { class: 'toolbar' }, search, seg),
    listBox);

  async function load() {
    const q = new URLSearchParams({ scope: state.scope, q: state.query });
    const { artifacts } = await api('GET', '/api/artifacts?' + q);
    if (!artifacts.length) {
      listBox.replaceChildren(h('div', { class: 'card empty' },
        h('h2', {}, state.query ? 'No matches' : 'No artifacts yet'),
        h('p', {}, state.query ? 'Try a different search.' : 'Connect Claude, then ask it to build something and publish it to apphost.'),
        !state.query && h('a', { class: 'btn', href: '#/connect' }, 'Connect Claude')));
      return;
    }
    listBox.replaceChildren(h('div', { class: 'list' }, ...artifacts.map(artifactRow)));
  }
  await load();
}

function artifactRow(a) {
  const mine = a.role === 'owner';
  return h('a', { class: 'item', href: '#/a/' + a.id },
    h('div', { class: 'title' }, a.title),
    a.description ? h('div', { class: 'desc' }, a.description) : null,
    h('div', { class: 'meta' },
      h('span', {}, mine ? 'You' : a.owner_name || a.owner_email),
      h('span', {}, 'Updated ' + ago(a.updated_at)),
      h('span', {}, 'v' + a.current_version),
      a.visibility === 'org' ? h('span', { class: 'badge accent' }, 'Org-wide') : h('span', { class: 'badge' }, 'Private'),
      !mine ? h('span', { class: 'badge' }, a.role) : null),
    h('div', { class: 'side' },
      h('span', { class: 'btn secondary', role: 'button', onclick: (e) => { e.preventDefault(); window.open(a.url, '_blank', 'noopener'); } }, 'Open ↗')));
}

// ---------- artifact detail ----------

async function renderArtifact(id) {
  view.replaceChildren(h('p', { class: 'muted' }, 'Loading…'));
  const a = await api('GET', '/api/artifacts/' + encodeURIComponent(id));
  const isOwner = a.role === 'owner';
  const canEdit = a.role === 'owner' || a.role === 'editor';

  const head = h('div', { class: 'page-head' },
    h('div', { class: 'grow' },
      h('a', { class: 'back', href: '#/' }, '← All artifacts'),
      h('h1', {}, a.title),
      h('p', { class: 'muted' }, a.description || 'No description')),
    h('div', { class: 'row' },
      canEdit && h('button', { class: 'secondary', onclick: () => editMeta(a) }, 'Edit details'),
      canEdit && h('button', { class: 'secondary', onclick: () => uploadDialog(a) }, 'Upload version'),
      h('a', { class: 'btn', href: a.url, target: '_blank', rel: 'noopener' }, 'Open ↗')));

  const about = h('div', { class: 'card' },
    h('h2', {}, 'About'),
    h('dl', { class: 'kv' },
      h('dt', {}, 'Link'), h('dd', {}, copyField(a.url, 'Artifact link')),
      h('dt', {}, 'Owner'), h('dd', {}, a.owner_name + ' (' + a.owner_email + ')'),
      h('dt', {}, 'Your access'), h('dd', {}, a.role),
      h('dt', {}, 'Version'), h('dd', {}, 'v' + a.current_version + ' · updated ' + ago(a.updated_at)),
      h('dt', {}, 'Created'), h('dd', {}, new Date(a.created_at).toLocaleString())));

  view.replaceChildren(head,
    h('div', { class: 'section-grid' },
      about,
      sharingCard(a, isOwner),
      dataCard(a, isOwner),
      versionsCard(a, canEdit),
      filesCard(a),
      isOwner && dangerCard(a)));
}

function sharingCard(a, isOwner) {
  const card = h('div', { class: 'card' }, h('h2', {}, 'Sharing'));
  if (!isOwner) {
    card.append(h('p', { class: 'muted' }, a.visibility === 'org'
      ? 'Everyone in the organization can open this artifact.'
      : 'Shared privately. Only the owner can change sharing.'));
    return card;
  }
  const vis = h('div', { class: 'segmented' },
    ...[['private', 'Private'], ['org', 'Everyone in org']].map(([k, label]) =>
      h('button', {
        type: 'button', class: a.visibility === k ? 'on' : '',
        onclick: async () => {
          await attempt(() => api('PATCH', '/api/artifacts/' + a.id, { visibility: k }), 'Visibility updated');
          renderArtifact(a.id);
        },
      }, label)));

  const rows = a.shares.length
    ? h('table', {}, h('tbody', {}, ...a.shares.map((s) => h('tr', {},
        h('td', {}, s.principal_type === 'group' ? h('span', {}, h('span', { class: 'badge' }, 'group'), ' ', h('code', {}, s.principal)) : s.principal),
        h('td', {}, s.role),
        h('td', { class: 'actions' }, h('button', {
          class: 'ghost', title: 'Remove',
          onclick: async () => {
            const q = new URLSearchParams({ principal_type: s.principal_type, principal: s.principal });
            await attempt(() => api('DELETE', '/api/artifacts/' + a.id + '/shares?' + q), 'Removed');
            renderArtifact(a.id);
          },
        }, 'Remove'))))))
    : h('p', { class: 'muted small' }, 'Not shared with specific people yet.');

  const who = h('input', { placeholder: 'name@company.com or group object id', required: true, class: 'grow', 'aria-label': 'Email or group id' });
  const role = h('select', { 'aria-label': 'Role' }, h('option', { value: 'viewer' }, 'Viewer'), h('option', { value: 'editor' }, 'Editor'));
  const form = h('form', {
    class: 'row',
    onsubmit: async (e) => {
      e.preventDefault();
      const v = who.value.trim();
      const isEmail = v.includes('@');
      await attempt(() => api('POST', '/api/artifacts/' + a.id + '/shares', {
        principal_type: isEmail ? 'user' : 'group', principal: v, role: role.value,
      }), 'Shared');
      renderArtifact(a.id);
    },
  }, who, role, h('button', { type: 'submit' }, 'Share'));

  card.append(
    h('div', { class: 'stack' },
      vis,
      h('p', { class: 'muted small' }, 'Viewers can open the page and use its shared data. Editors can also publish new versions.'),
      rows,
      form));
  return card;
}

function dataCard(a, isOwner) {
  const card = h('div', { class: 'card' }, h('h2', {}, 'Shared data'), h('p', { class: 'muted' }, 'Loading…'));
  api('GET', '/api/artifacts/' + a.id + '/data').then(({ collections }) => {
    card.lastChild.remove();
    if (!collections.length) {
      card.append(h('p', { class: 'muted small' }, 'This page has not stored any shared data.'));
      return;
    }
    card.append(h('table', {},
      h('thead', {}, h('tr', {}, h('th', {}, 'Collection'), h('th', {}, 'Docs'), h('th', {}, 'Updated'), h('th', {}))),
      h('tbody', {}, ...collections.map((c) => h('tr', {},
        h('td', {}, h('code', {}, c.name)),
        h('td', {}, c.count),
        h('td', {}, ago(c.updated_at)),
        h('td', { class: 'actions' },
          h('button', { class: 'ghost', onclick: () => showDocs(a, c.name) }, 'View'),
          isOwner && h('button', {
            class: 'ghost',
            onclick: async () => {
              if (!(await confirmDialog('Clear "' + c.name + '"?', 'This permanently deletes all ' + c.count + ' documents in this collection for everyone.', 'Clear collection', true))) return;
              await attempt(() => api('DELETE', '/api/artifacts/' + a.id + '/data/' + encodeURIComponent(c.name)), 'Collection cleared');
              renderArtifact(a.id);
            },
          }, 'Clear')))))));
  }).catch((e) => { card.lastChild.textContent = e.message; });
  return card;
}

async function showDocs(a, collection) {
  const { docs } = await attempt(() => api('GET', '/api/artifacts/' + a.id + '/data/' + encodeURIComponent(collection) + '?limit=200'));
  openDialog(h('form', { method: 'dialog' },
    h('div', { class: 'spread' }, h('h2', {}, collection), h('span', { class: 'muted small' }, docs.length + (docs.length === 200 ? '+' : '') + ' docs')),
    h('pre', { class: 'scroll' }, JSON.stringify(docs.map((d) => ({ id: d.id, data: d.data, updated_by: d.updated_by, updated_at: d.updated_at })), null, 2)),
    h('div', { class: 'foot' }, h('button', { value: 'close' }, 'Close'))), true);
}

function versionsCard(a, canEdit) {
  return h('div', { class: 'card' }, h('h2', {}, 'Versions'),
    h('table', {}, h('tbody', {}, ...a.versions.map((v) => h('tr', {},
      h('td', {}, h('strong', {}, 'v' + v.version), v.version === a.current_version ? h('span', {}, ' ', h('span', { class: 'badge ok' }, 'current')) : null),
      h('td', { class: 'muted small' }, (v.label ? v.label + ' · ' : '') + (v.created_by_email || '') + ' · ' + ago(v.created_at)),
      h('td', { class: 'actions' }, canEdit && v.version !== a.current_version && h('button', {
        class: 'ghost',
        onclick: async () => {
          if (!(await confirmDialog('Restore v' + v.version + '?', 'This publishes the files of v' + v.version + ' as a new version. Shared data is not changed.', 'Restore'))) return;
          await attempt(() => api('POST', '/api/artifacts/' + a.id + '/rollback', { version: v.version }), 'Restored');
          renderArtifact(a.id);
        },
      }, 'Restore')))))));
}

function filesCard(a) {
  return h('div', { class: 'card' }, h('h2', {}, 'Files'),
    h('table', {}, h('tbody', {}, ...a.files.map((f) => h('tr', {},
      h('td', {}, h('a', { href: a.url + '/' + f.path.split('/').map(encodeURIComponent).join('/'), target: '_blank', rel: 'noopener' }, f.path)),
      h('td', { class: 'muted small' }, f.content_type.split(';')[0]),
      h('td', { class: 'muted small actions' }, bytes(f.size)))))));
}

function dangerCard(a) {
  return h('div', { class: 'card danger-zone wide' },
    h('div', { class: 'spread' },
      h('div', {}, h('h2', {}, 'Delete artifact'), h('p', { class: 'muted small' }, 'Deletes all versions and shared data. The link stops working for everyone.')),
      h('button', {
        class: 'danger',
        onclick: async () => {
          if (!(await confirmDialog('Delete "' + a.title + '"?', 'This cannot be undone.', 'Delete permanently', true))) return;
          await attempt(() => api('DELETE', '/api/artifacts/' + a.id), 'Deleted');
          location.hash = '#/';
        },
      }, 'Delete')));
}

function editMeta(a) {
  const title = h('input', { value: a.title, required: true, maxlength: 200 });
  const desc = h('textarea', { maxlength: 2000 }, a.description);
  openDialog(h('form', {
    onsubmit: async (e) => {
      e.preventDefault();
      await attempt(() => api('PATCH', '/api/artifacts/' + a.id, { title: title.value, description: desc.value }), 'Saved');
      dialog.close();
      renderArtifact(a.id);
    },
  },
  h('h2', {}, 'Edit details'),
  h('label', {}, 'Title', title),
  h('label', {}, 'Description', desc),
  h('div', { class: 'foot' },
    h('button', { type: 'button', class: 'secondary', onclick: () => dialog.close() }, 'Cancel'),
    h('button', { type: 'submit' }, 'Save'))));
}

// ---------- upload ----------

function toBase64(buf) {
  const bytes = new Uint8Array(buf);
  let bin = '';
  for (let i = 0; i < bytes.length; i += 0x8000) bin += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
  return btoa(bin);
}

function uploadDialog(existing) {
  const files = h('input', { type: 'file', multiple: true });
  const folder = h('input', { type: 'file', webkitdirectory: true });
  const title = h('input', { placeholder: 'e.g. Team retro board', maxlength: 200, value: existing ? existing.title : '' });
  const desc = h('input', { placeholder: 'One sentence (optional)', maxlength: 2000, value: existing ? existing.description : '' });
  const status = h('p', { class: 'muted small' }, 'Choose an index.html (plus any assets), or a folder that contains one.');
  const submit = h('button', { type: 'submit' }, existing ? 'Publish version' : 'Publish');

  openDialog(h('form', {
    onsubmit: async (e) => {
      e.preventDefault();
      const picked = [...folder.files, ...files.files];
      if (!picked.length) return toast('Choose files first', true);
      submit.disabled = true;
      status.textContent = 'Reading ' + picked.length + ' file(s)…';
      try {
        const payload = [];
        for (const f of picked) {
          let path = f.webkitRelativePath ? f.webkitRelativePath.split('/').slice(1).join('/') : f.name;
          payload.push({ path, content_base64: toBase64(await f.arrayBuffer()), content_type: f.type || undefined });
        }
        status.textContent = 'Publishing…';
        const body = { files: payload, title: title.value || undefined, description: desc.value || undefined, replace_all: !!existing };
        const r = existing
          ? await api('POST', '/api/artifacts/' + existing.id + '/versions', body)
          : await api('POST', '/api/artifacts', body);
        dialog.close();
        toast('Published v' + r.version);
        location.hash = '#/a/' + r.artifact_id;
        if (existing) renderArtifact(existing.id);
      } catch (err) {
        status.textContent = err.message;
        submit.disabled = false;
      }
    },
  },
  h('h2', {}, existing ? 'Upload a new version' : 'Upload an artifact'),
  h('label', {}, 'Files', files),
  h('label', {}, 'Or a folder', folder),
  h('label', {}, 'Title', title),
  h('label', {}, 'Description', desc),
  existing && h('p', { class: 'muted small' }, 'The new version replaces all files of the current one. Shared data is kept.'),
  status,
  h('div', { class: 'foot' },
    h('button', { type: 'button', class: 'secondary', onclick: () => dialog.close() }, 'Cancel'),
    submit)));
}

// ---------- connect Claude ----------

async function renderConnect() {
  const me = state.me;
  const mcp = me.mcp_url;
  const tokensBox = h('div', {}, h('p', { class: 'muted' }, 'Loading…'));
  const connectionsBox = h('div', {});
  const created = h('div', {});

  const name = h('input', { placeholder: 'e.g. Claude Code on my laptop', required: true, maxlength: 100, class: 'grow', 'aria-label': 'Token name' });
  const days = h('select', { 'aria-label': 'Expiry' }, ...[30, 90, 180, 365].map((d) => h('option', { value: d, selected: d === 90 }, d + ' days')));

  async function load() {
    const { tokens, connections } = await api('GET', '/api/tokens');
    tokensBox.replaceChildren(tokens.length
      ? h('table', {}, h('thead', {}, h('tr', {}, h('th', {}, 'Name'), h('th', {}, 'Last used'), h('th', {}, 'Expires'), h('th', {}))),
          h('tbody', {}, ...tokens.map((t) => h('tr', {},
            h('td', {}, t.name), h('td', { class: 'muted' }, ago(t.last_used_at)),
            h('td', { class: 'muted' }, t.expires_at ? new Date(t.expires_at).toLocaleDateString() : 'never'),
            h('td', { class: 'actions' }, h('button', {
              class: 'ghost',
              onclick: async () => {
                if (!(await confirmDialog('Revoke "' + t.name + '"?', 'Anything using this token stops working immediately.', 'Revoke', true))) return;
                await attempt(() => api('DELETE', '/api/tokens/' + t.id), 'Revoked');
                load();
              },
            }, 'Revoke'))))))
      : h('p', { class: 'muted small' }, 'No active tokens.'));
    connectionsBox.replaceChildren(connections.length
      ? h('table', {}, h('tbody', {}, ...connections.map((c) => h('tr', {},
          h('td', {}, c.client_name), h('td', { class: 'muted small' }, 'connected ' + ago(c.connected_at) + ' · used ' + ago(c.last_used_at)),
          h('td', { class: 'actions' }, h('button', {
            class: 'ghost',
            onclick: async () => {
              if (!(await confirmDialog('Disconnect ' + c.client_name + '?', 'It will need to sign in again to use apphost.', 'Disconnect', true))) return;
              await attempt(() => api('DELETE', '/api/connections/' + encodeURIComponent(c.client_id)), 'Disconnected');
              load();
            },
          }, 'Disconnect'))))))
      : h('p', { class: 'muted small' }, 'No apps connected with OAuth yet.'));
  }

  const form = h('form', {
    class: 'row',
    onsubmit: async (e) => {
      e.preventDefault();
      const r = await attempt(() => api('POST', '/api/tokens', { name: name.value, expires_in_days: Number(days.value) }));
      name.value = '';
      created.replaceChildren(h('div', { class: 'callout stack' },
        h('strong', {}, 'Copy your token now — it will not be shown again.'),
        copyField(r.token, 'New token'),
        h('div', {}, h('p', { class: 'small' }, 'Claude Code:'),
          h('pre', {}, 'claude mcp add --transport http apphost ' + mcp + ' --header "Authorization: Bearer ' + r.token + '"'))));
      load();
    },
  }, name, days, h('button', { type: 'submit' }, 'Create token'));

  view.replaceChildren(
    h('div', { class: 'page-head' }, h('div', {}, h('h1', {}, 'Connect Claude'),
      h('p', { class: 'muted' }, 'Add apphost as an MCP connector so Claude can publish and update artifacts for you.'))),
    h('div', { class: 'card' },
      h('h2', {}, 'MCP server URL'),
      copyField(mcp, 'MCP server URL')),
    h('div', { class: 'section-grid' },
      h('div', { class: 'card' },
        h('h2', {}, 'Option A — Sign in with SSO (recommended)'),
        h('p', { class: 'small' }, h('strong', {}, 'Claude (web or desktop):')),
        h('ol', { class: 'steps small' },
          h('li', {}, 'Open Settings → Connectors → Add custom connector.'),
          h('li', {}, 'Name it apphost and paste the MCP server URL.'),
          h('li', {}, 'Click Connect and sign in with your company account, then Allow.')),
        h('p', { class: 'small' }, h('strong', {}, 'Claude Code:')),
        h('pre', {}, 'claude mcp add --transport http apphost ' + mcp),
        h('p', { class: 'muted small' }, 'Then run /mcp in Claude Code and choose apphost → Authenticate.'),
        h('h2', { class: 'mt' }, 'Connected apps'),
        connectionsBox),
      h('div', { class: 'card' },
        h('h2', {}, 'Option B — Personal access token'),
        h('p', { class: 'muted small' }, 'For clients without OAuth support, scripts and CI. Send it as an Authorization: Bearer header.'),
        h('div', { class: 'stack' }, form, created, tokensBox))));
  await load();
}

// ---------- boot ----------

(async function boot() {
  try {
    state.me = await api('GET', '/api/me');
    document.getElementById('user-name').textContent = state.me.user.name || state.me.user.email;
  } catch (e) {
    view.textContent = e.message;
    return;
  }
  window.addEventListener('hashchange', route);
  route();
})();
