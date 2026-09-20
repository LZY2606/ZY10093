'use strict';
// Thin viewer: every rule and every decision executes server-side.

const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];

const state = {
  defs: [], samples: [], rules: [], plans: [], batches: [],
  curDef: null, curRule: null, lastDry: null, curPlan: null,
  parseData: null, parseBytes: [], parseRanges: [], parseColor: new Map(),
  selectedSample: null,
};

const KIND_COLORS = {
  magic:'#e3b341', int:'#58a6ff', bitfield:'#bc8cff', bytes:'#3fb950',
  pad:'#6e7681', fixed:'#d29922', struct:'#79c0ff', branch:'#f0883e',
  array:'#56d4dd', ext_container:'#db61a2', ext_block:'#db61a2',
  checksum:'#f85149', unknown:'#484f58',
};

async function api(method, path, body, idem) {
  const opt = { method, headers: {} };
  if (body !== undefined) { opt.headers['Content-Type'] = 'application/json'; opt.body = JSON.stringify(body); }
  if (idem) opt.headers['Idempotency-Key'] = idem;
  const res = await fetch(path, opt);
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = text; }
  if (!res.ok) { const err = new Error(typeof data === 'string' ? data : (data?.error || JSON.stringify(data))); err.status = res.status; err.body = data; throw err; }
  return data;
}

function toast(msg, ms = 4000) {
  const t = $('#toast'); t.textContent = msg; t.style.display = 'block';
  clearTimeout(toast._t); toast._t = setTimeout(() => (t.style.display = 'none'), ms);
}
function esc(s) { return String(s).replace(/[&<>]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;'}[c])); }
function hexToBytes(h) { const c = h.replace(/\s+/g, ''); const out = []; for (let i = 0; i < c.length; i += 2) out.push(parseInt(c.slice(i, i + 2), 16)); return out; }
function bytesToHex(b) { return b.map(x => x.toString(16).padStart(2, '0')).join(''); }

// ------------------------------------------------------------ tabs
$$('.tab').forEach(b => b.addEventListener('click', () => {
  $$('.tab').forEach(x => x.classList.remove('active'));
  $$('.tabpane').forEach(x => x.classList.remove('active'));
  b.classList.add('active');
  $('#tab-' + b.dataset.tab).classList.add('active');
  if (b.dataset.tab === 'samples') refreshSamplesUI();
  if (b.dataset.tab === 'rules') refreshRuleSelectors();
  if (b.dataset.tab === 'plans') refreshPlansUI();
}));

// ------------------------------------------------------------ definitions
const DEF_TEMPLATE = {
  name: 'newfmt', version: 'v1',
  fields: [
    { name: 'magic', kind: 'magic', value: 'aabb' },
    { name: 'length', kind: 'int', width: 2, endian: 'big' },
    { name: 'payload', kind: 'bytes', length: 'length' },
  ],
};
const RULE_TEMPLATE = {
  name: 'newrule', from_format: 'old', from_revision: 1,
  to_format: 'new', to_revision: 1,
  mappings: [{ op: 'copy', from: 'field', to: 'field' }],
  drop_extensions: [], description: '',
};

async function loadDefs() {
  state.defs = (await api('GET', '/api/defs')).definitions || [];
  $('#def-list').innerHTML = state.defs.map(d =>
    `<div class="item" data-name="${esc(d.name)}" data-rev="${d.revision}">
       <div><b>${esc(d.name)}</b> r${d.revision}</div>
       <div class="meta">${esc(d.fingerprint.slice(0, 16))}…</div></div>`).join('');
  $$('#def-list .item').forEach(el => el.addEventListener('click', async () => {
    $$('#def-list .item').forEach(x => x.classList.remove('selected'));
    el.classList.add('selected');
    const d = await api('GET', `/api/defs/${el.dataset.name}/${el.dataset.rev}`);
    state.curDef = d;
    $('#def-editor').value = JSON.stringify(d.spec, null, 2);
    $('#def-base').value = latestRev(d.name);
    renderValidation(d.validation_issues, $('#def-result'));
    if (d.resolved) $('#def-resolved').textContent = '继承展开后字段: ' +
      (d.resolved.fields || []).map(f => f.name).join(', ');
  }));
}
function latestRev(name) {
  return state.defs.filter(d => d.name === name).reduce((m, d) => Math.max(m, d.revision), 0);
}

function renderValidation(issues, el) {
  if (!issues || issues.length === 0) { el.innerHTML = '<span class="ok">✓ 无问题</span>'; return; }
  el.innerHTML = issues.map(i =>
    `<div class="${fatalCode(i.code) ? 'err' : 'warn'}">[${esc(i.code)}]${i.offset != null ? ' @' + i.offset : ''} ${esc(i.message)}</div>`
  ).join('');
}
function fatalCode(c) {
  return ['short_read','length_out_of_bounds','overlap_fields','magic_mismatch','checksum_self_reference',
    'no_branch_case','bad_selector','bad_count','bad_length','inherit_cycle','shadow_field',
    'duplicate_field','bad_kind','bad_width'].includes(c);
}

$('#def-new').onclick = () => { $('#def-editor').value = JSON.stringify(DEF_TEMPLATE, null, 2); state.curDef = null; };
$('#def-save').onclick = async () => {
  try {
    const spec = JSON.parse($('#def-editor').value);
    const r = await api('POST', '/api/defs', { spec, base_revision: Number($('#def-base').value) }, 'def-' + crypto.randomUUID());
    toast(`已保存 ${r.name} r${r.revision}\n指纹 ${r.fingerprint.slice(0, 24)}…`);
    await loadDefs();
  } catch (e) { renderErr(e, $('#def-result')); }
};
$('#def-validate').onclick = async () => {
  try {
    const spec = JSON.parse($('#def-editor').value);
    // validate latest existing revision: save first is required; instead validate
    // by round-trip through a throwaway save is not allowed, so use existing pick.
    if (!state.curDef) throw new Error('请先选择已保存的定义，或保存后再校验');
    const v = await api('GET', `/api/defs/${state.curDef.name}/${state.curDef.revision}/validate`);
    renderValidation(v.issues, $('#def-result'));
  } catch (e) { renderErr(e, $('#def-result')); }
};
function renderErr(e, el) {
  el.innerHTML = `<span class="err">${esc(e.status ? e.status + ' ' : '')}${esc(e.message)}</span>`;
}

// ------------------------------------------------------------ samples + parse
async function refreshFormatSelect() {
  const opts = state.defs.map(d => `<option value="${esc(d.name)}@${d.revision}">${esc(d.name)} r${d.revision}</option>`).join('');
  $('#smp-format').innerHTML = opts;
  $('#parse-diff-ver').innerHTML = opts;
}
async function refreshSamplesUI() {
  await refreshFormatSelect();
  state.samples = (await api('GET', '/api/samples')).samples || [];
  $('#smp-list').innerHTML = state.samples.map(s =>
    `<div class="item" data-id="${esc(s.id)}">
       <div><b>${esc(s.name)}</b></div>
       <div class="meta">${esc(s.format)} r${s.revision} · ${esc(s.sha256.slice(0, 12))}…</div></div>`).join('');
  $$('#smp-list .item').forEach(el => el.onclick = () => {
    $$('#smp-list .item').forEach(x => x.classList.remove('selected'));
    el.classList.add('selected');
    state.selectedSample = el.dataset.id;
    selectParse(el.dataset.id, el.querySelector('.meta').textContent.split(' ')[0]);
  });
  $('#parse-pick').innerHTML = state.samples.map(s =>
    `<option value="${esc(s.id)}">${esc(s.name)} (${esc(s.format)} r${s.revision})</option>`).join('');
}
$('#smp-upload').onclick = async () => {
  try {
    const [fmt, rev] = $('#smp-format').value.split('@');
    const body = { name: $('#smp-name').value || 'sample', format_name: fmt, format_revision: Number(rev), hex: $('#smp-hex').value };
    const r = await api('POST', '/api/samples', body, 'smp-' + crypto.randomUUID());
    toast(`已导入 ${r.name}（${r.length} 字节）\nsha256 ${r.sha256.slice(0, 20)}…\n原始字节不可变`);
    $('#smp-hex').value = '';
    await refreshSamplesUI();
  } catch (e) { toast('导入失败: ' + e.message); }
};

function selectParse(id) { $('#parse-pick').value = id; }

$('#parse-go').onclick = async () => {
  try {
    const id = $('#parse-pick').value;
    const sample = state.samples.find(s => s.id === id);
    if (!sample) return;
    const url = `/api/parse?name=${encodeURIComponent(sample.format)}&revision=${sample.revision}&sample=${encodeURIComponent(id)}`;
    const data = await api('GET', url);
    state.parseData = data;
    state.parseSampleFmt = sample;
    // fetch raw bytes for highlighting
    const raw = await fetch(`/api/samples/${encodeURIComponent(id)}/bytes`);
    state.parseBytes = [...new Uint8Array(await raw.arrayBuffer())];
    renderParse(data);
    if ($('#parse-diff').checked) runDiff(sample);
  } catch (e) { $('#parse-issues').innerHTML = `<span class="err">${esc(e.message)}</span>`; }
};

function collectRanges(n, out, depth) {
  out.push({ node: n, depth });
  (n.children || []).forEach(c => collectRanges(c, out, depth + 1));
}

function renderParse(data) {
  renderValidation(data.issues && data.issues.length ? data.issues : data.spec_issues, $('#parse-issues'));
  const ident = data.identity_identical
    ? '<span class="ok">✓ 未修改写回逐字节一致</span>'
    : '<span class="err">✗ 写回结果与输入不一致（存在解析问题时不保证）</span>';
  $('#hex-status').innerHTML = ident;
  const ranges = [];
  if (data.root) collectRanges(data.root, ranges, 0);
  state.parseRanges = ranges;
  renderHex();
  renderTree(data.root, ranges);
}

function leafKind(n) { return n.kind; }

function renderHex() {
  const bytes = state.parseBytes;
  // pick the innermost non-container leaf for each byte for color+label
  const leaves = state.parseRanges
    .filter(r => !['root','struct','branch','array','ext_container'].includes(r.node.kind))
    .map(r => r.node)
    .sort((a, b) => (b.end - b.start) - (a.end - a.start) || a.start - b.start);
  const owner = new Array(bytes.length).fill(null);
  for (const n of leaves) for (let i = n.start; i < n.end; i++) if (!owner[i]) owner[i] = n;

  const per = 16;
  let html = '';
  for (let row = 0; row < bytes.length; row += per) {
    html += `<span class="off">${row.toString(16).padStart(8, '0')}  </span>`;
    for (let i = row; i < row + per; i++) {
      if (i >= bytes.length) { html += '   '; continue; }
      const n = owner[i];
      const color = n ? (KIND_COLORS[n.kind] || '#8b98a8') : '#8b98a8';
      const title = n ? `${n.name || n.kind} [${n.start}..${n.end})` : '';
      html += `<b data-i="${i}" style="color:${color};background:${color}22" title="${esc(title)}">${bytes[i].toString(16).padStart(2, '0')}</b> `;
    }
    html += ' ';
    for (let i = row; i < Math.min(row + per, bytes.length); i++) {
      const ch = bytes[i] >= 32 && bytes[i] < 127 ? String.fromCharCode(bytes[i]) : '·';
      html += esc(ch);
    }
    html += '\n';
  }
  $('#hexview').innerHTML = html;
  $$('#hexview b').forEach(b => b.onclick = () => {
    const i = Number(b.dataset.i);
    const n = owner[i];
    if (n) selectNodeByName(n.name || `${n.kind}@${n.start}`, n);
  });
}

function fmtVal(n) {
  if (n.value === undefined || n.value === null) return '';
  if (typeof n.value === 'number') return `<span class="val">= ${n.value}</span>`;
  if (typeof n.value === 'string') return `<span class="val" title="${esc(n.value)}">${esc(n.value.slice(0, 16))}${n.value.length > 16 ? '…' : ''}</span>`;
  return '';
}

function renderTree(root, ranges) {
  if (!root) { $('#tree').textContent = '无解析树'; return; }
  const byName = new Map();
  function walk(n, d, parent) {
    let html = `<div class="tnode" style="padding-left:${d * 14 + 4}px" data-key="${n.name || n.kind}@${n.start}">
      <span class="kind">${esc(n.kind)}${n.matched ? ':' + esc(n.matched) : ''}${n.tag != null ? ' #' + n.tag : ''}</span>
      ${esc(n.name || '<未识别>')}
      <span class="rng">[${n.start}..${n.end})</span> ${fmtVal(n)}
    </div>`;
    let kids = (n.children || []).map(c => walk(c, d + 1, n)).join('');
    return html + kids;
  }
  $('#tree').innerHTML = walk(root, 0, null);
  $$('#tree .tnode').forEach(el => el.onclick = () => {
    $$('#tree .tnode').forEach(x => x.classList.remove('sel'));
    el.classList.add('sel');
    const n = ranges.map(r => r.node).find(x => (x.name || x.kind) + '@' + x.start === el.dataset.key);
    if (n) showFieldDetail(n);
  });
}

function selectNodeByName(key, n) {
  const el = $(`#tree .tnode[data-key="${CSS.escape(key)}"]`);
  if (el) { $$('#tree .tnode').forEach(x => x.classList.remove('sel')); el.classList.add('sel'); }
  showFieldDetail(n);
}

function showFieldDetail(n) {
  const isExt = n.kind === 'ext_block';
  const readRange = `读取范围 [${n.start}..${n.end})（${n.end - n.start} 字节）`;
  const writeRange = isExt
    ? `写出范围 [${n.start}..${n.end})（迁移时整块保留，不重排未知扩展）`
    : `写出范围 [${n.start}..${n.end})（身份写回复用原始字节）`;
  $('#field-detail').innerHTML =
    `<div><b>${esc(n.name || '<未识别>')}</b> <span class="kind">${esc(n.kind)}</span></div>
     <div>${readRange}</div><div>${writeRange}</div>`;
}

async function runDiff(sample) {
  try {
    const [name, rev] = $('#parse-diff-ver').value.split('@');
    if (name === sample.format && Number(rev) === sample.revision) { $('#diff-view').textContent = '请选择不同版本'; return; }
    // parse same bytes against the other version (may fail structurally -> that is the diff story)
    const url = `/api/parse?name=${encodeURIComponent(name)}&revision=${rev}&hex=${encodeURIComponent(bytesToHex(state.parseBytes))}`;
    let other;
    try { other = await api('GET', url); } catch (e) { other = null; }
    const a = leafMap(state.parseData.root);
    const b = other && other.root ? leafMap(other.root) : new Map();
    const keys = new Set([...a.keys(), ...b.keys()]);
    const rows = [];
    for (const k of [...keys].sort()) {
      const x = a.get(k), y = b.get(k);
      const status = !x ? '<span class="ok">新增</span>' : !y ? '<span class="err">旧版独有</span>'
        : JSON.stringify(x.value) === JSON.stringify(y.value) ? '<span class="ok">相同</span>'
        : '<span class="warn">变化</span>';
      rows.push(`<div>${status} ${esc(k)}: ${esc(sval(x?.value))} → ${esc(sval(y?.value))}</div>`);
    }
    $('#diff-view').innerHTML = `<div>双版本差异（当前 ${sample.format} r${sample.revision} vs ${name} r${rev}）：</div>` + rows.join('');
  } catch (e) { $('#diff-view').textContent = '差异解析失败: ' + e.message; }
}
function sval(v) { return v === undefined ? '∅' : typeof v === 'string' ? v.slice(0, 24) : String(v); }
function leafMap(n, m = new Map(), pre = '') {
  const p = pre ? pre + '.' + (n.name || '?') : (n.name || 'root');
  const kids = n.children || [];
  if (kids.length === 0 && n.kind !== 'root') m.set(p, n);
  kids.forEach(c => leafMap(c, m, n.kind === 'root' ? '' : p));
  return m;
}

// ------------------------------------------------------------ rules + dry-run
async function refreshRuleSelectors() {
  state.rules = (await api('GET', '/api/rules')).rules || [];
  $('#rule-list').innerHTML = state.rules.map(r =>
    `<div class="item" data-name="${esc(r.name)}" data-rev="${r.revision}">
      <div><b>${esc(r.name)}</b> r${r.revision}</div>
      <div class="meta">${esc(r.fingerprint.slice(0, 16))}…</div></div>`).join('');
  const opts = state.rules.map(r => `<option value="${esc(r.name)}@${r.revision}">${esc(r.name)} r${r.revision}</option>`).join('');
  $('#dry-rule').innerHTML = opts; $('#plan-rule').innerHTML = opts;
  $$('#rule-list .item').forEach(el => el.onclick = async () => {
    $$('#rule-list .item').forEach(x => x.classList.remove('selected'));
    el.classList.add('selected');
    const r = await api('GET', `/api/rules/${el.dataset.name}/${el.dataset.rev}`);
    state.curRule = r;
    $('#rule-editor').value = JSON.stringify(r.rule, null, 2);
    $('#rule-base').value = el.dataset.rev;
  });
}
$('#rule-new').onclick = () => { $('#rule-editor').value = JSON.stringify(RULE_TEMPLATE, null, 2); state.curRule = null; };
$('#rule-save').onclick = async () => {
  try {
    const rule = JSON.parse($('#rule-editor').value);
    const r = await api('POST', '/api/rules', { rule, base_revision: Number($('#rule-base').value) }, 'rule-' + crypto.randomUUID());
    toast(`已保存规则 ${r.name} r${r.revision}`);
    await refreshRuleSelectors();
  } catch (e) { $('#rule-result').innerHTML = `<span class="err">${esc(e.message)}</span>`; }
};
$('#rule-validate').onclick = async () => {
  try {
    if (!state.curRule) throw new Error('请先选择已保存规则');
    const v = await api('GET', `/api/rules/${state.curRule.name}/${state.curRule.revision}/validate`);
    $('#rule-result').innerHTML = v.ok ? '<span class="ok">✓ 规则有效</span>'
      : v.issues.map(i => `<div class="err">[${esc(i.code)}]${i.path ? ' ' + esc(i.path) : ''} ${esc(i.message)}</div>`).join('');
  } catch (e) { $('#rule-result').innerHTML = `<span class="err">${esc(e.message)}</span>`; }
};

$('#dry-go').onclick = async () => {
  const [name, rev] = $('#dry-rule').value.split('@');
  try {
    const r = await api('POST', '/api/dryrun', { rule_name: name, rule_revision: Number(rev) });
    state.lastDry = { name, rev, report: r };
    renderDry(r);
  } catch (e) { $('#dry-report').innerHTML = `<span class="err">${esc(e.message)}</span>`; }
};

function tierBadge(t) { return `<span class="badge ${t}">${t}</span>`; }
function renderDry(r) {
  const ruleIssues = (r.rule_issues || []).map(i =>
    `<div class="err">[${esc(i.code)}] ${esc(i.message)}</div>`).join('');
  const losses = (r.losses || []).map(l =>
    `<div class="${l.kind === 'dropped' || l.kind.includes('drop') ? 'warn' : 'err'}">[${esc(l.kind)}] ${esc(l.path)} — ${esc(l.message)}</div>`).join('');
  const samples = (r.samples || []).map(s => `
    <div class="item">
      <div>${esc(s.sample_id)} ${s.ok ? '<span class="ok">成功</span>' : '<span class="err">失败</span>'} ${tierBadge(s.reverse_tier)} · 输出 ${s.output_len} 字节</div>
      ${s.error ? `<div class="err small">${esc(s.error)}</div>` : ''}
      <div class="small mono">${esc(s.output_hex_preview)}…</div>
      <div class="small">${(s.reverse_items || []).map(i =>
        `<span class="badge ${i.tier}">${esc(i.target)}: ${esc(i.tier)}</span>`).join(' ')}</div>
    </div>`).join('');
  $('#dry-report').innerHTML = `
    <div><b>反向验证总体等级：</b>${tierBadge(r.reverse_tier)}</div>
    ${ruleIssues ? '<h3>规则问题</h3>' + ruleIssues : ''}
    ${losses ? '<h3>损失 / 默认值</h3>' + losses : ''}
    <h3>样本结果与字段来源</h3>${samples || '<div class="small">（没有匹配的样本）</div>'}`;
}

// ------------------------------------------------------------ plans + batches
async function refreshPlansUI() {
  await refreshRuleSelectors();
  state.plans = (await api('GET', '/api/plans')).plans || [];
  state.batches = (await api('GET', '/api/batches')).batches || [];
  $('#plan-list').innerHTML = state.plans.map(p =>
    `<div class="item" data-id="${esc(p.id)}">
      <div><b>${esc(p.name)}</b> <span class="badge ${esc(p.status)}">${esc(p.status)}</span> r${p.revision}</div>
      <div class="meta">规则修订 ${p.rule_revision}</div></div>`).join('');
  $$('#plan-list .item').forEach(el => el.onclick = () => showPlan(el.dataset.id));
  $('#batch-list').innerHTML = state.batches.map(b =>
    `<div class="item"><div><b>${esc(b.id)}</b></div>
      <div class="meta">${esc(b.plan_id)} · ${b.count} 文件 · ${esc(b.status)}</div></div>`).join('');
}

$('#plan-create').onclick = async () => {
  if (!state.lastDry) return toast('请先在“迁移规则”页运行一次 dry-run');
  const [, ] = [null];
  const { name, rev, report } = state.lastDry;
  try {
    const fps = {
      rule_name: name, rule_revision: Number(rev),
      rule_fingerprint: state.rules.find(r => r.name === name && r.revision === Number(rev))?.fingerprint,
      from: report.rule_name, reverse_tier: report.reverse_tier,
      frozen_at: 'freeze-time',
    };
    const p = await api('POST', '/api/plans', {
      name: $('#plan-name').value || 'plan', rule_name: name, rule_revision: Number(rev),
      dryrun: report, fingerprints: fps,
    }, 'plan-' + crypto.randomUUID());
    toast(`计划 ${p.id} 已创建（draft），定义指纹将在冻结时固化`);
    await refreshPlansUI(); showPlan(p.id);
  } catch (e) { toast('创建失败: ' + e.message); }
};

async function showPlan(id) {
  const p = await api('GET', `/api/plans/${id}`);
  state.curPlan = p;
  const losses = collectLossy(p.dryrun);
  const accs = new Set((p.acceptances || []).map(a => a.path));
  const lossList = losses.map(path =>
    `<label class="small"><input type="checkbox" data-path="${esc(path)}" ${accs.has(path) ? 'checked' : ''} ${p.status !== 'draft' ? 'disabled' : ''}> ${esc(path)} — 绑定规则 ${p.rule_revision}</label>`
  ).join('<br>');
  const actions = {
    draft: `<button id="act-freeze" class="primary">冻结（固化定义指纹）</button>`,
    frozen: `<button id="act-publish" class="primary">发布</button> <button id="act-unfreeze">退回草稿</button>`,
    published: `<span class="ok">已发布，可运行批次</span> <button id="act-retire">退役</button>`,
    retired: `<span>已退役</span>`,
  }[p.status] || '';
  $('#plan-detail').innerHTML = `
    <h2>${esc(p.name)} <span class="badge ${esc(p.status)}">${esc(p.status)}</span> rev ${p.revision}</h2>
    <div class="small mono">冻结指纹: <pre>${esc(JSON.stringify(p.fingerprints, null, 2))}</pre></div>
    <h3>有损项接受（接受即绑定字段 × 规则版本）</h3>
    ${lossList || '<span class="ok">无 lossy 项（strict/semantic 无需接受）</span>'}
    <div class="row" id="plan-actions">${actions}</div>
    <div class="small">状态机：draft → frozen → published → retired；frozen 可退回 draft。落后修订号返回 409 差异。</div>`;
  const acceptance = () => losses.filter(path => $(`[data-path="${CSS.escape(path)}"]`)?.checked)
    .map(path => ({ path, rule_revision: p.rule_revision, accepted_at: 'freeze-time' }));
  const trans = async (action, withAcc) => {
    try {
      const body = { revision: p.revision, action };
      if (withAcc) body.acceptances = acceptance();
      await api('POST', `/api/plans/${id}/transition`, body);
      await refreshPlansUI(); showPlan(id);
    } catch (e) {
      if (e.status === 409) toast('修订冲突 409：\n' + (e.body?.diffs || []).join('\n') + '\n请刷新后重试');
      else toast('状态跳转失败: ' + e.message);
    }
  };
  $('#act-freeze')?.addEventListener('click', () => trans('freeze', true));
  $('#act-publish')?.addEventListener('click', () => trans('publish', false));
  $('#act-unfreeze')?.addEventListener('click', () => trans('unfreeze', false));
  $('#act-retire')?.addEventListener('click', () => trans('retire', false));
}

function collectLossy(dry) {
  const s = new Set();
  for (const smp of dry?.samples || [])
    for (const it of smp.reverse_items || [])
      if (it.tier === 'lossy') s.add(it.target);
  for (const l of dry?.losses || []) if (l.kind === 'dropped' || l.kind === 'dropped_extension') s.add(l.path);
  return [...s].sort();
}

$('#batch-run').onclick = async () => {
  if (!state.curPlan) return toast('请先选择一个计划');
  try {
    const r = await api('POST', '/api/batches', { plan_id: state.curPlan.id }, 'batch-' + crypto.randomUUID());
    toast(`批次 ${r.batch_id} 完成：${r.count} 个文件（全部成功才可见）`);
    await refreshPlansUI();
  } catch (e) {
    const f = e.body?.failures || [];
    toast(`批次未生成（${f.length} 个失败，已成功转换 ${e.body?.succeeded ?? 0}/${e.body?.attempted ?? '?'}）：\n` +
      f.slice(0, 5).map(x => `${x.sample_id}: ${x.error}`).join('\n'));
  }
};

// ------------------------------------------------------------ boot
(async function init() {
  await loadDefs();
  await refreshSamplesUI();
  await refreshRuleSelectors();
  await refreshPlansUI();
})().catch(e => toast('初始化失败: ' + e.message));
