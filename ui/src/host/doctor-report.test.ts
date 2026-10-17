import { describe, expect, it } from 'vitest';
import { formatReport, hasProblem, worstVerdict } from './doctor-report';
import type { Check } from './preferences-api';

const ok: Check = { name: 'Node', verdict: 'ok', detail: 'laptop' };
const warning: Check = { name: 'Passphrase', verdict: 'warning', detail: 'none' };
const problem: Check = { name: 'Disk', verdict: 'problem', detail: 'full' };

describe('worstVerdict', () => {
  it('is_ok_when_every_check_is', () => {
    expect(worstVerdict([ok, ok])).toBe('ok');
  });

  it('is_a_warning_when_one_check_warns', () => {
    expect(worstVerdict([ok, warning])).toBe('warning');
  });

  it('is_a_problem_when_one_check_fails_even_beside_warnings', () => {
    expect(worstVerdict([warning, problem, ok])).toBe('problem');
    expect(hasProblem([warning, problem])).toBe(true);
  });
});

describe('formatReport', () => {
  it('lists_each_check_with_its_result_under_a_header', () => {
    const report = formatReport([ok, problem], { userAgent: 'Test/1', when: new Date('2026-10-17T10:00:00Z') });
    expect(report.split('\n')).toEqual([
      'LADEX network report 2026-10-17T10:00:00.000Z',
      'Browser: Test/1',
      '',
      '[ok     ] Node: laptop',
      '[PROBLEM] Disk: full',
    ]);
  });
});
