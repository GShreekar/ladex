export interface DroppedItems {
  files: File[];
  folders: File[][];
}

function withRelativePath(file: File, path: string): File {
  Object.defineProperty(file, 'webkitRelativePath', { value: path });
  return file;
}

function readFile(entry: FileSystemFileEntry): Promise<File> {
  return new Promise((resolve, reject) => entry.file(resolve, reject));
}

async function readAllEntries(directory: FileSystemDirectoryEntry): Promise<FileSystemEntry[]> {
  const reader = directory.createReader();
  const entries: FileSystemEntry[] = [];
  // A reader hands back a batch at a time and signals the end with an empty one.
  for (;;) {
    const batch = await new Promise<FileSystemEntry[]>((resolve, reject) => reader.readEntries(resolve, reject));
    if (batch.length === 0) return entries;
    entries.push(...batch);
  }
}

async function collectFolder(directory: FileSystemDirectoryEntry, prefix: string): Promise<File[]> {
  const files: File[] = [];
  for (const entry of await readAllEntries(directory)) {
    const path = `${prefix}/${entry.name}`;
    if (entry.isDirectory) files.push(...(await collectFolder(entry as FileSystemDirectoryEntry, path)));
    else files.push(withRelativePath(await readFile(entry as FileSystemFileEntry), path));
  }
  return files;
}

/** Splits what was dropped into loose files and whole folders, which the browser only exposes as entries. */
export async function collectDropped(transfer: DataTransfer): Promise<DroppedItems> {
  const entries = [...transfer.items].map((item) => item.webkitGetAsEntry?.() ?? null);
  const hasEntries = entries.length > 0 && entries.every((entry) => entry !== null);
  if (!hasEntries) return { files: [...transfer.files], folders: [] };

  const result: DroppedItems = { files: [], folders: [] };
  for (const entry of entries as FileSystemEntry[]) {
    if (entry.isDirectory) {
      const files = await collectFolder(entry as FileSystemDirectoryEntry, entry.name);
      if (files.length > 0) result.folders.push(files);
    } else {
      result.files.push(await readFile(entry as FileSystemFileEntry));
    }
  }
  return result;
}
