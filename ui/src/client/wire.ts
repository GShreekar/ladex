// The JSON the node sends and receives, as `crates/ladex-core/src/types.rs` defines it.

export interface WirePeer {
  session_id: string;
  user_agent?: string | null;
  nickname?: string | null;
  hosting_node_id?: string | null;
  hosting_node_name?: string | null;
  node_rtt_ms?: number | null;
  left?: boolean;
}

export interface WireFile {
  id: string;
  name: string;
  size: number;
  mime_type: string;
  uploader_id: string;
  uploader_node: string;
  hosts: string[];
  // Older nodes leave it out; then a file is available when a node hosts it.
  available?: boolean;
  uploaded_at: string;
  is_folder: boolean;
  parent: string | null;
  folder_bytes: number;
  folder_files: number;
}

export interface WireMessage {
  id: string;
  content: string;
  sender_id: string;
  sender_name?: string | null;
  timestamp: string;
}

export interface WireOfferedFile {
  name: string;
  size: number;
}

export type ServerMessage =
  | { type: 'peer_joined'; peer: WirePeer; total_peers: number }
  | { type: 'peer_left'; session_id: string; total_peers: number }
  | { type: 'peer_sync'; peers: WirePeer[] }
  | { type: 'file_list_update'; files: WireFile[] }
  | { type: 'file_removed'; file_id: string }
  | { type: 'text_message'; message: WireMessage }
  | { type: 'message_history'; messages: WireMessage[] }
  | { type: 'incoming_file_offer'; file_id: string; from_session_id: string; pushed?: WireOfferedFile }
  | { type: 'file_offer_declined'; file_id: string; from_session_id: string }
  | { type: 'file_push_delivered'; file_id: string; to_session_id: string }
  | { type: 'no_peers_warning'; message: string }
  | { type: 'error'; message: string }
  | { type: 'pong' };

export interface AuthStatus {
  authenticated: boolean;
  auth_required: boolean;
  node_id: string;
  is_host: boolean;
}
