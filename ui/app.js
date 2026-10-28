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

const ICONS = {
  external: 'M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5',
  back: 'M15 18l-6-6 6-6',
  search: 'M11 18a7 7 0 1 0 0-14 7 7 0 0 0 0 14zM20 20l-4-4',
  upload: 'M12 16V4M7 9l5-5 5 5M5 20h14',
};

function icon(name) {
  const NS = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('class', 'icon');
  svg.setAttribute('aria-hidden', 'true');
  const path = document.createElementNS(NS, 'path');
  path.setAttribute('d', ICONS[name]);
  svg.append(path);
  return svg;
}

function hexmark(title, org, size) {
  const letter = (title || '?').trim().charAt(0).toUpperCase() || '?';
  return h('span', { class: 'hexmark' + (org ? ' org' : '') + (size ? ' ' + size : ''), 'aria-hidden': 'true' }, letter);
}

function skeletonRows(n) {
  return Array.from({ length: n }, () => h('div', { class: 'item loading', 'aria-hidden': 'true' },
    h('span', { class: 'hexmark' }),
    h('div', { class: 'name stack' }, h('span', { class: 'skel w40' }), h('span', { class: 'skel w60' })),
    h('div', { class: 'meta-cells' }, h('span', { class: 'skel w80' }), h('span', { class: 'skel w60' }), h('span', { class: 'skel w60' })),
    h('span', {})));
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
    toast('Copy failed. Select the text and copy it manually.', true);
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
    view.replaceChildren(h('div', { class: 'panel empty' },
      h('h2', {}, 'Something went wrong'),
      h('p', {}, e.message),
      h('div', { class: 'row' },
        h('button', { type: 'button', class: 'secondary', onclick: () => route() }, 'Try again'),
        h('a', { class: 'btn', href: '#/' }, 'All artifacts'))));
  }
  window.scrollTo(0, 0);
}

// ---------- artifact list ----------

async function renderList() {
  const listBox = h('div', { 'aria-busy': 'true' }, h('div', { class: 'list' }, ...skeletonRows(4)));
  const count = h('span', { class: 'count num' });
  const search = h('input', { type: 'search', placeholder: 'Search by title or description', value: state.query, 'aria-label': 'Search artifacts' });
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
      h('div', {}, h('h1', {}, 'Artifacts', count), h('p', { class: 'lede' }, 'Pages published by you and your colleagues through Claude.')),
      h('button', { type: 'button', onclick: () => uploadDialog() }, icon('upload'), 'Upload')),
    h('div', { class: 'toolbar' }, h('div', { class: 'search' }, icon('search'), search), seg),
    listBox);

  async function load() {
    const q = new URLSearchParams({ scope: state.scope, q: state.query });
    const { artifacts } = await api('GET', '/api/artifacts?' + q);
    listBox.removeAttribute('aria-busy');
    count.textContent = artifacts.length ? String(artifacts.length) : '';
    if (!artifacts.length) {
      listBox.replaceChildren(h('div', { class: 'panel empty' },
        hexmark(state.query ? '?' : '+', true, 'lg'),
        h('h2', {}, state.query ? 'No matches' : 'No artifacts yet'),
        h('p', {}, state.query ? 'Try a different search.' : 'Connect Claude, then ask it to build something and publish it to apphost.'),
        !state.query && h('a', { class: 'btn', href: '#/connect' }, 'Connect Claude')));
      return;
    }
    listBox.replaceChildren(h('div', { class: 'list' },
      h('div', { class: 'list-head', 'aria-hidden': 'true' },
        h('span', {}), h('span', {}, 'Name'), h('span', {}, 'Owner'), h('span', {}, 'Updated'), h('span', {}, 'Access'), h('span', {})),
      ...artifacts.map(artifactRow)));
  }
  await load();
}

function artifactRow(a) {
  const mine = a.role === 'owner';
  const org = a.visibility === 'org';
  return h('a', { class: 'item', href: '#/a/' + a.id },
    hexmark(a.title, org),
    h('div', { class: 'name' },
      h('div', { class: 'title' }, a.title),
      a.description ? h('div', { class: 'desc' }, a.description) : null),
    h('div', { class: 'meta-cells' },
      h('div', { class: 'cell' }, h('span', { class: 'mlabel' }, 'Owner '), mine ? 'You' : a.owner_name || a.owner_email),
      h('div', { class: 'cell num' }, ago(a.updated_at), h('span', { class: 'muted' }, 'v' + a.current_version)),
      h('div', { class: 'cell access' },
        org ? h('span', { class: 'badge accent' }, 'Org-wide') : h('span', { class: 'badge' }, 'Private'),
        !mine ? h('span', { class: 'badge' }, a.role) : null)),
    h('div', { class: 'side' },
      h('span', { class: 'btn secondary', role: 'button', 'aria-label': 'Open ' + a.title + ' in a new tab', onclick: (e) => { e.preventDefault(); window.open(a.url, '_blank', 'noopener'); } }, 'Open', icon('external'))));
}

// ---------- artifact detail ----------

async function renderArtifact(id) {
  view.replaceChildren(h('div', { class: 'detail', 'aria-busy': 'true' },
    h('div', { class: 'detail-main' },
      h('div', { class: 'panel stack' }, h('span', { class: 'skel w40' }), h('span', { class: 'skel w80' }), h('span', { class: 'skel w60' }))),
    h('div', { class: 'detail-side' },
      h('div', { class: 'panel stack' }, h('span', { class: 'skel w60' }), h('span', { class: 'skel w80' })))));
  const a = await api('GET', '/api/artifacts/' + encodeURIComponent(id));
  const isOwner = a.role === 'owner';
  const canEdit = a.role === 'owner' || a.role === 'editor';

  const head = h('div', { class: 'page-head' },
    h('div', { class: 'grow' },
      h('a', { class: 'back', href: '#/' }, icon('back'), 'All artifacts'),
      h('div', { class: 'title-line' },
        hexmark(a.title, a.visibility === 'org', 'lg'),
        h('div', {}, h('h1', {}, a.title), h('p', { class: 'lede' }, a.description || 'No description')))),
    h('div', { class: 'row actions' },
      canEdit && h('button', { type: 'button', class: 'secondary', onclick: () => editMeta(a) }, 'Edit details'),
      canEdit && h('button', { type: 'button', class: 'secondary', onclick: () => uploadDialog(a) }, 'Upload version'),
      h('a', { class: 'btn', href: a.url, target: '_blank', rel: 'noopener', 'aria-label': 'Open ' + a.title + ' in a new tab' }, 'Open', icon('external'))));

  const fact = (term, value) => h('div', {}, h('dt', {}, term), h('dd', {}, value));
  const about = h('section', { class: 'panel' },
    h('h2', {}, 'About'),
    h('dl', { class: 'kv' },
      fact('Link', copyField(a.url, 'Artifact link')),
      fact('Owner', h('span', {}, a.owner_name, h('span', { class: 'muted small' }, ' ' + a.owner_email))),
      fact('Your access', a.role),
      fact('Version', h('span', { class: 'num' }, 'v' + a.current_version + ', updated ' + ago(a.updated_at))),
      fact('Created', h('span', { class: 'num' }, new Date(a.created_at).toLocaleString()))));

  view.replaceChildren(head,
    h('div', { class: 'detail' },
      h('div', { class: 'detail-main' },
        sharingCard(a, isOwner),
        versionsCard(a, canEdit),
        dataCard(a, isOwner),
        filesCard(a),
        isOwner && dangerCard(a)),
      h('aside', { class: 'detail-side' }, about)));
}

function sharingCard(a, isOwner) {
  const card = h('section', { class: 'panel' }, h('h2', {}, 'Sharing'));
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
        h('td', {}, s.principal_type === 'group' ? h('span', { class: 'row' }, h('span', { class: 'badge' }, 'group'), h('code', {}, s.principal)) : s.principal),
        h('td', {}, s.role),
        h('td', { class: 'actions' }, h('button', {
          type: 'button', class: 'ghost warn-text', title: 'Remove',
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
    class: 'share-form',
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
      h('div', { class: 'spread' }, vis, h('p', { class: 'vis-note' }, 'Viewers can open the page and use its shared data. Editors can also publish new versions.')),
      form,
      rows));
  return card;
}

function dataCard(a, isOwner) {
  const card = h('section', { class: 'panel' }, h('h2', {}, 'Shared data'), h('div', { class: 'stack', 'aria-busy': 'true' }, h('span', { class: 'skel w60' }), h('span', { class: 'skel w40' })));
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
        h('td', { class: 'num' }, c.count),
        h('td', { class: 'num' }, ago(c.updated_at)),
        h('td', { class: 'actions' },
          h('button', { type: 'button', class: 'ghost', onclick: () => showDocs(a, c.name) }, 'View'),
          isOwner && h('button', {
            type: 'button', class: 'ghost warn-text',
            onclick: async () => {
              if (!(await confirmDialog('Clear "' + c.name + '"?', 'This permanently deletes all ' + c.count + ' documents in this collection for everyone.', 'Clear collection', true))) return;
              await attempt(() => api('DELETE', '/api/artifacts/' + a.id + '/data/' + encodeURIComponent(c.name)), 'Collection cleared');
              renderArtifact(a.id);
            },
          }, 'Clear')))))));
  }).catch((e) => { card.lastChild.replaceWith(h('p', { class: 'warn' }, e.message)); });
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
  return h('section', { class: 'panel' }, h('h2', {}, 'Versions'),
    h('table', { class: 'versions' }, h('tbody', {}, ...a.versions.map((v) => h('tr', {},
      h('td', {}, h('span', { class: 'ver-row' }, h('span', { class: 'ver num' }, 'v' + v.version), v.version === a.current_version ? h('span', { class: 'badge ok' }, 'current') : null)),
      h('td', {}, v.label ? h('span', {}, v.label) : null, h('span', { class: 'muted' }, v.created_by_email || '')),
      h('td', { class: 'muted num' }, ago(v.created_at)),
      h('td', { class: 'actions' }, canEdit && v.version !== a.current_version && h('button', {
        type: 'button', class: 'ghost',
        onclick: async () => {
          if (!(await confirmDialog('Restore v' + v.version + '?', 'This publishes the files of v' + v.version + ' as a new version. Shared data is not changed.', 'Restore'))) return;
          await attempt(() => api('POST', '/api/artifacts/' + a.id + '/rollback', { version: v.version }), 'Restored');
          renderArtifact(a.id);
        },
      }, 'Restore')))))));
}

function filesCard(a) {
  return h('section', { class: 'panel' }, h('h2', {}, 'Files'),
    h('table', {}, h('tbody', {}, ...a.files.map((f) => h('tr', {},
      h('td', {}, h('a', { class: 'mono', href: a.url + '/' + f.path.split('/').map(encodeURIComponent).join('/'), target: '_blank', rel: 'noopener' }, f.path)),
      h('td', { class: 'muted' }, f.content_type.split(';')[0]),
      h('td', { class: 'muted num actions' }, bytes(f.size)))))));
}

function dangerCard(a) {
  return h('section', { class: 'panel danger-zone' },
    h('div', { class: 'spread' },
      h('div', { class: 'stack' }, h('h2', {}, 'Delete artifact'), h('p', { class: 'muted small' }, 'Deletes all versions and shared data. The link stops working for everyone.')),
      h('button', {
        type: 'button', class: 'danger-outline',
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
  const tokensBox = h('div', { class: 'stack', 'aria-busy': 'true' }, h('span', { class: 'skel w60' }), h('span', { class: 'skel w40' }));
  const connectionsBox = h('div', {});
  const created = h('div', {});

  const name = h('input', { placeholder: 'e.g. Claude Code on my laptop', required: true, maxlength: 100, class: 'grow', 'aria-label': 'Token name' });
  const days = h('select', { 'aria-label': 'Expiry' }, ...[30, 90, 180, 365].map((d) => h('option', { value: d, selected: d === 90 }, d + ' days')));

  async function load() {
    const { tokens, connections } = await api('GET', '/api/tokens');
    tokensBox.replaceChildren(tokens.length
      ? h('table', {}, h('thead', {}, h('tr', {}, h('th', {}, 'Name'), h('th', {}, 'Last used'), h('th', {}, 'Expires'), h('th', {}))),
          h('tbody', {}, ...tokens.map((t) => h('tr', {},
            h('td', {}, t.name), h('td', { class: 'muted num' }, ago(t.last_used_at)),
            h('td', { class: 'muted num' }, t.expires_at ? new Date(t.expires_at).toLocaleDateString() : 'never'),
            h('td', { class: 'actions' }, h('button', {
              type: 'button', class: 'ghost warn-text',
              onclick: async () => {
                if (!(await confirmDialog('Revoke "' + t.name + '"?', 'Anything using this token stops working immediately.', 'Revoke', true))) return;
                await attempt(() => api('DELETE', '/api/tokens/' + t.id), 'Revoked');
                load();
              },
            }, 'Revoke'))))))
      : h('p', { class: 'muted small' }, 'No active tokens.'));
    connectionsBox.replaceChildren(connections.length
      ? h('table', {}, h('tbody', {}, ...connections.map((c) => h('tr', {},
          h('td', {}, c.client_name),
          h('td', { class: 'num' }, 'Connected ' + ago(c.connected_at), h('span', { class: 'muted' }, 'Last used ' + ago(c.last_used_at))),
          h('td', { class: 'actions' }, h('button', {
            type: 'button', class: 'ghost warn-text',
            onclick: async () => {
              if (!(await confirmDialog('Disconnect ' + c.client_name + '?', 'It will need to sign in again to use apphost.', 'Disconnect', true))) return;
              await attempt(() => api('DELETE', '/api/connections/' + encodeURIComponent(c.client_id)), 'Disconnected');
              load();
            },
          }, 'Disconnect'))))))
      : h('p', { class: 'muted small' }, 'No apps connected with OAuth yet.'));
  }

  const form = h('form', {
    class: 'share-form',
    onsubmit: async (e) => {
      e.preventDefault();
      const r = await attempt(() => api('POST', '/api/tokens', { name: name.value, expires_in_days: Number(days.value) }));
      name.value = '';
      created.replaceChildren(h('div', { class: 'fresh stack' },
        h('strong', {}, 'Copy your token now. It will not be shown again.'),
        copyField(r.token, 'New token'),
        h('div', {}, h('p', { class: 'small' }, 'Claude Code:'),
          h('pre', {}, 'claude mcp add --transport http apphost ' + mcp + ' --header "Authorization: Bearer ' + r.token + '"'))));
      load();
    },
  }, name, days, h('button', { type: 'submit' }, 'Create token'));

  const big = copyField(mcp, 'MCP server URL');
  big.classList.add('big');

  view.replaceChildren(
    h('div', { class: 'page-head' }, h('div', {}, h('h1', {}, 'Connect Claude'),
      h('p', { class: 'lede' }, 'Add apphost as an MCP connector so Claude can publish and update artifacts for you.'))),
    h('section', { class: 'panel connect-url' },
      h('h2', {}, 'MCP server URL'),
      big),
    h('div', { class: 'connect-grid' },
      h('section', { class: 'panel' },
        h('div', { class: 'panel-head' }, h('h2', {}, 'Sign in with SSO'), h('span', { class: 'badge accent' }, 'Recommended')),
        h('p', { class: 'client' }, 'Claude (web or desktop)'),
        h('ol', { class: 'steps' },
          h('li', {}, h('span', {}, 'Open Settings → Connectors → Add custom connector.')),
          h('li', {}, h('span', {}, 'Name it apphost and paste the MCP server URL.')),
          h('li', {}, h('span', {}, 'Click Connect and sign in with your company account, then Allow.'))),
        h('p', { class: 'client' }, 'Claude Code'),
        h('pre', {}, 'claude mcp add --transport http apphost ' + mcp),
        h('p', { class: 'muted small mt-xs' }, 'Then run /mcp in Claude Code and choose apphost → Authenticate.'),
        h('h3', { class: 'sub' }, 'Connected apps'),
        connectionsBox),
      h('section', { class: 'panel' },
        h('h2', {}, 'Personal access token'),
        h('p', { class: 'muted small' }, 'For clients without OAuth support, scripts and CI. Send it as an Authorization: Bearer header.'),
        h('div', { class: 'stack mt-md' }, form, created, h('h3', { class: 'sub' }, 'Active tokens'), tokensBox))));
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
