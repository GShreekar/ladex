import type { Device, IncomingOffer } from '../client/types';
import type { AutoAcceptRule } from '../host/preferences-api';

const BYTES_PER_MIB = 1024 * 1024;

/** Whether the rule lets a file of this size from this sender be saved without asking. */
export function isAcceptedWithoutAsking(rule: AutoAcceptRule, sizeBytes: number | null, isSenderTrusted: boolean): boolean {
  if (rule.from === 'nobody' || (rule.from === 'paired' && !isSenderTrusted)) return false;
  if (rule.ask_above_mib === null) return true;
  // A size nobody stated cannot be shown to be under the limit.
  return sizeBytes !== null && sizeBytes <= rule.ask_above_mib * BYTES_PER_MIB;
}

export interface AutoAcceptSource {
  rule(): Promise<AutoAcceptRule>;
  pairedNodeIds(): Promise<Set<string>>;
}

/** Decides, for the device whose window this is, which offered files are saved without a prompt. */
export class AutoAccept {
  constructor(private readonly source: AutoAcceptSource) {}

  async shouldAccept(offer: IncomingOffer, devices: Device[]): Promise<boolean> {
    const rule = await this.source.rule();
    if (rule.from === 'nobody') return false;
    const sender = devices.find((device) => device.id === offer.senderId);
    const self = devices.find((device) => device.isSelf);
    const isOwnNode = !!sender && !!self && sender.nodeId !== '' && sender.nodeId === self.nodeId;
    const isPaired = !!sender && (await this.source.pairedNodeIds()).has(sender.nodeId);
    return isAcceptedWithoutAsking(rule, offer.sizeBytes, isOwnNode || isPaired);
  }
}
