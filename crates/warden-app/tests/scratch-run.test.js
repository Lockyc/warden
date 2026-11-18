// The scratch column's run maths (ui/scratch-run.js), loaded as the page loads it: a classic
// script in its own global scope. Run with `node --test` (zero dependencies).
const { test } = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const ctx = vm.createContext({});
vm.runInContext(fs.readFileSync(path.join(__dirname, "../ui/scratch-run.js"), "utf8"), ctx, {
  filename: "scratch-run.js",
});
// The page sets it from `InitDto.scratch_band`, which is `rex::layout::RATIO_BAND`.
vm.runInContext("scratchBand = [0.05, 0.95];", ctx);
const {
  scratchResized,
  scratchPaneResized,
  scratchPaneShare,
  scratchChanged,
  scratchColumnSizes,
} = vm.runInContext(
  "({ scratchResized, scratchPaneResized, scratchPaneShare, scratchChanged, scratchColumnSizes })",
  ctx,
);

const r4 = (x) => Math.round(x * 10000) / 10000;
// Values built in the vm's realm carry its own Array/Object prototypes, which deepStrictEqual
// tells apart from this realm's; `Array.from` builds them here.
const heights = (v) => Array.from(scratchColumnSizes(v), r4);
const changed = (base, v) => Array.from(scratchChanged(base, v), (s) => [s.splitId, r4(s.ratio)]);
const V = (id, y, h, ratio) => ({ split_id: id, dir: "vertical", ratio, rect: { x: 0, y, w: 1, h } });
const T = (id, y, h, x = 0, w = 1) => ({ block_id: id, rect: { x, y, w, h } });

// Four panes of .25, nested as Rex nests a stack built top down: each split holds the first
// pane and the rest.
const stack = {
  tiles: [T("b1", 0, 0.25), T("b2", 0.25, 0.25), T("b3", 0.5, 0.25), T("b4", 0.75, 0.25)],
  dividers: [V("s1", 0, 1, 0.25), V("s2", 0.25, 0.75, 1 / 3), V("s3", 0.5, 0.5, 0.5)],
};

test("dragging the top divider moves only the two panes beside it", () => {
  const v = scratchResized(stack, 0, 0.4);
  assert.deepStrictEqual(heights(v), [0.4, 0.1, 0.25, 0.25]);
  assert.deepStrictEqual(changed(stack, v), [["s1", 0.4], ["s2", 0.1667]]);
});

test("a stack Rex nests the other way sends only the one split it needs", () => {
  const nested = {
    tiles: stack.tiles,
    dividers: [V("t1", 0, 1, 0.75), V("t2", 0, 0.75, 2 / 3), V("t3", 0, 0.5, 0.5)],
  };
  const v = scratchResized(nested, 2, 0.8);
  assert.deepStrictEqual(heights(v), [0.4, 0.1, 0.25, 0.25]);
  assert.deepStrictEqual(changed(nested, v), [["t3", 0.8]]);
});

test("a side-by-side pair inside the stack keeps its widths and offers Pane Width", () => {
  const sbs = {
    tiles: [
      T("b1", 0, 0.25),
      T("b2a", 0.25, 0.25, 0, 0.5),
      T("b2b", 0.25, 0.25, 0.5, 0.5),
      T("b3", 0.5, 0.25),
      T("b4", 0.75, 0.25),
    ],
    dividers: [
      V("s1", 0, 1, 0.25),
      V("s2", 0.25, 0.75, 1 / 3),
      { split_id: "h", dir: "horizontal", ratio: 0.5, rect: { x: 0, y: 0.25, w: 1, h: 0.25 } },
      V("s3", 0.5, 0.5, 0.5),
    ],
  };
  const v = scratchResized(sbs, 0, 0.4);
  const pair = Array.from(v.tiles.filter((t) => t.block_id.startsWith("b2")), (t) => [
    r4(t.rect.x),
    r4(t.rect.w),
  ]);
  assert.deepStrictEqual(pair, [[0, 0.5], [0.5, 0.5]]);
  assert.ok(!changed(sbs, v).some(([id]) => id === "h"), "the side-by-side split is not sent");
  const share = scratchPaneShare(sbs, 1);
  assert.strictEqual(share.axis, "width");
  assert.strictEqual(r4(share.share), 0.5);
});

test("the bottom pane's preset moves the divider above it", () => {
  const v = scratchPaneResized(stack, 3, 0.2);
  assert.deepStrictEqual(heights(v), [0.25, 0.25, 0.3, 0.2]);
  assert.deepStrictEqual(changed(stack, v), [["s3", 0.6]]);
});

test("a pane preset in a four-stack takes from the panes below it, then clamps", () => {
  assert.deepStrictEqual(heights(scratchPaneResized(stack, 1, 0.5)), [0.25, 0.5, 0.05, 0.2]);
  assert.deepStrictEqual(heights(scratchPaneResized(stack, 1, 0.45)), [0.25, 0.45, 0.05, 0.25]);
  assert.deepStrictEqual(heights(scratchPaneResized(stack, 0, 0.9)), [0.85, 0.05, 0.05, 0.05]);
});

test("two-pane presets", () => {
  const two = { tiles: [T("a", 0, 0.5), T("b", 0.5, 0.5)], dividers: [V("d", 0, 1, 0.5)] };
  assert.deepStrictEqual(heights(scratchPaneResized(two, 0, 0.1)), [0.1, 0.9]);
  assert.deepStrictEqual(heights(scratchPaneResized(two, 1, 0.1)), [0.9, 0.1]);
  const share = scratchPaneShare(two, 0);
  assert.strictEqual(share.axis, "height");
  assert.strictEqual(share.share, 0.5);
});

test("a re-derived ratio outside the band moves to its edge, as the backend clamps it", () => {
  const three = {
    tiles: [T("a", 0, 0.5), T("b", 0.5, 0.475), T("c", 0.975, 0.025)],
    dividers: [V("s1", 0, 1, 0.5), V("s2", 0.5, 0.5, 0.95)],
  };
  const v = scratchResized(three, 0, 0.2);
  const s2 = v.dividers.find((d) => d.split_id === "s2");
  assert.strictEqual(r4(s2.ratio), 0.9375);
});

// A seeded PRNG (mulberry32), so a failing case replays from its seed.
function prng(seed) {
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

// A stack of `n` panes nested top down as Rex builds one: each split holds one pane and the rest,
// at a ratio inside the band, often at its edge, where a drag re-derives a neighbour out of it.
function randomStack(rand, n) {
  const ratio = () => {
    const p = rand();
    if (p < 0.3) return 0.05 + rand() * 0.02;
    if (p < 0.6) return 0.95 - rand() * 0.02;
    return 0.05 + rand() * 0.9;
  };
  const tiles = [];
  const dividers = [];
  let y = 0;
  for (let i = 0; i < n - 1; i++) {
    const rest = 1 - y;
    const d = V("s" + i, y, rest, ratio());
    dividers.push(d);
    tiles.push(T("b" + i, y, rest * d.ratio));
    y += rest * d.ratio;
  }
  tiles.push(T("b" + (n - 1), y, 1 - y));
  return { tiles, dividers };
}

test("every ratio a drag or a pane preset sends is finite and inside the band", () => {
  for (let seed = 1; seed <= 500; seed++) {
    const rand = prng(seed);
    const view = randomStack(rand, 2 + Math.floor(rand() * 5));
    const di = Math.floor(rand() * view.dividers.length);
    const ti = Math.floor(rand() * view.tiles.length);
    const results = [
      ["drag", scratchResized(view, di, 0.05 + rand() * 0.9)],
      ["preset", scratchPaneResized(view, ti, rand())],
    ];
    for (const [how, v] of results) {
      for (const d of v.dividers) {
        assert.ok(Number.isFinite(d.ratio), `seed ${seed}: ${how} gave ${d.split_id} ratio ${d.ratio}`);
        assert.ok(
          d.ratio >= 0.05 - 1e-6 && d.ratio <= 0.95 + 1e-6,
          `seed ${seed}: ${how} left ${d.split_id} at ${d.ratio}, outside the band`,
        );
      }
      const total = v.tiles.reduce((a, t) => a + t.rect.h, 0);
      assert.ok(Math.abs(total - 1) < 1e-6, `seed ${seed}: ${how} panes sum to ${total}`);
    }
  }
});
