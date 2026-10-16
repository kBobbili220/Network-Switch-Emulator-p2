// Entry point. Loads the wasm bindings, wires drag/drop, drives the
// canvas + inspector.
import init, { LogPlayer } from './pkg/viewer.js';

const ui = {
  drop: document.getElementById('dropzone'),
  canvas: document.getElementById('topology'),
  play: document.getElementById('play'),
  stepBack: document.getElementById('step-back'),
  stepFwd: document.getElementById('step-fwd'),
  scrub: document.getElementById('scrubber'),
  time: document.getElementById('time'),
  speed: document.getElementById('speed'),
  threshold: document.getElementById('threshold'),
  inspector: document.getElementById('inspector'),
  inspectorTitle: document.getElementById('inspector-title'),
  inspectorClose: document.getElementById('inspector-close'),
  inspectorAddresses: document.getElementById('inspector-addresses'),
  inspectorPorts: document.getElementById('inspector-ports'),
  inspectorProgram: document.getElementById('inspector-program'),
  inspectorTables: document.getElementById('inspector-tables'),
  inspectorArrays: document.getElementById('inspector-arrays'),
  inspectorPacket: document.getElementById('inspector-packet'),
  inspectorLink: document.getElementById('inspector-link'),
};

function speedFactor() {
  const v = parseFloat(ui.speed.value);
  return Number.isFinite(v) && v > 0 ? v : 1;
}

function fmtIp(v) {
  const n = Number(v) >>> 0;
  return `${(n >>> 24) & 0xff}.${(n >>> 16) & 0xff}.${(n >>> 8) & 0xff}.${n & 0xff}`;
}

const state = {
  player: null,
  topology: null,
  cursorNs: 0,
  durationNs: 1,
  playing: false,
  lastFrameTs: 0,
  // Layout: { switches: Map<id, {x,y}>, apps: Map<id, {x,y}> }
  layout: null,
  selectedSwitch: null,
  // Last drawn packets, populated each `draw()`. Each item is
  // { packetId: BigInt, kind, x, y }. Used for click-to-inspect.
  lastPackets: [],
  // Last drawn summary badges. Each item is
  // { link, x, y, w, h, packetCount, fromNs, toNs, packets: undefined|Array }.
  lastSummaries: [],
  // Currently inspected packet (BigInt id), or null.
  selectedPacket: null,
  // Currently inspected link summary (link id as Number), or null.
  selectedLink: null,
};

await init();

ui.drop.addEventListener('dragover', e => {
  e.preventDefault();
  ui.drop.classList.add('dragover');
});
ui.drop.addEventListener('dragleave', () => ui.drop.classList.remove('dragover'));
ui.drop.addEventListener('drop', async e => {
  e.preventDefault();
  ui.drop.classList.remove('dragover');
  const file = e.dataTransfer.files[0];
  if (!file) return;
  const buf = new Uint8Array(await file.arrayBuffer());
  loadLog(buf);
});
ui.drop.addEventListener('click', async () => {
  const input = document.createElement('input');
  input.type = 'file';
  input.accept = '.simlog,application/octet-stream';
  input.addEventListener('change', async () => {
    if (input.files[0]) {
      const buf = new Uint8Array(await input.files[0].arrayBuffer());
      loadLog(buf);
    }
  });
  input.click();
});

ui.play.addEventListener('click', () => {
  state.playing = !state.playing;
  ui.play.textContent = state.playing ? '⏸' : '▶';
  state.lastFrameTs = performance.now();
  if (state.playing) requestAnimationFrame(tick);
  // When pausing, leave the inspector functional.
});
ui.stepBack.addEventListener('click', () => {
  if (!state.player) return;
  const prev = Number(state.player.prev_event_ns(BigInt(state.cursorNs)));
  state.playing = false;
  ui.play.textContent = '▶';
  seek(prev);
  draw();
});
ui.stepFwd.addEventListener('click', () => {
  if (!state.player) return;
  const next = Number(state.player.next_event_ns(BigInt(state.cursorNs)));
  state.playing = false;
  ui.play.textContent = '▶';
  seek(next);
  draw();
});
ui.scrub.addEventListener('input', () => {
  if (!state.player) return;
  const ns = Math.round((Number(ui.scrub.value) / 1000) * state.durationNs);
  seek(ns);
  if (!state.playing) draw();
});
ui.canvas.addEventListener('click', e => {
  if (!state.player || !state.layout) return;
  const rect = ui.canvas.getBoundingClientRect();
  const x = (e.clientX - rect.left) * (ui.canvas.width / rect.width);
  const y = (e.clientY - rect.top) * (ui.canvas.height / rect.height);
  // Packets first — they sit on top of links.
  const pkt = pickPacket(x, y);
  if (pkt !== null) {
    state.selectedSwitch = null;
    state.selectedLink = null;
    state.selectedPacket = pkt.packetId;
    showPacketInspector(pkt);
    return;
  }
  // Then link summary badges.
  const sum = pickSummary(x, y);
  if (sum !== null) {
    state.selectedSwitch = null;
    state.selectedPacket = null;
    state.selectedLink = sum.link;
    showLinkSummaryInspector(sum);
    return;
  }
  const sw = pickSwitch(x, y);
  if (sw !== null) {
    state.selectedSwitch = sw;
    state.selectedPacket = null;
    state.selectedLink = null;
    showInspector(sw);
  }
});
ui.inspectorClose.addEventListener('click', () => {
  ui.inspector.hidden = true;
  state.selectedSwitch = null;
  state.selectedPacket = null;
  state.selectedLink = null;
});

function loadLog(bytes) {
  state.player = new LogPlayer(bytes);
  state.topology = state.player.topology();
  state.durationNs = Number(state.player.duration_ns) || 1;
  state.cursorNs = 0;
  state.player.seek(0n);
  state.layout = computeLayout(state.topology);
  resizeCanvas();
  updateScrubber();
  draw();
}

function seek(ns) {
  state.cursorNs = ns;
  state.player.seek(BigInt(ns));
  updateScrubber();
  if (state.selectedSwitch !== null) showInspector(state.selectedSwitch);
  else if (state.selectedPacket !== null) {
    showPacketInspector({ packetId: state.selectedPacket, kind: 0 });
  }
}

function updateScrubber() {
  ui.scrub.value = String(Math.round((state.cursorNs / state.durationNs) * 1000));
  ui.time.textContent = `${(state.cursorNs / 1e9).toFixed(3)}s`;
}

function tick(ts) {
  if (!state.playing || !state.player) return;
  const dt = ts - state.lastFrameTs;
  state.lastFrameTs = ts;
  const speed = speedFactor();
  state.cursorNs += Math.round(dt * 1_000_000 * speed); // ms -> ns * speed
  if (state.cursorNs > state.durationNs) {
    state.cursorNs = state.durationNs;
    state.playing = false;
    ui.play.textContent = '▶';
  }
  state.player.seek(BigInt(state.cursorNs));
  updateScrubber();
  draw();
  if (state.playing) requestAnimationFrame(tick);
}

// ----- layout -----

function computeLayout(topo) {
  const switches = new Map();
  const apps = new Map();
  const W = ui.canvas.width;
  const H = ui.canvas.height;
  // Switches: arrange on a circle.
  const sCount = topo.switches.length;
  const r = Math.min(W, H) * 0.32;
  topo.switches.forEach((s, i) => {
    const angle = (i / Math.max(sCount, 1)) * Math.PI * 2 - Math.PI / 2;
    switches.set(s.id, {
      x: W / 2 + Math.cos(angle) * r,
      y: H / 2 + Math.sin(angle) * r,
    });
  });
  // Apps: anchor near their first connected switch (or just along the
  // top edge if none found).
  topo.apps.forEach((a, i) => {
    let anchor = null;
    for (const link of topo.links) {
      if (link.a_kind === 0 && link.a_id === a.id && link.b_kind === 1) {
        anchor = switches.get(link.b_id);
        break;
      }
      if (link.b_kind === 0 && link.b_id === a.id && link.a_kind === 1) {
        anchor = switches.get(link.a_id);
        break;
      }
    }
    if (anchor) {
      const cx = W / 2, cy = H / 2;
      const dx = anchor.x - cx, dy = anchor.y - cy;
      const len = Math.sqrt(dx * dx + dy * dy) || 1;
      apps.set(a.id, { x: anchor.x + (dx / len) * 80, y: anchor.y + (dy / len) * 80 });
    } else {
      apps.set(a.id, { x: 60 + i * 100, y: 40 });
    }
  });
  return { switches, apps };
}

function nodePos(kind, id) {
  if (kind === 0) return state.layout.apps.get(id);
  return state.layout.switches.get(id);
}

function pickSwitch(x, y) {
  const R = 22;
  for (const [id, pos] of state.layout.switches) {
    const dx = x - pos.x, dy = y - pos.y;
    if (dx * dx + dy * dy < R * R) return id;
  }
  return null;
}

function pickSummary(x, y) {
  for (const s of state.lastSummaries) {
    if (x >= s.x && x <= s.x + s.w && y >= s.y && y <= s.y + s.h) {
      return s;
    }
  }
  return null;
}

function pickPacket(x, y) {
  const R = 12; // generous hit radius (drawn dot is r=5)
  let best = null;
  let bestDist = R * R;
  for (const p of state.lastPackets) {
    const dx = x - p.x, dy = y - p.y;
    const d2 = dx * dx + dy * dy;
    if (d2 < bestDist) { bestDist = d2; best = p; }
  }
  return best;
}

function resizeCanvas() {
  const rect = ui.canvas.getBoundingClientRect();
  ui.canvas.width = Math.max(rect.width, 800);
  ui.canvas.height = Math.max(rect.height, 500);
  if (state.layout && state.topology) {
    state.layout = computeLayout(state.topology);
  }
}
window.addEventListener('resize', () => { resizeCanvas(); draw(); });

// ----- draw -----

function draw() {
  const ctx = ui.canvas.getContext('2d');
  const W = ui.canvas.width, H = ui.canvas.height;
  ctx.fillStyle = '#0d0d0f';
  ctx.fillRect(0, 0, W, H);

  if (!state.player || !state.topology) return;

  const dtNs = Math.round(16_000_000 * speedFactor());
  const threshold = parseInt(ui.threshold.value, 10) || 20;
  // Each packet is visible for at least 250 ms of *wall-clock* time,
  // regardless of how short its real flight is. In simulated-time terms
  // that's 250ms × speed.
  const minVisibleNs = Math.max(1, Math.round(0.25 * 1_000_000_000 * speedFactor()));
  const frame = state.player.frame(
    BigInt(state.cursorNs),
    BigInt(dtNs),
    threshold,
    BigInt(minVisibleNs),
  );

  const failedLinks = new Set(frame.link_failed.map(Number));
  const failedSwitches = new Set(frame.switch_failed.map(Number));
  const cpuFailed = new Set(frame.cpu_failed.map(Number));
  const summaryByLink = new Map(frame.summaries.map(s => [Number(s.link), s]));
  const summaryHits = [];

  // Links.
  for (const link of state.topology.links) {
    const a = nodePos(link.a_kind, link.a_id);
    const b = nodePos(link.b_kind, link.b_id);
    if (!a || !b) continue;
    const failed = failedLinks.has(link.id);
    ctx.strokeStyle = failed ? '#553' : '#444';
    ctx.lineWidth = failed ? 1 : 2;
    ctx.setLineDash(failed ? [4, 4] : []);
    ctx.beginPath();
    ctx.moveTo(a.x, a.y);
    ctx.lineTo(b.x, b.y);
    ctx.stroke();
    ctx.setLineDash([]);

    const sum = summaryByLink.get(link.id);
    if (sum) {
      const mx = (a.x + b.x) / 2, my = (a.y + b.y) / 2;
      const pps = sum.packets_per_sec;
      const bps = sum.bytes_per_sec * 8;
      const heat = Math.min(1, Math.log10(pps + 1) / 4);
      ctx.fillStyle = `rgba(255, ${Math.round(220 - heat * 200)}, 80, 0.85)`;
      ctx.fillRect(mx - 60, my - 10, 120, 20);
      ctx.fillStyle = '#000';
      ctx.font = '10px monospace';
      ctx.textAlign = 'center';
      ctx.textBaseline = 'middle';
      ctx.fillText(`${pps.toFixed(0)} pkt/s, ${formatRate(bps)}`, mx, my);
      summaryHits.push({
        link: link.id,
        x: mx - 60, y: my - 10, w: 120, h: 20,
        fromNs: BigInt(state.cursorNs) - BigInt(dtNs),
        toNs: BigInt(state.cursorNs),
      });
    }
  }

  // Moving packets.
  state.lastPackets.length = 0;
  for (const p of frame.packets) {
    const link = state.topology.links.find(l => l.id === Number(p.link));
    if (!link) continue;
    const a = nodePos(link.a_kind, link.a_id);
    const b = nodePos(link.b_kind, link.b_id);
    if (!a || !b) continue;
    const t = p.forward ? p.progress : 1 - p.progress;
    const x = a.x + (b.x - a.x) * t;
    const y = a.y + (b.y - a.y) * t;
    ctx.fillStyle = packetColor(p.kind);
    ctx.beginPath();
    ctx.arc(x, y, 5, 0, Math.PI * 2);
    ctx.fill();
    ctx.strokeStyle = '#000';
    ctx.lineWidth = 1;
    ctx.stroke();
    state.lastPackets.push({ packetId: p.packet_id, kind: p.kind, x, y });
  }

  // Apps.
  for (const a of state.topology.apps) {
    const p = state.layout.apps.get(a.id);
    if (!p) continue;
    ctx.fillStyle = '#88c';
    ctx.beginPath();
    ctx.arc(p.x, p.y, 12, 0, Math.PI * 2);
    ctx.fill();
    ctx.strokeStyle = '#558';
    ctx.lineWidth = 2;
    ctx.stroke();
    ctx.fillStyle = '#fff';
    ctx.font = 'bold 11px sans-serif';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillText(`A${a.id}`, p.x, p.y);
    // Address label below the node.
    ctx.font = '10px monospace';
    ctx.fillStyle = '#aac';
    ctx.textBaseline = 'top';
    ctx.fillText(fmtIp(a.ip), p.x, p.y + 14);
  }

  // Switches.
  for (const s of state.topology.switches) {
    const p = state.layout.switches.get(s.id);
    if (!p) continue;
    const dead = failedSwitches.has(s.id);
    const cpuDead = cpuFailed.has(s.id);
    ctx.fillStyle = dead ? '#444' : (cpuDead ? '#552' : '#2c5');
    ctx.beginPath();
    ctx.arc(p.x, p.y, 22, 0, Math.PI * 2);
    ctx.fill();
    ctx.strokeStyle = state.selectedSwitch === s.id ? '#fff' : '#333';
    ctx.lineWidth = state.selectedSwitch === s.id ? 3 : 2;
    ctx.stroke();
    ctx.fillStyle = '#fff';
    ctx.font = 'bold 12px sans-serif';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillText(`S${s.id}`, p.x, p.y);
    // Address label below: BGP speaker IP if known.
    if (s.bgp_speaker_ip != null) {
      ctx.font = '10px monospace';
      ctx.fillStyle = '#9c9';
      ctx.textBaseline = 'top';
      ctx.fillText(fmtIp(s.bgp_speaker_ip), p.x, p.y + 24);
    }
  }
  state.lastSummaries = summaryHits;
}

function packetColor(kind) {
  switch (kind) {
    case 1: return '#ffd34d'; // SimpleBgp
    case 2: return '#aaff77'; // TraceProbe (legacy / unused)
    case 3: return '#7777ff'; // TraceReply
    default: return '#fff';   // Data
  }
}

function formatRate(bps) {
  if (bps > 1e9) return `${(bps / 1e9).toFixed(1)} Gbps`;
  if (bps > 1e6) return `${(bps / 1e6).toFixed(1)} Mbps`;
  if (bps > 1e3) return `${(bps / 1e3).toFixed(1)} kbps`;
  return `${bps.toFixed(0)} bps`;
}

// ----- inspector -----

function setInspectorMode(mode) {
  ui.inspector.dataset.mode = mode;
  for (const sec of ui.inspector.querySelectorAll('section[data-mode]')) {
    sec.hidden = sec.dataset.mode !== mode;
  }
}

function showInspector(switchId) {
  const data = state.player.inspect_switch(switchId);
  if (!data) return;
  ui.inspector.hidden = false;
  ui.inspectorTitle.textContent = `S${switchId}`;
  setInspectorMode('switch');
  ui.inspectorProgram.textContent = data.program_text || '(no program)';
  // Addresses block.
  ui.inspectorAddresses.innerHTML = '';
  const addrDl = document.createElement('dl');
  addrDl.innerHTML += `<dt>switch_id</dt><dd>S${data.switch}</dd>`;
  if (data.bgp_speaker_ip != null) {
    addrDl.innerHTML += `<dt>BGP speaker</dt><dd>${fmtIp(data.bgp_speaker_ip)}</dd>`;
  } else {
    addrDl.innerHTML += `<dt>BGP speaker</dt><dd style="color:#666">(none)</dd>`;
  }
  ui.inspectorAddresses.appendChild(addrDl);
  // Ports block.
  ui.inspectorPorts.innerHTML = '';
  if (data.ports.length === 0) {
    ui.inspectorPorts.innerHTML = '<div style="color:#666">no links attached</div>';
  } else {
    const wrap = document.createElement('div');
    wrap.className = 'table-wrap';
    const tbl = document.createElement('table');
    tbl.innerHTML = `<thead><tr><th>port</th><th>peer</th><th>peer port</th><th>peer ip</th></tr></thead>`;
    const tbody = document.createElement('tbody');
    for (const p of data.ports) {
      const peer = p.peer_kind === 0 ? `A${p.peer_id}` : `S${p.peer_id}`;
      const peerIp = p.peer_ip != null ? fmtIp(p.peer_ip) : '—';
      const tr = document.createElement('tr');
      tr.innerHTML = `<td>${p.port}</td><td>${peer}</td><td>${p.peer_port}</td><td>${peerIp}</td>`;
      tbody.appendChild(tr);
    }
    tbl.appendChild(tbody);
    wrap.appendChild(tbl);
    ui.inspectorPorts.appendChild(wrap);
  }
  ui.inspectorTables.innerHTML = '';
  for (const t of data.tables) {
    const div = document.createElement('div');
    div.innerHTML = `<strong>t${t.table_id}</strong> · ${t.kind} · max ${t.max_entries}`;
    if (t.entries.length === 0) {
      div.innerHTML += '<div style="color:#666">empty</div>';
    } else {
      const wrap = document.createElement('div');
      wrap.className = 'table-wrap';
      const tbl = document.createElement('table');
      tbl.innerHTML = `<thead><tr><th>id</th><th>key</th><th>plen</th><th>action</th></tr></thead>`;
      const tbody = document.createElement('tbody');
      for (const e of t.entries) {
        const tr = document.createElement('tr');
        tr.innerHTML = `<td>${e.id}</td><td>0x${e.key.toString(16)}</td><td>${e.prefix_len}</td><td>${e.action}</td>`;
        tbody.appendChild(tr);
      }
      tbl.appendChild(tbody);
      wrap.appendChild(tbl);
      div.appendChild(wrap);
    }
    ui.inspectorTables.appendChild(div);
  }
  ui.inspectorArrays.innerHTML = '';
  for (const r of data.registers) {
    const d = document.createElement('div');
    d.textContent = `register a${r.array_id}: ${r.size} u64 slots`;
    ui.inspectorArrays.appendChild(d);
  }
  for (const c of data.counters) {
    const d = document.createElement('div');
    d.textContent = `counter c${c.array_id}: ${c.size} u64 slots`;
    ui.inspectorArrays.appendChild(d);
  }
}

const PACKET_KIND_NAMES = ['Data', 'SimpleBgp', 'TraceProbe', 'TraceReply'];

function showLinkSummaryInspector(sum) {
  ui.inspector.hidden = false;
  ui.inspectorTitle.textContent = `Link L${sum.link} · packets in window`;
  setInspectorMode('link');
  const list = state.player.inspect_link(sum.link, sum.fromNs, sum.toNs);
  ui.inspectorLink.innerHTML = '';
  if (!list || list.length === 0) {
    ui.inspectorLink.innerHTML = '<div style="color:#666">no packets in this window</div>';
    return;
  }
  const wrap = document.createElement('div');
  wrap.className = 'table-wrap';
  const tbl = document.createElement('table');
  tbl.innerHTML = `<thead><tr><th>packet_id</th><th>size</th><th>egress @</th></tr></thead>`;
  const tbody = document.createElement('tbody');
  for (const p of list) {
    const tr = document.createElement('tr');
    tr.style.cursor = 'pointer';
    const idHex = `0x${BigInt(p.packet_id).toString(16)}`;
    const tNs = Number(p.at_ns);
    tr.innerHTML = `<td>${idHex}</td><td>${p.size_bytes}</td><td>${(tNs / 1e9).toFixed(6)}s</td>`;
    tr.addEventListener('click', () => {
      state.selectedLink = null;
      state.selectedPacket = p.packet_id;
      showPacketInspector({ packetId: p.packet_id, kind: 0 });
    });
    tbody.appendChild(tr);
  }
  tbl.appendChild(tbody);
  wrap.appendChild(tbl);
  ui.inspectorLink.appendChild(wrap);
}

function showPacketInspector(pick) {
  ui.inspector.hidden = false;
  ui.inspectorTitle.textContent = `Packet 0x${BigInt(pick.packetId).toString(16)}`;
  setInspectorMode('packet');
  const data = state.player.inspect_packet(BigInt(pick.packetId), BigInt(state.cursorNs));
  ui.inspectorPacket.innerHTML = '';
  if (!data) {
    ui.inspectorPacket.textContent = '(no ingress event seen yet for this packet)';
    return;
  }
  const dl = document.createElement('dl');
  const kindName = PACKET_KIND_NAMES[Number(data.kind)] || `kind${data.kind}`;
  const rows = [
    ['kind', kindName],
    ['size', `${data.size_bytes} bytes`],
    ['ip_src', fmtIp(data.ip_src)],
    ['ip_dst', fmtIp(data.ip_dst)],
    ['ip_proto', String(data.ip_proto)],
    ['ip_ttl', String(data.ip_ttl)],
    ['label_top', String(data.label_top)],
    ['label_depth', String(data.label_depth)],
    ['hops', data.history.map(s => `S${s}`).join(' → ') || '(none)'],
  ];
  for (const [k, v] of rows) {
    const dt = document.createElement('dt');
    dt.textContent = k;
    const dd = document.createElement('dd');
    dd.textContent = v;
    dl.appendChild(dt);
    dl.appendChild(dd);
  }
  ui.inspectorPacket.appendChild(dl);
}
