// Wire shapes from docs/remote-access.md and src-tauri/src/remote/host.rs.

export interface Account { id: string; login: string; avatarUrl: string }

/** A presence row (also `GET /v1/devices`). */
export interface Device { id: string; name: string; platform: string; publicKey: string; host: boolean; online: boolean; lastSeen: number }

export interface Project { id: string; name: string; repo: string | null }

export type TaskStatus = 'idle' | 'running' | 'waiting' | 'interrupted' | 'failed' | 'stopped' | (string & {});

export interface TaskSummary {
  sessionId: string; title: string; status: TaskStatus; projectId: string; projectName: string;
  provider: string; model: string; effort: string | null; mode: string; pinned: boolean;
  updatedAt: number; lastPromptAt: number | null; branch?: string; chat?: boolean;
  /** Approvals and questions waiting on the host now. */
  pending?: PendingRequest[];
}

export interface TasksList { tasks: TaskSummary[]; projects: Project[] }

/** An approval or input request waiting on the host, from a subscribe reply. */
export interface PendingRequest { kind: 'approval' | 'input_request' | (string & {}); requestId: string; text: string; data?: unknown; tool?: ToolActivity }

export interface SubscribedTask { sessionId: string; status: TaskStatus; lastId: number; title: string; pending?: PendingRequest[] }

export interface Message { id: number; role: string; content: string; createdAt: number; turnDurationMs: number | null }
export interface MessagePage { messages: Message[]; hasMore: boolean; title: string }

export interface ReasoningEffort { id: string; name: string; description: string }
export interface ProviderModel { id: string; name: string; description: string; reasoningEfforts?: ReasoningEffort[]; defaultReasoningEffort?: string | null; contextWindow?: number | null }
export interface Provider { id: string; name: string; installed: boolean; path?: string | null; version?: string | null; models: ProviderModel[] }

export interface ToolActivity { id: string; name: string; status: 'running' | 'completed' | 'failed'; input: string; output: string }

/** The host's raw AgentEvent (src/types.ts). */
export interface AgentEvent { sessionId: string; kind: string; text: string; requestId?: string; data?: unknown; tool?: ToolActivity }

export interface RunFinished { sessionId: string; successful: boolean; stopped: boolean; text: string }

export interface StartParams {
  repo?: string; projectId?: string; branch?: string; worktree: boolean; prompt: string;
  provider: string; model: string; mode?: string; effort?: string;
}

export interface ConfigureParams { sessionId: string; provider?: string; model?: string; effort?: string | null; mode?: string }
