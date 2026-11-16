"use strict";
(function () {
  var pending = new Map();
  var handlers = Object.create(null);
  var seq = 0;
  function rpc(method, args) {
    return new Promise(function (resolve, reject) {
      var id = ++seq;
      pending.set(id, { resolve: resolve, reject: reject });
      parent.postMessage({ v: 1, id: id, method: method, args: args || {} }, "*");
    });
  }
  window.addEventListener("message", function (ev) {
    if (ev.source !== parent) return;
    var m = ev.data;
    if (!m || m.v !== 1) return;
    if (m.type === "command") {
      var fn = handlers[m.id];
      if (typeof fn === 'function') {
        Promise.resolve().then(function () { return fn(m.context || {}); }).catch(function (e) {
          var msg = String((e && e.message) || e).slice(0, 200);
          rpc('ui.notify', { text: 'Command failed: ' + msg }).catch(function () {});
        });
      }
      return;
    }
    var p = pending.get(m.id);
    if (!p) return;
    pending.delete(m.id);
    if (m.ok) p.resolve(m.result); else p.reject(new Error(String(m.error)));
  });
  function args(o) { var out = {}; for (var k in o) { if (o[k] !== undefined) out[k] = o[k]; } return out; }
  var api = {
    version: 1,
    vault: {
      search: function (q, limit) { return rpc('vault.search', args({ q: q, limit: limit })).then(function (r) { return r.results; }); },
      read: function (path) { return rpc('vault.read', { path: path }); },
      backlinks: function (path) { return rpc('vault.backlinks', { path: path }).then(function (r) { return r.backlinks; }); },
      createPage: function (title, markdown) { return rpc('vault.createPage', { title: title, markdown: markdown }); },
      appendBlocks: function (path, markdown) { return rpc('vault.appendBlocks', { path: path, markdown: markdown }); }
    },
    ui: {
      registerCommand: function (id, title, handler) {
        if (typeof handler !== 'function') return Promise.reject(new Error('registerCommand needs a handler function'));
        handlers[id] = handler;
        return rpc('ui.registerCommand', { id: id, title: title });
      },
      notify: function (text) { return rpc('ui.notify', { text: String(text) }); }
    }
  };
  Object.freeze(api.vault); Object.freeze(api.ui); Object.freeze(api);
  Object.defineProperty(window, 'selenite', { value: api, writable: false, configurable: false });
})();
