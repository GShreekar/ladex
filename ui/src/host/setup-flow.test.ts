import { describe, expect, it } from 'vitest';
import { chosenPassphrase, nextStep, previousStep, problemAt, storageProblem, suggestedStorageGib, type SetupAnswers } from './setup-flow';

const GIB = 1024 ** 3;

function answers(overrides: Partial<SetupAnswers> = {}): SetupAnswers {
  return {
    deviceName: 'Sam',
    meshChoice: 'create',
    createdPassphrase: 'abcd-efgh-jklm',
    joinedPassphrase: '',
    downloadsDir: null,
    storageLimitGib: 20,
    ...overrides,
  };
}

describe('chosenPassphrase', () => {
  it('is_the_made_one_when_creating_a_mesh', () => {
    expect(chosenPassphrase(answers({ joinedPassphrase: 'other' }))).toBe('abcd-efgh-jklm');
  });

  it('is_the_typed_one_when_joining_without_stray_spaces', () => {
    expect(chosenPassphrase(answers({ meshChoice: 'join', joinedPassphrase: '  river-stone-lamp ' }))).toBe('river-stone-lamp');
  });
});

describe('problemAt', () => {
  it('lets_the_person_leave_the_welcome_step', () => {
    expect(problemAt('welcome', answers({ deviceName: '' }), null)).toBeNull();
  });

  it('stops_a_join_without_a_passphrase', () => {
    expect(problemAt('mesh', answers({ meshChoice: 'join' }), null)).toMatch(/passphrase of the mesh/);
  });

  it('stops_a_create_without_a_passphrase', () => {
    expect(problemAt('mesh', answers({ createdPassphrase: '' }), null)).toMatch(/Make a passphrase/);
  });

  it('stops_a_storage_limit_the_disk_cannot_hold', () => {
    expect(problemAt('files', answers({ storageLimitGib: 100 }), 10 * GIB)).toMatch(/only 10 GB free/);
  });
});

describe('storageProblem', () => {
  it('refuses_zero_and_fractions', () => {
    expect(storageProblem(0, null)).not.toBeNull();
    expect(storageProblem(1.5, null)).not.toBeNull();
  });

  it('accepts_a_limit_when_the_free_space_is_unknown', () => {
    expect(storageProblem(500, null)).toBeNull();
  });
});

describe('suggestedStorageGib', () => {
  it('is_the_default_on_a_roomy_disk', () => {
    expect(suggestedStorageGib(20, 500 * GIB)).toBe(20);
  });

  it('leaves_some_of_a_small_disk_free', () => {
    expect(suggestedStorageGib(20, 10 * GIB)).toBe(8);
  });

  it('is_never_below_one', () => {
    expect(suggestedStorageGib(20, GIB)).toBe(1);
  });
});

describe('steps', () => {
  it('move_forward_and_back_without_leaving_the_ends', () => {
    expect(nextStep('welcome')).toBe('mesh');
    expect(nextStep('finish')).toBe('finish');
    expect(previousStep('mesh')).toBe('welcome');
    expect(previousStep('welcome')).toBe('welcome');
  });
});
