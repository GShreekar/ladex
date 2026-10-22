import type { Check, Verdict } from './preferences-api';

const MARKS: Record<Verdict, string> = { ok: 'ok     ', warning: 'warning', problem: 'PROBLEM' };

export function hasProblem(checks: Check[]): boolean {
  return checks.some((check) => check.verdict === 'problem');
}

export function worstVerdict(checks: Check[]): Verdict {
  if (hasProblem(checks)) return 'problem';
  return checks.some((check) => check.verdict === 'warning') ? 'warning' : 'ok';
}

/** Puts the phone's own checks right after the network address, where a network problem is looked for first. */
export function withDeviceChecks(checks: Check[], deviceChecks: Check[]): Check[] {
  const after = checks.findIndex((check) => check.name === 'Network address') + 1;
  return [...checks.slice(0, after), ...deviceChecks, ...checks.slice(after)];
}

/** Plain text to paste into a bug report. */
export function formatReport(checks: Check[], environment: { userAgent: string; when: Date }): string {
  const lines = checks.map((check) => `[${MARKS[check.verdict]}] ${check.name}: ${check.detail}`);
  return [
    `LADEX network report ${environment.when.toISOString()}`,
    `Browser: ${environment.userAgent}`,
    '',
    ...lines,
  ].join('\n');
}
