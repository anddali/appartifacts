//! Text Claude reads through MCP: server instructions and the page-authoring guide.

pub const INSTRUCTIONS: &str = "\
apphost is this organization's private hosting for HTML artifacts (apps, tools, dashboards, reports, games). \
Use it instead of claude.ai Artifacts.\n\
- publish_artifact publishes a page and returns its URL. Pages are private to the publisher until shared.\n\
- To change an artifact, call publish_artifact again with its artifact_id; files you omit are kept from the previous version.\n\
- Pages can keep SHARED, LIVE state (todo lists, sign-up sheets, trackers, polls) with the built-in SDK. \
Call get_authoring_guide before writing a page that stores data, and never use localStorage for state other people should see.\n\
- Ask the user before deleting artifacts, clearing data, or sharing with others.";

pub const AUTHORING_GUIDE: &str = r#"# Writing pages for apphost

## Hosting model
- Each artifact is served from its own origin: https://<artifact-id>.<content-domain>/ . Viewers sign in with company SSO automatically.
- `index.html` is required. Other files (js, css, images, json) are served at their relative paths, e.g. `<script src="app.js">`.
- Content Security Policy: scripts, styles and fonts may load from the page's own origin and the approved CDNs
  (cdnjs.cloudflare.com, cdn.jsdelivr.net, unpkg.com; fonts.googleapis.com / fonts.gstatic.com for fonts) unless the
  administrator changed this. Inline <script> and <style> are allowed. Network requests (fetch/XHR/WebSocket) are
  limited to the page's own origin, so external APIs cannot be called from the page. Images: own origin, data: and blob: URLs.
- Pin exact library versions in CDN URLs (e.g. react@18.3.1).
- Make layouts responsive and support dark mode with prefers-color-scheme.

## Shared state SDK
Include it with:

    <script src="/__apphost/sdk.js"></script>

Everything is scoped to this artifact. Everyone who can open the page can read and write its data.

    const me = await apphost.me();            // { id, email, name, role }
    const todos = apphost.collection('todos'); // collection names: letters, digits, _ and -

    await todos.list();                        // [doc, ...] oldest first
    await todos.get(id);                       // doc or null
    await todos.add({ text: 'Buy milk', done: false });   // creates with a generated id, returns doc
    await todos.set('my-id', { ... });         // create or replace with your own id
    await todos.update(id, { done: true });    // shallow merge into existing data
    await todos.remove(id);

    // Live: callback receives the full, current array of docs now and after every change by anyone.
    const stop = todos.live(docs => render(docs));

    // Low-level change events: { op: 'upsert'|'delete'|'clear'|'resync', collection, id, doc }
    const unsubscribe = todos.subscribe(ev => ...);

A doc looks like:

    { id, collection, data: {...your JSON...}, version, created_by, updated_by, created_at, updated_at }

`created_by` / `updated_by` are the email addresses of the people who wrote the doc.
Optimistic concurrency: `set(id, data, { expectedVersion: doc.version })` or `update(id, patch, { expectedVersion })`
fails with error.status 409 if someone else changed the doc first; `expectedVersion: 0` means "only create".
Limits: 256 KB per doc, 100,000 docs per artifact.

Patterns:
- One doc per item (each todo, each vote, each row). Avoid one big doc that everyone overwrites.
- Counters/votes: one doc per voter with id = me.id, then aggregate in the page.
- Per-user settings: id = me.id.
- Render from `live()`; call add/update/remove from event handlers and let `live()` re-render.
- Escape user-provided text when rendering (use textContent, not innerHTML).

## Minimal shared todo list

    <!doctype html>
    <html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
    <title>Team todos</title></head>
    <body>
      <form id="f"><input id="t" placeholder="Add a todo" required><button>Add</button></form>
      <ul id="list"></ul>
      <script src="/__apphost/sdk.js"></script>
      <script>
        const todos = apphost.collection('todos');
        const list = document.getElementById('list');
        todos.live(docs => {
          list.replaceChildren(...docs.map(d => {
            const li = document.createElement('li');
            const cb = Object.assign(document.createElement('input'), { type: 'checkbox', checked: !!d.data.done });
            cb.onchange = () => todos.update(d.id, { done: cb.checked });
            const del = Object.assign(document.createElement('button'), { textContent: '×' });
            del.onclick = () => todos.remove(d.id);
            li.append(cb, ' ', d.data.text, ' ', del);
            return li;
          }));
        });
        document.getElementById('f').onsubmit = async e => {
          e.preventDefault();
          const t = document.getElementById('t');
          await todos.add({ text: t.value, done: false });
          t.value = '';
        };
      </script>
    </body></html>

## Tools for data
The data_* MCP tools read and write the same collections, so you can seed data, inspect what users entered, or
migrate data after changing the page.
"#;
