import type { Check, Verdict } from './preferences-api';

const MARKS: Record<Verdict, string> = { ok: 'ok     ', warning: 'warning', problem: 'PROBLEM' };

export function hasProblem(checks: Check[]): boolean {
  return checks.some((check) => check.verdict === 'problem');
}

export function worstVerdict(checks: Check[]): Verdict {
  if (hasProblem(checks)) return 'problem';
  return checks.some((check) => check.verdict === 'warning') ? 'warning' : 'ok';
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
