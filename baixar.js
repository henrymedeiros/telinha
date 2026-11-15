// The download page (baixar.html): picks the package for this system from download/versions.json.
const $ = (id) => document.getElementById(id);
const OS = (() => {
  const p = (navigator.userAgentData?.platform || navigator.platform || "").toLowerCase();
  if (/android|iphone|ipad/i.test(navigator.userAgent)) return "phone";
  if (p.includes("win")) return "windows";
  if (p.includes("mac")) return "mac";
  if (p.includes("linux")) return "linux";
  return "other";
})();

// What each package is, in plain words. The first one of a system is its main download.
const PACKAGES = [
  { kind: "installer", os: "windows", label: "Baixar para Windows", short: "Windows" },
  { kind: "windows", os: "windows", label: "Versão portátil, sem instalar", short: "Windows portátil" },
  { kind: "appimage", os: "linux", label: "Baixar para Linux", short: "Linux (AppImage)" },
  { kind: "deb", os: "linux", label: "Pacote .deb para Ubuntu, Debian e Zorin", short: "Linux (.deb)" },
];

// On a phone the app is not an option, so what it adds would only get in the way.
$("why").hidden = OS === "phone";

const mb = (bytes) => `${Math.round(bytes / 1e6)} MB`;
const day = (iso) => new Date(`${iso}T12:00:00`).toLocaleDateString("pt-BR", { day: "numeric", month: "long", year: "numeric" });
const href = (f) => `download/${f.file}`;

function link(cls, text, file, size) {
  const a = document.createElement("a");
  a.className = cls; a.href = href(file); a.download = "";
  a.append(text);
  if (size) { const s = document.createElement("span"); s.className = "size"; s.textContent = ` ${mb(file.size)}`; a.append(s); }
  return a;
}

function render(versions) {
  const latest = versions[0];
  $("digits").replaceChildren(...latest.version.split(".").flatMap((n, i) => (i ? [document.createElement("i"), n] : [n])));
  document.title = `Baixar o Telinha ${latest.version}`;
  const get = $("get");
  const mine = PACKAGES.filter((p) => p.os === OS && latest.files[p.kind]);

  if (mine.length) {
    const main = link("get-app", mine[0].label, latest.files[mine[0].kind], true);
    main.insertAdjacentHTML("afterbegin", '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 4v11m0 0-4.5-4.5M12 15l4.5-4.5M5 20h14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg>');
    get.append(main);
    for (const p of mine.slice(1)) get.append(link("alt", p.label, latest.files[p.kind]));
  } else {
    const note = document.createElement("p");
    note.className = "note";
    note.textContent = OS === "phone"
      ? "No celular você não precisa do app. Abra o site e entre no canal."
      : OS === "mac"
        ? "Ainda não há versão para Mac. O site funciona no navegador."
        : "Escolha o seu sistema na lista abaixo.";
    get.append(note);
  }

  const more = $("more");
  const rows = versions.map((v, i) => ({
    name: `Versão ${v.version}`, sub: day(v.date),
    links: PACKAGES.filter((p) => v.files[p.kind] && (i === 0 ? p.kind !== mine[0]?.kind : p.os === OS || !mine.length)).map((p) => [p.short, v.files[p.kind]]),
  })).filter((r) => r.links.length);
  if (rows.length) more.append(group("Outros sistemas e versões", rows));
  more.hidden = !more.children.length;
}

function group(title, rows) {
  const d = document.createElement("details");
  const s = document.createElement("summary");
  s.textContent = title;
  const ul = document.createElement("ul");
  ul.className = "rows";
  for (const r of rows) {
    const li = document.createElement("li");
    li.className = "row";
    const name = document.createElement("div");
    name.className = "row-name";
    const b = document.createElement("b"); b.textContent = r.name;
    const small = document.createElement("small"); small.textContent = r.sub;
    name.append(b, small);
    const links = document.createElement("div");
    links.className = "row-links";
    for (const [text, file] of r.links) links.append(link("chip", text, file));
    li.append(name, links);
    ul.append(li);
  }
  d.append(s, ul);
  return d;
}

document.addEventListener("click", (e) => {
  for (const d of document.querySelectorAll(".more details[open]")) if (!d.contains(e.target)) d.open = false;
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") for (const d of document.querySelectorAll(".more details[open]")) d.open = false;
});

try {
  const r = await fetch("download/versions.json", { cache: "no-cache" });
  if (!r.ok) throw new Error(r.status);
  const { versions } = await r.json();
  if (!versions?.length) throw new Error("empty");
  render(versions);
} catch {
  $("digits").textContent = "----";
  const p = document.createElement("p");
  p.className = "empty-state";
  p.textContent = "Este servidor ainda não tem o app. O site funciona no navegador.";
  $("get").append(p);
}
