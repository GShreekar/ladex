export type SetupStep = 'welcome' | 'mesh' | 'files' | 'finish';

export const SETUP_STEPS: SetupStep[] = ['welcome', 'mesh', 'files', 'finish'];

export type MeshChoice = 'create' | 'join';

export interface SetupAnswers {
  deviceName: string;
  meshChoice: MeshChoice;
  createdPassphrase: string;
  joinedPassphrase: string;
  downloadsDir: string | null;
  storageLimitGib: number;
}

const BYTES_PER_GIB = 1024 ** 3;
// Left for the system and the person's own files, so a full share cannot fill the disk.
const KEEP_FREE_GIB = 2;

/** The passphrase the mesh will use, according to which way the person went. */
export function chosenPassphrase(answers: SetupAnswers): string {
  return (answers.meshChoice === 'create' ? answers.createdPassphrase : answers.joinedPassphrase).trim();
}

export function passphraseProblem(answers: SetupAnswers): string | null {
  if (chosenPassphrase(answers) === '') {
    return answers.meshChoice === 'create' ? 'Make a passphrase first.' : 'Type the passphrase of the mesh you want to join.';
  }
  return null;
}

export function storageProblem(gib: number, freeBytes: number | null): string | null {
  if (!Number.isInteger(gib) || gib < 1) return 'Choose a whole number of GB, at least 1.';
  if (freeBytes !== null && gib * BYTES_PER_GIB > freeBytes) {
    return `This disk has only ${Math.floor(freeBytes / BYTES_PER_GIB)} GB free. Choose less, or another folder.`;
  }
  return null;
}

/** A sensible limit for a disk: the default, unless that would leave the disk too full. */
export function suggestedStorageGib(defaultGib: number, freeBytes: number | null): number {
  if (freeBytes === null) return defaultGib;
  const roomGib = Math.floor(freeBytes / BYTES_PER_GIB) - KEEP_FREE_GIB;
  return Math.max(1, Math.min(defaultGib, roomGib));
}

/** Why the person cannot leave this step yet, or null when they can. */
export function problemAt(step: SetupStep, answers: SetupAnswers, freeBytes: number | null): string | null {
  if (step === 'mesh') return passphraseProblem(answers);
  if (step === 'files') return storageProblem(answers.storageLimitGib, freeBytes);
  return null;
}

export function nextStep(step: SetupStep): SetupStep {
  return SETUP_STEPS[Math.min(SETUP_STEPS.indexOf(step) + 1, SETUP_STEPS.length - 1)] ?? step;
}

export function previousStep(step: SetupStep): SetupStep {
  return SETUP_STEPS[Math.max(SETUP_STEPS.indexOf(step) - 1, 0)] ?? step;
}
