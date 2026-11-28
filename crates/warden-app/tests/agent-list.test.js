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
  assert.strictEqual(agentDropOnRow(fam, "a1", "a1x", true, shown), null, "a fork onto its own fork");
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

test("a list goes stale once a row's cache passes after the list was built", () => {
  const { agentColdDue } = vm.runInContext("({ agentColdDue })", ctx);
  const rows = [row("a", null, null, { cache: 100 }), row("b"), row("c", null, null, { cache: 200 })];
  assert.strictEqual(agentColdDue(rows, 50, 99), false, "nothing cold yet");
  assert.strictEqual(agentColdDue(rows, 50, 100), true, "a went cold");
  assert.strictEqual(agentColdDue(rows, 100, 150), false, "a was already cold when the list was built");
  assert.strictEqual(agentColdDue(rows, 100, 200), true, "c went cold");
});

// The All Agents list: rows keyed `<project>::agent:<record>`, each naming its project.
const all = {
  tabId: "warden:all-agents",
  groups: [],
  projects: [
    { project: "p2", title: "Two", colour: "#222", amber: false },
    { project: "p1", title: "One", colour: "#111", amber: true },
  ],
  rows: [
    row("p2::agent:x", null, "mine", { project: "p2" }),
    row("p1::agent:a", null, null, { project: "p1" }),
    row("p1::agent:a1", "p1::agent:a", null, { project: "p1" }),
  ],
};
const project = { tabId: "p1", groups: [], projects: [], rows: [row("a", null, null, { project: null })] };

test("the All Agents list groups its rows under their projects, in the projects' order", () => {
  const { agentGroupsForAll } = vm.runInContext("({ agentGroupsForAll })", ctx);
  assert.deepStrictEqual(plain(agentGroupsForAll(all)).map((g) => [g.project, g.title, g.colour, g.amber, g.rows.map((r) => r.id)]), [
    ["p2", "Two", "#222", false, ["p2::agent:x"]],
    ["p1", "One", "#111", true, ["p1::agent:a", "p1::agent:a1"]],
  ]);
});

test("the All Agents list neither drags nor opens new agents, and says why", () => {
  const { agentAllowsDrag, agentDragReason, agentNewReason } =
    vm.runInContext("({ agentAllowsDrag, agentDragReason, agentNewReason })", ctx);
  assert.strictEqual(agentAllowsDrag(all), false);
  assert.strictEqual(agentAllowsDrag(project), true);
  assert.strictEqual(agentDragReason(all), "Reorder agents in their project");
  assert.strictEqual(agentDragReason(project), null);
  assert.strictEqual(agentNewReason(all), "New agents open in a project");
  assert.strictEqual(agentNewReason(project), null);
});

test("a row's op runs on its own project's agent tab", () => {
  const { agentOpTarget } = vm.runInContext("({ agentOpTarget })", ctx);
  assert.deepStrictEqual(plain(agentOpTarget(all, all.rows[1])), { tabId: "p1", recordId: "a" });
  assert.deepStrictEqual(plain(agentOpTarget(project, project.rows[0])), { tabId: "p1", recordId: "a" });
  // The last separator splits it, as `split_agent_key` does.
  const odd = row("/x::agent:y/p::agent:r", null, null, { project: "/x::agent:y/p" });
  assert.deepStrictEqual(plain(agentOpTarget(all, odd)), { tabId: "/x::agent:y/p", recordId: "r" });
});

test("the All Agents row's badge counts the agents needing you", () => {
  const { agentAllAttention } = vm.runInContext("({ agentAllAttention })", ctx);
  const needy = row("p1::agent:b", null, null, { state: "blocked", blockedKind: "permission" });
  const unseen = row("p1::agent:c", null, null, { unseen: "t" });
  assert.strictEqual(agentAllAttention([row("a"), needy, unseen]), 2);
  assert.strictEqual(agentAllAttention([row("a")]), null);
});
