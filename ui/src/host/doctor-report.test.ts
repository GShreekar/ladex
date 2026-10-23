import { describe, expect, it } from 'vitest';
import { formatReport, hasProblem, withDeviceChecks, worstVerdict } from './doctor-report';
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

describe('withDeviceChecks', () => {
  const address: Check = { name: 'Network address', verdict: 'ok', detail: '192.168.1.20' };
  const wifi: Check = { name: 'Wi-Fi', verdict: 'ok', detail: 'connected' };

  it('puts_the_phone_checks_right_after_the_network_address', () => {
    expect(withDeviceChecks([ok, address, problem], [wifi])).toEqual([ok, address, wifi, problem]);
  });

  it('puts_them_first_when_there_is_no_network_address', () => {
    expect(withDeviceChecks([ok, problem], [wifi])).toEqual([wifi, ok, problem]);
  });

  it('leaves_the_checks_as_they_are_without_phone_checks', () => {
    expect(withDeviceChecks([ok, address], [])).toEqual([ok, address]);
  });
});
