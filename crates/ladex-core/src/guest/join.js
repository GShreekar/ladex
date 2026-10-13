'use strict';

const PIECE_BYTES = 8 * 1024 * 1024; // a multiple of the node's 1 MiB chunk
const TUS = { 'Tus-Resumable': '1.0.0' };
const $ = (id) => document.getElementById(id);

function show(section) {
  $('join').hidden = section !== 'join';
  $('session').hidden = section !== 'session';
}

function formatBytes(bytes) {
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let unit = 0;
  while (bytes >= 1024 && unit < units.length - 1) { bytes /= 1024; unit++; }
  return `${bytes.toFixed(unit ? 1 : 0)} ${units[unit]}`;
}

function row(list, label, detail, link) {
  const item = document.createElement('li');
  const name = document.createElement(link ? 'a' : 'span');
  name.textContent = label;
  if (link) name.href = link;
  const note = document.createElement('span');
  note.className = 'muted';
  note.textContent = detail;
  item.append(name, note);
  list.append(item);
  return note;
}

async function join(secret) {
  const field = secret.length > 32 ? 'token' : 'code';
  const response = await fetch('/join', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ [field]: secret }),
  });
  if (response.ok) return null;
  return (await response.json()).message || 'Could not join';
}

async function loadDevices() {
  const { devices } = await (await fetch('/api/devices')).json();
  $('devices').replaceChildren();
  for (const device of devices) row($('devices'), device.name || device.id, device.node_name);
}

function showCatalog(files) {
  $('files').replaceChildren();
  for (const file of files.filter((f) => !f.parent)) {
    const link = `/blobs/${encodeURIComponent(file.id)}${file.is_folder ? '.zip' : ''}`;
    const size = file.is_folder ? `${file.folder_files} files, ${formatBytes(file.folder_bytes)}` : formatBytes(file.size);
    row($('files'), file.name, file.is_available ? size : `${size} (host offline)`, file.is_available ? link : null);
  }
}

function watchCatalog() {
  const events = new EventSource('/api/catalog');
  events.addEventListener('catalog', (event) => { showCatalog(JSON.parse(event.data).files); loadDevices(); });
  events.addEventListener('ended', () => { events.close(); show('join'); });
}

// A piece that fails is retried from the offset the node confirms, until it goes through.
async function sendFile(file, note) {
  const encode = (text) => btoa(String.fromCharCode(...new TextEncoder().encode(text)));
  const metadata = `filename ${encode(file.name)},filetype ${encode(file.type || 'application/octet-stream')}`;
  const created = await fetch('/uploads', {
    method: 'POST',
    headers: { ...TUS, 'Upload-Length': String(file.size), 'Upload-Metadata': metadata },
  });
  if (created.status !== 201) throw new Error((await created.json()).message);
  const location = created.headers.get('Location');
  let state = created.headers.get('Ladex-Upload-State');
  let offset = 0;
  while (offset < file.size) {
    try {
      const sent = await fetch(location, {
        method: 'PATCH',
        headers: { ...TUS, 'Content-Type': 'application/offset+octet-stream', 'Upload-Offset': String(offset) },
        body: file.slice(offset, offset + PIECE_BYTES),
      });
      if (sent.status !== 204 && sent.status !== 409) throw new Error((await sent.json()).message);
      offset = Number(sent.headers.get('Upload-Offset'));
      state = sent.headers.get('Ladex-Upload-State') || state;
      note.textContent = `${Math.round((offset / file.size) * 100)}%`;
    } catch (error) {
      if (error instanceof TypeError) { // the network dropped; ask where the node got to
        await new Promise((resolve) => setTimeout(resolve, 2000));
        const head = await fetch(location, { method: 'HEAD', headers: TUS }).catch(() => null);
        if (head && head.ok) offset = Number(head.headers.get('Upload-Offset'));
      } else throw error;
    }
  }
  return state;
}

async function upload(files) {
  for (const file of files) {
    const note = row($('uploads'), file.name, 'starting…');
    try {
      const state = await sendFile(file, note);
      note.textContent = state === 'awaiting-approval' ? 'waiting for the host to accept it' : 'shared';
    } catch (error) {
      note.textContent = error.message;
      note.className = 'error';
    }
  }
}

async function start() {
  const me = await fetch('/api/me');
  if (!me.ok) return show('join');
  const session = await me.json();
  $('who').textContent = `Joined ${session.node_name} as ${session.label}`;
  $('upload-section').hidden = !session.can_upload;
  show('session');
  watchCatalog();
}

$('join-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  $('join-error').textContent = '';
  const error = await join($('code').value.trim());
  if (error) $('join-error').textContent = error; else start();
});

$('picker').addEventListener('change', (event) => { upload([...event.target.files]); event.target.value = ''; });

// The invitation token in the QR code's fragment is used once and then removed from the address bar.
const token = location.hash.slice(1);
if (token) {
  history.replaceState(null, '', location.pathname);
  join(token).then((error) => { if (error) { $('join-error').textContent = error; show('join'); } else start(); });
} else {
  start();
}
