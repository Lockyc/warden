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

// The All Agents list: each row names its project and its record there.
const all = {
  tabId: "warden:all-agents",
  allAgents: true,
  groups: [],
  projects: [
    { project: "p2", title: "Two", colour: "#222", amber: false },
    { project: "p1", title: "One", colour: "#111", amber: true },
  ],
  rows: [
    row("p2::agent:x", null, "mine", { project: "p2", record: "x" }),
    row("p1::agent:a", null, null, { project: "p1", record: "a" }),
    row("p1::agent:a1", "p1::agent:a", null, { project: "p1", record: "a1" }),
  ],
};
const empty = { ...all, projects: [], rows: [] };
const project = {
  tabId: "p1",
  allAgents: false,
  groups: [],
  projects: [],
  rows: [row("a", null, null, { project: null, record: null })],
};

test("the All Agents list groups its rows under their projects, in the projects' order", () => {
  const { agentGroupsForAll } = vm.runInContext("({ agentGroupsForAll })", ctx);
  assert.deepStrictEqual(plain(agentGroupsForAll(all)).map((g) => [g.project, g.title, g.colour, g.amber, g.rows.map((r) => r.id)]), [
    ["p2", "Two", "#222", false, ["p2::agent:x"]],
    ["p1", "One", "#111", true, ["p1::agent:a", "p1::agent:a1"]],
  ]);
});

test("the All Agents list, empty or not, neither drags, regroups nor opens new agents, and says why", () => {
  const { agentAllowsDrag, agentDragReason, agentGroupReason, agentNewReason } =
    vm.runInContext("({ agentAllowsDrag, agentDragReason, agentGroupReason, agentNewReason })", ctx);
  for (const dto of [all, empty]) {
    assert.strictEqual(agentAllowsDrag(dto), false);
    assert.strictEqual(agentDragReason(dto), "Reorder agents in their project");
    assert.strictEqual(agentGroupReason(dto), "Open the agent's project for this");
    assert.strictEqual(agentNewReason(dto), "New agents open in a project");
  }
  assert.strictEqual(agentAllowsDrag(project), true);
  assert.strictEqual(agentDragReason(project), null);
  assert.strictEqual(agentGroupReason(project), null);
  assert.strictEqual(agentNewReason(project), null);
});

test("a row's op runs on its own project's agent tab", () => {
  const { agentOpTarget } = vm.runInContext("({ agentOpTarget })", ctx);
  assert.deepStrictEqual(plain(agentOpTarget(all, all.rows[1])), { tabId: "p1", recordId: "a" });
  assert.deepStrictEqual(plain(agentOpTarget(project, project.rows[0])), { tabId: "p1", recordId: "a" });
});

test("the All Agents row's badge counts the agents needing you", () => {
  const { agentAllAttention } = vm.runInContext("({ agentAllAttention })", ctx);
  const needy = row("p1::agent:b", null, null, { state: "blocked", blockedKind: "permission" });
  const unseen = row("p1::agent:c", null, null, { unseen: "t" });
  assert.strictEqual(agentAllAttention([row("a"), needy, unseen]), 2);
  assert.strictEqual(agentAllAttention([row("a")]), null);
});

test("the All Agents selection: a click holds until its select answers", () => {
  const { agentAllSelInit: init, agentAllSelStep: step } = vm.runInContext("({ agentAllSelInit, agentAllSelStep })", ctx);
  const snap = (key) => ({ kind: "snapshot", key });
  let s = step(init, snap("a"));
  assert.strictEqual(s.shown, "a");
  s = step(s, { kind: "select", key: "b" });
  assert.strictEqual(s.shown, "b", "highlights at once");
  s = step(s, snap("a"));
  assert.strictEqual(s.shown, "b", "a snapshot sent before the click moves nothing");
  s = step(s, snap("b"));
  assert.deepStrictEqual(plain(s), { shown: "b", snap: "b", pending: null }, "its own snapshot ends the hold");
  s = step(s, snap("c"));
  assert.strictEqual(s.shown, "c", "a lean to a neighbour shows");

  // Refused: back to the snapshot's key.
  s = step(step(s, { kind: "select", key: "d" }), { kind: "settled", key: "d", ok: false });
  assert.deepStrictEqual(plain(s), { shown: "c", snap: "c", pending: null });
  // Answered before its snapshot: the hold ends, the highlight stays.
  s = step(step(s, { kind: "select", key: "e" }), { kind: "settled", key: "e", ok: true });
  assert.deepStrictEqual(plain(s), { shown: "e", snap: "c", pending: null });
  // An older click's answer after a newer click leaves the newer one pending.
  s = step(step(s, { kind: "select", key: "f" }), { kind: "settled", key: "e", ok: false });
  assert.strictEqual(s.pending, "f");
});
