// The agent tab list's pure logic (ui/agent-list.js), loaded as the page loads it: a classic script
// in its own global scope. Run with `node --test` (zero dependencies).
const { test } = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const ctx = vm.createContext({});
vm.runInContext(fs.readFileSync(path.join(__dirname, "../ui/agent-list.js"), "utf8"), ctx, {
  filename: "agent-list.js",
});
const { agentTitle, agentTabAttention, agentAncestors, agentDropOnRow } = vm.runInContext(
  "({ agentTitle, agentTabAttention, agentAncestors, agentDropOnRow })",
  ctx,
);
const plain = (v) => (v === null ? null : JSON.parse(JSON.stringify(v)));

const row = (id, parent = null, group = null, extra = {}) => ({ id, parent, group, state: "idle", ...extra });

test("an agent's title: a name the user gave it, else the topic, else the window's name", () => {
  assert.strictEqual(agentTitle({ name: "mine", renamed: true, topic: "t" }), "mine");
  assert.strictEqual(agentTitle({ name: "win", renamed: false, topic: "topic" }), "topic");
  assert.strictEqual(agentTitle({ name: "win", renamed: false, topic: null }), "win");
});

test("a tab row's attention: a padlock beats the dot, a bell alone lights it", () => {
  const blocked = (kind) => row("b", null, null, { state: "blocked", blockedKind: kind });
  assert.strictEqual(agentTabAttention([row("a"), blocked("permission")], false), "lock");
  assert.strictEqual(agentTabAttention([blocked("question")], false), true);
  assert.strictEqual(agentTabAttention([row("a", null, null, { unseen: "x" })], false), true);
  assert.strictEqual(agentTabAttention([row("a")], true), true);
  assert.strictEqual(agentTabAttention([row("a")], false), null);
});

// a ─ a1 ─ a1x, a ─ a2; then b.
const fam = [row("a"), row("a1", "a"), row("a1x", "a1"), row("a2", "a"), row("b")];
const shown = fam.map((r) => r.id);

test("a drop after a tab with forks marks under its last fork", () => {
  assert.deepStrictEqual(plain(agentDropOnRow(fam, "b", "a", true, shown)), {
    group: null,
    index: 1,
    mark: "a2",
    cls: "drop-after",
  });
  assert.deepStrictEqual(plain(agentDropOnRow(fam, "b", "a", false, shown)), {
    group: null,
    index: 0,
    mark: "a",
    cls: "drop-before",
  });
});

test("a drop on a fork lands after its whole family", () => {
  assert.deepStrictEqual(plain(agentDropOnRow(fam, "b", "a1", false, shown)), {
    group: null,
    index: 1,
    mark: "a2",
    cls: "drop-after",
  });
});

test("a fork dropped among its source's other forks reorders in the family", () => {
  assert.deepStrictEqual(plain(agentDropOnRow(fam, "a2", "a1", false, shown)), {
    reorder: true,
    index: 0,
    mark: "a1",
    cls: "drop-before",
  });
  assert.deepStrictEqual(plain(agentDropOnRow(fam, "a2", "a1", true, shown)), {
    reorder: true,
    index: 1,
    mark: "a1x",
    cls: "drop-after",
  });
});

test("no drop on itself, its own forks, or a row the list no longer holds", () => {
  assert.strictEqual(agentDropOnRow(fam, "a", "a", true, shown), null);
  assert.strictEqual(agentDropOnRow(fam, "a", "a1x", true, shown), null);
  assert.strictEqual(agentDropOnRow(fam, "b", "gone", true, shown), null);
  assert.strictEqual(agentDropOnRow(fam, "b", "a1", true, ["b", "a1"]), null, "its root isn't drawn");
});

test("a lineage cycle from a tampered file bounds the parent walk", () => {
  const loop = [row("x", "y"), row("y", "x"), row("z")];
  assert.ok(agentAncestors(loop, "x").length <= loop.length);
  const t = agentDropOnRow(loop, "z", "x", true, ["x", "y", "z"]);
  assert.ok(t === null || typeof t.index === "number");
});

test("an index counts Asleep-group siblings though they are drawn apart", () => {
  const rows = [row("s", null, null, { state: "asleep" }), row("a"), row("b")];
  assert.deepStrictEqual(plain(agentDropOnRow(rows, "b", "a", false, ["a", "b"])), {
    group: null,
    index: 1,
    mark: "a",
    cls: "drop-before",
  });
});
