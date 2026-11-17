import { invoke, Channel } from '@tauri-apps/api/core';
import type { ToolExecutionRecord } from '../toolMapper';
import { formatToolExecution } from '../toolMapper';
import { settingsStore, type ReasoningLevel } from './settings.svelte';
import { workspaceStore } from './workspace.svelte';

export interface ChatAttachment {
  id: string;
  name: string;
  path: string;
  size?: number;
  kind: 'image' | 'video' | 'file';
  previewUrl?: string;
}

export interface TodoBoardItem {
  id: string;
  text: string;
  status: 'pending' | 'in_progress' | 'completed' | 'failed' | 'blocked';
}

export type TimelineStep =
  | { kind: 'thought'; id: string; text: string; startedAt: number; finishedAt?: number }
  | { kind: 'tool'; record: ToolExecutionRecord }
  | { kind: 'message'; id: string; text: string }
  | { kind: 'compact'; id: string; beforeTokens: number; afterTokens: number; summary: string };

export interface ChatMessage {
  id: string;
  chatId: string;
  role: 'user' | 'assistant' | 'system' | 'tool';
  content: string;
  modelId?: string;
  attachments?: ChatAttachment[];
  reasoningContent?: string;
  toolCalls?: ToolExecutionRecord[];
  steps?: TimelineStep[];
  todos?: TodoBoardItem[];
  statusText?: string;
  retryReason?: string;
  startedAt?: number;
  finishedAt?: number;
  error?: string;
  createdAt: number;
  streaming?: boolean;
  paused?: boolean;
  snapshotId?: string;
  modifiedFiles?: string[];
  tokenUsage?: {
    promptTokens: number;
    completionTokens: number;
    reasoningTokens?: number;
  };
}

export interface ChatSession {
  id: string;
  title: string;
  createdAt: number;
  updatedAt: number;
  pinned: boolean;
  archived?: boolean;
  isDraft?: boolean;
  projectPath?: string;
  providerId?: string;
  modelId?: string;
  hasError?: boolean;
  isPaused?: boolean;
  hasUnreadCompletion?: boolean;
  messageCount: number;
  preview: string;
}

export interface PendingQueueItem {
  id: string;
  text: string;
  createdAt: number;
  priority?: 'normal' | 'intervention';
}

export interface GoalStatusInfo {
  status: 'active' | 'achieved' | 'paused' | 'failed' | 'idle' | 'cleared';
  condition: string;
  turn: number;
  maxTurns: number;
  reason?: string | null;
  elapsedSecs: number;
}

export interface ContextTelemetryData {
  usedTokens: number;
  contextWindow: number;
  cachedTokens: number;
  cacheHitRate: number;
}

export interface FilePreviewRequest {
  path: string;
  addedTexts?: string[];
  removedTexts?: string[];
}

export interface TurnFileDiff {
  path: string;
  beforeText: string;
  afterText: string;
  addedLines: number;
  removedLines: number;
  existedBefore: boolean;
}

export interface InlineReviewComment {
  id: string;
  path: string;
  lineNumber: number;
  lineType: 'add' | 'del' | 'ctx';
  lineText: string;
  comment: string;
}

class ChatStore {
  sessions = $state<ChatSession[]>([]);
  activeChatId = $state<string | null>(null);
  messagesByChat = $state<Record<string, ChatMessage[]>>({});
  streamingChats = $state<Record<string, boolean>>({});
  pendingQueueByChat = $state<Record<string, PendingQueueItem[]>>({});
  draftTextByChat = $state<Record<string, string>>({});
  draftAttachmentsByChat = $state<Record<string, ChatAttachment[]>>({});
  todosByChat = $state<Record<string, TodoBoardItem[]>>({});
  contextTelemetryByChat = $state<Record<string, ContextTelemetryData>>({});
  goalByChat = $state<Record<string, GoalStatusInfo | null>>({});
  pendingPlanByChat = $state<Record<string, { summary: string } | null>>({});
  reasoningElapsedByChat = $state<Record<string, number>>({});
  streamTokenCountByChat = $state<Record<string, number>>({});

  filterSearchQuery = $state<string>('');
  showArchived = $state<boolean>(false);
  agentMode = $state<'build' | 'plan'>('build');
  filePreview = $state<FilePreviewRequest | null>(null);
  diffReview = $state<{
    messageId: string;
    title: string;
    focusPath?: string;
    files: TurnFileDiff[];
    loading: boolean;
  } | null>(null);
  inlineReviewComments = $state<InlineReviewComment[]>([]);
  sideQuestion = $state<{
    chatId: string;
    question: string;
    answer: string;
    streaming: boolean;
    error: string | null;
    visible: boolean;
  } | null>(null);

  private todoHideTimers: Record<string, ReturnType<typeof setTimeout>> = {};
  private reasoningTimers: Record<string, number> = {};
  private resolveByChat: Record<string, (() => void) | null> = {};
  private streamSaveTimers: Record<string, number> = {};
  private pendingTextByChat: Record<string, string> = {};
  private pendingReasoningByChat: Record<string, string> = {};
  private smoothRafByChat: Record<string, number | null> = {};
  private activeSmoothMsgByChat: Record<string, string | null> = {};

  messages = $derived.by(() => {
    return this.activeChatId ? this.messagesByChat[this.activeChatId] || [] : [];
  });

  isStreaming = $derived.by(() => {
    return Boolean(this.activeChatId && this.streamingChats[this.activeChatId]);
  });

  pendingQueue = $derived.by(() => {
    return this.activeChatId ? this.pendingQueueByChat[this.activeChatId] || [] : [];
  });

  pendingPlan = $derived.by(() => {
    return this.activeChatId ? this.pendingPlanByChat[this.activeChatId] || null : null;
  });

  activeGoal = $derived.by(() => {
    return this.activeChatId ? this.goalByChat[this.activeChatId] || null : null;
  });

  activeTodos = $derived.by(() => {
    return this.activeChatId ? this.todosByChat[this.activeChatId] || [] : [];
  });

  currentStreamTokenCount = $derived.by(() => {
    return this.activeChatId ? this.streamTokenCountByChat[this.activeChatId] || 0 : 0;
  });

  reasoningElapsedMs = $derived.by(() => {
    return this.activeChatId ? this.reasoningElapsedByChat[this.activeChatId] || 0 : 0;
  });

  contextTelemetry = $derived.by<ContextTelemetryData>(() => {
    if (this.activeChatId && this.contextTelemetryByChat[this.activeChatId]) {
      return this.contextTelemetryByChat[this.activeChatId];
    }
    return {
      usedTokens: 0,
      contextWindow: settingsStore.currentModelCapabilities.contextWindow || 128000,
      cachedTokens: 0,
      cacheHitRate: 0
    };
  });

  hasPausedTurn = $derived.by(() => {
    if (this.isStreaming) return false;
    const list = this.messages;
    for (let i = list.length - 1; i >= 0; i--) {
      if (list[i].role === 'system') continue;
      if (list[i].role === 'assistant') {
        return Boolean(list[i].paused);
      }
      return false;
    }
    return false;
  });

  activeSession = $derived.by(() => {
    return this.sessions.find((s) => s.id === this.activeChatId) || null;
  });

  pinnedSessions = $derived.by(() => {
    const q = this.filterSearchQuery.toLowerCase().trim();
    return this.sessions.filter(
      (s) => !s.isDraft && !s.archived && s.pinned && (!q || s.title.toLowerCase().includes(q) || s.preview.toLowerCase().includes(q))
    );
  });

  recentSessions = $derived.by(() => {
    const q = this.filterSearchQuery.toLowerCase().trim();
    return this.sessions.filter((s) => {
      if (s.isDraft) return false;
      const matchArchive = this.showArchived ? Boolean(s.archived) : !s.archived && !s.pinned;
      return matchArchive && (!q || s.title.toLowerCase().includes(q) || s.preview.toLowerCase().includes(q));
    });
  });

  archivedCount = $derived.by(() => {
    return this.sessions.filter((s) => !s.isDraft && Boolean(s.archived)).length;
  });

  timelineStats = $derived.by(() => {
    let totalTools = 0;
    let activeTools = 0;
    let promptTokens = 0;
    let completionTokens = 0;

    for (const msg of this.messages) {
      if (msg.toolCalls) {
        totalTools += msg.toolCalls.length;
        for (const tc of msg.toolCalls) {
          if (tc.status === 'running' || tc.status === 'pending') {
            activeTools++;
          }
        }
      }
      if (msg.tokenUsage) {
        promptTokens += msg.tokenUsage.promptTokens || 0;
        completionTokens += msg.tokenUsage.completionTokens || 0;
      }
    }

    return {
      totalToolsExecuted: totalTools,
      activeToolsCount: activeTools,
      reasoningElapsedMs: this.reasoningElapsedMs,
      totalTokens: promptTokens + completionTokens + this.currentStreamTokenCount,
      promptTokens,
      completionTokens: completionTokens + this.currentStreamTokenCount
    };
  });

  constructor() {
    this.init();
  }

  public setSessionModel(providerId: string, modelId: string, targetChatId?: string): void {
    const cid = targetChatId || this.activeChatId;
    if (!cid) return;
    const sess = this.sessions.find((s) => s.id === cid);
    if (sess) {
      sess.providerId = providerId;
      sess.modelId = modelId;
      this.saveSessions(cid);
    }
  }

  public recordModelChanged(newModelId: string): void {
    const cid = this.activeChatId;
    if (!cid || !newModelId) return;
    const list = this.messagesByChat[cid] || [];
    if (list.length === 0) return;
    const last = list[list.length - 1];
    if (last.role === 'system' && last.content === newModelId) return;
    const sysMsg: ChatMessage = {
      id: `sys-model-${Date.now()}-${Math.random().toString(36).slice(2, 5)}`,
      chatId: cid,
      role: 'system',
      content: newModelId,
      createdAt: Date.now()
    };
    this.messagesByChat[cid] = [...list, sysMsg];
    this.saveSessions(cid);
  }

  public isChatStreaming(chatId: string): boolean {
    return Boolean(this.streamingChats[chatId]);
  }

  public openFilePreview(path: string, addedTexts?: string[], removedTexts?: string[]): void {
    this.filePreview = { path, addedTexts, removedTexts };
  }

  public closeFilePreview(): void {
    this.filePreview = null;
  }

  public async refreshGoalState(targetChatId?: string): Promise<void> {
    const cid = targetChatId || this.activeChatId;
    if (!cid) return;
    try {
      const g = await invoke<any>('get_goal_state', { chatId: cid });
      if (g && g.condition) {
        const startedAt = typeof g.startedAt === 'number' ? g.startedAt : Math.floor(Date.now() / 1000);
        this.goalByChat[cid] = {
          status: g.status || 'idle',
          condition: g.condition,
          turn: g.turn || 1,
          maxTurns: g.maxTurns || 100,
          reason: g.lastReason || null,
          elapsedSecs: Math.max(0, Math.floor(Date.now() / 1000) - startedAt)
        };
      } else {
        this.goalByChat[cid] = null;
      }
    } catch {}
  }

  public async approvePlan(targetChatId?: string): Promise<void> {
    const cid = targetChatId || this.activeChatId;
    if (!cid) return;
    this.pendingPlanByChat[cid] = null;
    await this.sendMessage('Plan approved. Execute the plan now.', { targetChatId: cid, forceExecute: true });
  }

  public async requestPlanChanges(feedback: string, targetChatId?: string): Promise<void> {
    const cid = targetChatId || this.activeChatId;
    if (!cid || !feedback.trim()) return;
    this.pendingPlanByChat[cid] = null;
    await this.sendMessage(`/plan ${feedback.trim()}`, { targetChatId: cid });
  }

  public rejectPlan(targetChatId?: string): void {
    const cid = targetChatId || this.activeChatId;
    if (!cid) return;
    this.pendingPlanByChat[cid] = null;
  }

  public async clearGoal(targetChatId?: string): Promise<void> {
    const cid = targetChatId || this.activeChatId;
    if (!cid) return;
    try {
      await invoke('clear_goal_state', { chatId: cid });
    } catch {}
    if (this.streamingChats[cid] && this.goalByChat[cid]) {
      this.goalByChat[cid] = {
        ...this.goalByChat[cid]!,
        status: 'cleared',
        reason: 'Goal cleared · finishing current step'
      };
    } else {
      this.goalByChat[cid] = null;
    }
  }

  public hideSideQuestion(): void {
    if (this.sideQuestion) {
      this.sideQuestion = { ...this.sideQuestion, visible: false };
    }
  }

  public clearSideQuestion(): void {
    this.sideQuestion = null;
  }

  public async openDiffReview(
    messageId: string,
    title: string,
    snapshotIds: string[],
    focusPath?: string
  ): Promise<void> {
    this.diffReview = { messageId, title, focusPath, files: [], loading: true };
    try {
      const files = await invoke<TurnFileDiff[]>('get_turn_file_diffs', { snapshotIds });
      if (this.diffReview?.messageId === messageId) {
        this.diffReview = { messageId, title, focusPath, files, loading: false };
      }
    } catch {
      if (this.diffReview?.messageId === messageId) {
        this.diffReview = { messageId, title, focusPath, files: [], loading: false };
      }
    }
  }

  public closeDiffReview(): void {
    this.diffReview = null;
  }

  public addInlineReviewComment(c: Omit<InlineReviewComment, 'id'>): void {
    this.inlineReviewComments = [
      ...this.inlineReviewComments,
      { ...c, id: `rev-${Date.now()}-${Math.random().toString(36).slice(2, 6)}` }
    ];
  }

  public removeInlineReviewComment(id: string): void {
    this.inlineReviewComments = this.inlineReviewComments.filter((c) => c.id !== id);
  }

  public async sendInlineReviewComments(): Promise<void> {
    if (this.inlineReviewComments.length === 0) return;
    const lines = ['[Inline Code Review]'];
    for (const c of this.inlineReviewComments) {
      lines.push(`- ${c.path}:${c.lineNumber} (\`${c.lineText.trim().slice(0, 120)}\`): ${c.comment}`);
    }
    this.inlineReviewComments = [];
    await this.sendMessage(lines.join('\n'));
  }

  public async askSideQuestion(rawInput: string): Promise<void> {
    const trimmed = rawInput.trim();
    const q = trimmed.replace(/^\/btw\b/i, '').trim();
    const chatId = this.activeChatId;
    if (!q) {
      if (this.sideQuestion && this.sideQuestion.chatId === chatId) {
        this.sideQuestion = { ...this.sideQuestion, visible: !this.sideQuestion.visible };
      }
      return;
    }

    this.sideQuestion = {
      chatId: chatId || '',
      question: q,
      answer: '',
      streaming: true,
      error: null,
      visible: true
    };

    try {
      const prov = settingsStore.currentProvider;
      const detectedInfo = prov?.modelCapabilities?.[settingsStore.data.activeModel];
      const list = chatId ? this.messagesByChat[chatId] || [] : [];
      const history = list
        .filter((m) => m.role === 'user' || m.role === 'assistant')
        .map((m) => {
          let text = m.content.trim();
          if (m.steps && m.steps.length > 0) {
            const parts: string[] = [];
            for (const s of m.steps) {
              if (s.kind === 'message' && s.text.trim()) parts.push(s.text.trim());
              else if (s.kind === 'tool' && s.record) {
                const out = s.record.output ? ` -> ${s.record.output.slice(0, 600)}` : '';
                parts.push(`[Tool ${s.record.toolName}${out}]`);
              }
            }
            if (parts.length > 0) text = parts.join('\n');
          }
          return { role: m.role, content: text };
        })
        .filter((m) => m.content.length > 0);

      const onEvent = new Channel<any>();
      onEvent.onmessage = (evt: any) => {
        if (!this.sideQuestion || this.sideQuestion.chatId !== chatId) return;
        if (evt.type === 'delta' && typeof evt.content === 'string') {
          this.sideQuestion = {
            ...this.sideQuestion,
            answer: this.sideQuestion.answer + evt.content
          };
        } else if (evt.type === 'done') {
          this.sideQuestion = { ...this.sideQuestion, streaming: false };
        } else if (evt.type === 'error') {
          this.sideQuestion = {
            ...this.sideQuestion,
            streaming: false,
            error: evt.message || 'Side question failed'
          };
        }
      };

      await invoke('ask_side_question', {
        chatId: chatId || 'side-question',
        question: q,
        modelId: settingsStore.data.activeModel,
        providerName: prov?.name || null,
        baseUrl: prov?.baseUrl || null,
        apiKey: prov?.apiKey || null,
        apiProtocol: prov?.apiProtocol || 'auto',
        proxyEnabled: Boolean(prov?.proxyEnabled),
        proxyUrl: prov?.proxyUrl || null,
        contextWindow: detectedInfo?.contextWindow || null,
        maxOutputTokens: detectedInfo?.maxOutputTokens || null,
        history,
        onEvent
      });
      if (this.sideQuestion && this.sideQuestion.streaming) {
        this.sideQuestion = { ...this.sideQuestion, streaming: false };
      }
    } catch (err: any) {
      if (this.sideQuestion) {
        this.sideQuestion = {
          ...this.sideQuestion,
          streaming: false,
          error: err instanceof Error ? err.message : String(err)
        };
      }
    }
  }

  private async init(): Promise<void> {
    await this.loadSessions();
    this.syncAllSessionsToBackend();
    await this.pullBackendChats();
    if (this.sessions.length > 0 && !this.activeChatId) {
      const firstActive = this.sessions.find((s) => !s.archived && !s.isDraft);
      if (firstActive) {
        await this.selectSession(firstActive.id);
      }
    }
    import('@tauri-apps/api/event')
      .then(({ listen }) => {
        listen<{ chatId?: string }>('telegram-chat-updated', (ev) => {
          this.pullBackendChats(ev.payload?.chatId);
        });
        listen<{
          chatId?: string;
          type?: string;
          userText?: string;
          delta?: string;
          id?: string;
          name?: string;
          arguments?: string;
          success?: boolean;
          elapsedMs?: number;
          attempt?: number;
          reason?: string;
          error?: string;
        }>('telegram-chat-stream', (ev) => {
          this.handleTelegramStreamEvent(ev.payload);
        });
      })
      .catch(() => {});
  }

  public syncAllSessionsToBackend(): void {
    for (const sess of this.sessions) {
      if (sess.isDraft || this.streamingChats[sess.id]) continue;
      const rawMsgs = this.messagesByChat[sess.id] || [];
      if (rawMsgs.length === 0) continue;
      const msgs = rawMsgs.map((m) => ({
        role: m.role,
        content: m.content,
        createdAt: m.createdAt
      }));
      invoke('sync_chat_context', {
        chatId: sess.id,
        title: sess.title || sess.id,
        messagesJson: JSON.stringify(msgs),
        projectPath: sess.projectPath || null,
        modelId: settingsStore.data.activeModel || null
      }).catch(() => {});
    }
  }

  private handleTelegramStreamEvent(payload: {
    chatId?: string;
    type?: string;
    userText?: string;
    delta?: string;
    id?: string;
    name?: string;
    arguments?: string;
    success?: boolean;
    elapsedMs?: number;
    attempt?: number;
    reason?: string;
    error?: string;
  } | undefined): void {
    const chatId = payload?.chatId;
    const evtType = payload?.type;
    if (!chatId || !evtType) return;

    const sess = this.sessions.find((s) => s.id === chatId);
    if (evtType === 'start') {
      if (sess) {
        sess.hasError = false;
        sess.isPaused = false;
        sess.hasUnreadCompletion = false;
        sess.updatedAt = Date.now();
      }
      this.streamingChats[chatId] = true;
      const list = this.messagesByChat[chatId] || [];
      const userText = (payload?.userText || '').trim();
      const lastMsg = list[list.length - 1];
      if (userText && (!lastMsg || lastMsg.role !== 'user' || lastMsg.content !== userText)) {
        list.push({
          id: `tg-u-${Date.now()}`,
          chatId,
          role: 'user',
          content: userText,
          createdAt: Date.now()
        });
      }
      const tail = list[list.length - 1];
      if (!tail || tail.role !== 'assistant' || !tail.streaming) {
        list.push({
          id: `tg-a-${Date.now()}`,
          chatId,
          role: 'assistant',
          content: '',
          reasoningContent: '',
          toolCalls: [],
          steps: [],
          startedAt: Date.now(),
          createdAt: Date.now(),
          streaming: true
        });
      } else {
        tail.error = undefined;
      }
      this.messagesByChat[chatId] = [...list];
      this.saveSessions(chatId);
      return;
    }

    const list = this.messagesByChat[chatId];
    if (!list || list.length === 0) return;
    const msg = list[list.length - 1];
    if (!msg || msg.role !== 'assistant') return;

    if (evtType === 'thinking' && payload?.delta) {
      this.applyReasoningChunk(chatId, msg, payload.delta);
      return;
    }
    if (evtType === 'content' && payload?.delta) {
      this.applyContentChunk(chatId, msg, payload.delta);
      return;
    }
    if (evtType === 'tool_start' && payload?.id && payload?.name) {
      if (!msg.toolCalls) msg.toolCalls = [];
      if (!msg.steps) msg.steps = [];
      const rec = formatToolExecution({
        id: payload.id,
        name: payload.name,
        arguments: payload.arguments || '',
        status: 'running',
        startedAt: Date.now()
      });
      msg.toolCalls.push(rec);
      msg.steps.push({ kind: 'tool', record: rec });
      this.messagesByChat[chatId] = [...list];
      return;
    }
    if (evtType === 'tool_end' && payload?.id) {
      const rec = msg.toolCalls?.find((t) => t.toolCallId === payload.id);
      if (rec) {
        rec.status = payload.success ? 'success' : 'error';
        rec.finishedAt = Date.now();
        rec.elapsedMs = payload.elapsedMs;
        this.messagesByChat[chatId] = [...list];
      }
      return;
    }
    if (evtType === 'retry') {
      msg.statusText = `Reconnecting (attempt ${payload?.attempt || 1})...`;
      msg.retryReason = payload?.reason;
      this.messagesByChat[chatId] = [...list];
      return;
    }
    if (evtType === 'error') {
      msg.error = payload?.error || 'Telegram execution failed';
      msg.streaming = false;
      msg.finishedAt = Date.now();
      this.streamingChats[chatId] = false;
      if (sess) sess.hasError = true;
      this.messagesByChat[chatId] = [...list];
      this.saveSessions(chatId);
      return;
    }
    if (evtType === 'done') {
      msg.streaming = false;
      msg.statusText = undefined;
      msg.finishedAt = Date.now();
      this.streamingChats[chatId] = false;
      if (sess && this.activeChatId !== chatId && !msg.error) {
        sess.hasUnreadCompletion = true;
      }
      this.messagesByChat[chatId] = [...list];
      this.saveChatMessagesToStorage(chatId, list);
      this.saveSessions(chatId);
    }
  }

  public async pullBackendChats(targetChatId?: string): Promise<void> {
    try {
      const backendChats = await invoke<
        Array<{
          id: string;
          projectId?: string | null;
          title: string;
          modelId: string;
          isPinned: boolean;
          createdAt: string;
          updatedAt: string;
        }>
      >('list_chats', { projectId: null });
      if (!Array.isArray(backendChats)) return;

      const backendIds = new Set(backendChats.map((c) => c.id));
      let sessionsChanged = false;

      if (targetChatId && !backendIds.has(targetChatId)) {
        const had = this.sessions.some((s) => s.id === targetChatId);
        if (had) {
          this.sessions = this.sessions.filter((s) => s.id !== targetChatId);
          delete this.messagesByChat[targetChatId];
          sessionsChanged = true;
        }
      }

      for (const bc of backendChats) {
        const existing = this.sessions.find((s) => s.id === bc.id);
        if (!existing) {
          const ts = Date.parse(bc.updatedAt) || Date.now();
          const newSess: ChatSession = {
            id: bc.id,
            title: bc.title || 'Telegram Chat',
            createdAt: Date.parse(bc.createdAt) || ts,
            updatedAt: ts,
            pinned: Boolean(bc.isPinned),
            projectPath: bc.projectId || undefined,
            messageCount: 0,
            preview: ''
          };
          this.sessions = [newSess, ...this.sessions];
          sessionsChanged = true;
        } else if (bc.title && existing.title !== bc.title && targetChatId === bc.id) {
          existing.title = bc.title;
          sessionsChanged = true;
        }

        if ((!targetChatId || targetChatId === bc.id) && !this.streamingChats[bc.id]) {
          const dbMsgs = await invoke<
            Array<{
              id: string;
              chatId: string;
              role: 'user' | 'assistant' | 'system' | 'tool';
              content: string;
              reasoningContent?: string | null;
              createdAt: string;
            }>
          >('list_messages', { chatId: bc.id });
          if (Array.isArray(dbMsgs)) {
            const localMsgs = this.messagesByChat[bc.id] || [];
            const lastDb = dbMsgs[dbMsgs.length - 1];
            const lastLocal = localMsgs[localMsgs.length - 1];
            const changed =
              dbMsgs.length !== localMsgs.length ||
              (lastDb && lastLocal && (lastDb.content !== lastLocal.content || lastDb.id !== lastLocal.id));
            if (changed && dbMsgs.length >= localMsgs.length) {
              const converted: ChatMessage[] = dbMsgs.map((m) => ({
                id: m.id,
                chatId: m.chatId,
                role: m.role,
                content: m.content,
                reasoningContent: m.reasoningContent || undefined,
                createdAt: Date.parse(m.createdAt) || Date.now()
              }));
              this.messagesByChat[bc.id] = converted;
              this.saveChatMessagesToStorage(bc.id, converted);
              const sess = this.sessions.find((s) => s.id === bc.id);
              if (sess) {
                sess.messageCount = converted.length;
                if (lastDb && lastDb.content) {
                  sess.preview = lastDb.content.slice(0, 80);
                }
                sess.updatedAt = Date.parse(bc.updatedAt) || Date.now();
                sessionsChanged = true;
              }
            }
          }
        }
      }

      if (sessionsChanged) {
        this.safeSetStorage('omni_chat_sessions', JSON.stringify(this.sessions.filter((s) => !s.isDraft)));
      }
    } catch {}
  }

  private compactMessagesForStorage(msgs: ChatMessage[]): ChatMessage[] {
    return msgs.map((m, idx) => {
      const isRecent = idx >= msgs.length - 6;
      const compactedAttachments = m.attachments?.map((a) => ({
        id: a.id,
        name: a.name,
        path: a.path,
        size: a.size,
        kind: a.kind
      }));
      const compactedTools = m.toolCalls?.map((tc) => ({
        ...tc,
        output: tc.output && tc.output.length > (isRecent ? 4000 : 800)
          ? tc.output.slice(0, isRecent ? 4000 : 800) + "..."
          : tc.output,
        terminalData: tc.terminalData
          ? {
              ...tc.terminalData,
              stdout: tc.terminalData.stdout && tc.terminalData.stdout.length > (isRecent ? 4000 : 800)
                ? tc.terminalData.stdout.slice(0, isRecent ? 4000 : 800) + "..."
                : tc.terminalData.stdout
            }
          : undefined
      }));
      const compactedSteps = m.steps?.map((s) => {
        if (s.kind !== "tool") return s;
        return {
          kind: "tool" as const,
          record: {
            ...s.record,
            output: s.record.output && s.record.output.length > (isRecent ? 4000 : 800)
              ? s.record.output.slice(0, isRecent ? 4000 : 800) + "..."
              : s.record.output,
            terminalData: s.record.terminalData
              ? {
                  ...s.record.terminalData,
                  stdout: s.record.terminalData.stdout && s.record.terminalData.stdout.length > (isRecent ? 4000 : 800)
                    ? s.record.terminalData.stdout.slice(0, isRecent ? 4000 : 800) + "..."
                    : s.record.terminalData.stdout
                }
              : undefined
          }
        };
      });
      return {
        ...m,
        attachments: compactedAttachments,
        toolCalls: compactedTools,
        steps: compactedSteps,
        reasoningContent: !isRecent && m.reasoningContent && m.reasoningContent.length > 1000
          ? m.reasoningContent.slice(0, 1000) + "..."
          : m.reasoningContent
      };
    });
  }

  private safeSetStorage(key: string, value: string, protectChatId?: string): boolean {
    if (typeof localStorage === "undefined") return false;
    try {
      localStorage.setItem(key, value);
      return true;
    } catch {
      try {
        const candidates = this.sessions
          .filter((s) => s.id !== protectChatId && s.id !== this.activeChatId && !s.pinned)
          .sort((a, b) => (a.updatedAt || 0) - (b.updatedAt || 0));
        for (const s of candidates) {
          localStorage.removeItem(`omni_chat_msgs_${s.id}`);
          try {
            localStorage.setItem(key, value);
            return true;
          } catch {}
        }
      } catch {}
      return false;
    }
  }

  private saveChatMessagesToStorage(chatId: string, msgs: ChatMessage[]): void {
    const raw = JSON.stringify(msgs);
    if (this.safeSetStorage(`omni_chat_msgs_${chatId}`, raw, chatId)) return;
    const compacted = JSON.stringify(this.compactMessagesForStorage(msgs));
    this.safeSetStorage(`omni_chat_msgs_${chatId}`, compacted, chatId);
  }

  private persistDrafts(): void {
    this.safeSetStorage(
      'omni_chat_drafts',
      JSON.stringify({
        texts: this.draftTextByChat,
        attachments: this.draftAttachmentsByChat
      })
    );
  }

  private persistPendingQueues(): void {
    this.safeSetStorage('omni_pending_queues', JSON.stringify(this.pendingQueueByChat));
  }

  public getDraftText(chatId: string | null): string {
    const key = chatId || '__new__';
    return this.draftTextByChat[key] || '';
  }

  public setDraftText(chatId: string | null, text: string): void {
    const key = chatId || '__new__';
    if ((this.draftTextByChat[key] || '') === text) return;
    if (!text) {
      delete this.draftTextByChat[key];
    } else {
      this.draftTextByChat[key] = text;
    }
    this.persistDrafts();
  }

  public getDraftAttachments(chatId: string | null): ChatAttachment[] {
    const key = chatId || '__new__';
    return this.draftAttachmentsByChat[key] ? [...this.draftAttachmentsByChat[key]] : [];
  }

  public setDraftAttachments(chatId: string | null, atts: ChatAttachment[]): void {
    const key = chatId || '__new__';
    const clean = atts.map((a) => ({
      id: a.id,
      name: a.name,
      path: a.path,
      size: a.size,
      kind: a.kind,
      previewUrl: a.previewUrl && a.previewUrl.startsWith('data:') ? undefined : a.previewUrl
    }));
    const prevJson = JSON.stringify(this.draftAttachmentsByChat[key] || []);
    const nextJson = JSON.stringify(clean);
    if (prevJson === nextJson) return;
    if (clean.length === 0) {
      delete this.draftAttachmentsByChat[key];
    } else {
      this.draftAttachmentsByChat[key] = clean;
    }
    this.persistDrafts();
  }

  public async loadSessions(): Promise<void> {
    if (typeof localStorage !== 'undefined') {
      try {
        const rawDrafts = localStorage.getItem('omni_chat_drafts');
        if (rawDrafts) {
          const parsedDrafts = JSON.parse(rawDrafts);
          if (parsedDrafts && typeof parsedDrafts === 'object') {
            if (parsedDrafts.texts && typeof parsedDrafts.texts === 'object') {
              this.draftTextByChat = parsedDrafts.texts;
            }
            if (parsedDrafts.attachments && typeof parsedDrafts.attachments === 'object') {
              this.draftAttachmentsByChat = parsedDrafts.attachments;
            }
          }
        }
      } catch {}
      try {
        const rawQueues = localStorage.getItem('omni_pending_queues');
        if (rawQueues) {
          const parsedQueues = JSON.parse(rawQueues);
          if (parsedQueues && typeof parsedQueues === 'object') {
            this.pendingQueueByChat = parsedQueues;
          }
        }
      } catch {}
      const saved = localStorage.getItem('omni_chat_sessions');
      if (saved) {
        try {
          const parsed = JSON.parse(saved);
          if (Array.isArray(parsed)) {
            const valid = parsed.filter((s) => !s.isDraft);
            for (const s of valid) {
              const rawMsgs = localStorage.getItem(`omni_chat_msgs_${s.id}`);
              if (rawMsgs) {
                try {
                  const msgs = JSON.parse(rawMsgs);
                  if (Array.isArray(msgs)) {
                    let changed = false;
                    let hasPaused = false;
                    for (const m of msgs) {
                      if (m.streaming) {
                        m.streaming = false;
                        m.paused = true;
                        m.statusText = 'Paused';
                        const lastActive = Math.max(
                          m.startedAt || 0,
                          s.updatedAt || 0,
                          ...(m.steps || []).map((st: any) => st.finishedAt || st.startedAt || 0),
                          ...(m.toolCalls || []).map((tc: any) => tc.finishedAt || tc.startedAt || 0)
                        ) || m.startedAt || Date.now();
                        m.finishedAt = m.finishedAt || lastActive;
                        this.finalizeRunningTools(m, 'paused', undefined, m.finishedAt);
                        changed = true;
                      }
                      if (m.paused) hasPaused = true;
                    }
                    s.isPaused = hasPaused;
                    this.messagesByChat[s.id] = msgs;
                    if (changed) {
                      this.saveChatMessagesToStorage(s.id, msgs);
                    }
                  }
                } catch {}
              }
            }
            this.sessions = valid;
            return;
          }
        } catch {}
      }
    }
    this.sessions = [];
  }

  public async saveSessions(targetChatId?: string): Promise<void> {
    const cid = targetChatId || this.activeChatId;
    const persistableSessions = this.sessions.filter((s) => !s.isDraft);
    try {
      this.safeSetStorage('omni_chat_sessions', JSON.stringify(persistableSessions), cid || undefined);
      if (cid) {
        const sess = this.sessions.find((s) => s.id === cid);
        if (sess && !sess.isDraft) {
          const msgs = this.messagesByChat[cid] || [];
          this.saveChatMessagesToStorage(cid, msgs);
        }
      }
    } catch {}
    if (cid) {
      const sess = this.sessions.find((s) => s.id === cid);
      if (sess && !sess.isDraft) {
        const msgs = (this.messagesByChat[cid] || []).map((m) => ({
          role: m.role,
          content: m.content,
          createdAt: m.createdAt
        }));
        invoke('sync_chat_context', {
          chatId: cid,
          title: sess.title || cid,
          messagesJson: JSON.stringify(msgs),
          projectPath: sess.projectPath || null,
          modelId: settingsStore.data.activeModel || null
        }).catch(() => {});
      }
    }
  }

  private finalizeRunningTools(
    msg: ChatMessage,
    targetStatus: 'error' | 'cancelled' | 'paused' | 'success',
    errText?: string,
    fallbackEndTime?: number
  ): void {
    const now = fallbackEndTime ?? Date.now();
    if (Array.isArray(msg.toolCalls)) {
      msg.toolCalls = msg.toolCalls.map((tc) => {
        if (tc.status !== 'running' && tc.status !== 'pending') return tc;
        return {
          ...tc,
          status: targetStatus,
          finishedAt: tc.finishedAt || now,
          elapsedMs: tc.startedAt ? (tc.finishedAt || now) - tc.startedAt : tc.elapsedMs,
          error: errText ?? tc.error
        };
      });
    }
    if (Array.isArray(msg.steps)) {
      msg.steps = msg.steps.map((s) => {
        if (s.kind === 'thought') {
          if (!s.finishedAt) {
            return { ...s, finishedAt: now };
          }
          return s;
        }
        if (s.kind !== 'tool') return s;
        if (s.record.status !== 'running' && s.record.status !== 'pending') return s;
        return {
          kind: 'tool',
          record: {
            ...s.record,
            status: targetStatus,
            finishedAt: s.record.finishedAt || now,
            elapsedMs: s.record.startedAt ? (s.record.finishedAt || now) - s.record.startedAt : s.record.elapsedMs,
            error: errText ?? s.record.error
          }
        };
      });
    }
  }

  private scheduleStreamingSave(chatId: string): void {
    if (this.streamSaveTimers[chatId]) return;
    this.streamSaveTimers[chatId] = window.setTimeout(() => {
      delete this.streamSaveTimers[chatId];
      this.saveSessions(chatId);
    }, 900);
  }

  private takeSmoothSlice(buf: string): [string, string] {
    if (buf.length <= 3) return [buf, ''];
    let n = Math.max(2, Math.min(48, Math.ceil(buf.length / 8)));
    if (n < buf.length) {
      const code = buf.charCodeAt(n - 1);
      if (code >= 0xd800 && code <= 0xdbff) {
        n += 1;
      }
    }
    return [buf.slice(0, n), buf.slice(n)];
  }

  private applyReasoningChunk(chatId: string, msg: ChatMessage, chunk: string): void {
    if (!chunk) return;
    msg.statusText = undefined;
    msg.reasoningContent = (msg.reasoningContent || '') + chunk;
    if (!msg.steps) msg.steps = [];
    const last = msg.steps[msg.steps.length - 1];
    if (last && last.kind === 'thought') {
      last.text += chunk;
      last.finishedAt = Date.now();
    } else {
      msg.steps.push({
        kind: 'thought',
        id: `th-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
        text: chunk,
        startedAt: Date.now(),
        finishedAt: Date.now()
      });
    }
    this.streamTokenCountByChat[chatId] = (this.streamTokenCountByChat[chatId] || 0) + 1;
    this.scheduleStreamingSave(chatId);
  }

  private applyContentChunk(chatId: string, msg: ChatMessage, chunk: string): void {
    if (!chunk) return;
    msg.statusText = undefined;
    msg.content += chunk;
    if (!msg.steps) msg.steps = [];
    const last = msg.steps[msg.steps.length - 1];
    if (last && last.kind === 'message') {
      last.text += chunk;
    } else {
      msg.steps.push({
        kind: 'message',
        id: `msg-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
        text: chunk
      });
    }
    this.streamTokenCountByChat[chatId] = (this.streamTokenCountByChat[chatId] || 0) + 1;
    this.scheduleStreamingSave(chatId);
  }

  private startSmoothFlush(chatId: string): void {
    if (this.smoothRafByChat[chatId]) return;
    const step = () => {
      this.smoothRafByChat[chatId] = null;
      const list = this.messagesByChat[chatId] || [];
      const targetId = this.activeSmoothMsgByChat[chatId];
      const msg = targetId ? list.find((m) => m.id === targetId) : list[list.length - 1];
      if (!msg || msg.role !== 'assistant') {
        this.pendingReasoningByChat[chatId] = '';
        this.pendingTextByChat[chatId] = '';
        return;
      }
      const rBuf = this.pendingReasoningByChat[chatId] || '';
      const tBuf = this.pendingTextByChat[chatId] || '';
      if (rBuf.length > 0) {
        const [slice, rest] = this.takeSmoothSlice(rBuf);
        this.pendingReasoningByChat[chatId] = rest;
        this.applyReasoningChunk(chatId, msg, slice);
      } else if (tBuf.length > 0) {
        const [slice, rest] = this.takeSmoothSlice(tBuf);
        this.pendingTextByChat[chatId] = rest;
        this.applyContentChunk(chatId, msg, slice);
      }
      if ((this.pendingReasoningByChat[chatId] || '').length > 0 || (this.pendingTextByChat[chatId] || '').length > 0) {
        this.smoothRafByChat[chatId] = requestAnimationFrame(step);
      }
    };
    this.smoothRafByChat[chatId] = requestAnimationFrame(step);
  }

  private flushSmoothBuffers(chatId: string): void {
    const rafId = this.smoothRafByChat[chatId];
    if (rafId) {
      cancelAnimationFrame(rafId);
      this.smoothRafByChat[chatId] = null;
    }
    const list = this.messagesByChat[chatId] || [];
    const targetId = this.activeSmoothMsgByChat[chatId];
    const msg = targetId ? list.find((m) => m.id === targetId) : list[list.length - 1];
    if (msg && msg.role === 'assistant') {
      const rBuf = this.pendingReasoningByChat[chatId] || '';
      const tBuf = this.pendingTextByChat[chatId] || '';
      if (rBuf.length > 0) {
        this.applyReasoningChunk(chatId, msg, rBuf);
      }
      if (tBuf.length > 0) {
        this.applyContentChunk(chatId, msg, tBuf);
      }
    }
    this.pendingReasoningByChat[chatId] = '';
    this.pendingTextByChat[chatId] = '';
  }

  public async createSession(title = 'New task', projectPath?: string): Promise<string> {
    const id = `chat-${Date.now()}-${Math.random().toString(36).substring(2, 7)}`;
    const newSession: ChatSession = {
      id,
      title,
      createdAt: Date.now(),
      updatedAt: Date.now(),
      pinned: false,
      archived: false,
      isDraft: true,
      providerId: settingsStore.data.activeProvider || undefined,
      modelId: settingsStore.data.activeModel || undefined,
      projectPath: projectPath || workspaceStore.currentPath || undefined,
      hasError: false,
      hasUnreadCompletion: false,
      messageCount: 0,
      preview: ''
    };

    this.showArchived = false;
    this.sessions = [newSession, ...this.sessions.filter((s) => !s.isDraft)];
    this.messagesByChat[id] = [];
    this.contextTelemetryByChat[id] = {
      usedTokens: 0,
      contextWindow: settingsStore.currentModelCapabilities.contextWindow || 128000,
      cachedTokens: 0,
      cacheHitRate: 0
    };
    this.activeChatId = id;
    return id;
  }

  public async selectSession(id: string): Promise<void> {
    this.activeChatId = id;
    const session = this.sessions.find((s) => s.id === id);
    if (session?.projectPath && workspaceStore.currentPath !== session.projectPath) {
      workspaceStore.setWorkspaceDir(session.projectPath).catch(() => {});
    }
    if (session?.providerId && session?.modelId) {
      settingsStore.setActiveModel(session.providerId, session.modelId);
    }
    if (session && session.hasUnreadCompletion) {
      session.hasUnreadCompletion = false;
      this.saveSessions(id);
    }

    this.refreshGoalState(id);
    if (this.messagesByChat[id]) {
      return;
    }

    if (typeof localStorage !== 'undefined') {
      const saved = localStorage.getItem(`omni_chat_msgs_${id}`);
      if (saved) {
        try {
          const parsedMsgs = JSON.parse(saved);
          if (Array.isArray(parsedMsgs)) {
            let changed = false;
            for (const m of parsedMsgs) {
              if (m.streaming && !this.streamingChats[id]) {
                m.streaming = false;
                m.paused = true;
                m.statusText = 'Paused';
                const lastActive = Math.max(
                  m.startedAt || 0,
                  session?.updatedAt || 0,
                  ...(m.steps || []).map((st: any) => st.finishedAt || st.startedAt || 0),
                  ...(m.toolCalls || []).map((tc: any) => tc.finishedAt || tc.startedAt || 0)
                ) || m.startedAt || Date.now();
                m.finishedAt = m.finishedAt || lastActive;
                this.finalizeRunningTools(m, 'paused', undefined, m.finishedAt);
                changed = true;
              }
            }
            this.messagesByChat[id] = parsedMsgs;
            if (changed) {
              this.saveChatMessagesToStorage(id, parsedMsgs);
            }
            return;
          }
        } catch {}
      }
    }
    this.messagesByChat[id] = [];
  }

  public async deleteSession(id: string): Promise<void> {
    if (this.streamingChats[id]) {
      await this.stopStreaming(id);
    }
    this.sessions = this.sessions.filter((s) => s.id !== id);
    delete this.messagesByChat[id];
    delete this.streamingChats[id];
    delete this.pendingQueueByChat[id];
    delete this.draftTextByChat[id];
    delete this.draftAttachmentsByChat[id];
    this.persistPendingQueues();
    this.persistDrafts();
    delete this.todosByChat[id];
    delete this.contextTelemetryByChat[id];
    delete this.reasoningElapsedByChat[id];
    delete this.streamTokenCountByChat[id];

    if (typeof localStorage !== 'undefined') {
      localStorage.removeItem(`omni_chat_msgs_${id}`);
    }
    try {
      await invoke('delete_chat', { id });
    } catch {}

    if (this.activeChatId === id) {
      const next = this.sessions.find((s) => !s.archived && !s.isDraft);
      if (next) {
        await this.selectSession(next.id);
      } else {
        this.activeChatId = null;
      }
    }
    await this.saveSessions();
  }

  public async togglePinSession(id: string): Promise<void> {
    const session = this.sessions.find((s) => s.id === id);
    if (session) {
      session.pinned = !session.pinned;
      await this.saveSessions(id);
    }
  }

  public async toggleArchiveSession(id: string): Promise<void> {
    const session = this.sessions.find((s) => s.id === id);
    if (session) {
      session.archived = !session.archived;
      if (session.archived) {
        session.pinned = false;
        if (this.activeChatId === id) {
          const next = this.sessions.find((s) => !s.archived && !s.isDraft);
          if (next) {
            await this.selectSession(next.id);
          } else {
            this.activeChatId = null;
          }
        }
      }
      await this.saveSessions(id);
    }
  }

  public async renameSession(id: string, newTitle: string): Promise<void> {
    const session = this.sessions.find((s) => s.id === id);
    if (session && newTitle.trim()) {
      session.title = newTitle.trim();
      await this.saveSessions(id);
    }
  }

  private async runAssistantStream(chatId: string, prompt: string, forceExecute = false): Promise<void> {
    const assistantMsgId = `asst-${Date.now()}-${Math.random().toString(36).slice(2, 5)}`;
    const assistantMessage: ChatMessage = {
      id: assistantMsgId,
      chatId,
      role: 'assistant',
      content: '',
      modelId: settingsStore.data.activeModel || undefined,
      reasoningContent: '',
      toolCalls: [],
      steps: [],
      statusText: 'Working',
      startedAt: Date.now(),
      createdAt: Date.now(),
      streaming: true
    };

    this.messagesByChat[chatId] = [...(this.messagesByChat[chatId] || []), assistantMessage];
    this.streamingChats = { ...this.streamingChats, [chatId]: true };
    this.streamTokenCountByChat[chatId] = 0;
    this.reasoningElapsedByChat[chatId] = 0;

    const startTime = Date.now();
    this.reasoningTimers[chatId] = window.setInterval(() => {
      this.reasoningElapsedByChat[chatId] = Date.now() - startTime;
    }, 100);

    let failed = false;
    try {
      await this.simulateOrExecuteAgentStream(chatId, assistantMsgId, prompt, false, forceExecute);
    } catch (err: any) {
      failed = true;
      const s = this.sessions.find((x) => x.id === chatId);
      if (s) s.hasError = true;
      const list = this.messagesByChat[chatId] || [];
      const idx = list.findIndex((m) => m.id === assistantMsgId);
      if (idx !== -1) {
        const rawErr = err instanceof Error ? err.message : typeof err === 'string' ? err : 'Connection or streaming error';
        list[idx].error = rawErr;
        list[idx].streaming = false;
        list[idx].paused = false;
        this.finalizeRunningTools(list[idx], 'error', rawErr);
      }
    } finally {
      if (this.reasoningTimers[chatId]) {
        clearInterval(this.reasoningTimers[chatId]);
        delete this.reasoningTimers[chatId];
      }
      if (this.streamSaveTimers[chatId]) {
        clearTimeout(this.streamSaveTimers[chatId]);
        delete this.streamSaveTimers[chatId];
      }
      this.streamingChats = { ...this.streamingChats, [chatId]: false };
      const list = this.messagesByChat[chatId] || [];
      const idx = list.findIndex((m) => m.id === assistantMsgId);
      if (idx !== -1) {
        list[idx].streaming = false;
        this.finalizeRunningTools(list[idx], failed ? 'error' : 'success');
      }
      if (!failed && this.activeChatId !== chatId) {
        const s = this.sessions.find((x) => x.id === chatId);
        if (s) s.hasUnreadCompletion = true;
      }
      await this.saveSessions(chatId);
      await this.processNextInQueue(chatId);
    }
  }

  public async sendMessage(
    text: string,
    options: { reasoningLevel?: ReasoningLevel; files?: string[]; attachments?: ChatAttachment[]; targetChatId?: string; forceExecute?: boolean } = {}
  ): Promise<void> {
    const hasAttachments = Boolean(options.attachments && options.attachments.length > 0);
    if (!text.trim() && !hasAttachments) {
      return;
    }

    const titleSeed = text.trim() || options.attachments?.[0]?.name || 'New task';
    let chatId = options.targetChatId || this.activeChatId;
    if (!chatId) {
      chatId = await this.createSession(titleSeed.slice(0, 36), workspaceStore.currentPath || undefined);
    }

    const userMsgId = `user-${Date.now()}-${Math.random().toString(36).slice(2, 5)}`;
    const userMessage: ChatMessage = {
      id: userMsgId,
      chatId,
      role: 'user',
      content: text.trim(),
      attachments: options.attachments,
      createdAt: Date.now(),
      modifiedFiles: options.files
    };

    const existingList = (this.messagesByChat[chatId] || []).map((m) =>
      m.paused ? { ...m, paused: false, statusText: m.statusText === 'Paused' ? undefined : m.statusText } : m
    );
    this.messagesByChat[chatId] = [...existingList, userMessage];

    const currentSession = this.sessions.find((s) => s.id === chatId);
    if (currentSession) {
      if (!currentSession.projectPath && workspaceStore.currentPath) {
        currentSession.projectPath = workspaceStore.currentPath;
      }
      currentSession.isDraft = false;
      currentSession.updatedAt = Date.now();
      currentSession.messageCount = this.messagesByChat[chatId].length;
      currentSession.preview = text.trim().slice(0, 80);
      currentSession.hasError = false;
      currentSession.isPaused = false;
      currentSession.hasUnreadCompletion = false;
      if (this.messagesByChat[chatId].length <= 2 && currentSession.title.startsWith('New')) {
        currentSession.title = titleSeed.slice(0, 36);
      }
    }

    await this.saveSessions(chatId).catch(() => {});
    await this.runAssistantStream(chatId, text.trim(), Boolean(options.forceExecute));
  }

  private async simulateOrExecuteAgentStream(
    chatId: string,
    messageId: string,
    prompt: string,
    resume = false,
    forceExecute = false
  ): Promise<void> {
    const prov = settingsStore.currentProvider;
    const modelCaps = settingsStore.currentModelCapabilities;
    const detectedInfo = prov?.modelCapabilities?.[settingsStore.data.activeModel];
    const sess = this.sessions.find((s) => s.id === chatId);

    await new Promise<void>(async (resolve, reject) => {
      let settled = false;
      const done = () => {
        if (!settled) {
          settled = true;
          this.resolveByChat[chatId] = null;
          resolve();
        }
      };
      this.resolveByChat[chatId] = done;

      const onEvent = new Channel<any>();
      onEvent.onmessage = (event) => {
        this.handleStreamEvent(event, chatId, messageId);
        if (event?.type === 'done' || event?.type === 'error') {
          done();
        }
      };

      try {
        const list = this.messagesByChat[chatId] || [];
        const priorHistory = list
          .slice(0, Math.max(0, list.length - 2))
          .filter((m) => m.role === 'user' || m.role === 'assistant')
          .map((m) => {
            let text = m.content.trim();
            if (!text && m.steps && m.steps.length > 0) {
              text = m.steps
                .filter((s) => s.kind === 'message')
                .map((s) => (s as { kind: 'message'; text: string }).text)
                .join('\n')
                .trim();
            }
            return { role: m.role, content: text };
          })
          .filter((m) => m.content.length > 0);

        await invoke('start_chat_stream', {
          chatId,
          prompt,
          modelId: settingsStore.data.activeModel,
          reasoningEffort: settingsStore.data.reasoningLevel,
          providerName: prov?.name || null,
          baseUrl: prov?.baseUrl || null,
          apiKey: prov?.apiKey || null,
          apiProtocol: prov?.apiProtocol || 'auto',
          proxyEnabled: Boolean(prov?.proxyEnabled),
          proxyUrl: prov?.proxyUrl || null,
          contextWindow: detectedInfo?.contextWindow || modelCaps.contextWindow || null,
          maxOutputTokens: detectedInfo?.maxOutputTokens || null,
          supportsReasoning: detectedInfo?.supportsReasoning ?? modelCaps.supportsReasoning,
          workspaceDir: sess?.projectPath || workspaceStore.currentPath || null,
          planMode: !forceExecute && /(?:^|\s)\/plan(?:\s|$)/i.test(prompt.trim()),
          resume,
          history: priorHistory,
          onEvent
        });
      } catch (err) {
        if (!settled) {
          settled = true;
          this.resolveByChat[chatId] = null;
          reject(err);
        }
      }
    });
  }

  public async stopStreaming(targetChatId?: string): Promise<void> {
    const cid = targetChatId || this.activeChatId;
    if (!cid) return;
    this.flushSmoothBuffers(cid);
    this.streamingChats = { ...this.streamingChats, [cid]: false };
    const resFn = this.resolveByChat[cid];
    if (resFn) {
      resFn();
    }
    if (this.reasoningTimers[cid]) {
      clearInterval(this.reasoningTimers[cid]);
      delete this.reasoningTimers[cid];
    }
    const list = this.messagesByChat[cid] || [];
    for (const msg of list) {
      if (msg.streaming) {
        msg.streaming = false;
        msg.paused = false;
        msg.statusText = undefined;
        msg.finishedAt = Date.now();
        if (!msg.error) {
          msg.error = 'User canceled execution';
        }
        this.finalizeRunningTools(msg, 'cancelled', 'User canceled execution');
      }
    }
    try {
      await invoke('cancel_stream', { sessionId: cid });
    } catch {}
    await this.saveSessions(cid);
  }

  public async continueAssistantMessage(messageId?: string): Promise<void> {
    const chatId = this.activeChatId;
    if (!chatId) return;
    const list = this.messagesByChat[chatId] || [];
    let targetIdx = -1;
    if (messageId) {
      targetIdx = list.findIndex((m) => m.id === messageId);
    } else {
      for (let i = list.length - 1; i >= 0; i--) {
        if (list[i].role === 'assistant') {
          targetIdx = i;
          break;
        }
      }
    }
    if (targetIdx === -1) return;

    const targetMsg = list[targetIdx];
    if (this.streamingChats[chatId] && targetMsg.streaming) return;
    if (this.streamingChats[chatId]) {
      this.streamingChats = { ...this.streamingChats, [chatId]: false };
    }
    let prevUserPrompt = '';
    for (let i = targetIdx - 1; i >= 0; i--) {
      if (list[i].role === 'user') {
        prevUserPrompt = list[i].content;
        break;
      }
    }
    if (!prevUserPrompt) return;

    targetMsg.error = undefined;
    targetMsg.paused = false;
    targetMsg.streaming = true;
    targetMsg.statusText = 'Resuming';
    targetMsg.retryReason = undefined;
    targetMsg.modelId = settingsStore.data.activeModel || targetMsg.modelId;

    const prevElapsed = (targetMsg.finishedAt && targetMsg.startedAt && targetMsg.finishedAt >= targetMsg.startedAt)
      ? targetMsg.finishedAt - targetMsg.startedAt
      : (targetMsg.toolCalls?.reduce((acc, tc) => acc + (tc.elapsedMs || 0), 0) || 0);

    targetMsg.startedAt = Date.now() - prevElapsed;
    targetMsg.finishedAt = undefined;

    const currentSession = this.sessions.find((s) => s.id === chatId);
    if (currentSession) {
      currentSession.hasError = false;
      currentSession.isPaused = false;
    }

    this.streamingChats = { ...this.streamingChats, [chatId]: true };
    this.streamTokenCountByChat[chatId] = 0;
    const baseElapsed = prevElapsed;
    const startTime = Date.now() - baseElapsed;
    this.reasoningTimers[chatId] = window.setInterval(() => {
      this.reasoningElapsedByChat[chatId] = Date.now() - startTime;
    }, 100);

    let failed = false;
    try {
      await this.simulateOrExecuteAgentStream(chatId, targetMsg.id, prevUserPrompt, true);
    } catch (err: any) {
      failed = true;
      if (currentSession) currentSession.hasError = true;
      const rawErr = err instanceof Error ? err.message : typeof err === 'string' ? err : 'Connection or streaming error';
      targetMsg.error = rawErr;
      targetMsg.streaming = false;
      targetMsg.paused = false;
      this.finalizeRunningTools(targetMsg, 'error', rawErr);
    } finally {
      if (this.reasoningTimers[chatId]) {
        clearInterval(this.reasoningTimers[chatId]);
        delete this.reasoningTimers[chatId];
      }
      if (this.streamSaveTimers[chatId]) {
        clearTimeout(this.streamSaveTimers[chatId]);
        delete this.streamSaveTimers[chatId];
      }
      this.streamingChats = { ...this.streamingChats, [chatId]: false };
      targetMsg.streaming = false;
      this.finalizeRunningTools(targetMsg, failed ? 'error' : 'success');
      await this.saveSessions(chatId);
      await this.processNextInQueue(chatId);
    }
  }

  public async regenerateAssistantMessage(messageId?: string): Promise<void> {
    const chatId = this.activeChatId;
    if (!chatId || this.streamingChats[chatId]) return;
    const list = this.messagesByChat[chatId] || [];
    let targetIdx = -1;
    if (messageId) {
      targetIdx = list.findIndex((m) => m.id === messageId);
    } else {
      for (let i = list.length - 1; i >= 0; i--) {
        if (list[i].role === 'assistant') {
          targetIdx = i;
          break;
        }
      }
    }
    if (targetIdx === -1) return;

    let prevUserPrompt = '';
    for (let i = targetIdx - 1; i >= 0; i--) {
      if (list[i].role === 'user') {
        prevUserPrompt = list[i].content;
        break;
      }
    }
    if (!prevUserPrompt) return;

    this.messagesByChat[chatId] = list.slice(0, targetIdx);
    const currentSession = this.sessions.find((s) => s.id === chatId);
    if (currentSession) {
      currentSession.hasError = false;
    }
    await this.runAssistantStream(chatId, prevUserPrompt);
  }

  public async editUserMessage(messageId: string, newText: string, rollbackFiles: boolean): Promise<void> {
    const chatId = this.activeChatId;
    if (!chatId || this.streamingChats[chatId]) return;
    const list = this.messagesByChat[chatId] || [];
    const idx = list.findIndex((m) => m.id === messageId);
    if (idx === -1 || !newText.trim()) return;

    const targetMsg = list[idx];
    if (rollbackFiles && targetMsg.snapshotId) {
      await this.rollbackSnapshot(targetMsg.snapshotId);
    }

    targetMsg.content = newText.trim();
    this.messagesByChat[chatId] = list.slice(0, idx + 1);
    const currentSession = this.sessions.find((s) => s.id === chatId);
    if (currentSession) {
      currentSession.hasError = false;
      currentSession.preview = newText.trim().slice(0, 80);
    }
    await this.runAssistantStream(chatId, newText.trim());
  }

  public async rollbackSnapshot(snapshotId: string): Promise<void> {
    try {
      await invoke('rollback_file_snapshot', { snapshotId });
    } catch {}
  }

  public enqueueMessage(text: string, priority: 'normal' | 'intervention' = 'normal'): void {
    const cid = this.activeChatId;
    if (!cid || !text.trim()) return;
    const item: PendingQueueItem = {
      id: `queue-${Date.now()}-${Math.random().toString(36).substring(2, 6)}`,
      text: text.trim(),
      createdAt: Date.now(),
      priority
    };
    const cur = this.pendingQueueByChat[cid] || [];
    this.pendingQueueByChat[cid] = priority === 'intervention' ? [item, ...cur] : [...cur, item];
    this.persistPendingQueues();
  }

  public dequeueMessage(id: string): void {
    const cid = this.activeChatId;
    if (!cid) return;
    this.pendingQueueByChat[cid] = (this.pendingQueueByChat[cid] || []).filter((item) => item.id !== id);
    this.persistPendingQueues();
  }

  public updatePendingItem(id: string, text: string): void {
    const cid = this.activeChatId;
    if (!cid) return;
    const item = (this.pendingQueueByChat[cid] || []).find((i) => i.id === id);
    if (item && text.trim()) {
      item.text = text.trim();
      this.persistPendingQueues();
    }
  }

  public async sendNow(id: string): Promise<void> {
    const cid = this.activeChatId;
    if (!cid) return;
    const item = (this.pendingQueueByChat[cid] || []).find((i) => i.id === id);
    if (!item) return;
    this.dequeueMessage(id);
    if (this.streamingChats[cid]) await this.stopStreaming(cid);
    await this.sendMessage(item.text, { targetChatId: cid });
  }

  public async processNextInQueue(targetChatId?: string): Promise<void> {
    const cid = targetChatId || this.activeChatId;
    if (!cid || this.streamingChats[cid]) return;
    const q = this.pendingQueueByChat[cid] || [];
    if (q.length === 0) return;
    const next = q[0];
    this.pendingQueueByChat[cid] = q.slice(1);
    this.persistPendingQueues();
    await this.sendMessage(next.text, { targetChatId: cid });
  }

  public handleStreamEvent(event: any, targetChatId?: string, targetMessageId?: string): void {
    if (!event) return;
    const cid = targetChatId || this.activeChatId;
    if (!cid) return;
    const list = this.messagesByChat[cid] || [];
    const currentAssistantMsg = targetMessageId
      ? list.find((m) => m.id === targetMessageId)
      : list[list.length - 1];
    if (!currentAssistantMsg || currentAssistantMsg.role !== 'assistant') return;

    if (!currentAssistantMsg.steps) currentAssistantMsg.steps = [];
    if (!currentAssistantMsg.toolCalls) currentAssistantMsg.toolCalls = [];

    if (event.type === 'retry_notice') {
      const attempt = event.attempt || 1;
      const maxA = Math.max(5, attempt);
      currentAssistantMsg.statusText = `Retrying (${attempt}/${maxA})...`;
      currentAssistantMsg.retryReason = event.reason || '';
      return;
    }

    if (event.type === 'context_telemetry') {
      this.contextTelemetryByChat[cid] = {
        usedTokens: event.usedTokens || 0,
        contextWindow: event.contextWindow || 128000,
        cachedTokens: event.cachedTokens || 0,
        cacheHitRate: event.cacheHitRate || 0
      };
      return;
    }

    if (event.type === 'plan_approval_required') {
      this.pendingPlanByChat[cid] = { summary: event.planSummary || '' };
      return;
    }

    if (event.type === 'goal_status_updated') {
      if (event.status === 'cleared') {
        this.goalByChat[cid] = null;
      } else {
        this.goalByChat[cid] = {
          status: event.status || 'active',
          condition: event.condition || '',
          turn: event.turn || 1,
          maxTurns: event.maxTurns || 100,
          reason: event.reason || null,
          elapsedSecs: event.elapsedSecs || 0
        };
      }
      return;
    }

    if (event.type === 'task_board_updated') {
      if (this.todoHideTimers[cid]) {
        clearTimeout(this.todoHideTimers[cid]);
        delete this.todoHideTimers[cid];
      }
      const nextTodos = Array.isArray(event.items) ? event.items : [];
      currentAssistantMsg.todos = nextTodos;
      this.todosByChat[cid] = nextTodos;
      return;
    }

    if (event.type === 'context_compacted') {
      currentAssistantMsg.steps = [
        ...currentAssistantMsg.steps,
        {
          kind: 'compact',
          id: `cmp-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
          beforeTokens: event.beforeTokens || 0,
          afterTokens: event.afterTokens || 0,
          summary: event.summary || ''
        }
      ];
      return;
    }

    if (event.type === 'delta') {
      const chunk = event.content ?? event.text ?? '';
      if (chunk) {
        this.activeSmoothMsgByChat[cid] = currentAssistantMsg.id;
        this.pendingTextByChat[cid] = (this.pendingTextByChat[cid] || '') + chunk;
        this.startSmoothFlush(cid);
      }
    } else if (event.type === 'reasoning_delta' || event.type === 'reasoning') {
      const chunk = event.content ?? event.text ?? '';
      if (chunk) {
        this.activeSmoothMsgByChat[cid] = currentAssistantMsg.id;
        this.pendingReasoningByChat[cid] = (this.pendingReasoningByChat[cid] || '') + chunk;
        this.startSmoothFlush(cid);
      }
    } else if (event.type === 'retroactive_reasoning_move') {
      const text = event.reasoningContent || '';
      currentAssistantMsg.reasoningContent = text;
      if (text) {
        currentAssistantMsg.steps = [
          ...currentAssistantMsg.steps,
          {
            kind: 'thought',
            id: `th-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
            text,
            startedAt: Date.now(),
            finishedAt: Date.now()
          }
        ];
      }
    } else if (event.type === 'tool_call_delta') {
      this.flushSmoothBuffers(cid);
      if (!event.id) return;
      currentAssistantMsg.statusText = undefined;
      const existingIdx = currentAssistantMsg.toolCalls.findIndex((c) => c.toolCallId === event.id);
      if (existingIdx === -1) {
        const record = formatToolExecution({
          id: event.id,
          name: event.name || 'tool',
          arguments: event.argumentsDelta || '',
          status: 'running',
          startedAt: Date.now()
        });
        currentAssistantMsg.toolCalls = [...currentAssistantMsg.toolCalls, record];
        currentAssistantMsg.steps = [...currentAssistantMsg.steps, { kind: 'tool', record }];
      }
    } else if (event.type === 'tool_call_complete' || event.type === 'tool_start') {
      this.flushSmoothBuffers(cid);
      currentAssistantMsg.statusText = undefined;
      const existingTool = currentAssistantMsg.toolCalls.find((c) => c.toolCallId === event.id);
      const startedAt = existingTool?.startedAt || Date.now();
      const record = formatToolExecution({
        id: event.id,
        name: event.name,
        arguments: event.arguments,
        status: 'running',
        startedAt
      });
      if (existingTool) {
        currentAssistantMsg.toolCalls = currentAssistantMsg.toolCalls.map((c) =>
          c.toolCallId === event.id ? record : c
        );
        currentAssistantMsg.steps = currentAssistantMsg.steps.map((s) =>
          s.kind === 'tool' && s.record.toolCallId === event.id ? { kind: 'tool', record } : s
        );
      } else {
        currentAssistantMsg.toolCalls = [...currentAssistantMsg.toolCalls, record];
        currentAssistantMsg.steps = [...currentAssistantMsg.steps, { kind: 'tool', record }];
      }
      this.saveSessions(cid);
    } else if (event.type === 'tool_output_delta') {
      const chunk = event.chunk || '';
      if (!chunk) return;
      currentAssistantMsg.toolCalls = currentAssistantMsg.toolCalls.map((tc) => {
        if (tc.toolCallId !== event.id) return tc;
        const nextOut = (tc.output || '') + chunk;
        return {
          ...tc,
          output: nextOut,
          terminalData: tc.terminalData
            ? { ...tc.terminalData, stdout: (tc.terminalData.stdout || '') + chunk }
            : undefined
        };
      });
      currentAssistantMsg.steps = currentAssistantMsg.steps.map((s) => {
        if (s.kind !== 'tool' || s.record.toolCallId !== event.id) return s;
        const nextOut = (s.record.output || '') + chunk;
        return {
          kind: 'tool',
          record: {
            ...s.record,
            output: nextOut,
            terminalData: s.record.terminalData
              ? { ...s.record.terminalData, stdout: (s.record.terminalData.stdout || '') + chunk }
              : undefined
          }
        };
      });
    } else if (event.type === 'tool_end') {
      const now = Date.now();
      const isErr = event.success === false || Boolean(event.error);
      currentAssistantMsg.toolCalls = currentAssistantMsg.toolCalls.map((tc) => {
        if (tc.toolCallId !== event.id) return tc;
        return {
          ...tc,
          status: isErr ? 'error' : 'success',
          finishedAt: now,
          elapsedMs: typeof event.elapsedMs === 'number' ? event.elapsedMs : now - tc.startedAt,
          output: event.output,
          error: event.error
        };
      });
      currentAssistantMsg.steps = currentAssistantMsg.steps.map((s) => {
        if (s.kind !== 'tool' || s.record.toolCallId !== event.id) return s;
        return {
          kind: 'tool',
          record: {
            ...s.record,
            status: isErr ? 'error' : 'success',
            finishedAt: now,
            elapsedMs: typeof event.elapsedMs === 'number' ? event.elapsedMs : now - s.record.startedAt,
            output: event.output,
            error: event.error
          }
        };
      });
      this.saveSessions(cid);
    } else if (event.type === 'done') {
      this.flushSmoothBuffers(cid);
      this.streamingChats = { ...this.streamingChats, [cid]: false };
      if (this.reasoningTimers[cid]) {
        clearInterval(this.reasoningTimers[cid]);
        delete this.reasoningTimers[cid];
      }
      if (event.usage) {
        currentAssistantMsg.tokenUsage = {
          promptTokens: event.usage.prompt_tokens || event.usage.promptTokens || 0,
          completionTokens: event.usage.completion_tokens || event.usage.completionTokens || 0,
          reasoningTokens: event.usage.reasoning_tokens || event.usage.reasoningTokens
        };
      }
      currentAssistantMsg.finishedAt = Date.now();
      currentAssistantMsg.statusText = undefined;
      currentAssistantMsg.retryReason = undefined;
      currentAssistantMsg.modelId = settingsStore.data.activeModel || currentAssistantMsg.modelId;
      currentAssistantMsg.streaming = false;
      currentAssistantMsg.paused = false;
      this.finalizeRunningTools(currentAssistantMsg, 'success');
      const todos = this.todosByChat[cid] || [];
      if (todos.length > 0 && todos.every((t) => t.status === 'completed')) {
        this.todoHideTimers[cid] = setTimeout(() => {
          this.todosByChat[cid] = [];
        }, 3500);
      }
      this.saveSessions(cid);
    } else if (event.type === 'error') {
      this.flushSmoothBuffers(cid);
      this.streamingChats = { ...this.streamingChats, [cid]: false };
      if (this.reasoningTimers[cid]) {
        clearInterval(this.reasoningTimers[cid]);
        delete this.reasoningTimers[cid];
      }
      currentAssistantMsg.finishedAt = Date.now();
      currentAssistantMsg.statusText = undefined;
      currentAssistantMsg.error = event.message || event.error || 'Stream error';
      currentAssistantMsg.streaming = false;
      currentAssistantMsg.paused = false;
      const isCancel = currentAssistantMsg.error === 'User canceled execution';
      this.finalizeRunningTools(currentAssistantMsg, isCancel ? 'cancelled' : 'error', currentAssistantMsg.error);
      const s = this.sessions.find((x) => x.id === cid);
      if (s) {
        s.hasError = !isCancel;
        s.isPaused = false;
      }
      this.saveSessions(cid);
    }
  }
}

export const chatStore = new ChatStore();
