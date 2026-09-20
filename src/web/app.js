"use strict";
const $ = (id) => document.getElementById(id);
let FORMATS = [], SAMPLES = [], RULES = [], PLANS = [];
let LAST_PARSE = null, LAST_INPUT_HEX = "";

async function api(method, path, body) {
  const opts = { method, headers: {} };
  if (body !== undefined) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  const res = await fetch(path, opts);
  let data = null;
  try { data = await res.json(); } catch (e) { data = {}; }
  return { status: res.status, data };
}

function esc(s) {
  return String(s ?? "").replace(/[&<>"]/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
}

function fmtValue(v) {
  if (v === null || v === undefined) return "";
  if (typeof v === "object") return JSON.stringify(v);
  return String(v);
}

async function loadMeta() {
  const [f, s, r, p] = await Promise.all([
    api("GET", "/api/formats"), api("GET", "/api/samples"),
    api("GET", "/api/rules"), api("GET", "/api/plans"),
  ]);
  FORMATS = f.data.formats || [];
  SAMPLES = s.data.samples || [];
  RULES = (r.data.rules || []).map((x) => x.doc);
  PLANS = p.data.plans || [];
  populateFormats();
  populateSamples();
  populateRules();
  renderPlans();
}

function formatLabel(x) { return `${x.id}@v${x.version}`; }
function formatOpts() {
  return FORMATS.map((x) => `<option value="${x.id}:${x.version}">${esc(formatLabel(x))} — ${esc(x.description||"")}</option>`).join("");
}
function populateFormats() {
  $("in-format").innerHTML = formatOpts();
  $("diff-a").innerHTML = formatOpts();
  $("diff-b").innerHTML = formatOpts();
  const v2 = FORMATS.find((x) => x.id === "img" && x.version === 2);
  if (v2) $("diff-b").value = "img:2";
}

function populateSamples() {
  const opts = '<option value="">— 直接粘贴 hex —</option>' +
    SAMPLES.map((s) => `<option value="${esc(s.id)}">${esc(s.id)} (${esc(s.name)})</option>`).join("");
  $("in-sample").innerHTML = opts;
  const ds = SAMPLES.map((s) => `<option value="${esc(s.id)}">${esc(s.id)}</option>`).join("");
  $("diff-sample").innerHTML = ds;
  $("m-sample").innerHTML = ds;
}

function populateRules() {
  $("m-rule").innerHTML = RULES.map((r) =>
    `<option value="${esc(r.id)}:${r.version}">${esc(r.id)} v${r.version} — ${esc(formatLabel(r.from))} → ${esc(formatLabel(r.to))}</option>`
  ).join("");
}

function parseRef(str) {
  const [id, version] = str.split(":");
  return { id, version: Number(version) };
}

function flatten(nodes, depth, rows) {
  for (const n of nodes) {
    rows.push({ n, depth });
    flatten(n.children || [], depth + 1, rows);
  }
}

function kindBadge(kind, identified) {
  if (!identified) return `<span class="badge b-unknown">unknown</span>`;
  return esc(kind);
}

function renderTree(nodes, tbody, onSelect) {
  const rows = [];
  flatten(nodes, 0, rows);
  tbody.innerHTML = rows.map(({ n, depth }) => {
    const indent = "　".repeat(depth);
    const val = n.kind === "bytes" ? (n.value?.len + "B") : fmtValue(n.kind === "branch" ? ("arm" + n.arm) : n.value);
    return `<tr class="node" data-start="${n.start}" data-end="${n.end}" data-path="${esc(n.path)}">
      <td>${indent}${esc(n.path)} ${!n.identified ? '<span class="badge b-unknown">未识别</span>' : ""}</td>
      <td>${kindBadge(n.kind, n.identified)}</td>
      <td class="muted">[${n.start}, ${n.end})</td>
      <td>${esc(val)}</td></tr>`;
  }).join("");
  tbody.querySelectorAll("tr.node").forEach((tr) => {
    tr.onclick = () => onSelect(tr);
  });
}

function byteColorMap(nodes, len, errOffsets) {
  const map = new Array(len).fill(null);
  (function paint(list, identified) {
    for (const n of list) {
      const isUnknown = n.kind === "unidentified" || n.identified === false;
      if (n.children && n.children.length && n.kind !== "bits") {
        paint(n.children, identified && n.identified);
      } else {
        for (let i = n.start; i < n.end && i < len; i++) {
          if (map[i] === null || isUnknown) map[i] = isUnknown ? "unknown" : "field";
        }
      }
      if (n.kind === "bits" && n.children.length === 0) {
        for (let i = n.start; i < n.end && i < len; i++) map[i] = "field";
      }
    }
  })(nodes, true);
  for (const o of errOffsets) { if (o < len) map[o] = "error"; }
  return map;
}

function renderHex(hex, nodes, errOffsets) {
  const bytes = [];
  const clean = hex.replace(/\s+/g, "");
  for (let i = 0; i < clean.length; i += 2) bytes.push(parseInt(clean.substr(i, 2), 16));
  const colors = byteColorMap(nodes, bytes.length, errOffsets || []);
  const rows = [];
  const perRow = 16;
  for (let off = 0; off < bytes.length; off += perRow) {
    let cells = "";
    for (let i = 0; i < perRow; i++) {
      const pos = off + i;
      if (pos >= bytes.length) { cells += "   "; continue; }
      const c = colors[pos];
      const bg = c === "unknown" ? "background:#3a3f46;color:#c9d1d9"
        : c === "error" ? "background:#5c2420;color:#ffb3ac"
        : "background:#16324f;color:#9fd0ff";
      cells += `<span class="hb" data-offset="${pos}" style="${bg}">${bytes[pos].toString(16).padStart(2, "0")}</span> `;
    }
    rows.push(`<span class="muted">${off.toString(16).padStart(6, "0")}</span>  ${cells}`);
  }
  $("hexview").innerHTML = rows.join("\n");
  $("hexview").querySelectorAll(".hb").forEach((el) => {
    el.onmouseenter = () => {
      const off = Number(el.dataset.offset);
      const hits = [];
      (function walk(ns) {
        for (const n of ns) {
          if (off >= n.start && off < n.end) hits.push(n);
          walk(n.children || []);
        }
      })(nodes);
      if (hits.length) showTooltip(el, `偏移 ${off}<br>` + hits.map((n) => esc(n.path) + ` <span class="muted">[${n.start},${n.end})</span>`).join("<br>"));
    };
    el.onmouseleave = hideTooltip;
  });
}

function showTooltip(el, html) {
  const t = $("tooltip");
  t.innerHTML = html;
  t.style.display = "block";
  const r = el.getBoundingClientRect();
  t.style.left = Math.min(r.left, window.innerWidth - 360) + "px";
  t.style.top = (r.bottom + 8) + "px";
}
function hideTooltip() { $("tooltip").style.display = "none"; }

function renderIssues(d) {
  const parts = [];
  for (const e of d.errors || []) {
    parts.push(`<div class="err-line">✗ [${esc(e.code)}] 偏移 ${e.offset} 路径 ${esc(e.path)} — ${esc(e.message)}</div>`);
  }
  for (const w of d.warnings || []) {
    parts.push(`<div class="warn-line">⚠ [${esc(w.code)}] 偏移 ${w.offset} 路径 ${esc(w.path)} — ${esc(w.message)}</div>`);
  }
  $("issues").innerHTML = parts.join("") || '<span class="muted">无错误、无告警</span>';
}

async function doParse() {
  const sampleId = $("in-sample").value;
  const payload = sampleId
    ? { sample_id: sampleId }
    : { format: parseRef($("in-format").value), hex: $("in-hex").value };
  const { status, data } = await api("POST", "/api/parse", payload);
  LAST_PARSE = data;
  LAST_INPUT_HEX = sampleId
    ? (SAMPLES.find((s) => s.id === sampleId)?.hex || "")
    : $("in-hex").value;
  $("parse-verdict").innerHTML = data.ok
    ? '<span class="badge b-ok">解析成功</span>'
    : '<span class="badge b-err">解析错误 422</span>';
  const tbody = $("tree-table").querySelector("tbody");
  renderTree(data.tree || [], tbody, selectNode);
  const errOffsets = (data.errors || []).map((e) => e.offset);
  renderHex(LAST_INPUT_HEX, data.tree || [], errOffsets);
  renderIssues(data);
  $("selection").textContent = "点击结构树节点查看读取与写出范围";
}

let SELECTED_PATH = null;
function selectNode(tr) {
  document.querySelectorAll("#tree-table tr.node").forEach((x) => x.classList.remove("selected"));
  tr.classList.add("selected");
  SELECTED_PATH = tr.dataset.path;
  const start = Number(tr.dataset.start), end = Number(tr.dataset.end);
  const readRange = `读取范围 [${start}, ${end})`;
  let writeInfo = "写出范围：执行“原样写回”后显示";
  if (LAST_PARSE && LAST_PARSE._writeRanges) {
    const w = LAST_PARSE._writeRanges.find((r) => r.path === SELECTED_PATH);
    if (w) writeInfo = `写出范围 [${w.start}, ${w.end})${w.auto ? "（自动回填字段）" : ""}`;
  }
  $("selection").innerHTML = `<b>${esc(SELECTED_PATH)}</b><br>${readRange}<br>${writeInfo}`;
}

async function doEmit() {
  const sampleId = $("in-sample").value;
  const payload = {
    edits: {},
    ...(sampleId ? { sample_id: sampleId } : { format: parseRef($("in-format").value), hex: $("in-hex").value }),
  };
  const { status, data } = await api("POST", "/api/emit", payload);
  if (!data.ok) { $("emit-result").innerHTML = `<span class="err-line">${esc(JSON.stringify(data.errors))}</span>`; return; }
  if (LAST_PARSE) LAST_PARSE._writeRanges = data.write_ranges;
  $("emit-result").innerHTML = data.byte_identical
    ? '<span class="badge b-ok">逐字节一致</span>'
    : '<span class="badge b-warn">字节发生变化</span>';
  $("emit-result").innerHTML += ` <span class="muted">自动回填：${esc((data.auto_fields||[]).join(", ") || "无")}</span>`;
  if (SELECTED_PATH) {
    const tr = document.querySelector(`#tree-table tr[data-path="${CSS.escape(SELECTED_PATH)}"]`);
    if (tr) selectNode(tr);
  }
}

function miniTree(containerId, title, d) {
  $(containerId).previous = null;
  const rows = [];
  flatten(d.tree || [], 0, rows);
  const body = rows.map(({ n, depth }) =>
    `<tr><td>${"　".repeat(depth)}${esc(n.path)}</td><td class="muted">[${n.start},${n.end})</td><td>${esc(fmtValue(n.value))}</td></tr>`
  ).join("");
  return `<table><thead><tr><th>路径</th><th>范围</th><th>值</th></tr></thead><tbody>${body}</tbody></table>`;
}

async function doDiff() {
  const sid = $("diff-sample").value;
  const sample = SAMPLES.find((s) => s.id === sid);
  if (!sample) return;
  const a = parseRef($("diff-a").value), b = parseRef($("diff-b").value);
  const q = `sample_id=${encodeURIComponent(sid)}&from_id=${a.id}&from_version=${a.version}&to_id=${b.id}&to_version=${b.version}`;
  const res = await fetch("/api/diff?" + q);
  const d = await res.json();
  $("diff-title-a").textContent = `A ${formatLabel(a)} fp ${(d.from?.fingerprint||"").slice(0,10)}`;
  $("diff-title-b").textContent = `B ${formatLabel(b)} fp ${(d.to?.fingerprint||"").slice(0,10)}`;
  $("diff-tree-a").innerHTML = miniTree("x", "", d.from);
  $("diff-tree-b").innerHTML = miniTree("x", "", d.to);
  const badge = { equal: ["b-ok","相同"], changed: ["b-warn","改变"], removed: ["b-err","仅A有"], added: ["b-strict","仅B有"] };
  $("diff-summary").innerHTML = `<table><thead><tr><th>路径</th><th>差异</th><th>A 值</th><th>B 值</th></tr></thead><tbody>` +
    (d.field_diffs || []).map((x) => {
      const [cls, label] = badge[x.kind];
      return `<tr><td>${esc(x.path)}</td><td><span class="badge ${cls}">${label}</span></td>
        <td class="muted">${esc(fmtValue(x.from))}</td><td class="muted">${esc(fmtValue(x.to))}</td></tr>`;
    }).join("") + "</tbody></table>";
}

async function doDry() {
  const sid = $("m-sample").value;
  const rule = parseRef($("m-rule").value);
  const { status, data } = await api("POST", "/api/migrate/dry-run", { sample_id: sid, rule });
  if (!data.ok) {
    $("dry-verdict").innerHTML = `<span class="badge b-err">dry-run 失败</span><div class="err-line">${esc(JSON.stringify(data.errors))}</div>`;
    return;
  }
  window.LAST_DRY = data;
  const cls = { strict: "b-strict", semantic: "b-semantic", lossy: "b-lossy" }[data.equivalence];
  $("dry-verdict").innerHTML = `反向验证：<span class="badge ${cls}">${esc(data.equivalence)}</span>
    <span class="muted"> 严格字节相等：${data.strict_bytes_equal}</span>`;
  $("provenance").querySelector("tbody").innerHTML = (data.provenance || []).map((p) =>
    `<tr><td>${esc(p.target)}</td><td>${esc(p.source)}</td><td class="muted">${esc(p.detail)}</td><td class="muted">${esc(p.note)}</td></tr>`
  ).join("");
  $("dry-losses").innerHTML = (data.losses || []).length
    ? data.losses.map((l) => `<span class="badge b-lossy">${esc(l)}</span> `).join("")
    : '<span class="muted">无损</span>';
  $("dry-layout").innerHTML = `<table><thead><tr><th>字段</th><th>写出范围</th><th>自动</th></tr></thead><tbody>` +
    (data.write_ranges || []).map((r) => `<tr><td>${esc(r.path)}</td><td class="muted">[${r.start},${r.end})</td><td>${r.auto ? "✓" : ""}</td></tr>`).join("") +
    `</tbody></table><div class="muted" style="margin-top:6px">输出 ${data.output_len} 字节：<code>${esc(data.output_hex)}</code></div>`;
}

function selectedRuleRef() { return parseRef($("m-rule").value); }
function selectedSampleIds() { return $("m-sample").value ? [$("m-sample").value] : []; }

async function doPublish() {
  const losses = (window.LAST_DRY?.losses || []).map((path) => ({
    path, rule_id: selectedRuleRef().id, rule_version: selectedRuleRef().version,
    note: "accepted in UI",
  }));
  const body = {
    plan: {
      id: $("plan-id").value, rev: 0,
      rule: selectedRuleRef(),
      state: "draft",
      accepted_losses: losses,
    },
    sample_ids: selectedSampleIds(),
  };
  const { status, data } = await api("POST", "/api/plans/publish", body);
  if (!data.ok) {
    $("plan-result").innerHTML = `<span class="badge b-err">${status} 发布被拒绝</span><pre class="json">${esc(JSON.stringify(data.failures||data,null,2))}</pre>`;
  } else {
    $("plan-result").innerHTML = `<span class="badge b-ok">已发布，指纹已冻结</span><pre class="json">${esc(JSON.stringify(data.plan.fingerprints,null,2))}</pre>`;
  }
  await loadMeta();
}

async function doBatch() {
  const body = { plan_id: $("plan-id").value, sample_ids: selectedSampleIds() };
  const { status, data } = await api("POST", "/api/plans/batch", body);
  if (!data.ok) {
    $("plan-result").innerHTML = `<span class="badge b-err">${status} 批次失败：未生成可见批次</span><pre class="json">${esc(JSON.stringify(data.failures||data,null,2))}</pre>`;
  } else {
    $("plan-result").innerHTML = `<span class="badge b-ok">批次 ${esc(data.batch.id)} / ${esc(data.batch.equivalence)}</span>
      <pre class="json">${esc(JSON.stringify(data.batch.outputs,null,2))}</pre>`;
  }
  await loadMeta();
}

function renderPlans() {
  $("plans").innerHTML = PLANS.length ? PLANS.map((p) => `
    <div style="border-bottom:1px solid var(--border); padding:6px 0">
      <b>${esc(p.id)}</b> rev ${p.rev}
      <span class="badge ${p.state === "published" ? "b-strict" : "b-unknown"}">${esc(p.state)}</span>
      <span class="muted">规则 ${esc(p.rule.id)} v${p.rule.version}</span>
      <div class="muted">指纹：${Object.entries(p.fingerprints || {}).map(([k,v]) => `${esc(k)}=${esc(String(v).slice(0,10))}`).join(" ")}</div>
      <div class="muted">接受损失：${(p.accepted_losses||[]).map((a) => esc(a.path)).join(", ") || "无"}</div>
      <div class="muted">批次：${(p.batches||[]).map((b) => `${b.id}(${b.equivalence},${b.sample_ids.length}文件)`).join(" ") || "无"}</div>
    </div>`).join("") : '<span class="muted">暂无计划</span>';
}

document.querySelectorAll(".tabs button").forEach((b) => {
  b.onclick = () => {
    document.querySelectorAll(".tabs button").forEach((x) => x.classList.remove("active"));
    b.classList.add("active");
    ["inspect", "diff", "migrate"].forEach((t) => {
      $(`tab-${t}`).classList.toggle("hidden", t !== b.dataset.tab);
    });
  };
});

$("in-sample").onchange = async () => {
  const s = SAMPLES.find((x) => x.id === $("in-sample").value);
  if (s) $("in-hex").value = s.hex;
};
$("btn-parse").onclick = doParse;
$("btn-emit").onclick = doEmit;
$("btn-diff").onclick = doDiff;
$("btn-dry").onclick = doDry;
$("btn-publish").onclick = doPublish;
$("btn-batch").onclick = doBatch;

loadMeta().then(() => {
  if (SAMPLES[0]) { $("in-sample").value = SAMPLES[0].id; $("in-hex").value = SAMPLES[0].hex; }
});
