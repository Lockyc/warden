// The window's event routing: which events are this window's, and which kind of error its error
// bar holds. `emit_to` leaks to sibling webviews, so every per-window event carries a `label`.
// Loaded by index.html as a classic script ahead of its controller, and by tests/route.test.js in
// a `vm` context. Classic scripts share the page's global scope with chrome-core.js, so every name
// here carries the `route` prefix.

// Whether event `e` is for the window labelled `label`: unlabelled, or labelled with it. Before
// init_tabs answers, `label` is null and no event is this window's.
function routeForMe(label, e) {
  return label !== null && !(e.payload && e.payload.label && e.payload.label !== label);
}

// What `warden:error` payload `p` means for the window labelled `label`: `null` when it isn't this
// window's (or the window has no label yet), a per-window error (`{label, message}`, own window
// only), or a global config error (a bare string, or a label-less object, shown everywhere).
function routeError(label, p) {
  if (label === null) return null;
  const obj = !!p && typeof p === "object";
  if (obj && p.label) return p.label === label ? { scope: "window", message: "⚠ " + p.message } : null;
  return { scope: "global", message: "⚠ " + (obj ? p.message : p) };
}

// The error bar over `show(msg)` (`null` hides it). It remembers whether it holds a global error,
// the one kind a clean config reload clears: a per-window error replaces it and stays until
// dismissed or replaced.
function routeErrorBar(show) {
  let global = false;
  return {
    window(msg) {
      global = false;
      show(msg);
    },
    global(msg) {
      global = true;
      show(msg);
    },
    clearGlobal() {
      if (!global) return;
      global = false;
      show(null);
    },
    holdsGlobal: () => global,
  };
}
