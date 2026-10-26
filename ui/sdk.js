/* apphost page SDK: shared, live state for artifact pages. See the authoring guide. */
(function () {
  'use strict';
  var API = '/__apphost/api';
  var listeners = new Set();
  var source = null;
  var everOpened = false;
  var mePromise = null;

  function ApphostError(message, status, code) {
    var e = new Error(message);
    e.name = 'ApphostError';
    e.status = status;
    e.code = code;
    return e;
  }

  async function request(method, path, body) {
    var res = await fetch(API + path, {
      method: method,
      credentials: 'same-origin',
      headers: { 'content-type': 'application/json', 'x-apphost-csrf': '1' },
      body: body === undefined ? undefined : JSON.stringify(body)
    });
    if (res.status === 401) {
      // Session expired: reloading runs the SSO hand-off again.
      location.reload();
      throw ApphostError('Your session expired. Reloading…', 401, 'unauthorized');
    }
    if (res.status === 204) return null;
    var text = await res.text();
    var json = null;
    try { json = text ? JSON.parse(text) : null; } catch (_) { /* not JSON */ }
    if (!res.ok) {
      var err = (json && json.error) || {};
      throw ApphostError(err.message || res.statusText || 'Request failed', res.status, err.code);
    }
    return json;
  }

  function emit(ev) {
    listeners.forEach(function (fn) {
      try { fn(ev); } catch (e) { console.error('[apphost] listener error', e); }
    });
  }

  function ensureStream() {
    if (source || typeof EventSource === 'undefined') return;
    source = new EventSource(API + '/events');
    source.addEventListener('change', function (m) {
      try { emit(JSON.parse(m.data)); } catch (e) { console.error('[apphost] bad event', e); }
    });
    source.addEventListener('open', function () {
      // After a reconnect we may have missed changes.
      if (everOpened) emit({ op: 'resync' });
      everOpened = true;
    });
  }

  function subscribe(fn) {
    listeners.add(fn);
    ensureStream();
    return function () { listeners.delete(fn); };
  }

  function seg(s) {
    return encodeURIComponent(String(s));
  }

  function Collection(name) {
    if (!/^[A-Za-z0-9_-]{1,64}$/.test(name)) throw ApphostError('Invalid collection name: ' + name, 400, 'bad_request');
    this.name = name;
    this._base = '/data/' + seg(name);
  }

  Collection.prototype.list = async function () {
    var all = [];
    for (var offset = 0; ; offset += 1000) {
      var page = await request('GET', this._base + '?limit=1000&offset=' + offset);
      all = all.concat(page.docs);
      if (page.docs.length < 1000) return all;
    }
  };

  Collection.prototype.get = async function (id) {
    try {
      return await request('GET', this._base + '/' + seg(id));
    } catch (e) {
      if (e.status === 404) return null;
      throw e;
    }
  };

  Collection.prototype._local = function (doc) {
    emit({ op: 'upsert', collection: this.name, id: doc.id, doc: doc, local: true });
    return doc;
  };

  Collection.prototype.add = async function (data) {
    return this._local(await request('POST', this._base, { data: data }));
  };

  Collection.prototype.set = async function (id, data, opts) {
    var body = { data: data };
    if (opts && opts.expectedVersion !== undefined) body.expected_version = opts.expectedVersion;
    return this._local(await request('PUT', this._base + '/' + seg(id), body));
  };

  Collection.prototype.update = async function (id, patch, opts) {
    var body = { data: patch };
    if (opts && opts.expectedVersion !== undefined) body.expected_version = opts.expectedVersion;
    return this._local(await request('PATCH', this._base + '/' + seg(id), body));
  };

  Collection.prototype.remove = async function (id) {
    try {
      await request('DELETE', this._base + '/' + seg(id));
    } catch (e) {
      if (e.status !== 404) throw e;
    }
    emit({ op: 'delete', collection: this.name, id: id, local: true });
  };

  Collection.prototype.subscribe = function (fn) {
    var name = this.name;
    return subscribe(function (ev) {
      if (ev.op === 'resync' || ev.collection === name) fn(ev);
    });
  };

  /** Calls fn(docs) with the full current list now and after every change. Returns a stop function. */
  Collection.prototype.live = function (fn) {
    var self = this;
    var docs = new Map();
    var loaded = false;
    var pending = [];
    var stopped = false;

    function sorted() {
      return Array.from(docs.values()).sort(function (a, b) {
        return a.created_at < b.created_at ? -1 : a.created_at > b.created_at ? 1 : a.id < b.id ? -1 : 1;
      });
    }
    function publish() {
      if (!stopped) fn(sorted());
    }
    function apply(ev) {
      if (ev.op === 'upsert' && ev.doc) {
        var cur = docs.get(ev.doc.id);
        if (!cur || cur.version <= ev.doc.version) docs.set(ev.doc.id, ev.doc);
      } else if (ev.op === 'delete') {
        docs.delete(ev.id);
      }
    }
    async function reload() {
      var list = await self.list();
      docs = new Map(list.map(function (d) { return [d.id, d]; }));
      loaded = true;
      pending.forEach(apply);
      pending = [];
      publish();
    }

    var unsubscribe = this.subscribe(function (ev) {
      if (ev.op === 'resync' || ev.op === 'clear') {
        reload().catch(function (e) { console.error('[apphost] reload failed', e); });
        return;
      }
      if (!loaded) { pending.push(ev); return; }
      apply(ev);
      publish();
    });
    reload().catch(function (e) { console.error('[apphost] initial load failed', e); });
    return function () { stopped = true; unsubscribe(); };
  };

  window.apphost = {
    version: '1',
    me: function () {
      if (!mePromise) mePromise = request('GET', '/me').catch(function (e) { mePromise = null; throw e; });
      return mePromise;
    },
    collection: function (name) { return new Collection(name); },
    subscribe: subscribe
  };
})();
