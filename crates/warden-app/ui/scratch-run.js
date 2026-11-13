// A scratch column's run maths: where its dividers and panes sit after a drag or a pane-size pick.
// Pure functions over a column view (`{ tiles, dividers }`, rects as column fractions), loaded by
// index.html as a classic script ahead of its controller, and by tests/scratch-run.test.js in a
// `vm` context. Classic scripts share the page's global scope with chrome-core.js, so every name
// here carries the `scratch` prefix.

// Two positions closer than this are the same.
const SCRATCH_EPS = 1e-6;
// [min, max] ratio a scratch column divider may take within its split — `InitDto.scratch_band`,
// set by index.html from every snapshot.
let scratchBand;

// The split ratio for divider `d` dragged to `pointer` (a column fraction along its axis):
// relative to the split's own rect, kept to `scratchBand`.
function scratchDragRatio(d, pointer) {
  const vert = d.dir === "vertical";
  const origin = vert ? d.rect.y : d.rect.x;
  const extent = vert ? d.rect.h : d.rect.w;
  const [lo, hi] = scratchBand;
  return extent > 0 ? Math.min(hi, Math.max(lo, (pointer - origin) / extent)) : 0.5;
}
// One axis of rect `r`, and of divider `x`'s boundary along it.
const scratchAxis = (r, vert) => (vert ? [r.y, r.y + r.h] : [r.x, r.x + r.w]);
const scratchBoundary = (x) => {
  const [s, e] = scratchAxis(x.rect, x.dir === "vertical");
  return s + x.ratio * (e - s);
};
// The run divider `d` belongs to: every divider along the same line (its direction, its
// cross-axis span) — a stack's nested splits. `idx` indexes `view.dividers`; `c0`/`c1` is the
// cross-axis span, `start`/`end` the run's extent along its axis.
function scratchRun(view, d) {
  const vert = d.dir === "vertical";
  const near = (a, b) => Math.abs(a - b) <= SCRATCH_EPS;
  const [c0, c1] = scratchAxis(d.rect, !vert);
  const idx = view.dividers.flatMap((x, i) => x.dir === d.dir &&
    near(scratchAxis(x.rect, !vert)[0], c0) && near(scratchAxis(x.rect, !vert)[1], c1) ? [i] : []);
  const run = idx.map((i) => view.dividers[i]);
  const start = Math.min(...run.map((x) => scratchAxis(x.rect, vert)[0]));
  const end = Math.max(...run.map((x) => scratchAxis(x.rect, vert)[1]));
  return { vert, c0, c1, idx, start, end, bounds: run.map(scratchBoundary) };
}
// Tile `ti`'s innermost run — the smallest run holding the tile — and the item of it the
// tile is in, `[itemStart, itemEnd]` along the run's axis; null for a lone tile.
function scratchPaneRun(view, ti) {
  const t = view.tiles[ti] && view.tiles[ti].rect;
  let best = null;
  for (const d of t ? view.dividers : []) {
    const r = scratchRun(view, d);
    const [x0, x1] = scratchAxis(t, !r.vert);
    const [a0, a1] = scratchAxis(t, r.vert);
    if (x0 < r.c0 - SCRATCH_EPS || x1 > r.c1 + SCRATCH_EPS) continue;
    if (a0 < r.start - SCRATCH_EPS || a1 > r.end + SCRATCH_EPS) continue;
    const area = (r.c1 - r.c0) * (r.end - r.start);
    if (best && area >= best.area - SCRATCH_EPS) continue;
    const cuts = [r.start, ...r.bounds, r.end];
    const itemStart = Math.max(...cuts.filter((b) => b <= a0 + SCRATCH_EPS));
    const itemEnd = Math.min(...cuts.filter((b) => b >= a1 - SCRATCH_EPS));
    best = { ...r, area, itemStart, itemEnd };
  }
  return best;
}
// Tile `ti`'s share of its innermost run, as `pane_menu` takes it; null for a lone tile.
function scratchPaneShare(view, ti) {
  const r = scratchPaneRun(view, ti);
  if (!r) return null;
  return { axis: r.vert ? "height" : "width", share: (r.itemEnd - r.itemStart) / (r.end - r.start) };
}
// `view` with tile `ti`'s item taking `share` of its innermost run: its trailing boundary
// moves to `itemStart + share·run`, or for the run's last item its leading one to
// `end − share·run`, through `scratchResized`. When the neighbour can't give that much
// above `scratchBand[0]` of the run, the boundaries beyond it move out first, farthest
// first, each leaving its pane the band minimum — the next pane along gives the rest, then
// the one after; only a run that can't cover it all clamps. Null for a lone tile.
function scratchPaneResized(view, ti, share) {
  const r = scratchPaneRun(view, ti);
  if (!r) return null;
  const extent = r.end - r.start;
  const keep = scratchBand[0] * extent;
  const last = r.itemEnd >= r.end - SCRATCH_EPS;
  const at = last ? r.itemStart : r.itemEnd;
  const target = last ? r.end - share * extent : r.itemStart + share * extent;
  const order = r.idx.map((i, n) => ({ id: view.dividers[i].split_id, b: r.bounds[n] }))
    .sort((a, b) => a.b - b.b);
  const k = order.findIndex((o) => Math.abs(o.b - at) <= SCRATCH_EPS);
  if (k < 0) return null;
  // Growing moves the boundary away from the item, toward `step`'s side.
  const step = last ? -1 : 1;
  const grows = step * (target - at) > SCRATCH_EPS;
  const edge = (j) => (j < 0 ? r.start : j >= order.length ? r.end : order[j].b);
  let v = view;
  if (grows && step * (edge(k + step) - step * keep - target) < -SCRATCH_EPS) {
    const far = step > 0 ? order.length - 1 : 0;
    for (let j = far; j !== k; j -= step) {
      const need = target + step * Math.abs(j - k) * keep;
      if (step * (order[j].b - need) < -SCRATCH_EPS) v = scratchBoundaryTo(v, order[j].id, need);
    }
  }
  return scratchBoundaryTo(v, order[k].id, target);
}
// `view` with split `splitId`'s boundary moved to `at` (a column fraction), through
// `scratchResized`.
function scratchBoundaryTo(view, splitId, at) {
  const di = view.dividers.findIndex((x) => x.split_id === splitId);
  if (di < 0) return view;
  const d = view.dividers[di];
  const [ds, de] = scratchAxis(d.rect, d.dir === "vertical");
  return de > ds ? scratchResized(view, di, (at - ds) / (de - ds)) : view;
}
// `view` with divider `di` moved to `ratio` of its split, the rest of its run (`scratchRun`)
// held. Only the two items beside the boundary resize: whatever lies between the
// run's neighbouring boundaries scales into its new side, everything else stays where it
// is, and every divider's ratio is re-derived over its new rect. Each moved item keeps at
// least `scratchBand[0]` of the run. A re-derived ratio outside `scratchBand` — a pane
// already under it, its split's rect grown — is moved to the band's edge the same way, as
// the backend clamps it, so the view shows what Rex will.
function scratchResized(view, di, ratio) {
  const [lo, hi] = scratchBand;
  const out = (x, i) => Math.abs(x.ratio - view.dividers[i].ratio) > SCRATCH_EPS &&
    (x.ratio < lo - SCRATCH_EPS || x.ratio > hi + SCRATCH_EPS);
  let v = scratchMoved(view, di, ratio);
  for (let n = 0; n < view.dividers.length; n++) {
    const i = v.dividers.findIndex(out);
    if (i < 0) break;
    v = scratchMoved(v, i, Math.min(hi, Math.max(lo, v.dividers[i].ratio)));
  }
  return v;
}
// `scratchResized`'s one move, its re-derived ratios as they fall.
function scratchMoved(view, di, ratio) {
  const d = view.dividers[di];
  const { vert, c0, c1, start: runStart, end: runEnd, bounds } = scratchRun(view, d);
  const b0 = scratchBoundary(d);
  const prev = Math.max(runStart, ...bounds.filter((b) => b < b0 - SCRATCH_EPS));
  const next = Math.min(runEnd, ...bounds.filter((b) => b > b0 + SCRATCH_EPS));
  const [ds, de] = scratchAxis(d.rect, vert);
  const keep = scratchBand[0] * (runEnd - runStart);
  let b1 = ds + ratio * (de - ds);
  b1 = prev + keep <= next - keep ? Math.min(next - keep, Math.max(prev + keep, b1)) : b0;
  // A position along the axis, moved: [prev, b0] onto [prev, b1], [b0, next] onto [b1, next].
  const f = (p) => {
    if (p <= prev || p >= next) return p;
    return p <= b0 ? prev + (p - prev) * (b1 - prev) / (b0 - prev)
                   : b1 + (p - b0) * (next - b1) / (next - b0);
  };
  // Only rects within the run's cross-axis span move — a column beside it doesn't.
  const map = (r) => {
    const [x0, x1] = scratchAxis(r, !vert);
    if (x0 < c0 - SCRATCH_EPS || x1 > c1 + SCRATCH_EPS) return r;
    const [s, e] = scratchAxis(r, vert).map(f);
    return vert ? { ...r, y: s, h: e - s } : { ...r, x: s, w: e - s };
  };
  return {
    ...view,
    tiles: view.tiles.map((t) => ({ ...t, rect: map(t.rect) })),
    dividers: view.dividers.map((x) => {
      const rect = map(x.rect);
      const xv = x.dir === "vertical";
      const at = xv === vert && rect !== x.rect ? f(scratchBoundary(x)) : scratchBoundary(x);
      const [s, e] = scratchAxis(rect, xv);
      return { ...x, rect, ratio: e > s ? (at - s) / (e - s) : x.ratio };
    }),
  };
}
// Every divider of `view` whose ratio moved from `base`'s, as `scratch_resize` takes them.
function scratchChanged(base, view) {
  return view.dividers
    .filter((x, i) => Math.abs(x.ratio - base.dividers[i].ratio) > SCRATCH_EPS)
    .map((x) => ({ splitId: x.split_id, ratio: x.ratio }));
}
// The column's pane shares top to bottom — a plain stack only, every tile full width.
function scratchColumnSizes(view) {
  if (!view.tiles.every((t) => t.rect.x <= SCRATCH_EPS && t.rect.w >= 1 - SCRATCH_EPS)) return null;
  return [...view.tiles].sort((a, b) => a.rect.y - b.rect.y).map((t) => t.rect.h);
}
