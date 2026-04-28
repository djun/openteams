import { useState, useMemo, useCallback, useEffect, useRef } from 'react';
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import {
  ArrowClockwiseIcon,
  ArrowUpIcon,
  CaretDownIcon,
  FunnelIcon,
  PlayIcon,
  PauseIcon,
  StopIcon,
} from '@phosphor-icons/react';
import { chatApi } from '@/lib/api';
import { cn } from '@/lib/utils';
import { ChatMarkdown } from '@/components/ui-new/primitives/conversation/ChatMarkdown';
import { WorkflowGraphBoard } from './WorkflowGraphBoard';
import {
  parseWorkflowTranscriptMeta,
  toWorkflowFinalReviewAction,
} from './WorkflowFinalReviewCard';
import {
  canPauseWorkflowExecution,
  canResumeWorkflowExecution,
  isRetryableWorkflowStepStatus,
  isWorkflowExecutionRecompiling,
} from './workflowControlContract';

type WorkflowCardStep = {
  id: string;
  step_key: string;
  title: string;
  step_type: string;
  status: string;
  agent_name?: string | null;
  summary_text?: string | null;
  content?: string | null;
};

type WorkflowCardAgent = {
  session_agent_id: string;
  workflow_agent_session_id?: string | null;
  agent_id: string;
  name: string;
};

type WorkflowCardNode = {
  id: string;
  position: { x: number; y: number };
  data: {
    stepType: string;
    title: string;
    instructions: string;
    agentId?: string | null;
    status?: string | null;
  };
};

type WorkflowCardEdge = {
  id: string;
  source: string;
  target: string;
};

export type WorkflowWindowProjection = {
  execution_id?: string | null;
  plan_id?: string;
  title: string;
  goal: string;
  state: string;
  execution_status: string;
  error_message?: string | null;
  completed_step_count: number;
  total_step_count: number;
  result_summary?: string | null;
  outputs: string[];
  steps: WorkflowCardStep[];
  agents?: WorkflowCardAgent[];
  plan: {
    nodes: WorkflowCardNode[];
    edges: WorkflowCardEdge[];
    viewport?: { x?: number; y?: number; zoom?: number };
  };
  validation_errors?: string | null;
};

type WorkflowTranscriptEntry = {
  id: string;
  step_id?: string | null;
  step_key?: string | null;
  workflow_agent_session_id?: string | null;
  agent_name?: string | null;
  message_type: 'system' | 'agent' | 'user' | 'control';
  entry_type: string;
  content: string;
  meta_json?: string | null;
  created_at: string;
};

type WorkflowRuntimeMessage = {
  id: string;
  executionId: string;
  workflowAgentSessionId: string | null;
  stepId: string;
  stepKey: string;
  agentId: string;
  agentName: string;
  streamType: 'assistant' | 'thinking' | 'error';
  content: string;
  createdAt: string;
};

type WorkflowTranscriptSummaryPayload = {
  summary?: string;
  content?: string;
  outputs?: string[];
};

const WORKFLOW_TERMINAL_STEP_STATUSES = new Set([
  'completed',
  'failed',
  'interrupted',
  'skipped',
  'cancelled',
]);

const WORKFLOW_FAILURE_STEP_STATUSES = new Set([
  'failed',
  'interrupted',
  'cancelled',
]);

function mergeAndSortTranscriptEntries(
  primary: WorkflowTranscriptEntry[],
  secondary: WorkflowTranscriptEntry[]
): WorkflowTranscriptEntry[] {
  const mergedMap = new Map<string, WorkflowTranscriptEntry>();

  for (const entry of primary) {
    mergedMap.set(entry.id, entry);
  }
  for (const entry of secondary) {
    mergedMap.set(entry.id, entry);
  }

  return [...mergedMap.values()].sort((left, right) => {
    const leftAt = Date.parse(left.created_at);
    const rightAt = Date.parse(right.created_at);
    return (
      (Number.isNaN(leftAt) ? 0 : leftAt) -
      (Number.isNaN(rightAt) ? 0 : rightAt)
    );
  });
}

function parseTranscriptSummaryPayload(
  metaJson: string | null | undefined
): WorkflowTranscriptSummaryPayload | null {
  if (!metaJson) {
    return null;
  }

  try {
    const parsed = JSON.parse(metaJson) as unknown;
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
      return null;
    }

    const payload = parsed as Record<string, unknown>;
    return {
      summary:
        typeof payload.summary === 'string' ? payload.summary : undefined,
      content:
        typeof payload.content === 'string' ? payload.content : undefined,
      outputs: Array.isArray(payload.outputs)
        ? payload.outputs.filter(
            (item): item is string => typeof item === 'string'
          )
        : undefined,
    };
  } catch {
    return null;
  }
}

function getTranscriptMarkdown(entry: WorkflowTranscriptEntry): string | null {
  const payload = parseTranscriptSummaryPayload(entry.meta_json);
  if (payload?.content) {
    const content = payload.content.trim();
    return content.length > 0 ? content : null;
  }

  if (
    (entry.entry_type === 'message' && entry.message_type === 'agent') ||
    entry.entry_type === 'error'
  ) {
    const content = entry.content.trim();
    return content.length > 0 ? content : null;
  }

  return null;
}

type WorkflowTranscriptRenderItem =
  | {
      kind: 'entry';
      id: string;
      entry: WorkflowTranscriptEntry;
    }
  | {
      kind: 'thinking_group';
      id: string;
      entries: WorkflowTranscriptEntry[];
      workflowAgentSessionId?: string | null;
      stepId?: string | null;
      agentName?: string | null;
      createdAt: string;
      content: string;
    };

function buildWorkflowTranscriptRenderItems(
  entries: WorkflowTranscriptEntry[]
): WorkflowTranscriptRenderItem[] {
  const items: WorkflowTranscriptRenderItem[] = [];

  for (let index = 0; index < entries.length; index += 1) {
    const entry = entries[index];
    if (entry.entry_type !== 'thinking') {
      items.push({
        kind: 'entry',
        id: entry.id,
        entry,
      });
      continue;
    }

    const groupedEntries = [entry];
    let cursor = index + 1;

    while (cursor < entries.length) {
      const candidate = entries[cursor];
      if (
        candidate.entry_type !== 'thinking' ||
        candidate.workflow_agent_session_id !==
          entry.workflow_agent_session_id ||
        candidate.step_id !== entry.step_id ||
        candidate.agent_name !== entry.agent_name
      ) {
        break;
      }

      groupedEntries.push(candidate);
      cursor += 1;
    }

    items.push({
      kind: 'thinking_group',
      id: `thinking-group-${groupedEntries[0].id}`,
      entries: groupedEntries,
      workflowAgentSessionId: entry.workflow_agent_session_id,
      stepId: entry.step_id,
      agentName: entry.agent_name,
      createdAt: groupedEntries[0].created_at,
      content: groupedEntries.map((item) => item.content).join('\n'),
    });

    index = cursor - 1;
  }

  return items;
}

function resolveWorkflowTranscriptStepTitle(
  entry: Pick<WorkflowTranscriptEntry, 'step_id' | 'step_key'>,
  stepById: Map<string, WorkflowCardStep>,
  stepByKey: Map<string, WorkflowCardStep>
): string | null {
  const titleFromId = entry.step_id ? stepById.get(entry.step_id)?.title : null;
  if (titleFromId?.trim()) {
    return titleFromId.trim();
  }

  const titleFromKey = entry.step_key
    ? stepByKey.get(entry.step_key)?.title
    : null;
  return titleFromKey?.trim() || null;
}

function hasAgentTranscriptMessageForStep(
  entries: WorkflowTranscriptEntry[],
  stepId?: string | null,
  stepKey?: string | null
): boolean {
  return entries.some(
    (entry) =>
      entry.message_type === 'agent' &&
      entry.entry_type === 'message' &&
      ((stepId && entry.step_id === stepId) ||
        (stepKey && entry.step_key === stepKey))
  );
}

function buildStepContentTranscriptEntries(
  steps: WorkflowCardStep[],
  existingEntries: WorkflowTranscriptEntry[],
  resolveStepAgentSessionId: (step?: WorkflowCardStep | null) => string | null,
  selectedWorkflowAgentSessionId?: string | null
): WorkflowTranscriptEntry[] {
  let offset = 1;

  return steps
    .filter((step) => step.content?.trim())
    .filter((step) => {
      const workflowAgentSessionId = resolveStepAgentSessionId(step);
      if (!selectedWorkflowAgentSessionId) {
        return true;
      }
      return workflowAgentSessionId === selectedWorkflowAgentSessionId;
    })
    .filter(
      (step) =>
        !hasAgentTranscriptMessageForStep(
          existingEntries,
          step.id,
          step.step_key
        )
    )
    .map((step) => {
      const relatedEntries = existingEntries.filter(
        (entry) => entry.step_id === step.id || entry.step_key === step.step_key
      );
      const latestRelatedTimestamp = Math.max(
        ...relatedEntries.map((entry) => Date.parse(entry.created_at)),
        ...existingEntries.map((entry) => Date.parse(entry.created_at)),
        Date.now()
      );
      const createdAt = new Date(
        (Number.isFinite(latestRelatedTimestamp)
          ? latestRelatedTimestamp
          : Date.now()) + offset
      ).toISOString();
      offset += 1;

      return {
        id: `step-content-${step.id}`,
        step_id: step.id,
        step_key: step.step_key,
        workflow_agent_session_id: resolveStepAgentSessionId(step),
        agent_name: step.agent_name,
        message_type: 'agent' as const,
        entry_type: 'message',
        content: step.content!.trim(),
        meta_json: JSON.stringify({
          source: 'workflow_card_step_content',
        }),
        created_at: createdAt,
      };
    });
}

function workflowMessageTone(
  messageType: WorkflowTranscriptEntry['message_type'],
  entryType: string
) {
  if (entryType === 'approval_request') {
    return {
      rail: 'bg-[#F59E0B]',
      badge: 'bg-[#FFFBEB] text-[#92400E]',
      label: 'text-[#B45309]',
    };
  }
  if (entryType === 'permission_request') {
    return {
      rail: 'bg-[#2563EB]',
      badge: 'bg-[#EFF6FF] text-[#1D4ED8]',
      label: 'text-[#1D4ED8]',
    };
  }
  if (entryType === 'continue_confirmation') {
    return {
      rail: 'bg-[#16A34A]',
      badge: 'bg-[#ECFDF5] text-[#166534]',
      label: 'text-[#15803D]',
    };
  }
  if (entryType === 'input_request') {
    return {
      rail: 'bg-[#4F46E5]',
      badge: 'bg-[#EEF2FF] text-[#3730A3]',
      label: 'text-[#4338CA]',
    };
  }
  switch (messageType) {
    case 'agent':
      return {
        rail: 'bg-[#2563EB]',
        badge: 'bg-[#EFF6FF] text-[#1D4ED8]',
        label: 'text-[#1E3A8A]',
      };
    case 'user':
      return {
        rail: 'bg-[#16A34A]',
        badge: 'bg-[#F0FDF4] text-[#166534]',
        label: 'text-[#166534]',
      };
    case 'control':
      return {
        rail: 'bg-[#D97706]',
        badge: 'bg-[#FFF7ED] text-[#9A3412]',
        label: 'text-[#B45309]',
      };
    default:
      return {
        rail: 'bg-[#94A3B8]',
        badge: 'bg-[#F8FAFC] text-[#475569]',
        label: 'text-[#475569]',
      };
  }
}

const WORKFLOW_COMPOSER_MIN_HEIGHT = 104;
const WORKFLOW_COMPOSER_MAX_HEIGHT = 192;

// -----------------------------------------------------------------------
// Props
// -----------------------------------------------------------------------

export type WorkflowWindowProps = {
  sessionId?: string | null;
  projection: WorkflowWindowProjection;
  transcript?: WorkflowTranscriptEntry[];
  runtimeMessages?: WorkflowRuntimeMessage[];
  isOpen: boolean;
  onClose: () => void;
  onExecute?: (planId: string) => void;
  onPauseAll?: (executionId: string) => void;
  onResume?: (executionId: string) => void;
  onInterruptStep?: (stepId: string) => void;
  onStopStep?: (stepId: string) => void;
  onRetryStep?: (stepId: string) => void;
  onSubmitStepInput?: (stepId: string, inputText: string) => void;
  onApproval?: (
    stepId: string,
    action: string,
    transcriptId: string,
    inputText?: string
  ) => void;
  onResolveFinalReview?: (
    executionId: string,
    transcriptId: string,
    action: 'accepted' | 'rejected'
  ) => void;
  pendingActionId?: string | null;
};

function AgentSelector({
  agents,
  selectedAgentId,
  onSelect,
}: {
  agents: WorkflowCardAgent[];
  selectedAgentId: string | null;
  onSelect: (id: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const selected =
    agents.find(
      (agent) =>
        (agent.workflow_agent_session_id ?? agent.session_agent_id) ===
        selectedAgentId
    ) ?? agents[0];

  if (agents.length === 0 || !selected) {
    return null;
  }

  return (
    <div className="relative">
      <button
        type="button"
        onClick={() => setOpen((prev) => !prev)}
        className="flex items-center gap-1.5 rounded-lg border border-[#E2E8F0] bg-white px-3 py-1.5 text-xs font-medium text-[#334155] transition-colors hover:bg-[#F8FAFC]"
      >
        <FunnelIcon className="size-3.5" weight="bold" />
        {selected.name}
        <CaretDownIcon className="size-3" weight="bold" />
      </button>
      {open && (
        <div className="absolute left-0 top-full z-10 mt-1 min-w-[180px] rounded-lg border border-[#E2E8F0] bg-white py-1 shadow-lg">
          {agents.map((agent) => {
            const agentSessionId =
              agent.workflow_agent_session_id ?? agent.session_agent_id;
            return (
              <button
                type="button"
                key={agent.session_agent_id}
                onClick={() => {
                  onSelect(agentSessionId);
                  setOpen(false);
                }}
                className={cn(
                  'block w-full px-3 py-1.5 text-left text-xs transition-colors hover:bg-[#F1F5F9]',
                  selectedAgentId === agentSessionId &&
                    'font-bold text-[#1D4ED8]'
                )}
              >
                {agent.name}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}

// -----------------------------------------------------------------------
// Approval Card
// -----------------------------------------------------------------------

export function ApprovalCard({
  title,
  description,
  stepId,
  transcriptId,
  onApprove,
  onReject,
  disabled,
}: {
  title: string;
  description?: string;
  stepId: string;
  transcriptId: string;
  onApprove: (stepId: string, transcriptId: string) => void;
  onReject: (stepId: string, transcriptId: string) => void;
  disabled?: boolean;
}) {
  return (
    <div className="rounded-2xl border border-[#FDE68A] bg-[#FFFBEB] p-3">
      <div className="text-xs font-bold uppercase tracking-wider text-[#92400E]">
        Approval Required
      </div>
      <div className="mt-1 text-sm font-semibold text-[#0F172A]">{title}</div>
      {description && (
        <div className="mt-1 text-xs text-[#475569]">{description}</div>
      )}
      <div className="mt-2 flex gap-2">
        <button
          type="button"
          onClick={() => onApprove(stepId, transcriptId)}
          disabled={disabled}
          className="rounded-full bg-[#16A34A] px-3 py-1 text-xs font-semibold text-white hover:bg-[#15803D] disabled:opacity-50 transition-colors"
        >
          Approve
        </button>
        <button
          type="button"
          onClick={() => onReject(stepId, transcriptId)}
          disabled={disabled}
          className="rounded-full bg-[#DC2626] px-3 py-1 text-xs font-semibold text-white hover:bg-[#B91C1C] disabled:opacity-50 transition-colors"
        >
          Reject
        </button>
      </div>
    </div>
  );
}

// -----------------------------------------------------------------------
// Permission Request Card
// -----------------------------------------------------------------------

export function PermissionRequestCard({
  title,
  description,
  stepId,
  transcriptId,
  onGrant,
  onDeny,
  disabled,
}: {
  title: string;
  description?: string;
  stepId: string;
  transcriptId: string;
  onGrant: (stepId: string, transcriptId: string) => void;
  onDeny: (stepId: string, transcriptId: string) => void;
  disabled?: boolean;
}) {
  return (
    <div className="rounded-2xl border border-[#BFDBFE] bg-[#EFF6FF] p-3">
      <div className="text-xs font-bold uppercase tracking-wider text-[#1E40AF]">
        Permission Request
      </div>
      <div className="mt-1 text-sm font-semibold text-[#0F172A]">{title}</div>
      {description && (
        <div className="mt-1 text-xs text-[#475569]">{description}</div>
      )}
      <div className="mt-2 flex gap-2">
        <button
          type="button"
          onClick={() => onGrant(stepId, transcriptId)}
          disabled={disabled}
          className="rounded-full bg-[#2563EB] px-3 py-1 text-xs font-semibold text-white hover:bg-[#1D4ED8] disabled:opacity-50 transition-colors"
        >
          Grant
        </button>
        <button
          type="button"
          onClick={() => onDeny(stepId, transcriptId)}
          disabled={disabled}
          className="rounded-full border border-[#CBD5E1] bg-white px-3 py-1 text-xs font-semibold text-[#475569] hover:bg-[#F1F5F9] disabled:opacity-50 transition-colors"
        >
          Deny
        </button>
      </div>
    </div>
  );
}

// -----------------------------------------------------------------------
// Continue Confirmation Card
// -----------------------------------------------------------------------

export function ContinueConfirmationCard({
  message,
  stepId,
  transcriptId,
  onContinue,
  disabled,
}: {
  message: string;
  stepId: string;
  transcriptId: string;
  onContinue: (stepId: string, transcriptId: string) => void;
  disabled?: boolean;
}) {
  return (
    <div className="rounded-2xl border border-[#D1FAE5] bg-[#ECFDF5] p-3">
      <div className="text-xs font-bold uppercase tracking-wider text-[#15803D]">
        Continue?
      </div>
      <div className="mt-1 text-sm text-[#166534]">{message}</div>
      <div className="mt-2">
        <button
          type="button"
          onClick={() => onContinue(stepId, transcriptId)}
          disabled={disabled}
          className="rounded-full bg-[#16A34A] px-3 py-1 text-xs font-semibold text-white hover:bg-[#15803D] disabled:opacity-50 transition-colors"
        >
          Continue
        </button>
      </div>
    </div>
  );
}

export function InputRequestCard({
  prompt,
  description,
  placeholder,
  stepId,
  transcriptId,
  onSubmit,
  disabled,
}: {
  prompt: string;
  description?: string;
  placeholder?: string;
  stepId: string;
  transcriptId: string;
  onSubmit: (stepId: string, transcriptId: string, inputText: string) => void;
  disabled?: boolean;
}) {
  const [value, setValue] = useState('');

  useEffect(() => {
    setValue('');
  }, [stepId]);

  const trimmedValue = value.trim();

  return (
    <div className="rounded-2xl border border-[#C7D2FE] bg-[#EEF2FF] p-3">
      <div className="text-xs font-bold uppercase tracking-wider text-[#4338CA]">
        Input Required
      </div>
      <div className="mt-1 text-sm font-semibold text-[#0F172A]">{prompt}</div>
      {description && (
        <div className="mt-1 text-xs text-[#475569]">{description}</div>
      )}
      <textarea
        value={value}
        onChange={(event) => setValue(event.target.value)}
        placeholder={placeholder ?? 'Type your response here'}
        disabled={disabled}
        rows={4}
        className="mt-3 w-full resize-y rounded-xl border border-[#C7D2FE] bg-white px-3 py-2 text-xs text-[#0F172A] outline-none transition-colors placeholder:text-[#94A3B8] focus:border-[#818CF8] disabled:cursor-not-allowed disabled:opacity-60"
      />
      <div className="mt-2 flex justify-end">
        <button
          type="button"
          onClick={() => onSubmit(stepId, transcriptId, trimmedValue)}
          disabled={disabled || trimmedValue.length === 0}
          className="rounded-full bg-[#4F46E5] px-3 py-1 text-xs font-semibold text-white transition-colors hover:bg-[#4338CA] disabled:opacity-50"
        >
          Submit
        </button>
      </div>
    </div>
  );
}

function workflowStatusBadgeClass(status?: string | null) {
  switch (status) {
    case 'completed':
      return 'border-[#86EFAC] bg-[#DCFCE7] text-[#166534]';
    case 'running':
      return 'border-[#93C5FD] bg-[#DBEAFE] text-[#1D4ED8]';
    case 'failed':
    case 'interrupted':
      return 'border-[#FCA5A5] bg-[#FEE2E2] text-[#991B1B]';
    case 'interrupt_requested':
      return 'border-[#FCD34D] bg-[#FEF3C7] text-[#92400E]';
    case 'ready':
      return 'border-[#FCD34D] bg-[#FEF3C7] text-[#92400E]';
    case 'waiting_input':
    case 'waiting_review':
      return 'border-[#C7D2FE] bg-[#E0E7FF] text-[#4338CA]';
    default:
      return 'border-[#CBD5E1] bg-[#F1F5F9] text-[#334155]';
  }
}

function WorkflowTranscriptFeed({
  steps,
  entries,
  isLoading,
  emptyMessage,
  pendingActionId,
  onApproval,
}: {
  steps: WorkflowCardStep[];
  entries: WorkflowTranscriptEntry[];
  isLoading?: boolean;
  emptyMessage: string;
  pendingActionId?: string | null;
  onApproval?: (
    stepId: string,
    action: string,
    transcriptId: string,
    inputText?: string
  ) => void;
}) {
  const renderItems = useMemo(
    () => buildWorkflowTranscriptRenderItems(entries),
    [entries]
  );
  const stepById = useMemo(
    () => new Map(steps.map((step) => [step.id, step])),
    [steps]
  );
  const stepByKey = useMemo(
    () => new Map(steps.map((step) => [step.step_key, step])),
    [steps]
  );
  const [collapsedThinkingGroups, setCollapsedThinkingGroups] = useState<
    Record<string, boolean>
  >({});

  if (entries.length === 0) {
    return (
      <div className="flex h-full min-h-[240px] items-center justify-center rounded-[24px] border border-dashed border-[#CBD5E1] bg-[#F8FAFC] px-5 text-center text-sm text-[#94A3B8] dark:border-[#334155] dark:bg-[rgba(15,23,42,0.45)]">
        {isLoading ? 'Loading step transcript...' : emptyMessage}
      </div>
    );
  }

  return (
    <div className="divide-y divide-[#CBD5E1] dark:divide-[#334155]">
      {renderItems.map((item) => {
        if (item.kind === 'thinking_group') {
          const tone = workflowMessageTone('agent', 'thinking');
          const collapsed = collapsedThinkingGroups[item.id] ?? true;
          const stepTitle = resolveWorkflowTranscriptStepTitle(
            item.entries[0],
            stepById,
            stepByKey
          );
          const agentLabel = stepTitle ?? item.agentName ?? 'agent';

          return (
            <div key={item.id} className="py-5 first:pt-0 last:pb-0">
              <div className="flex gap-3">
                <div
                  className={cn(
                    'mt-1 h-4 w-1 shrink-0 rounded-full',
                    tone.rail
                  )}
                />
                <div className="min-w-0 flex-1">
                  <div className="flex items-center justify-between gap-3 text-[11px]">
                    <span
                      className={cn(
                        'rounded-full px-2 py-0.5 font-semibold uppercase tracking-[0.14em]',
                        tone.badge
                      )}
                    >
                      {agentLabel}
                    </span>
                    <button
                      type="button"
                      onClick={() =>
                        setCollapsedThinkingGroups((previous) => ({
                          ...previous,
                          [item.id]: !collapsed,
                        }))
                      }
                      className="shrink-0 rounded-full border border-[#CBD5E1] bg-white px-2.5 py-1 text-[10px] font-semibold uppercase tracking-[0.14em] text-[#64748B] transition-colors hover:bg-[#F8FAFC] dark:border-[#334155] dark:bg-transparent dark:text-[#CBD5E1]"
                    >
                      {collapsed
                        ? `Show ${item.entries.length} line${
                            item.entries.length === 1 ? '' : 's'
                          }`
                        : 'Hide details'}
                    </button>
                  </div>

                  {!collapsed && (
                    <div
                      className="mt-2 space-y-1 overflow-auto select-text"
                      style={{ maxHeight: WORKFLOW_COMPOSER_MAX_HEIGHT }}
                    >
                      {item.entries.map((thinkingEntry) => (
                        <div
                          key={thinkingEntry.id}
                          className="truncate select-text text-[12px] leading-6 text-[#334155] dark:text-[#CBD5E1]"
                          title={thinkingEntry.content}
                        >
                          {thinkingEntry.content}
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              </div>
            </div>
          );
        }

        const { entry } = item;
        const meta = parseWorkflowTranscriptMeta(entry.meta_json);
        const tone = workflowMessageTone(entry.message_type, entry.entry_type);
        const stepTitle = resolveWorkflowTranscriptStepTitle(
          entry,
          stepById,
          stepByKey
        );
        const headerLabel = stepTitle ?? entry.agent_name ?? entry.message_type;
        const secondaryLabel = stepTitle
          ? entry.agent_name && entry.agent_name !== 'BACKEND'
            ? entry.agent_name
            : null
          : entry.entry_type;
        const resolved = meta?.resolved === true;
        const markdownContent = getTranscriptMarkdown(entry);
        const descriptionText =
          typeof meta?.description === 'string' ? meta.description : null;
        const markdownTextClassName =
          entry.entry_type === 'error'
            ? 'text-[13px] leading-6 text-[#991B1B] dark:text-[#FECACA]'
            : 'text-[13px] leading-6 text-[#0F172A] dark:text-white';

        return (
          <div key={entry.id} className="py-5 first:pt-0 last:pb-0">
            <div className="flex gap-3">
              <div
                className={cn('mt-1 h-4 w-1 shrink-0 rounded-full', tone.rail)}
              />
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2 text-[11px]">
                  <span
                    className={cn(
                      'rounded-full px-2 py-0.5 font-semibold uppercase tracking-[0.14em]',
                      tone.badge
                    )}
                  >
                    {headerLabel}
                  </span>
                  {secondaryLabel ? (
                    <span
                      className={cn(
                        'font-medium uppercase tracking-[0.14em]',
                        tone.label
                      )}
                    >
                      {secondaryLabel}
                    </span>
                  ) : null}
                </div>
                {markdownContent ? (
                  <ChatMarkdown
                    content={markdownContent}
                    maxWidth="100%"
                    hideCopyButton
                    textClassName={markdownTextClassName}
                    className="mt-2 w-full select-text"
                  />
                ) : (
                  <div className="mt-2 whitespace-pre-wrap select-text text-[13px] leading-6 text-[#0F172A] dark:text-white">
                    {entry.content}
                    {entry.entry_type === 'input_request' && descriptionText
                      ? `\n\n${descriptionText}`
                      : ''}
                  </div>
                )}

                {entry.entry_type !== 'input_request' && descriptionText ? (
                  <div className="mt-1 whitespace-pre-wrap select-text text-[13px] leading-6 text-[#64748B] dark:text-[#94A3B8]">
                    {descriptionText}
                  </div>
                ) : null}

                {entry.entry_type === 'approval_request' ? (
                  <div className="mt-3 flex flex-wrap gap-2">
                    <button
                      type="button"
                      onClick={() =>
                        entry.step_id &&
                        onApproval?.(entry.step_id, 'approved', entry.id)
                      }
                      disabled={
                        !entry.step_id ||
                        resolved ||
                        !onApproval ||
                        pendingActionId === entry.id
                      }
                      className="rounded-full bg-[#16A34A] px-3 py-1 text-xs font-semibold text-white transition-colors hover:bg-[#15803D] disabled:opacity-50"
                    >
                      Approve
                    </button>
                    <button
                      type="button"
                      onClick={() =>
                        entry.step_id &&
                        onApproval?.(entry.step_id, 'rejected', entry.id)
                      }
                      disabled={
                        !entry.step_id ||
                        resolved ||
                        !onApproval ||
                        pendingActionId === entry.id
                      }
                      className="rounded-full border border-[#CBD5E1] bg-white px-3 py-1 text-xs font-semibold text-[#475569] transition-colors hover:bg-[#F8FAFC] disabled:opacity-50 dark:border-[#334155] dark:bg-transparent dark:text-[#CBD5E1]"
                    >
                      Reject
                    </button>
                  </div>
                ) : null}

                {entry.entry_type === 'permission_request' ? (
                  <div className="mt-3 flex flex-wrap gap-2">
                    <button
                      type="button"
                      onClick={() =>
                        entry.step_id &&
                        onApproval?.(entry.step_id, 'granted', entry.id)
                      }
                      disabled={
                        !entry.step_id ||
                        resolved ||
                        !onApproval ||
                        pendingActionId === entry.id
                      }
                      className="rounded-full bg-[#2563EB] px-3 py-1 text-xs font-semibold text-white transition-colors hover:bg-[#1D4ED8] disabled:opacity-50"
                    >
                      Grant
                    </button>
                    <button
                      type="button"
                      onClick={() =>
                        entry.step_id &&
                        onApproval?.(entry.step_id, 'denied', entry.id)
                      }
                      disabled={
                        !entry.step_id ||
                        resolved ||
                        !onApproval ||
                        pendingActionId === entry.id
                      }
                      className="rounded-full border border-[#CBD5E1] bg-white px-3 py-1 text-xs font-semibold text-[#475569] transition-colors hover:bg-[#F8FAFC] disabled:opacity-50 dark:border-[#334155] dark:bg-transparent dark:text-[#CBD5E1]"
                    >
                      Deny
                    </button>
                  </div>
                ) : null}

                {entry.entry_type === 'continue_confirmation' ? (
                  <div className="mt-3">
                    <button
                      type="button"
                      onClick={() =>
                        entry.step_id &&
                        onApproval?.(entry.step_id, 'continued', entry.id)
                      }
                      disabled={
                        !entry.step_id ||
                        resolved ||
                        !onApproval ||
                        pendingActionId === entry.id
                      }
                      className="rounded-full bg-[#16A34A] px-3 py-1 text-xs font-semibold text-white transition-colors hover:bg-[#15803D] disabled:opacity-50"
                    >
                      Continue
                    </button>
                  </div>
                ) : null}
              </div>
            </div>
          </div>
        );
      })}
    </div>
  );
}

// -----------------------------------------------------------------------
// Workflow Window
// -----------------------------------------------------------------------

export function WorkflowWindow({
  sessionId,
  projection,
  transcript = [],
  runtimeMessages = [],
  isOpen,
  onClose,
  onExecute,
  onPauseAll,
  onResume,
  onInterruptStep,
  onStopStep,
  onRetryStep,
  onSubmitStepInput,
  onApproval,
  onResolveFinalReview,
  pendingActionId,
}: WorkflowWindowProps) {
  const { t } = useTranslation('chat');
  const [selectedStepId, setSelectedStepId] = useState<string | null>(null);
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  const [detailStepId, setDetailStepId] = useState<string | null>(null);
  const [composerValue, setComposerValue] = useState('');
  const [runtimeInputTranscripts, setRuntimeInputTranscripts] = useState<
    WorkflowTranscriptEntry[]
  >([]);
  const initializedWorkflowKeyRef = useRef<string | null>(null);
  const previousExecutionIdRef = useRef<string | null>(null);
  const composerTextareaRef = useRef<HTMLTextAreaElement | null>(null);

  const isPreview =
    projection.state === 'preview_ready' ||
    projection.state === 'preview_invalid';
  const canPauseExecution = canPauseWorkflowExecution(projection);
  const canResumeExecution = canResumeWorkflowExecution(projection);
  const isExecutionRecompiling = isWorkflowExecutionRecompiling(projection);
  const normalizedResultSummary = projection.result_summary?.trim() ?? '';
  const normalizedErrorMessage = projection.error_message?.trim() ?? '';
  const hasFailedWorkflowStep = projection.steps.some((step) =>
    WORKFLOW_FAILURE_STEP_STATUSES.has(step.status)
  );
  const hasTerminalWorkflowSteps =
    projection.steps.length > 0 &&
    projection.steps.every((step) =>
      WORKFLOW_TERMINAL_STEP_STATUSES.has(step.status)
    );
  const hasWorkflowCompleted =
    projection.state === 'completed' ||
    projection.execution_status === 'completed' ||
    (normalizedResultSummary.length > 0 &&
      hasTerminalWorkflowSteps &&
      !hasFailedWorkflowStep);
  const hasWorkflowFailed =
    projection.state === 'failed' ||
    projection.execution_status === 'failed' ||
    (normalizedErrorMessage.length > 0 && hasFailedWorkflowStep);
  const agents = useMemo(() => projection.agents ?? [], [projection.agents]);
  const leadAgentId =
    agents[0]?.workflow_agent_session_id ?? agents[0]?.session_agent_id ?? null;
  const leadAgentName = agents[0]?.name ?? 'Lead';
  const agentSessionIdByLookup = useMemo(() => {
    const lookup = new Map<string, string>();

    for (const agent of agents) {
      const agentSessionId =
        agent.workflow_agent_session_id ?? agent.session_agent_id;
      const keys = [
        agent.name,
        agent.agent_id,
        agent.session_agent_id,
        agent.workflow_agent_session_id,
      ];

      for (const key of keys) {
        const normalizedKey = key?.trim();
        if (!normalizedKey || lookup.has(normalizedKey)) {
          continue;
        }
        lookup.set(normalizedKey, agentSessionId);
      }
    }

    return lookup;
  }, [agents]);
  const agentNameByLookup = useMemo(() => {
    const lookup = new Map<string, string>();

    for (const agent of agents) {
      const keys = [
        agent.name,
        agent.agent_id,
        agent.session_agent_id,
        agent.workflow_agent_session_id,
      ];

      for (const key of keys) {
        const normalizedKey = key?.trim();
        if (!normalizedKey || lookup.has(normalizedKey)) {
          continue;
        }
        lookup.set(normalizedKey, agent.name);
      }
    }

    return lookup;
  }, [agents]);
  const stepByKey = useMemo(
    () => new Map(projection.steps.map((step) => [step.step_key, step])),
    [projection.steps]
  );
  const planNodeById = useMemo(
    () => new Map(projection.plan.nodes.map((node) => [node.id, node])),
    [projection.plan.nodes]
  );
  const orderedActionableSteps = useMemo(
    () =>
      [...projection.steps].sort((left, right) => {
        const priority = (status: string) => {
          switch (status) {
            case 'running':
              return 0;
            case 'waiting_input':
            case 'waiting_review':
              return 1;
            case 'failed':
              return 2;
            case 'ready':
              return 3;
            default:
              return 10;
          }
        };

        return priority(left.status) - priority(right.status);
      }),
    [projection.steps]
  );
  const workflowInstanceKey = useMemo(
    () => `${projection.execution_id ?? ''}::${projection.plan_id ?? ''}`,
    [projection.execution_id, projection.plan_id]
  );
  const resolveStepAgentName = useCallback(
    (step?: WorkflowCardStep | null) => {
      const rawAgent = step?.agent_name?.trim();
      if (!rawAgent) {
        return leadAgentName;
      }
      return agentNameByLookup.get(rawAgent) ?? rawAgent;
    },
    [agentNameByLookup, leadAgentName]
  );
  const resolveStepAgentId = useCallback(
    (step?: WorkflowCardStep | null) => {
      if (!step) {
        return leadAgentId;
      }
      const rawAgent = step.agent_name?.trim();
      if (!rawAgent) {
        return leadAgentId;
      }
      return agentSessionIdByLookup.get(rawAgent) ?? leadAgentId;
    },
    [agentSessionIdByLookup, leadAgentId]
  );
  const findPreferredStepForAgent = useCallback(
    (agentId: string | null) => {
      if (!agentId) {
        return orderedActionableSteps[0] ?? projection.steps[0] ?? null;
      }
      return (
        orderedActionableSteps.find(
          (step) => resolveStepAgentId(step) === agentId
        ) ??
        projection.steps.find((step) => resolveStepAgentId(step) === agentId) ??
        null
      );
    },
    [orderedActionableSteps, projection.steps, resolveStepAgentId]
  );

  useEffect(() => {
    if (!isOpen) {
      return;
    }

    const initialStep =
      orderedActionableSteps[0] ?? projection.steps[0] ?? null;
    const initialStepKey = initialStep?.step_key ?? null;
    const initialAgentId = resolveStepAgentId(initialStep);

    if (initializedWorkflowKeyRef.current !== workflowInstanceKey) {
      initializedWorkflowKeyRef.current = workflowInstanceKey;
      setSelectedStepId(initialStepKey);
      setSelectedAgentId(initialAgentId);
      setDetailStepId(null);
      setComposerValue('');
      return;
    }

    setSelectedStepId((prev) => {
      if (!prev) {
        return initialStepKey;
      }
      return stepByKey.has(prev) ? prev : initialStepKey;
    });

    setSelectedAgentId((prev) => {
      if (
        prev &&
        agents.some(
          (agent) =>
            (agent.workflow_agent_session_id ?? agent.session_agent_id) === prev
        )
      ) {
        return prev;
      }
      return initialAgentId;
    });

    setDetailStepId((prev) => (prev && stepByKey.has(prev) ? prev : null));
  }, [
    agents,
    isOpen,
    orderedActionableSteps,
    projection.steps,
    resolveStepAgentId,
    stepByKey,
    workflowInstanceKey,
  ]);

  useEffect(() => {
    if (!isOpen || typeof document === 'undefined') {
      return undefined;
    }

    const previousOverflow = document.body.style.overflow;
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        if (detailStepId) {
          setDetailStepId(null);
          return;
        }
        onClose();
      }
    };

    document.body.style.overflow = 'hidden';
    window.addEventListener('keydown', handleKeyDown);

    return () => {
      document.body.style.overflow = previousOverflow;
      window.removeEventListener('keydown', handleKeyDown);
    };
  }, [detailStepId, isOpen, onClose]);

  useEffect(() => {
    if (!isOpen) {
      setDetailStepId(null);
    }
  }, [isOpen]);

  useEffect(() => {
    const textarea = composerTextareaRef.current;
    if (!textarea) {
      return;
    }

    textarea.style.height = `${WORKFLOW_COMPOSER_MIN_HEIGHT}px`;
    const fullHeight = textarea.scrollHeight;
    const shouldEnableScroll = fullHeight > WORKFLOW_COMPOSER_MAX_HEIGHT;
    const nextHeight = Math.min(fullHeight, WORKFLOW_COMPOSER_MAX_HEIGHT);
    textarea.style.height = `${Math.max(
      nextHeight,
      WORKFLOW_COMPOSER_MIN_HEIGHT
    )}px`;
    textarea.style.overflowY = shouldEnableScroll ? 'auto' : 'hidden';
  }, [composerValue, isOpen]);

  const selectedStep = projection.steps.find(
    (s) => s.step_key === selectedStepId
  );
  const selectedAgent =
    agents.find(
      (agent) =>
        (agent.workflow_agent_session_id ?? agent.session_agent_id) ===
        selectedAgentId
    ) ??
    agents[0] ??
    null;
  const selectedWorkflowAgentSessionId = selectedAgent
    ? (selectedAgent.workflow_agent_session_id ??
      selectedAgent.session_agent_id)
    : null;
  const selectedStepInputRequest = useMemo(() => {
    if (!selectedStep || selectedStep.status !== 'waiting_input') {
      return null;
    }

    for (let index = transcript.length - 1; index >= 0; index -= 1) {
      const entry = transcript[index];
      if (
        entry.entry_type !== 'input_request' ||
        (entry.step_id !== selectedStep.id &&
          entry.step_key !== selectedStep.step_key)
      ) {
        continue;
      }

      const meta = parseWorkflowTranscriptMeta(entry.meta_json);
      if (meta?.resolved === true) {
        continue;
      }

      return {
        prompt: entry.content,
        description:
          typeof meta?.description === 'string' ? meta.description : undefined,
        placeholder:
          typeof meta?.placeholder === 'string' ? meta.placeholder : undefined,
      };
    }

    return null;
  }, [selectedStep, transcript]);
  const composerPlaceholder = useMemo(() => {
    if (selectedStepInputRequest?.placeholder?.trim()) {
      return selectedStepInputRequest.placeholder.trim();
    }
    if (selectedStepInputRequest?.prompt?.trim()) {
      return selectedStepInputRequest.prompt.trim();
    }
    if (selectedStep) {
      return `Send input to ${selectedAgent?.name ?? 'selected agent'}`;
    }
    if (selectedAgent) {
      return 'No step available for the selected agent';
    }
    return 'Pick a node before sending input';
  }, [selectedAgent, selectedStep, selectedStepInputRequest]);
  const detailStep = projection.steps.find((s) => s.step_key === detailStepId);
  const detailStepNode = detailStepId ? planNodeById.get(detailStepId) : null;
  const detailAgentSessionId = detailStep?.agent_name
    ? (agentSessionIdByLookup.get(detailStep.agent_name.trim()) ?? leadAgentId)
    : leadAgentId;
  const liveThinkingTranscriptEntries = useMemo(() => {
    const persistedThinkingKeys = new Set(
      transcript
        .filter((entry) => entry.entry_type === 'thinking')
        .map(
          (entry) =>
            `${entry.workflow_agent_session_id ?? ''}::${entry.step_id ?? ''}::${entry.content}`
        )
    );

    return runtimeMessages
      .filter((entry) => entry.streamType === 'thinking')
      .map((entry) => ({
        id: entry.id,
        step_id: entry.stepId,
        step_key: entry.stepKey,
        workflow_agent_session_id: entry.workflowAgentSessionId,
        agent_name: entry.agentName,
        message_type: 'agent' as const,
        entry_type: 'thinking',
        content: entry.content,
        meta_json: JSON.stringify({ source: 'workflow_runtime_line' }),
        created_at: entry.createdAt,
      }))
      .filter(
        (entry) =>
          !persistedThinkingKeys.has(
            `${entry.workflow_agent_session_id ?? ''}::${entry.step_id ?? ''}::${entry.content}`
          )
      );
  }, [runtimeMessages, transcript]);
  const selectedRuntimeTranscript = useMemo(() => {
    let entries = transcript;
    if (selectedWorkflowAgentSessionId) {
      entries = entries.filter(
        (entry) =>
          entry.workflow_agent_session_id === selectedWorkflowAgentSessionId
      );
    }
    const liveThinkingEntries = liveThinkingTranscriptEntries.filter(
      (entry) => {
        if (!selectedWorkflowAgentSessionId) {
          return true;
        }
        return (
          entry.workflow_agent_session_id === selectedWorkflowAgentSessionId
        );
      }
    );
    const stepContentEntries = buildStepContentTranscriptEntries(
      projection.steps,
      entries,
      resolveStepAgentId,
      selectedWorkflowAgentSessionId
    );
    const localEntries = runtimeInputTranscripts.filter((entry) => {
      if (!selectedWorkflowAgentSessionId) {
        return true;
      }
      return entry.workflow_agent_session_id === selectedWorkflowAgentSessionId;
    });

    return mergeAndSortTranscriptEntries(
      mergeAndSortTranscriptEntries(
        mergeAndSortTranscriptEntries(entries, liveThinkingEntries),
        stepContentEntries
      ),
      localEntries
    );
  }, [
    projection.steps,
    liveThinkingTranscriptEntries,
    runtimeInputTranscripts,
    resolveStepAgentId,
    transcript,
    selectedWorkflowAgentSessionId,
  ]);
  const transcriptWithLocalInputs = useMemo(
    () => mergeAndSortTranscriptEntries(transcript, runtimeInputTranscripts),
    [runtimeInputTranscripts, transcript]
  );

  const {
    data: detailStepTranscriptData,
    isFetching: isFetchingDetailStepTranscript,
  } = useQuery({
    queryKey: [
      'workflowStepTranscripts',
      sessionId,
      detailStep?.id,
      detailAgentSessionId,
    ],
    queryFn: () => {
      if (!sessionId || !detailStep?.id) {
        return [];
      }

      return chatApi.getWorkflowStepTranscripts(sessionId, detailStep.id, {
        stepKey: detailStep.step_key,
        workflowAgentSessionId: detailAgentSessionId,
      });
    },
    enabled: !!sessionId && !!detailStep?.id && !isPreview && isOpen,
    refetchInterval:
      isOpen && !isPreview && !!sessionId && !!detailStep?.id ? 5000 : false,
  });

  const detailFallbackTranscript = useMemo(() => {
    if (!detailStep) {
      return [];
    }

    let entries = transcriptWithLocalInputs.filter(
      (entry) =>
        entry.step_id === detailStep.id ||
        entry.step_key === detailStep.step_key
    );
    if (detailAgentSessionId) {
      entries = entries.filter(
        (entry) => entry.workflow_agent_session_id === detailAgentSessionId
      );
    }
    return entries;
  }, [detailAgentSessionId, detailStep, transcriptWithLocalInputs]);

  const detailStepScopedTranscript = useMemo(() => {
    const entries = detailStepTranscriptData ?? [];
    const remoteEntries = entries.map((entry) => ({
      id: entry.id,
      step_id: entry.step_id,
      step_key: entry.step_key,
      workflow_agent_session_id: entry.workflow_agent_session_id,
      agent_name: entry.agent_name,
      message_type: entry.sender_type as
        | 'system'
        | 'agent'
        | 'user'
        | 'control',
      content: entry.content,
      entry_type: entry.entry_type,
      meta_json: entry.meta_json,
      created_at: entry.created_at,
    }));
    const localEntries = transcriptWithLocalInputs.filter(
      (entry) =>
        entry.step_id === detailStep?.id ||
        entry.step_key === detailStep?.step_key
    );
    const mergedEntries = mergeAndSortTranscriptEntries(
      remoteEntries,
      localEntries
    );
    const stepContentEntries = detailStep
      ? buildStepContentTranscriptEntries(
          [detailStep],
          mergedEntries,
          resolveStepAgentId,
          detailAgentSessionId
        )
      : [];
    return mergeAndSortTranscriptEntries(mergedEntries, stepContentEntries);
  }, [
    detailAgentSessionId,
    detailStep,
    detailStepTranscriptData,
    resolveStepAgentId,
    transcriptWithLocalInputs,
  ]);

  const visibleDetailTranscript =
    detailStepScopedTranscript.length > 0
      ? detailStepScopedTranscript
      : detailFallbackTranscript;
  const workflowFinalReviewAction = useMemo(
    () => toWorkflowFinalReviewAction(projection.execution_id, transcript),
    [projection.execution_id, transcript]
  );
  const handleSelectStep = useCallback(
    (id: string) => {
      const nextStep = stepByKey.get(id);
      if (!nextStep) {
        return;
      }
      setSelectedStepId(id);
      setSelectedAgentId(resolveStepAgentId(nextStep));
      setDetailStepId(id);
    },
    [resolveStepAgentId, stepByKey]
  );
  const handleSelectAgent = useCallback(
    (agentId: string) => {
      setSelectedAgentId(agentId);
      const nextStep = findPreferredStepForAgent(agentId);
      setSelectedStepId(nextStep?.step_key ?? null);
      setDetailStepId(null);
    },
    [findPreferredStepForAgent]
  );
  const handleSendStepInput = useCallback(() => {
    if (!selectedStep || !onSubmitStepInput) {
      return;
    }
    const nextValue = composerValue.trim();
    if (!nextValue) {
      return;
    }

    onSubmitStepInput(selectedStep.id, nextValue);

    setRuntimeInputTranscripts((previous) => [
      ...previous,
      {
        id: `runtime-user-${Date.now()}-${Math.floor(Math.random() * 99999)}`,
        step_id: selectedStep.id,
        step_key: selectedStep.step_key,
        workflow_agent_session_id: selectedWorkflowAgentSessionId,
        agent_name: 'You',
        message_type: 'user',
        entry_type: 'message',
        content: nextValue,
        meta_json: JSON.stringify({ source: 'workflow_window_input' }),
        created_at: new Date().toISOString(),
      },
    ]);
    setComposerValue('');
  }, [
    composerValue,
    onSubmitStepInput,
    selectedStep,
    selectedWorkflowAgentSessionId,
  ]);

  useEffect(() => {
    if (!isOpen || !projection.execution_id) {
      setRuntimeInputTranscripts([]);
      return;
    }

    if (previousExecutionIdRef.current !== projection.execution_id) {
      previousExecutionIdRef.current = projection.execution_id;
      setRuntimeInputTranscripts([]);
    }
  }, [isOpen, projection.execution_id]);

  if (!isOpen) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-slate-950/42 p-3 backdrop-blur-sm md:p-6"
      onClick={onClose}
      role="dialog"
      aria-modal="true"
      aria-label={`Workflow window: ${projection.title}`}
    >
      <div
        className="relative flex h-[min(92vh,880px)] w-full max-w-[1360px] flex-col overflow-hidden rounded-[28px] border border-white/70 bg-[linear-gradient(180deg,rgba(255,255,255,0.95)_0%,rgba(248,250,252,0.98)_100%)] shadow-[0_30px_100px_rgba(15,23,42,0.28)] backdrop-blur-xl dark:border-[#243041] dark:bg-[linear-gradient(180deg,rgba(11,16,23,0.96)_0%,rgba(15,23,42,0.94)_100%)] dark:shadow-[0_28px_100px_rgba(0,0,0,0.5)]"
        onClick={(event) => event.stopPropagation()}
      >
        {/* Header */}
        <div className="flex items-start justify-between gap-4 border-b border-[#E2E8F0] px-5 py-4 md:px-6">
          <div className="min-w-0">
            <div className="text-[11px] font-bold uppercase tracking-[0.24em] text-[#64748B]">
              Workflow Window
            </div>
            <div className="mt-2 flex flex-wrap items-center gap-2">
              <div className="truncate text-lg font-semibold text-[#0F172A] dark:text-white">
                {projection.title}
              </div>
              <div className="rounded-full bg-[#EEF4FF] px-3 py-1 text-[11px] font-semibold text-[#1D4ED8]">
                {projection.completed_step_count}/{projection.total_step_count}
              </div>
            </div>
            <div className="mt-2 max-w-3xl select-text text-sm leading-6 text-[#475569] dark:text-[#94A3B8]">
              {projection.goal}
            </div>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="inline-flex size-10 shrink-0 items-center justify-center rounded-2xl border border-white/70 bg-white/75 text-[#64748B] shadow-sm transition-colors hover:bg-white hover:text-[#0F172A] dark:border-[#2A3445] dark:bg-[rgba(25,34,51,0.82)] dark:text-[#94A3B8] dark:hover:text-white"
            aria-label="Close workflow window"
          >
            <svg
              className="size-5"
              fill="none"
              viewBox="0 0 24 24"
              stroke="currentColor"
            >
              <path
                strokeLinecap="round"
                strokeLinejoin="round"
                strokeWidth={2}
                d="M6 18L18 6M6 6l12 12"
              />
            </svg>
          </button>
        </div>

        {/* Two-pane body */}
        <div className="flex min-h-0 flex-1 flex-col overflow-hidden lg:flex-row">
          {/* Left pane: Graph */}
          <div className="w-full shrink-0 overflow-auto border-b border-[#E2E8F0] bg-[radial-gradient(circle_at_top_left,rgba(191,219,254,0.45),rgba(248,250,252,0.8)_34%,rgba(248,250,252,1)_72%)] p-4 lg:basis-3/4 lg:border-b-0 lg:border-r lg:p-5 dark:border-[#243041] dark:bg-[radial-gradient(circle_at_top_left,rgba(37,99,235,0.18),rgba(15,23,42,0.92)_38%,rgba(11,16,23,0.98)_78%)]">
            <div className="mb-3 text-[10px] font-bold uppercase tracking-[0.2em] text-[#64748B]">
              Plan Graph
            </div>
            <WorkflowGraphBoard
              nodes={projection.plan.nodes}
              edges={projection.plan.edges}
              steps={projection.steps}
              agents={agents}
              selectedStepId={selectedStepId}
              onSelectStep={handleSelectStep}
              onRetryStep={onRetryStep}
              pendingActionId={pendingActionId}
            />

            <div className="mt-4 flex items-center justify-between gap-3 rounded-[22px] border border-white/70 bg-white/80 px-4 py-3 text-xs text-[#475569] shadow-[inset_0_1px_0_rgba(255,255,255,0.6)] dark:border-[#243041] dark:bg-[rgba(15,23,42,0.78)] dark:text-[#CBD5E1]">
              <div className="min-w-0 flex-1">
                <div className="text-[10px] font-bold uppercase tracking-[0.18em] text-[#94A3B8]">
                  Step Inspector
                </div>
                <div className="mt-1 select-text text-xs leading-5 text-[#475569] dark:text-[#CBD5E1]">
                  Click a step node to open its detail card with task
                  instructions, agent, status and transcript.
                </div>
                <div className="mt-2 flex flex-wrap items-center gap-2 text-[10px]">
                  <span className="rounded-full bg-[#EEF4FF] px-2.5 py-1 font-semibold text-[#1D4ED8] dark:bg-[rgba(37,99,235,0.18)] dark:text-[#BFDBFE]">
                    {projection.completed_step_count}/
                    {projection.total_step_count} steps completed
                  </span>
                  {hasWorkflowCompleted && normalizedResultSummary && (
                    <span className="select-text rounded-[10px] border border-[#16A34A] px-2.5 py-1 font-semibold text-[#166534] dark:border-[#22C55E] dark:text-[#BBF7D0]">
                      {normalizedResultSummary}
                    </span>
                  )}
                  {isExecutionRecompiling && (
                    <span className="rounded-[10px] border border-[#14B8A6] px-2.5 py-1 font-semibold text-[#0F766E] dark:border-[#2DD4BF] dark:text-[#99F6E4]">
                      Recompiling plan
                    </span>
                  )}
                  {hasWorkflowFailed && normalizedErrorMessage && (
                    <span className="select-text rounded-[10px] border border-[#DC2626] px-2.5 py-1 font-semibold text-[#991B1B] dark:border-[#F87171] dark:text-[#FECACA]">
                      {normalizedErrorMessage}
                    </span>
                  )}
                </div>
              </div>
              <div className="flex shrink-0 flex-wrap items-center justify-end gap-2">
                {projection.state === 'preview_ready' &&
                  projection.plan_id &&
                  onExecute && (
                    <button
                      type="button"
                      onClick={() => onExecute(projection.plan_id!)}
                      className="flex items-center gap-2 rounded-full bg-[#2563EB] px-4 py-2 text-xs font-semibold text-white shadow-sm transition-colors hover:bg-[#1D4ED8]"
                    >
                      <PlayIcon className="size-3.5" weight="bold" />
                      Execute Plan
                    </button>
                  )}
                {canPauseExecution && projection.execution_id && onPauseAll && (
                  <button
                    type="button"
                    onClick={() => onPauseAll(projection.execution_id!)}
                    className="flex items-center gap-1 rounded-full bg-[#D97706] px-3 py-1.5 text-xs font-semibold text-white transition-colors hover:bg-[#B45309]"
                  >
                    <PauseIcon className="size-3.5" weight="bold" />
                    Pause All
                  </button>
                )}
                {canResumeExecution && projection.execution_id && onResume && (
                  <button
                    type="button"
                    onClick={() => onResume(projection.execution_id!)}
                    className="flex items-center gap-1 rounded-full bg-[#2563EB] px-3 py-1.5 text-xs font-semibold text-white transition-colors hover:bg-[#1D4ED8]"
                  >
                    <PlayIcon className="size-3.5" weight="bold" />
                    Resume
                  </button>
                )}
              </div>
            </div>
          </div>

          {/* Right pane: Panel */}
          <div className="flex min-h-0 flex-1 flex-col overflow-hidden bg-white/70 lg:basis-1/4 dark:bg-transparent">
            {/* Preview mode */}
            {isPreview && (
              <div className="flex-1 overflow-auto p-5 md:p-6">
                <div className="max-w-3xl rounded-[24px] border border-white/70 bg-white/82 p-5 shadow-[0_18px_42px_rgba(148,163,184,0.16)] dark:border-[#2A3445] dark:bg-[rgba(15,23,42,0.78)] dark:shadow-none">
                  <div className="text-sm font-semibold text-[#0F172A] dark:text-white">
                    Plan Summary
                  </div>
                  <div className="mt-2 select-text text-sm leading-6 text-[#475569] dark:text-[#94A3B8]">
                    {projection.goal}
                  </div>

                  {projection.validation_errors && (
                    <div className="mt-4 rounded-2xl border border-[#FECACA] bg-[#FEF2F2] p-3 dark:border-[#7F1D1D] dark:bg-[rgba(127,29,29,0.18)]">
                      <div className="text-xs font-bold uppercase tracking-wider text-[#991B1B] dark:text-[#FCA5A5]">
                        Validation Errors
                      </div>
                      <div className="mt-1 select-text text-sm text-[#991B1B] dark:text-[#FECACA]">
                        {projection.validation_errors}
                      </div>
                    </div>
                  )}
                </div>
              </div>
            )}

            {/* Execution mode */}
            {!isPreview && (
              <>
                {agents.length > 0 && (
                  <div className="border-b border-[#E2E8F0] px-5 py-3 dark:border-[#243041] md:px-6">
                    <AgentSelector
                      agents={agents}
                      selectedAgentId={
                        selectedAgent
                          ? (selectedAgent.workflow_agent_session_id ??
                            selectedAgent.session_agent_id)
                          : selectedAgentId
                      }
                      onSelect={handleSelectAgent}
                    />
                  </div>
                )}
                <div className="flex min-h-0 flex-1 flex-col">
                  <div className="min-h-0 flex-1 overflow-auto px-5 pt-4 md:px-6">
                    {workflowFinalReviewAction && onResolveFinalReview && (
                      <div className="mb-5 flex gap-3">
                        <div className="mt-1 h-4 w-1 shrink-0 rounded-full bg-[#7C3AED]" />
                        <div className="min-w-0 flex-1">
                          <div className="flex items-center gap-2 text-[11px]">
                            <span className="rounded-full bg-[#F3E8FF] px-2 py-0.5 font-semibold uppercase tracking-[0.14em] text-[#7C3AED]">
                              workflow
                            </span>
                            <span className="font-medium uppercase tracking-[0.14em] text-[#7C3AED]">
                              final review
                            </span>
                          </div>
                          <div className="mt-2 whitespace-pre-wrap select-text text-[13px] leading-6 text-[#0F172A] dark:text-white">
                            {workflowFinalReviewAction.message}
                          </div>
                          {workflowFinalReviewAction.description ? (
                            <div className="mt-1 whitespace-pre-wrap select-text text-[13px] leading-6 text-[#64748B] dark:text-[#94A3B8]">
                              {workflowFinalReviewAction.description}
                            </div>
                          ) : null}
                          <div className="mt-3 flex flex-wrap gap-2">
                            <button
                              type="button"
                              onClick={() =>
                                onResolveFinalReview(
                                  workflowFinalReviewAction.executionId,
                                  workflowFinalReviewAction.transcriptId,
                                  'accepted'
                                )
                              }
                              disabled={
                                pendingActionId ===
                                workflowFinalReviewAction.transcriptId
                              }
                              className="rounded-full bg-[#16A34A] px-3 py-1 text-xs font-semibold text-white transition-colors hover:bg-[#15803D] disabled:opacity-50"
                            >
                              Accept
                            </button>
                            <button
                              type="button"
                              onClick={() =>
                                onResolveFinalReview(
                                  workflowFinalReviewAction.executionId,
                                  workflowFinalReviewAction.transcriptId,
                                  'rejected'
                                )
                              }
                              disabled={
                                pendingActionId ===
                                workflowFinalReviewAction.transcriptId
                              }
                              className="rounded-full border border-[#CBD5E1] bg-white px-3 py-1 text-xs font-semibold text-[#475569] transition-colors hover:bg-[#F8FAFC] disabled:opacity-50 dark:border-[#334155] dark:bg-transparent dark:text-[#CBD5E1]"
                            >
                              Reject
                            </button>
                          </div>
                        </div>
                      </div>
                    )}
                    <WorkflowTranscriptFeed
                      steps={projection.steps}
                      entries={selectedRuntimeTranscript}
                      isLoading={false}
                      emptyMessage={
                        canPauseExecution
                          ? 'Execution has not produced runtime messages yet.'
                          : 'No runtime messages for this agent yet.'
                      }
                    />
                  </div>

                  <div className="border-t border-white bg-white px-5 pb-3 pt-3 dark:border-white dark:bg-white md:px-6">
                    <div className="relative">
                      {selectedStepInputRequest && (
                        <div className="mb-2 rounded-2xl border border-[#C7D2FE] bg-[#EEF2FF] px-4 py-3 text-xs text-[#3730A3] dark:border-[#312E81] dark:bg-[rgba(49,46,129,0.24)] dark:text-[#C7D2FE]">
                          <div className="text-[10px] font-bold uppercase tracking-[0.16em] text-[#4338CA] dark:text-[#A5B4FC]">
                            Input Prompt
                          </div>
                          <div className="mt-1 whitespace-pre-wrap select-text text-xs leading-5 text-[#312E81] dark:text-[#E0E7FF]">
                            {selectedStepInputRequest.prompt}
                          </div>
                          {selectedStepInputRequest.description && (
                            <div className="mt-1 whitespace-pre-wrap select-text text-xs leading-5 text-[#4338CA]/90 dark:text-[#C7D2FE]/90">
                              {selectedStepInputRequest.description}
                            </div>
                          )}
                        </div>
                      )}
                      <textarea
                        ref={composerTextareaRef}
                        value={composerValue}
                        onChange={(event) =>
                          setComposerValue(event.target.value)
                        }
                        placeholder={composerPlaceholder}
                        rows={4}
                        disabled={!selectedStep || !onSubmitStepInput}
                        className="w-full resize-none overflow-y-hidden rounded-[24px] border border-[#CBD5E1] bg-white/96 px-4 py-3 pb-12 pr-12 text-[14px] leading-5 text-[#0F172A] shadow-[0_16px_34px_rgba(15,23,42,0.12)] outline-none transition-colors placeholder:text-[#94A3B8] focus:border-[#60A5FA] disabled:cursor-not-allowed disabled:bg-[#F8FAFC] disabled:text-[#94A3B8] dark:border-[#243041] dark:bg-[rgba(15,23,42,0.92)] dark:text-white dark:shadow-[0_18px_36px_rgba(0,0,0,0.28)]"
                        style={{
                          height: WORKFLOW_COMPOSER_MIN_HEIGHT,
                          maxHeight: WORKFLOW_COMPOSER_MAX_HEIGHT,
                        }}
                      />
                      <button
                        type="button"
                        onClick={handleSendStepInput}
                        disabled={
                          !selectedStep ||
                          !onSubmitStepInput ||
                          composerValue.trim().length === 0
                        }
                        className="absolute bottom-3 right-3 inline-flex size-8 items-center justify-center rounded-full bg-[#0F172A] text-white transition-colors hover:bg-[#1E293B] disabled:opacity-40"
                        aria-label="Send step input"
                      >
                        <ArrowUpIcon className="size-3" weight="bold" />
                      </button>
                    </div>
                  </div>
                </div>
              </>
            )}
          </div>
        </div>

        {detailStep && (
          <div
            className="absolute inset-0 z-20 flex items-center justify-center bg-slate-950/24 p-4 md:p-6"
            onClick={() => setDetailStepId(null)}
          >
            <div
              className="flex max-h-full w-full max-w-[980px] flex-col overflow-hidden rounded-[28px] border border-white/75 bg-[linear-gradient(180deg,rgba(255,255,255,0.98)_0%,rgba(248,250,252,0.98)_100%)] shadow-[0_28px_90px_rgba(15,23,42,0.28)] dark:border-[#243041] dark:bg-[linear-gradient(180deg,rgba(11,16,23,0.98)_0%,rgba(15,23,42,0.96)_100%)] dark:shadow-[0_28px_100px_rgba(0,0,0,0.55)]"
              onClick={(event) => event.stopPropagation()}
            >
              <div className="flex items-start justify-between gap-4 border-b border-[#E2E8F0] px-5 py-4 dark:border-[#243041] md:px-6">
                <div className="min-w-0">
                  <div className="text-[11px] font-bold uppercase tracking-[0.22em] text-[#64748B]">
                    Step Details
                  </div>
                  <div className="mt-2 flex flex-wrap items-center gap-2">
                    <div className="truncate select-text text-lg font-semibold text-[#0F172A] dark:text-white">
                      {detailStep.title}
                    </div>
                    <span
                      className={cn(
                        'select-text rounded-full border px-3 py-1 text-[10px] font-bold uppercase tracking-[0.16em]',
                        workflowStatusBadgeClass(detailStep.status)
                      )}
                    >
                      {detailStep.status}
                    </span>
                    {detailStep.status === 'running' &&
                      (onInterruptStep || onStopStep) && (
                        <button
                          type="button"
                          onClick={() => {
                            if (onInterruptStep) {
                              onInterruptStep(detailStep.id);
                              return;
                            }
                            onStopStep?.(detailStep.id);
                          }}
                          className="inline-flex items-center gap-1 rounded-full bg-[#991B1B] px-3 py-1.5 text-xs font-semibold text-white transition-colors hover:bg-[#7F1D1D]"
                        >
                          <StopIcon className="size-3.5" weight="bold" />
                          Terminate
                        </button>
                      )}
                    {isRetryableWorkflowStepStatus(detailStep.status) &&
                      onRetryStep && (
                        <button
                          type="button"
                          onClick={() => onRetryStep(detailStep.id)}
                          disabled={pendingActionId === detailStep.id}
                          className="inline-flex items-center gap-1 rounded-full bg-[#DC2626] px-3 py-1.5 text-xs font-semibold text-white transition-colors hover:bg-[#B91C1C] disabled:cursor-not-allowed disabled:bg-[#FCA5A5] disabled:text-white/90"
                        >
                          <ArrowClockwiseIcon
                            className={cn(
                              'size-3.5',
                              pendingActionId === detailStep.id &&
                                'animate-spin'
                            )}
                            weight="bold"
                          />
                          {t('workflow_retry', {
                            defaultValue: '重试',
                          })}
                        </button>
                      )}
                  </div>
                  <div className="mt-2 select-text text-xs text-[#64748B] dark:text-[#94A3B8]">
                    {detailStep.step_type}
                    {detailStep.agent_name
                      ? ` · ${resolveStepAgentName(detailStep)}`
                      : ''}
                  </div>
                </div>
                <div className="flex items-center gap-2">
                  <button
                    type="button"
                    onClick={() => setDetailStepId(null)}
                    className="inline-flex size-10 items-center justify-center rounded-2xl border border-white/70 bg-white/75 text-[#64748B] shadow-sm transition-colors hover:bg-white hover:text-[#0F172A] dark:border-[#2A3445] dark:bg-[rgba(25,34,51,0.82)] dark:text-[#94A3B8] dark:hover:text-white"
                    aria-label="Close step details"
                  >
                    <svg
                      className="size-5"
                      fill="none"
                      viewBox="0 0 24 24"
                      stroke="currentColor"
                    >
                      <path
                        strokeLinecap="round"
                        strokeLinejoin="round"
                        strokeWidth={2}
                        d="M6 18L18 6M6 6l12 12"
                      />
                    </svg>
                  </button>
                </div>
              </div>

              <div className="grid min-h-0 flex-1 gap-4 overflow-hidden p-5 md:grid-cols-[minmax(0,0.9fr)_minmax(0,1.35fr)] md:p-6">
                <div className="min-h-0 space-y-4 overflow-auto pr-1">
                  <div className="rounded-[22px] border border-white/70 bg-white/82 p-4 shadow-[inset_0_1px_0_rgba(255,255,255,0.68)] dark:border-[#243041] dark:bg-[rgba(15,23,42,0.72)]">
                    <div className="text-[10px] font-bold uppercase tracking-[0.18em] text-[#94A3B8]">
                      Task Instruction
                    </div>
                    <div className="mt-2 whitespace-pre-wrap select-text text-sm leading-6 text-[#334155] dark:text-[#CBD5E1]">
                      {detailStepNode?.data.instructions?.trim() ||
                        'No task instructions were provided for this step.'}
                    </div>
                  </div>

                  <div className="rounded-[22px] border border-white/70 bg-white/82 p-4 shadow-[inset_0_1px_0_rgba(255,255,255,0.68)] dark:border-[#243041] dark:bg-[rgba(15,23,42,0.72)]">
                    <div className="text-[10px] font-bold uppercase tracking-[0.18em] text-[#94A3B8]">
                      Task Summary
                    </div>
                    <div className="mt-2 whitespace-pre-wrap select-text text-sm leading-6 text-[#334155] dark:text-[#CBD5E1]">
                      {detailStep.summary_text?.trim() ||
                        'No summary has been generated for this step yet.'}
                    </div>
                  </div>

                  <div className="grid gap-3 sm:grid-cols-2">
                    <div className="rounded-[22px] border border-white/70 bg-white/82 p-4 shadow-[inset_0_1px_0_rgba(255,255,255,0.68)] dark:border-[#243041] dark:bg-[rgba(15,23,42,0.72)]">
                      <div className="text-[10px] font-bold uppercase tracking-[0.18em] text-[#94A3B8]">
                        Agent
                      </div>
                      <div className="mt-2 select-text text-sm font-semibold text-[#0F172A] dark:text-white">
                        {resolveStepAgentName(detailStep)}
                      </div>
                    </div>
                    <div className="rounded-[22px] border border-white/70 bg-white/82 p-4 shadow-[inset_0_1px_0_rgba(255,255,255,0.68)] dark:border-[#243041] dark:bg-[rgba(15,23,42,0.72)]">
                      <div className="text-[10px] font-bold uppercase tracking-[0.18em] text-[#94A3B8]">
                        Current Status
                      </div>
                      <div className="mt-2">
                        <span
                          className={cn(
                            'inline-flex select-text rounded-full border px-3 py-1 text-xs font-semibold uppercase tracking-[0.16em]',
                            workflowStatusBadgeClass(detailStep.status)
                          )}
                        >
                          {detailStep.status}
                        </span>
                      </div>
                    </div>
                  </div>
                </div>

                <div className="flex min-h-0 flex-col rounded-[24px] border border-white/70 bg-white/82 p-4 shadow-[inset_0_1px_0_rgba(255,255,255,0.68)] dark:border-[#243041] dark:bg-[rgba(15,23,42,0.72)]">
                  <div className="text-[10px] font-bold uppercase tracking-[0.18em] text-[#94A3B8]">
                    Transcript
                  </div>
                  <div className="mt-3 min-h-0 flex-1 overflow-auto pr-1">
                    <WorkflowTranscriptFeed
                      steps={projection.steps}
                      entries={visibleDetailTranscript}
                      isLoading={isFetchingDetailStepTranscript}
                      emptyMessage={
                        isPreview
                          ? 'Preview mode does not have transcript messages yet.'
                          : 'No transcript messages for this step yet.'
                      }
                      pendingActionId={pendingActionId}
                      onApproval={onApproval}
                    />
                  </div>
                </div>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
