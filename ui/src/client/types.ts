export type ConnectionState = 'connecting' | 'online' | 'offline' | 'signed-out' | 'ended';

export type DeviceKind = 'laptop' | 'phone' | 'tablet' | 'unknown';

export interface Device {
  id: string;
  name: string;
  nodeName: string;
  // The node the device is connected to; devices of one node share it.
  nodeId: string;
  kind: DeviceKind;
  isSelf: boolean;
  latencyMs: number | null;
}

export interface SharedFile {
  id: string;
  name: string;
  mimeType: string;
  isFolder: boolean;
  // A folder's own size is its listing; this is what a person means by "how big".
  sizeBytes: number;
  folderFileCount: number;
  sharedAt: number;
  sharedBy: string;
  // Shared from this device, so it already has the file.
  isOwn: boolean;
  // The nodes with a whole copy; browsers that downloaded it are not among them.
  holderNames: string[];
  isAvailable: boolean;
  canUnshare: boolean;
}

export interface ChatMessage {
  id: string;
  text: string;
  senderName: string;
  isOwn: boolean;
  sentAt: number;
}

export interface IncomingOffer {
  fileId: string;
  fileName: string;
  sizeBytes: number | null;
  senderName: string;
  senderId: string;
  // Sent to this device alone, as opposed to pointing at something in the catalog.
  isDirect: boolean;
}

export type NoticeLevel = 'info' | 'success' | 'warning' | 'error';

export interface Notice {
  level: NoticeLevel;
  text: string;
}

export interface Capabilities {
  canUpload: boolean;
  uploadNeedsApproval: boolean;
  canShareFolders: boolean;
  canChat: boolean;
  canRename: boolean;
  canSendToDevice: boolean;
  canManageGuests: boolean;
  canLeave: boolean;
}

export interface SelfInfo {
  id: string;
  name: string;
  nodeName: string;
}

export type ClientEvent =
  | { type: 'connection'; state: ConnectionState }
  | { type: 'self'; self: SelfInfo }
  | { type: 'devices'; devices: Device[] }
  | { type: 'files'; files: SharedFile[] }
  | { type: 'messages'; messages: ChatMessage[] }
  | { type: 'offer'; offer: IncomingOffer }
  | { type: 'no-peers' }
  | { type: 'notice'; notice: Notice };

export type ClientListener = (event: ClientEvent) => void;

export interface UploadRequest {
  signal: AbortSignal;
  onProgress: (bytesSent: number) => void;
}

export interface FileUploadRequest extends UploadRequest {
  toDeviceId?: string;
  // For an upload the host has to approve: called once, when they accept or decline it.
  onAnswered?: (isAccepted: boolean) => void;
}

export type UploadOutcome = 'shared' | 'awaiting-approval' | 'offered';

export interface DownloadTicket {
  url: string;
  filename: string;
  // Set when the client writes the file to disk itself; resolves to where it went.
  saveToDisk?: () => Promise<string>;
}

/** Why a transfer stopped, in words fit for a toast. */
export class TransferError extends Error {}

export class TransferCancelled extends Error {
  constructor() {
    super('cancelled');
  }
}

/** Everything the UI needs from a node, whichever way it is reached. */
export interface LadexClient {
  readonly capabilities: Capabilities;
  subscribe(listener: ClientListener): () => void;
  start(): Promise<void>;
  stop(): void;
  uploadFile(file: File, request: FileUploadRequest): Promise<UploadOutcome>;
  uploadFolder(files: File[], request: UploadRequest): Promise<UploadOutcome>;
  prepareDownload(file: SharedFile): Promise<DownloadTicket>;
  unshare(file: SharedFile): void;
  sendMessage(text: string): void;
  rename(nickname: string): void;
  offerFile(file: SharedFile, device: Device): void;
  answerOffer(offer: IncomingOffer, accepted: boolean): Promise<DownloadTicket | null>;
  leave(): Promise<void>;
}
