// The bottom strip's rendering (see strip.css). `WardenStrip.render(el, dto)` paints a
// `StripDto` ({ name, colour, rows: { done, now, next } | null }) into `el`, or hides `el` for a
// null dto. Pure DOM, so the page and an off-screen harness share it.
window.WardenStrip = (function () {
  const ROWS = [["done", "Done"], ["now", "Now"], ["next", "Next"]];
  function render(el, dto) {
    el.textContent = "";
    el.classList.toggle("on", !!dto);
    if (!dto) return;
    el.style.setProperty("--strip-colour", dto.colour);
    const name = document.createElement("div");
    name.className = "strip-name";
    name.textContent = dto.name;
    name.title = dto.name;
    el.appendChild(name);
    const rows = document.createElement("div");
    rows.className = "strip-rows";
    for (const [key, label] of ROWS) {
      const text = dto.rows && dto.rows[key];
      if (!text) continue;
      const row = document.createElement("div");
      row.className = "strip-row";
      const k = document.createElement("span");
      k.className = "strip-k";
      k.textContent = label;
      const v = document.createElement("span");
      v.className = "strip-v";
      v.textContent = text;
      v.title = text;
      row.append(k, v);
      rows.appendChild(row);
    }
    el.appendChild(rows);
  }
  return { render };
})();
