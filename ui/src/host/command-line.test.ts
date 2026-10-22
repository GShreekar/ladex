import { describe, expect, it } from 'vitest';
import { autostartCommand, daemonCommand, dataDirVariable, nodeCommands, quote, settingsCommands } from './command-line';
import type { PreferencesView } from './preferences-api';

function view(overrides: Partial<PreferencesView> = {}): PreferencesView {
  return {
    preferences: { is_setup_done: true, downloads_dir: null, storage_limit_gib: 20, auto_accept: { from: 'nobody', ask_above_mib: null }, peer_addresses: [] },
    data_dir: '/home/sam/.ladex/files',
    is_default_data_dir: true,
    has_passphrase: true,
    needs_restart: false,
    store_used_bytes: 0,
    disk_free_bytes: null,
    platform: 'unix',
    this_address: '192.168.1.5:8080',
    ...overrides,
  };
}

describe('quote', () => {
  it('leaves_a_plain_word_alone', () => {
    expect(quote('/home/sam/files', 'unix')).toBe('/home/sam/files');
  });

  it('wraps_a_path_with_spaces_in_single_quotes_on_unix', () => {
    expect(quote('/home/sam/My Files', 'unix')).toBe("'/home/sam/My Files'");
  });

  it('escapes_a_single_quote_inside_a_unix_word', () => {
    expect(quote("/home/sam/it's", 'unix')).toBe(`'/home/sam/it'\\''s'`);
  });

  it('wraps_a_path_with_spaces_in_double_quotes_on_windows', () => {
    expect(quote('C:\\Users\\Sam Smith\\ladex', 'windows')).toBe('"C:\\Users\\Sam Smith\\ladex"');
  });
});

describe('commands', () => {
  it('need_no_data_folder_when_the_node_uses_the_default_one', () => {
    expect(nodeCommands(view()).map((hint) => hint.command)).toContain('ladex invite');
  });

  it('name_the_data_folder_when_it_is_not_the_default', () => {
    const custom = view({ is_default_data_dir: false, data_dir: '/srv/ladex data' });
    expect(nodeCommands(custom).find((hint) => hint.label === 'List connected devices')?.command).toBe("ladex --data-dir '/srv/ladex data' devices");
  });

  it('set_the_storage_limit_as_it_is_now', () => {
    const custom = view({ preferences: { ...view().preferences, storage_limit_gib: 64 } });
    expect(settingsCommands(custom).map((hint) => hint.command)).toContain('ladex settings --storage-limit-gb 64');
  });

  it('set_the_incoming_rule_with_its_size_limit', () => {
    const custom = view({ preferences: { ...view().preferences, auto_accept: { from: 'paired', ask_above_mib: 500 } } });
    expect(settingsCommands(custom).map((hint) => hint.command)).toContain('ladex settings --auto-accept paired --ask-above-mb 500');
  });

  it('set_the_incoming_rule_without_a_size_limit', () => {
    const custom = view({ preferences: { ...view().preferences, auto_accept: { from: 'mesh', ask_above_mib: null } } });
    expect(settingsCommands(custom).map((hint) => hint.command)).toContain('ladex settings --auto-accept mesh --no-size-limit');
  });

  it('offer_a_downloads_folder_command_only_when_one_is_chosen', () => {
    expect(settingsCommands(view()).some((hint) => hint.command.includes('--downloads-dir'))).toBe(false);
    const chosen = view({ preferences: { ...view().preferences, downloads_dir: '/home/sam/Received Files' } });
    expect(settingsCommands(chosen).map((hint) => hint.command)).toContain("ladex settings --downloads-dir '/home/sam/Received Files'");
  });

  it('offer_adding_a_device_by_address', () => {
    expect(settingsCommands(view()).map((hint) => hint.command)).toContain('ladex settings --add-peer <address>');
  });

  it('run_the_node_without_the_app_with_its_storage_limit', () => {
    expect(daemonCommand(view()).command).toBe('ladex --storage-limit-gb 20');
  });

  it('cover_what_only_the_app_did_before', () => {
    const commands = nodeCommands(view()).map((hint) => hint.command);
    expect(commands).toEqual(expect.arrayContaining(['ladex receive', 'ladex unshare <name>', 'ladex guests', 'ladex chat "<message>"']));
    expect(settingsCommands(view()).map((hint) => hint.command)).toContain('ladex passphrase set --generate');
  });

  it('start_the_node_at_login_on_its_own_data_folder', () => {
    const custom = view({ is_default_data_dir: false, data_dir: '/srv/ladex' });
    expect(autostartCommand(custom).command).toBe('ladex --data-dir /srv/ladex autostart on');
  });
});

describe('dataDirVariable', () => {
  it('uses_export_on_unix', () => {
    expect(dataDirVariable(view())).toBe('export LADEX_DATA_DIR=/home/sam/.ladex/files');
  });

  it('uses_set_on_windows', () => {
    expect(dataDirVariable(view({ platform: 'windows', data_dir: 'C:\\Users\\Sam\\.ladex' }))).toBe('set LADEX_DATA_DIR=C:\\Users\\Sam\\.ladex');
  });
});
