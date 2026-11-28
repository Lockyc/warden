// The agent tab list's pure logic: a row's title, a tab row's attention, and where a dragged row
// drops. Rows are `AgentRowDto`s (`warden:agents`). Loaded by index.html as a classic script
// ahead of its controller, and by tests/agent-list.test.js in a `vm` context. Classic scripts share
// the page's global scope with chrome-core.js, so every name here carries the `agent` prefix.

// Whether row `r` needs you: blocked, or with a finish you haven't seen.
const agentNeedsYou = (r) => r.state === "blocked" || !!r.unseen;

// An agent's title, in its row and the summary alike: a name the user gave it, else the
// conversation's topic, else its window's name (the terminal title the program set).
function agentTitle(r) {
  return r.renamed ? r.name : r.topic || r.name;
}

// A tab row's attention, as chrome-core takes it, from its agent list's `rows` and whether a bell
// is unread there (`badged`): a padlock while an agent waits on a permission, else the dot while
// one needs you or the bell is unread, else none.
function agentTabAttention(rows, badged) {
  const needy = rows.filter(agentNeedsYou);
  if (needy.some((r) => r.state === "blocked" && r.blockedKind === "permission")) return "lock";
  return needy.length || badged ? true : null;
}

// Row `id`'s parents in `rows`, nearest first; bounded by the row count, as a tampered lineage
// can loop.
function agentAncestors(rows, id) {
  const parentOf = (x) => rows.find((r) => r.id === x)?.parent ?? null;
  const up = [];
  for (let p = parentOf(id); p && up.length < rows.length; p = parentOf(p)) up.push(p);
  return up;
}

// Where row `dragId` dropped on row `rowId` lands — on its lower half when `after`. `shown` is the
// ids of the rows drawn outside the Asleep and Cold Cache groups, in list order. A drop's index
// counts its siblings in the store (`rows`), those groups' too, though they are drawn apart. `null` for
// no drop: on itself or its own fork, or a row the list no longer holds. Otherwise
// `{ reorder: true, index }` (among its own source's other forks) or `{ group, index }` (a
// top-level tab, before or after a whole family; on a fork, after that fork's family), with
// `mark`, the id of the row the drop mark goes on, and `cls`, the mark's side.
function agentDropOnRow(rows, dragId, rowId, after, shown) {
  const target = rows.find((r) => r.id === rowId);
  if (!target) return null;
  const ancestors = (id) => agentAncestors(rows, id);
  if (rowId === dragId || ancestors(rowId).includes(dragId)) return null;
  const drawn = shown.filter((id) => id !== dragId);
  const sibs = (pred) => rows.filter((x) => x.id !== dragId && pred(x)).map((x) => x.id);
  const rootOf = (id) => ancestors(id).pop() ?? id;
  // A drop after a row lands after its forks too, so the mark goes under the last of them.
  const lastOf = (id) => drawn.filter((x) => x === id || ancestors(x).includes(id)).pop();
  const mark = (id) => (after ? { mark: lastOf(id), cls: "drop-after" } : { mark: id, cls: "drop-before" });
  const parent = rows.find((r) => r.id === dragId)?.parent ?? null;
  if (parent && target.parent === parent) {
    const peers = sibs((x) => x.parent === parent);
    return { reorder: true, index: peers.indexOf(rowId) + (after ? 1 : 0), ...mark(rowId) };
  }
  const root = drawn.find((x) => x === rootOf(rowId));
  if (!root) return null; // the list changed under the drag
  const group = target.group || null;
  const peers = sibs((x) => !x.parent && (x.group || null) === group);
  if (rowId === root) return { group, index: peers.indexOf(rowId) + (after ? 1 : 0), ...mark(rowId) };
  return { group, index: peers.indexOf(root) + 1, mark: lastOf(root), cls: "drop-after" };
}

// Whether a row of `rows` had its prompt cache go cold (`cache`, epoch seconds) after `since`, when
// the list was built, and by `now`: the list's Cold Cache group is then stale and is pulled again.
const agentColdDue = (rows, since, now) => rows.some((r) => r.cache && r.cache > since && r.cache <= now);
