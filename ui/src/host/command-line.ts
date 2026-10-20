import type { PreferencesView } from './preferences-api';

export interface CommandHint {
  label: string;
  command: string;
}

type Platform = PreferencesView['platform'];

const SAFE_WORD = /^[A-Za-z0-9_@%+=:,./-]+$/;

/** Quotes a word for the shell the person is most likely to paste into. */
export function quote(word: string, platform: Platform): string {
  if (SAFE_WORD.test(word)) return word;
  if (platform === 'windows') return `"${word.replaceAll('"', '\\"')}"`;
  return `'${word.replaceAll("'", `'\\''`)}'`;
}

// Commands find the running node through the data folder, so a node kept anywhere but the default needs it named.
function ladex(view: PreferencesView, ...words: string[]): string {
  const dataDir = view.is_default_data_dir ? [] : ['--data-dir', quote(view.data_dir, view.platform)];
  return ['ladex', ...dataDir, ...words].join(' ');
}

/** The commands that do what the app does, for the node as it is set up now. */
export function settingsCommands(view: PreferencesView): CommandHint[] {
  const { preferences } = view;
  const rule = preferences.auto_accept;
  const sizeLimit = rule.ask_above_mib === null ? ['--no-size-limit'] : ['--ask-above-mb', String(rule.ask_above_mib)];
  const downloads = preferences.downloads_dir ? ['--downloads-dir', quote(preferences.downloads_dir, view.platform)] : [];
  return [
    { label: 'Show these settings', command: ladex(view, 'settings') },
    { label: 'Show the mesh passphrase', command: ladex(view, 'passphrase') },
    { label: 'Change the mesh passphrase', command: ladex(view, 'passphrase', 'set', '--generate') },
    { label: 'Set the storage limit', command: ladex(view, 'settings', '--storage-limit-gb', String(preferences.storage_limit_gib)) },
    { label: 'Set the incoming files rule', command: ladex(view, 'settings', '--auto-accept', rule.from, ...sizeLimit) },
    ...(downloads.length ? [{ label: 'Set the downloads folder', command: ladex(view, 'settings', ...downloads) }] : []),
  ];
}

export function nodeCommands(view: PreferencesView): CommandHint[] {
  return [
    { label: 'Invite a phone or laptop', command: ladex(view, 'invite') },
    { label: 'Share files or a folder', command: ladex(view, 'send', '<path>') },
    { label: 'List what is shared', command: ladex(view, 'ls') },
    { label: 'List connected devices', command: ladex(view, 'devices') },
    { label: 'Download a shared file', command: ladex(view, 'get', '<name>') },
    { label: 'Stop sharing a file', command: ladex(view, 'unshare', '<name>') },
    { label: 'Receive files sent to this device', command: ladex(view, 'receive') },
    { label: 'Read and post in the room chat', command: ladex(view, 'chat', '"<message>"') },
    { label: 'See and sign out guests, answer their uploads', command: ladex(view, 'guests') },
    { label: 'Check the network', command: ladex(view, 'doctor') },
  ];
}

/** Runs the same node from a terminal instead of the app; the passphrase saved in the data folder is used. */
export function daemonCommand(view: PreferencesView): CommandHint {
  const dataDir = view.is_default_data_dir ? [] : ['--data-dir', quote(view.data_dir, view.platform)];
  const limit = ['--storage-limit-gb', String(view.preferences.storage_limit_gib)];
  return { label: 'Run this node without the app', command: ['ladex', ...dataDir, ...limit].join(' ') };
}

/** Starts that node when the person logs in, as Launch at login does for the app. */
export function autostartCommand(view: PreferencesView): CommandHint {
  return { label: 'Start this node when you log in', command: ladex(view, 'autostart', 'on') };
}

/** How the data folder is named for another command line: on Windows the variable is set differently. */
export function dataDirVariable(view: PreferencesView): string {
  const dir = quote(view.data_dir, view.platform);
  return view.platform === 'windows' ? `set LADEX_DATA_DIR=${view.data_dir}` : `export LADEX_DATA_DIR=${dir}`;
}
