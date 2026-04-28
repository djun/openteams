import {
  ArrowClockwiseIcon,
  ArrowsInSimpleIcon,
  ArrowsOutSimpleIcon,
  MagnifyingGlassMinusIcon,
  MagnifyingGlassPlusIcon,
} from '@phosphor-icons/react';
import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { isRetryableWorkflowStepStatus } from './workflowControlContract';

type WorkflowGraphStep = {
  id: string;
  step_key: string;
  title: string;
  step_type: string;
  status: string;
  agent_name?: string | null;
  summary_text?: string | null;
  content?: string | null;
};

type WorkflowGraphNode = {
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

type WorkflowGraphAgent = {
  session_agent_id: string;
  workflow_agent_session_id?: string | null;
  agent_id: string;
  name: string;
};

type WorkflowGraphEdge = {
  id: string;
  source: string;
  target: string;
};

type WorkflowGraphBoardProps = {
  nodes: WorkflowGraphNode[];
  edges: WorkflowGraphEdge[];
  steps: WorkflowGraphStep[];
  agents?: WorkflowGraphAgent[];
  selectedStepId?: string | null;
  onSelectStep?: (id: string) => void;
  onRetryStep?: (stepId: string) => void;
  pendingActionId?: string | null;
  compact?: boolean;
  className?: string;
};

type LayoutNode = WorkflowGraphNode & {
  step?: WorkflowGraphStep;
  x: number;
  y: number;
  rank: number;
  order: number;
};

type RenderEdge = {
  id: string;
  sourceId: string;
  targetId: string;
  path: string;
};

const MIN_NODE_SCALE = 0.82;
const MAX_NODE_SCALE = 1.36;
const NODE_SCALE_STEP = 0.09;

function statusTone(status?: string | null, selected?: boolean) {
  const base = (() => {
    switch (status) {
      case 'completed':
        return {
          badge: 'bg-[#DCFCE7] text-[#166534]',
          border: '#16A34A',
          accent: 'rgba(34,197,94,0.18)',
          glow: 'shadow-[0_20px_40px_rgba(34,197,94,0.10)]',
        };
      case 'running':
        return {
          badge: 'bg-[#DBEAFE] text-[#1D4ED8]',
          border: '#2563EB',
          accent: 'rgba(59,130,246,0.18)',
          glow: 'shadow-[0_20px_40px_rgba(37,99,235,0.14)]',
        };
      case 'failed':
      case 'interrupted':
        return {
          badge: 'bg-[#FEE2E2] text-[#991B1B]',
          border: '#DC2626',
          accent: 'rgba(239,68,68,0.18)',
          glow: 'shadow-[0_20px_40px_rgba(220,38,38,0.12)]',
        };
      case 'interrupt_requested':
        return {
          badge: 'bg-[#FEF3C7] text-[#92400E]',
          border: '#D97706',
          accent: 'rgba(245,158,11,0.18)',
          glow: 'shadow-[0_20px_40px_rgba(217,119,6,0.12)]',
        };
      case 'ready':
        return {
          badge: 'bg-[#FEF3C7] text-[#92400E]',
          border: '#D97706',
          accent: 'rgba(245,158,11,0.16)',
          glow: 'shadow-[0_20px_40px_rgba(217,119,6,0.12)]',
        };
      case 'waiting_input':
      case 'waiting_review':
        return {
          badge: 'bg-[#E0E7FF] text-[#4338CA]',
          border: '#6366F1',
          accent: 'rgba(99,102,241,0.16)',
          glow: 'shadow-[0_20px_40px_rgba(99,102,241,0.12)]',
        };
      default:
        return {
          badge: 'bg-[#E2E8F0] text-[#334155]',
          border: '#CBD5E1',
          accent: 'rgba(148,163,184,0.14)',
          glow: 'shadow-[0_16px_34px_rgba(15,23,42,0.08)]',
        };
    }
  })();

  return {
    ...base,
    border: selected ? '#1D4ED8' : base.border,
  };
}

function layoutGraph(
  nodes: WorkflowGraphNode[],
  edges: WorkflowGraphEdge[],
  steps: WorkflowGraphStep[],
  compact: boolean,
  nodeScale: number
) {
  if (nodes.length === 0) {
    return null;
  }

  const cardWidth = Math.round((compact ? 184 : 208) * nodeScale);
  const cardHeight = Math.round((compact ? 104 : 118) * nodeScale);
  const horizontalGap = Math.round((compact ? 44 : 60) * nodeScale);
  const verticalGap = Math.round((compact ? 18 : 24) * nodeScale);
  const paddingX = Math.round((compact ? 24 : 36) * nodeScale);
  const paddingY = Math.round((compact ? 24 : 36) * nodeScale);
  const verticalScale = 2;

  const sortedNodes = [...nodes].sort(
    (left, right) =>
      left.position.x - right.position.x ||
      left.position.y - right.position.y ||
      left.id.localeCompare(right.id)
  );
  const originalOrder = new Map(
    sortedNodes.map((node, index) => [node.id, index])
  );
  const stepByKey = new Map(steps.map((step) => [step.step_key, step]));
  const incoming = new Map<string, string[]>();
  const outgoing = new Map<string, string[]>();
  const indegree = new Map<string, number>();

  for (const node of sortedNodes) {
    incoming.set(node.id, []);
    outgoing.set(node.id, []);
    indegree.set(node.id, 0);
  }

  for (const edge of edges) {
    if (!indegree.has(edge.source) || !indegree.has(edge.target)) {
      continue;
    }
    outgoing.get(edge.source)?.push(edge.target);
    incoming.get(edge.target)?.push(edge.source);
    indegree.set(edge.target, (indegree.get(edge.target) ?? 0) + 1);
  }

  const queue = sortedNodes
    .filter((node) => (indegree.get(node.id) ?? 0) === 0)
    .map((node) => node.id);
  queue.sort(
    (left, right) =>
      (originalOrder.get(left) ?? 0) - (originalOrder.get(right) ?? 0)
  );

  const topo: string[] = [];
  while (queue.length > 0) {
    const currentId = queue.shift();
    if (!currentId) {
      continue;
    }
    topo.push(currentId);
    for (const targetId of outgoing.get(currentId) ?? []) {
      const nextIndegree = (indegree.get(targetId) ?? 0) - 1;
      indegree.set(targetId, nextIndegree);
      if (nextIndegree === 0) {
        queue.push(targetId);
        queue.sort(
          (left, right) =>
            (originalOrder.get(left) ?? 0) - (originalOrder.get(right) ?? 0)
        );
      }
    }
  }

  for (const node of sortedNodes) {
    if (!topo.includes(node.id)) {
      topo.push(node.id);
    }
  }

  const rankById = new Map<string, number>();
  for (const nodeId of topo) {
    const rank = Math.max(
      0,
      ...(incoming.get(nodeId) ?? []).map(
        (sourceId) => (rankById.get(sourceId) ?? 0) + 1
      )
    );
    rankById.set(nodeId, rank);
  }

  const groups = new Map<number, string[]>();
  for (const nodeId of topo) {
    const rank = rankById.get(nodeId) ?? 0;
    const group = groups.get(rank) ?? [];
    group.push(nodeId);
    groups.set(rank, group);
  }

  const orderById = new Map<string, number>();
  const ranks = [...groups.keys()].sort((left, right) => left - right);
  for (const rank of ranks) {
    const group = [...(groups.get(rank) ?? [])];
    group.sort((left, right) => {
      const leftIncoming = incoming.get(left) ?? [];
      const rightIncoming = incoming.get(right) ?? [];
      const leftScore =
        leftIncoming.length > 0
          ? leftIncoming.reduce(
              (sum, item) => sum + (orderById.get(item) ?? 0),
              0
            ) / leftIncoming.length
          : (originalOrder.get(left) ?? 0);
      const rightScore =
        rightIncoming.length > 0
          ? rightIncoming.reduce(
              (sum, item) => sum + (orderById.get(item) ?? 0),
              0
            ) / rightIncoming.length
          : (originalOrder.get(right) ?? 0);

      return (
        leftScore - rightScore ||
        (originalOrder.get(left) ?? 0) - (originalOrder.get(right) ?? 0)
      );
    });

    group.forEach((nodeId, index) => {
      orderById.set(nodeId, index);
    });
    groups.set(rank, group);
  }

  const maxRankSize = Math.max(
    ...[...groups.values()].map((group) => group.length)
  );
  const totalHeight =
    paddingY * 2 +
    maxRankSize * cardHeight +
    Math.max(maxRankSize - 1, 0) * verticalGap;

  const layoutNodes: LayoutNode[] = topo.map((nodeId) => {
    const node = sortedNodes.find((item) => item.id === nodeId)!;
    const rank = rankById.get(nodeId) ?? 0;
    const group = groups.get(rank) ?? [];
    const order = group.indexOf(nodeId);
    const groupHeight =
      group.length * cardHeight + Math.max(group.length - 1, 0) * verticalGap;
    const topOffset = paddingY + (totalHeight - paddingY * 2 - groupHeight) / 2;

    return {
      ...node,
      step: stepByKey.get(node.id),
      rank,
      order,
      x: paddingX + rank * (cardWidth + horizontalGap),
      y: (topOffset + order * (cardHeight + verticalGap)) * verticalScale,
    };
  });

  const maxRank = Math.max(...layoutNodes.map((node) => node.rank));
  const totalWidth =
    paddingX * 2 + (maxRank + 1) * cardWidth + maxRank * horizontalGap;

  return {
    nodes: layoutNodes,
    width: totalWidth,
    height: totalHeight * verticalScale,
    cardWidth,
    cardHeight,
  };
}

function buildSmoothStepPath({
  x1,
  y1,
  x2,
  y2,
  laneOffset,
  compact,
}: {
  x1: number;
  y1: number;
  x2: number;
  y2: number;
  laneOffset: number;
  compact: boolean;
}) {
  if (Math.abs(y2 - y1) < 1) {
    return `M ${x1} ${y1} L ${x2} ${y2}`;
  }

  const turnPadding = compact ? 22 : 28;
  const controlX = x1 + (x2 - x1) / 2 + laneOffset;
  const minX = x1 + turnPadding;
  const maxX = x2 - turnPadding;
  const midX = Math.min(maxX, Math.max(minX, controlX));
  const radius = Math.max(
    0,
    Math.min(
      compact ? 10 : 14,
      Math.abs(y2 - y1) / 2,
      Math.abs(x2 - x1) / 2 - 4
    )
  );

  if (midX <= minX || midX >= maxX || radius <= 0) {
    const curveOffset = Math.max((x2 - x1) / 2, compact ? 36 : 48);
    return `M ${x1} ${y1} C ${x1 + curveOffset} ${y1}, ${x2 - curveOffset} ${y2}, ${x2} ${y2}`;
  }

  const directionY = y2 > y1 ? 1 : -1;
  return [
    `M ${x1} ${y1}`,
    `L ${midX - radius} ${y1}`,
    `Q ${midX} ${y1} ${midX} ${y1 + directionY * radius}`,
    `L ${midX} ${y2 - directionY * radius}`,
    `Q ${midX} ${y2} ${midX + radius} ${y2}`,
    `L ${x2} ${y2}`,
  ].join(' ');
}

function clampNodeScale(value: number) {
  return Math.min(MAX_NODE_SCALE, Math.max(MIN_NODE_SCALE, value));
}

export function WorkflowGraphBoard({
  nodes,
  edges,
  steps,
  agents = [],
  selectedStepId = null,
  onSelectStep,
  onRetryStep,
  pendingActionId = null,
  compact = false,
  className,
}: WorkflowGraphBoardProps) {
  const { t } = useTranslation('chat');
  const [hoveredNodeId, setHoveredNodeId] = useState<string | null>(null);
  const [isFullscreen, setIsFullscreen] = useState(false);
  const [nodeScale, setNodeScale] = useState(1);
  useEffect(() => {
    if (!isFullscreen) {
      return;
    }

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        setIsFullscreen(false);
      }
    };

    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [isFullscreen]);
  const layout = useMemo(
    () => layoutGraph(nodes, edges, steps, compact, nodeScale),
    [compact, edges, nodeScale, nodes, steps]
  );
  const nodeById = useMemo(
    () => new Map((layout?.nodes ?? []).map((node) => [node.id, node])),
    [layout]
  );
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
  const emphasizedNodeId = hoveredNodeId ?? selectedStepId ?? null;
  const renderedEdges = useMemo<RenderEdge[]>(() => {
    if (!layout) {
      return [];
    }

    const groupedEdges = new Map<
      string,
      Array<{ edge: WorkflowGraphEdge; source: LayoutNode; target: LayoutNode }>
    >();

    for (const edge of edges) {
      const source = nodeById.get(edge.source);
      const target = nodeById.get(edge.target);
      if (!source || !target) {
        continue;
      }

      const groupKey = `${source.rank}-${target.rank}`;
      const group = groupedEdges.get(groupKey) ?? [];
      group.push({ edge, source, target });
      groupedEdges.set(groupKey, group);
    }

    const next: RenderEdge[] = [];
    for (const group of groupedEdges.values()) {
      group.sort(
        (left, right) =>
          left.source.order - right.source.order ||
          left.target.order - right.target.order ||
          left.edge.id.localeCompare(right.edge.id)
      );

      group.forEach(({ edge, source, target }, index) => {
        const x1 = source.x + layout.cardWidth;
        const y1 = source.y + layout.cardHeight / 2;
        const x2 = target.x;
        const y2 = target.y + layout.cardHeight / 2;
        const laneOffset =
          (index - (group.length - 1) / 2) * (compact ? 12 : 16);

        next.push({
          id: edge.id,
          sourceId: edge.source,
          targetId: edge.target,
          path: buildSmoothStepPath({
            x1,
            y1,
            x2,
            y2,
            laneOffset,
            compact,
          }),
        });
      });
    }

    return next;
  }, [compact, edges, layout, nodeById]);

  if (!layout) {
    return null;
  }

  const decreaseNodeScale = () =>
    setNodeScale((value) => clampNodeScale(value - NODE_SCALE_STEP));
  const increaseNodeScale = () =>
    setNodeScale((value) => clampNodeScale(value + NODE_SCALE_STEP));
  const scaleLabel = `${Math.round(nodeScale * 100)}%`;

  return (
    <div
      className={cn(
        'overflow-auto border border-white/70 bg-[linear-gradient(180deg,rgba(255,255,255,0.95)_0%,rgba(241,245,249,0.92)_100%)] p-4 dark:border-[#243041] dark:bg-[linear-gradient(180deg,rgba(15,23,42,0.88)_0%,rgba(11,16,23,0.94)_100%)]',
        isFullscreen
          ? 'fixed inset-3 z-[80] rounded-[28px] shadow-[0_30px_100px_rgba(15,23,42,0.34)] md:inset-6'
          : 'relative rounded-[28px]',
        className
      )}
    >
      <div className="sticky left-0 top-0 z-20 mb-3 flex w-fit items-center gap-2 rounded-full border border-[#CBD5E1] bg-white/90 p-1 shadow-sm backdrop-blur dark:border-[#334155] dark:bg-[rgba(15,23,42,0.86)]">
        <button
          type="button"
          onClick={decreaseNodeScale}
          disabled={nodeScale <= MIN_NODE_SCALE}
          className="inline-flex size-8 items-center justify-center rounded-full text-[#475569] transition-colors hover:bg-[#F1F5F9] hover:text-[#0F172A] disabled:cursor-not-allowed disabled:text-[#CBD5E1] dark:text-[#CBD5E1] dark:hover:bg-[#1E293B] dark:hover:text-white dark:disabled:text-[#475569]"
          aria-label="Decrease node size"
          title="Decrease node size"
        >
          <MagnifyingGlassMinusIcon className="size-4" weight="bold" />
        </button>
        <div className="min-w-[46px] select-none text-center text-xs font-semibold text-[#334155] dark:text-[#CBD5E1]">
          {scaleLabel}
        </div>
        <button
          type="button"
          onClick={increaseNodeScale}
          disabled={nodeScale >= MAX_NODE_SCALE}
          className="inline-flex size-8 items-center justify-center rounded-full text-[#475569] transition-colors hover:bg-[#F1F5F9] hover:text-[#0F172A] disabled:cursor-not-allowed disabled:text-[#CBD5E1] dark:text-[#CBD5E1] dark:hover:bg-[#1E293B] dark:hover:text-white dark:disabled:text-[#475569]"
          aria-label="Increase node size"
          title="Increase node size"
        >
          <MagnifyingGlassPlusIcon className="size-4" weight="bold" />
        </button>
        <button
          type="button"
          onClick={() => setIsFullscreen((value) => !value)}
          className="inline-flex size-8 items-center justify-center rounded-full text-[#475569] transition-colors hover:bg-[#F1F5F9] hover:text-[#0F172A] dark:text-[#CBD5E1] dark:hover:bg-[#1E293B] dark:hover:text-white"
          aria-label={isFullscreen ? 'Exit fullscreen' : 'Enter fullscreen'}
          title={isFullscreen ? 'Exit fullscreen' : 'Enter fullscreen'}
        >
          {isFullscreen ? (
            <ArrowsInSimpleIcon className="size-4" weight="bold" />
          ) : (
            <ArrowsOutSimpleIcon className="size-4" weight="bold" />
          )}
        </button>
      </div>
      <div
        className="relative"
        style={{ width: layout.width, height: layout.height }}
      >
        <svg
          className="pointer-events-none absolute inset-0"
          width={layout.width}
          height={layout.height}
          viewBox={`0 0 ${layout.width} ${layout.height}`}
        >
          {renderedEdges.map((edge) => {
            const isHighlighted =
              !!emphasizedNodeId &&
              (edge.sourceId === emphasizedNodeId ||
                edge.targetId === emphasizedNodeId);
            return (
              <g key={edge.id}>
                <path
                  d={edge.path}
                  fill="none"
                  stroke={
                    isHighlighted
                      ? 'rgba(37,99,235,0.22)'
                      : 'rgba(148,163,184,0.14)'
                  }
                  strokeWidth={(compact ? 8 : 10) * nodeScale}
                  strokeLinecap="round"
                  strokeLinejoin="round"
                />
                <path
                  d={edge.path}
                  fill="none"
                  stroke={isHighlighted ? '#2563EB' : '#94A3B8'}
                  strokeWidth={
                    (isHighlighted
                      ? compact
                        ? 3
                        : 3.5
                      : compact
                        ? 2.25
                        : 2.75) * nodeScale
                  }
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  opacity={isHighlighted ? 1 : 0.88}
                />
              </g>
            );
          })}
        </svg>

        {layout.nodes.map((node) => {
          const step = node.step;
          const retryStepId = step?.id ?? null;
          const canRetryStep =
            !!onRetryStep &&
            !!retryStepId &&
            isRetryableWorkflowStepStatus(step?.status);
          const isRetryPending =
            !!retryStepId && pendingActionId === retryStepId;
          const tone = statusTone(
            step?.status ?? node.data.status,
            node.id === selectedStepId
          );
          const stepAgentLabel = step?.agent_name?.trim();
          const agentName =
            (stepAgentLabel
              ? (agentNameByLookup.get(stepAgentLabel) ?? stepAgentLabel)
              : null) ??
            (node.data.agentId
              ? (agentNameByLookup.get(node.data.agentId.trim()) ?? null)
              : null) ??
            'Lead';

          return (
            <div
              key={node.id}
              role={onSelectStep ? 'button' : undefined}
              tabIndex={onSelectStep ? 0 : -1}
              onClick={() => onSelectStep?.(node.id)}
              onKeyDown={(event) => {
                if (!onSelectStep) {
                  return;
                }
                if (event.key === 'Enter' || event.key === ' ') {
                  event.preventDefault();
                  onSelectStep(node.id);
                }
              }}
              onMouseEnter={() => setHoveredNodeId(node.id)}
              onMouseLeave={() =>
                setHoveredNodeId((prev) => (prev === node.id ? null : prev))
              }
              onFocus={() => setHoveredNodeId(node.id)}
              onBlur={() =>
                setHoveredNodeId((prev) => (prev === node.id ? null : prev))
              }
              className={cn(
                'absolute flex flex-col rounded-[26px] border bg-white/92 text-left transition-all duration-200 hover:-translate-y-0.5 hover:bg-white dark:bg-[rgba(15,23,42,0.92)] dark:hover:bg-[rgba(15,23,42,0.98)]',
                compact ? 'p-3' : 'p-3.5',
                onSelectStep && 'cursor-pointer',
                tone.glow,
                (node.id === selectedStepId || node.id === hoveredNodeId) &&
                  'ring-2 ring-[#60A5FA]/70'
              )}
              style={{
                left: node.x,
                top: node.y,
                width: layout.cardWidth,
                height: layout.cardHeight,
                borderColor: tone.border,
                boxShadow: `inset 0 1px 0 rgba(255,255,255,0.7), 0 0 0 1px ${tone.accent}`,
              }}
            >
              <div className="flex items-start justify-between gap-2">
                <div className="min-w-0 flex-1">
                  <div className="line-clamp-2 text-sm font-semibold leading-5 text-[#0F172A] dark:text-white">
                    {step?.title ?? node.data.title}
                  </div>
                </div>
                <span
                  className={cn(
                    'shrink-0 rounded-full px-2 py-1 text-[10px] font-bold uppercase tracking-[0.16em]',
                    tone.badge
                  )}
                >
                  {step?.status ?? node.data.status ?? 'pending'}
                </span>
              </div>

              <div className="mt-auto flex items-end justify-between gap-2">
                <div className="min-w-0 flex-1">
                  <div className="text-[10px] font-bold uppercase tracking-[0.16em] text-[#94A3B8]">
                    Agent
                  </div>
                  <div className="min-w-0">
                    <div className="mt-1 truncate text-xs font-semibold text-[#0F172A] dark:text-white">
                      {agentName}
                    </div>
                  </div>
                </div>
                {canRetryStep ? (
                  <button
                    type="button"
                    onClick={(event) => {
                      event.stopPropagation();
                      if (retryStepId) {
                        onRetryStep?.(retryStepId);
                      }
                    }}
                    disabled={isRetryPending}
                    className="inline-flex shrink-0 items-center gap-1 rounded-full bg-[#DC2626] px-2.5 py-1 text-[10px] font-semibold text-white transition-colors hover:bg-[#B91C1C] disabled:cursor-not-allowed disabled:bg-[#FCA5A5] disabled:text-white/90"
                    aria-label={t('workflow_retry', {
                      defaultValue: '重试',
                    })}
                  >
                    <ArrowClockwiseIcon
                      className={cn(
                        'size-3.5',
                        isRetryPending && 'animate-spin'
                      )}
                      weight="bold"
                    />
                    <span>
                      {t('workflow_retry', {
                        defaultValue: '重试',
                      })}
                    </span>
                  </button>
                ) : null}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
