import type { WorkflowCardData } from '@/lib/api';

type WorkflowStepLike = Pick<
  WorkflowCardData['steps'][number],
  'status' | 'step_key'
>;

type WorkflowProjectionLike = Pick<
  WorkflowCardData,
  'execution_status' | 'steps' | 'plan'
>;

function hasRecoverablePendingStep(
  steps: WorkflowStepLike[],
  edges: WorkflowCardData['plan']['edges']
) {
  const stepByKey = new Map(steps.map((step) => [step.step_key, step]));

  return steps.some((step) => {
    if (step.status !== 'pending') {
      return false;
    }

    const incomingEdges = edges.filter((edge) => edge.target === step.step_key);
    return incomingEdges.every(
      (edge) => stepByKey.get(edge.source)?.status === 'completed'
    );
  });
}

export function isRetryableWorkflowStepStatus(status?: string | null) {
  return status === 'failed' || status === 'interrupted';
}

export function isWorkflowExecutionRecompiling(
  projection: Pick<WorkflowProjectionLike, 'execution_status'>
) {
  return projection.execution_status === 'recompiling';
}

export function canPauseWorkflowExecution(projection: WorkflowProjectionLike) {
  return projection.execution_status === 'running';
}

export function canResumeWorkflowExecution(projection: WorkflowProjectionLike) {
  if (projection.execution_status === 'paused') {
    return true;
  }

  if (projection.execution_status !== 'failed') {
    return false;
  }

  return (
    projection.steps.some((step) => step.status === 'ready') ||
    hasRecoverablePendingStep(projection.steps, projection.plan.edges)
  );
}
