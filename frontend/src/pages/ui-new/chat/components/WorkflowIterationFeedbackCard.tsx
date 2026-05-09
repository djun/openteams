import { useEffect, useMemo, useState } from 'react';
import { ChevronUp, ChevronDown } from 'lucide-react';
import type { WorkflowIterationSummaryData } from '@/lib/api';
import { cn } from '@/lib/utils';

type WorkflowIterationFeedbackPayload = {
  action: 'accept' | 'reject';
  feedback?: {
    what_wrong: string;
    expected: string;
    priority: 'high' | 'medium' | 'low';
    additional_notes?: string;
  };
};

type WorkflowIterationFeedbackCardProps = {
  currentRound: number;
  completedSteps: number;
  totalSteps: number;
  runningStepTitle?: string | null;
  iterationHistory: WorkflowIterationSummaryData[];
  canReviewCurrentRound?: boolean;
  pendingActionId?: string | null;
  onSubmit?: (payload: WorkflowIterationFeedbackPayload) => void;
};

export function WorkflowIterationFeedbackCard({
  currentRound,
  completedSteps,
  totalSteps,
  runningStepTitle,
  iterationHistory,
  canReviewCurrentRound: canReviewCurrentRoundProp = false,
  pendingActionId,
  onSubmit,
}: WorkflowIterationFeedbackCardProps) {
  const [isCollapsed, setIsCollapsed] = useState(false);
  const [showReview, setShowReview] = useState(false);
  const [expandedReject, setExpandedReject] = useState(false);
  const [whatWrong, setWhatWrong] = useState('');
  const [expected, setExpected] = useState('');
  const [priority, setPriority] = useState<'high' | 'medium' | 'low'>('high');
  const [additionalNotes, setAdditionalNotes] = useState('');
  const [validationError, setValidationError] = useState<string | null>(null);

  const orderedHistory = useMemo(
    () =>
      [...iterationHistory].sort(
        (left, right) => right.round_index - left.round_index
      ),
    [iterationHistory]
  );

  const latestIteration = orderedHistory[0] ?? null;
  const canReviewCurrentRound =
    canReviewCurrentRoundProp &&
    currentRound > 0 &&
    latestIteration?.round_index === currentRound;
  const canSubmit = !!onSubmit;
  const disabled = !!pendingActionId;

  useEffect(() => {
    if (canReviewCurrentRound) {
      setShowReview(true);
    }
  }, [canReviewCurrentRound]);

  const handleAccept = () => {
    setExpandedReject(false);
    setValidationError(null);
    onSubmit?.({ action: 'accept' });
  };

  const handleReject = () => {
    if (!expandedReject) {
      setExpandedReject(true);
      return;
    }
    const nextWhatWrong = whatWrong.trim();
    const nextExpected = expected.trim();
    if (!nextWhatWrong || !nextExpected) {
      setValidationError('Reject requires what_wrong and expected.');
      return;
    }
    setValidationError(null);
    onSubmit?.({
      action: 'reject',
      feedback: {
        what_wrong: nextWhatWrong,
        expected: nextExpected,
        priority,
        additional_notes: additionalNotes.trim() || undefined,
      },
    });
  };

  const progressPercent =
    totalSteps > 0 ? Math.round((completedSteps / totalSteps) * 100) : 0;

  if (isCollapsed) {
    return (
      <button
        type="button"
        onClick={() => setIsCollapsed(false)}
        className="flex items-center gap-3 bg-white border border-slate-200 rounded-full px-4 py-1.5 shadow-sm hover:border-blue-400 transition-all group"
        title={`Round ${currentRound} · ${completedSteps}/${totalSteps} completed${runningStepTitle ? ` · Running: ${runningStepTitle}` : ''}`}
      >
        <div className="flex items-center gap-1.5">
          <div className={cn(
            "w-2 h-2 bg-blue-500 rounded-full",
            runningStepTitle && "animate-pulse"
          )} />
          <span className="text-xs font-bold text-slate-700">R{currentRound}</span>
        </div>
        <div className="h-3 w-[1px] bg-slate-200" />
        <span className="text-xs font-medium text-slate-600">{completedSteps}/{totalSteps} Steps</span>
        <div className="h-3 w-[1px] bg-slate-200" />
        <span className="text-xs font-bold text-blue-600">{progressPercent}%</span>
      </button>
    );
  }

  return (
    <div className="bg-white border border-slate-200 rounded-2xl shadow-sm overflow-hidden transition-all duration-300 hover:border-blue-200 max-w-md w-full">
      {/* Header/Expandable Area */}
      <button
        type="button"
        onClick={() => setIsCollapsed(true)}
        className="w-full text-left p-3.5 focus:outline-none group hover:bg-slate-50/50 transition-colors"
      >
        <div className="flex items-center gap-3 mb-2.5">
          <div className="bg-blue-50 text-blue-600 px-2 py-0.5 rounded-lg text-[10px] font-bold tracking-tight border border-blue-100 uppercase shrink-0">
            Round {currentRound}
          </div>
          <div className="flex-1 h-1.5 bg-slate-100 rounded-full overflow-hidden relative">
            <div
              className="h-full bg-blue-500 rounded-full shadow-[0_0_8px_rgba(59,130,246,0.5)] transition-all duration-500"
              style={{ width: `${progressPercent}%` }}
            />
          </div>
          <span className="text-[10px] font-bold text-blue-600 shrink-0">{progressPercent}%</span>
        </div>

        <div className="flex items-center justify-between">
          <div className="flex items-center gap-4">
            <div className="flex flex-col">
              <span className="text-[10px] text-slate-400 uppercase font-medium">Steps</span>
              <span className="text-xs font-bold text-slate-700">{completedSteps} / {totalSteps}</span>
            </div>
            <div className="h-6 w-[1px] bg-slate-100" />
            <div className="flex flex-col">
              <span className="text-[10px] text-slate-400 uppercase font-medium">Status</span>
              <div className="flex items-center gap-1.5">
                {runningStepTitle ? (
                  <>
                    <div className="w-2 h-2 bg-emerald-500 rounded-full animate-pulse shadow-[0_0_8px_rgba(16,185,129,0.5)]" />
                    <span className="text-xs font-bold text-emerald-600">Running</span>
                  </>
                ) : (
                  <>
                    <div className="w-2 h-2 bg-slate-300 rounded-full" />
                    <span className="text-xs font-bold text-slate-400">Idle</span>
                  </>
                )}
              </div>
            </div>
          </div>
          <ChevronUp className="w-4 h-4 text-slate-300 group-hover:text-blue-500 transition-colors" />
        </div>

        {runningStepTitle && (
          <div className="mt-3 py-2 px-3 bg-slate-50 rounded-xl border border-slate-100">
            <span className="text-[10px] text-slate-400 block mb-0.5 uppercase">Current Step</span>
            <p className="text-xs text-slate-600 font-medium truncate">{runningStepTitle}</p>
          </div>
        )}
      </button>

      {/* Review Section */}
      {canReviewCurrentRound && showReview && (
        <div className={cn(
          "border-t transition-all duration-300",
          expandedReject ? "bg-rose-50/50 border-rose-100 p-4" : "bg-indigo-50/50 border-indigo-100 p-4"
        )}>
          <div className="flex items-center justify-between mb-4">
            <div className="flex items-center gap-2">
              <div className={cn(
                "w-1.5 h-1.5 rounded-full",
                expandedReject ? "bg-rose-500" : "bg-indigo-500"
              )} />
              <span className={cn(
                "text-[10px] font-bold uppercase tracking-wider",
                expandedReject ? "text-rose-700" : "text-indigo-700"
              )}>
                {expandedReject ? "Reject with Feedback" : "Review Required"}
              </span>
            </div>
            <button
              type="button"
              onClick={() => setShowReview(false)}
              className="text-slate-400 hover:text-slate-600 transition-colors"
            >
              <ChevronDown className="w-4 h-4" />
            </button>
          </div>

          {expandedReject && (
            <div className="space-y-3 mb-4">
              <div>
                <label className="text-[10px] font-bold text-slate-400 uppercase mb-1 block">What went wrong?</label>
                <textarea
                  value={whatWrong}
                  onChange={(e) => setWhatWrong(e.target.value)}
                  rows={2}
                  disabled={disabled || !canSubmit}
                  placeholder="Describe the issue..."
                  className="w-full bg-white border border-slate-200 rounded-xl p-3 text-xs text-slate-700 outline-none focus:ring-2 focus:ring-rose-200 focus:border-rose-300 transition-all placeholder:text-slate-300 disabled:opacity-60"
                />
              </div>
              <div>
                <label className="text-[10px] font-bold text-slate-400 uppercase mb-1 block">Expected outcome</label>
                <textarea
                  value={expected}
                  onChange={(e) => setExpected(e.target.value)}
                  rows={2}
                  disabled={disabled || !canSubmit}
                  placeholder="What should have happened?"
                  className="w-full bg-white border border-slate-200 rounded-xl p-3 text-xs text-slate-700 outline-none focus:ring-2 focus:ring-rose-200 focus:border-rose-300 transition-all placeholder:text-slate-300 disabled:opacity-60"
                />
              </div>
              <div className="flex gap-3">
                <div className="flex-1">
                  <label className="text-[10px] font-bold text-slate-400 uppercase mb-1 block">Priority</label>
                  <select
                    value={priority}
                    onChange={(e) => setPriority(e.target.value as 'high' | 'medium' | 'low')}
                    disabled={disabled || !canSubmit}
                    className="w-full bg-white border border-slate-200 rounded-xl px-3 py-2 text-xs text-slate-700 outline-none focus:ring-2 focus:ring-rose-200 focus:border-rose-300 disabled:opacity-60"
                  >
                    <option value="high">High</option>
                    <option value="medium">Medium</option>
                    <option value="low">Low</option>
                  </select>
                </div>
              </div>
              <div>
                <label className="text-[10px] font-bold text-slate-400 uppercase mb-1 block">Additional Notes</label>
                <textarea
                  value={additionalNotes}
                  onChange={(e) => setAdditionalNotes(e.target.value)}
                  rows={2}
                  disabled={disabled || !canSubmit}
                  placeholder="Optional notes..."
                  className="w-full bg-white border border-slate-200 rounded-xl p-3 text-xs text-slate-700 outline-none focus:ring-2 focus:ring-rose-200 focus:border-rose-300 transition-all placeholder:text-slate-300 disabled:opacity-60"
                />
              </div>
              {validationError && (
                <div className="text-[10px] text-rose-600 font-medium">
                  {validationError}
                </div>
              )}
            </div>
          )}

          <div className="flex gap-3">
            {!expandedReject && (
              <button
                type="button"
                onClick={handleAccept}
                disabled={disabled || !canSubmit}
                className="flex-1 bg-indigo-50 border border-indigo-100 text-indigo-700 py-2.5 rounded-xl text-xs font-bold hover:bg-indigo-100 hover:border-indigo-200 transition-all active:scale-95 disabled:opacity-50 disabled:active:scale-100 shadow-sm"
              >
                ACCEPT
              </button>
            )}
            <button
              type="button"
              onClick={handleReject}
              disabled={disabled || !canSubmit}
              className={cn(
                "flex-1 py-2.5 rounded-xl text-xs font-bold transition-all active:scale-95 disabled:opacity-50 disabled:active:scale-100 shadow-sm",
                expandedReject
                  ? "bg-rose-50 border border-rose-100 text-rose-700 hover:bg-rose-100 hover:border-rose-200"
                  : "bg-white border border-slate-200 text-slate-500 hover:bg-slate-50 hover:text-slate-700"
              )}
            >
              {expandedReject ? 'SUBMIT REJECTION' : 'REJECT'}
            </button>
            {expandedReject && (
              <button
                type="button"
                onClick={() => {
                  setExpandedReject(false);
                  setValidationError(null);
                }}
                className="px-4 bg-white border border-slate-200 text-slate-400 py-2.5 rounded-xl text-xs font-bold hover:bg-slate-50 hover:text-slate-600 transition-all"
              >
                CANCEL
              </button>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

