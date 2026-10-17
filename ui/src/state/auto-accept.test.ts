import { describe, expect, it } from 'vitest';
import type { Device, IncomingOffer } from '../client/types';
import type { AutoAcceptRule } from '../host/preferences-api';
import { AutoAccept, isAcceptedWithoutAsking } from './auto-accept';

const MIB = 1024 * 1024;

function rule(from: AutoAcceptRule['from'], askAbove: number | null = null): AutoAcceptRule {
  return { from, ask_above_mib: askAbove };
}

function device(id: string, nodeId: string, isSelf = false): Device {
  return { id, name: id, nodeName: nodeId, nodeId, kind: 'laptop', isSelf, latencyMs: null };
}

function offer(overrides: Partial<IncomingOffer> = {}): IncomingOffer {
  return { fileId: 'file_1', fileName: 'notes.txt', sizeBytes: MIB, senderName: 'Sam', senderId: 'sender', isDirect: true, ...overrides };
}

function accept(chosen: AutoAcceptRule, paired: string[] = []): AutoAccept {
  return new AutoAccept({ rule: async () => chosen, pairedNodeIds: async () => new Set(paired) });
}

const devices = [device('me', 'node_me', true), device('sender', 'node_other')];

describe('isAcceptedWithoutAsking', () => {
  it('never_accepts_for_the_nobody_rule', () => {
    expect(isAcceptedWithoutAsking(rule('nobody'), 1, true)).toBe(false);
  });

  it('accepts_anyone_for_the_mesh_rule', () => {
    expect(isAcceptedWithoutAsking(rule('mesh'), 1, false)).toBe(true);
  });

  it('asks_about_a_sender_that_is_not_trusted_for_the_paired_rule', () => {
    expect(isAcceptedWithoutAsking(rule('paired'), 1, false)).toBe(false);
    expect(isAcceptedWithoutAsking(rule('paired'), 1, true)).toBe(true);
  });

  it('accepts_a_file_exactly_at_the_size_limit_and_asks_about_a_larger_one', () => {
    expect(isAcceptedWithoutAsking(rule('mesh', 100), 100 * MIB, true)).toBe(true);
    expect(isAcceptedWithoutAsking(rule('mesh', 100), 100 * MIB + 1, true)).toBe(false);
  });

  it('asks_about_a_file_of_unknown_size_when_there_is_a_limit', () => {
    expect(isAcceptedWithoutAsking(rule('mesh', 100), null, true)).toBe(false);
    expect(isAcceptedWithoutAsking(rule('mesh'), null, true)).toBe(true);
  });
});

describe('AutoAccept', () => {
  it('accepts_from_a_device_on_a_paired_node', async () => {
    expect(await accept(rule('paired'), ['node_other']).shouldAccept(offer(), devices)).toBe(true);
  });

  it('asks_about_a_device_on_a_node_that_is_only_in_the_mesh', async () => {
    expect(await accept(rule('paired')).shouldAccept(offer(), devices)).toBe(false);
  });

  it('accepts_from_another_device_on_this_very_node', async () => {
    const sameNode = [device('me', 'node_me', true), device('sender', 'node_me')];
    expect(await accept(rule('paired')).shouldAccept(offer(), sameNode)).toBe(true);
  });

  it('asks_about_a_sender_it_does_not_know', async () => {
    expect(await accept(rule('paired'), ['node_other']).shouldAccept(offer({ senderId: 'stranger' }), devices)).toBe(false);
  });

  it('accepts_anyone_in_the_mesh_even_an_unknown_sender', async () => {
    expect(await accept(rule('mesh')).shouldAccept(offer({ senderId: 'stranger' }), devices)).toBe(true);
  });
});
