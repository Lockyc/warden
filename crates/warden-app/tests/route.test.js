// The window's event routing (ui/route.js), loaded as the page loads it: a classic script in its
// own global scope. Run with `node --test` (zero dependencies).
const { test } = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const ctx = vm.createContext({});
vm.runInContext(fs.readFileSync(path.join(__dirname, "../ui/route.js"), "utf8"), ctx, {
  filename: "route.js",
});
const { routeForMe, routeError, routeErrorBar } = vm.runInContext(
  "({ routeForMe, routeError, routeErrorBar })",
  ctx,
);
// Values built in the vm's realm carry its own prototypes; JSON round-trips them into this one.
const plain = (v) => JSON.parse(JSON.stringify(v));

const events = [
  { payload: { label: "main" } },
  { payload: { label: "other" } },
  { payload: { id: "t" } },
  { payload: null },
  {},
];

test("forMe rejects every labelled event until the label is set", () => {
  for (const e of events) assert.strictEqual(routeForMe(null, e), false, JSON.stringify(e));
});

test("forMe takes its own label and unlabelled events, never a sibling's", () => {
  const got = events.map((e) => routeForMe("main", e));
  assert.deepStrictEqual(got, [true, false, true, true, true]);
});

test("an error event is this window's only once it has a label, and only if unlabelled or its own", () => {
  assert.strictEqual(routeError(null, { label: "main", message: "x" }), null);
  assert.strictEqual(routeError(null, "bad config"), null);
  assert.strictEqual(routeError("main", { label: "other", message: "x" }), null);
  assert.deepStrictEqual(plain(routeError("main", { label: "main", message: "spawn" })), {
    scope: "window",
    message: "⚠ spawn",
  });
  assert.deepStrictEqual(plain(routeError("main", "bad config")), { scope: "global", message: "⚠ bad config" });
  assert.deepStrictEqual(plain(routeError("main", { message: "no window" })), {
    scope: "global",
    message: "⚠ no window",
  });
});

function bar() {
  const shown = [];
  const b = routeErrorBar((msg) => shown.push(msg));
  return { b, shown, now: () => shown[shown.length - 1] ?? null };
}

test("a label-less error-clear leaves a per-window error", () => {
  const { b, now } = bar();
  b.window("⚠ spawn failed");
  b.clearGlobal();
  assert.strictEqual(now(), "⚠ spawn failed");
});

test("a per-window error replaces a global one, which a clear then leaves alone", () => {
  const { b, now } = bar();
  b.global("⚠ bad config");
  b.window("⚠ split failed");
  b.clearGlobal();
  assert.strictEqual(now(), "⚠ split failed");
});

test("a clean reload clears a global error once", () => {
  const { b, shown, now } = bar();
  b.global("⚠ bad config");
  assert.ok(b.holdsGlobal());
  b.clearGlobal();
  b.clearGlobal();
  assert.strictEqual(now(), null);
  assert.deepStrictEqual(shown.slice(), ["⚠ bad config", null]);
  assert.ok(!b.holdsGlobal());
});
