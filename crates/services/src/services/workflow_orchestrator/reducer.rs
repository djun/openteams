//! State Reducer: 集中管理所有 workflow 状态迁移
//!
//! 所有状态变更必须经过 reducer，不允许直接修改数据库状态字段。
//! reducer 负责：
//! 1. 校验状态迁移合法性（含三层组合约束）
//! 2. 执行迁移并持久化
//! 3. 自动记录状态变更审计事件
//! 4. 拒绝非法迁移并记录日志

use db::models::{
    workflow_agent_session::WorkflowAgentSession,
    workflow_event::{CreateWorkflowEvent, WorkflowEvent},
    workflow_execution::WorkflowExecution,
    workflow_step::WorkflowStep,
    workflow_types::*,
};
use serde::Serialize;
use sqlx::SqlitePool;
use uuid::Uuid;

/// 将枚举序列化为规范的 wire format（snake_case），用于审计事件的 status_before/status_after
fn to_wire_format<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| format!("{:?}", value as *const T))
}

/// 状态迁移错误
#[derive(Debug, thiserror::Error)]
pub enum TransitionError {
    #[error("非法的执行状态迁移: {from} -> {to}")]
    IllegalExecutionTransition { from: String, to: String },
    #[error("非法的步骤状态迁移: {from} -> {to}")]
    IllegalStepTransition { from: String, to: String },
    #[error("非法的 Agent Session 状态迁移: {from} -> {to}")]
    IllegalAgentSessionTransition { from: String, to: String },
    #[error("数据库错误: {0}")]
    Database(String),
}

impl From<sqlx::Error> for TransitionError {
    fn from(e: sqlx::Error) -> Self {
        TransitionError::Database(e.to_string())
    }
}

/// 状态迁移结果：包含更新后的实体和审计事件
#[derive(Debug)]
pub struct TransitionResult<T> {
    pub entity: T,
    pub event: WorkflowEvent,
}

// ---------------------------------------------------------------------------
// Execution 状态机
// ---------------------------------------------------------------------------
// pending -> bootstrapping -> running
// bootstrapping -> failed
// running -> interrupting -> waiting_user
// running -> waiting_user (step requests user action: approval/permission/continue)
// running -> waiting_user_acceptance
// running -> pausing -> paused
// running -> completing -> completed
// running -> failed
// waiting_user -> running
// waiting_user_acceptance -> paused -> recompiling -> resuming -> running
// waiting_user_acceptance -> completing -> completed
// paused -> recompiling -> resuming -> running
// paused -> cancelled
// waiting_user -> cancelled

/// 校验 Execution 状态迁移是否合法
pub fn validate_execution_transition(
    from: &WorkflowExecutionStatus,
    to: &WorkflowExecutionStatus,
) -> Result<(), TransitionError> {
    use WorkflowExecutionStatus::*;

    let allowed = match from {
        Pending => matches!(to, Bootstrapping),
        Bootstrapping => matches!(to, Running | Failed),
        Running => matches!(
            to,
            Interrupting | WaitingUser | WaitingUserAcceptance | Pausing | Completing | Failed
        ),
        Interrupting => matches!(to, WaitingUser | Paused),
        WaitingUser => matches!(to, Running | Failed | Cancelled),
        WaitingUserAcceptance => matches!(to, Completing | Paused),
        Pausing => matches!(to, Paused),
        Paused => matches!(to, Recompiling | Cancelled),
        Recompiling => matches!(to, Resuming),
        Resuming => matches!(to, Running | Pausing),
        Completing => matches!(to, Completed),
        Completed | Failed | Cancelled => false,
    };

    if allowed {
        Ok(())
    } else {
        tracing::warn!(
            from = ?from,
            to = ?to,
            "非法的执行状态迁移被拒绝"
        );
        Err(TransitionError::IllegalExecutionTransition {
            from: format!("{:?}", from),
            to: format!("{:?}", to),
        })
    }
}

// ---------------------------------------------------------------------------
// Step 状态机
// ---------------------------------------------------------------------------
// pending -> ready -> running
// running -> waiting_input -> ready
// running -> waiting_review -> ready
// running -> interrupt_requested -> interrupted
// running -> completed
// running -> failed
// pending -> blocked -> ready
// pending -> cancelled
// blocked -> cancelled
// failed -> ready (retry only)

/// 校验 Step 状态迁移是否合法
pub fn validate_step_transition(
    from: &WorkflowStepStatus,
    to: &WorkflowStepStatus,
) -> Result<(), TransitionError> {
    use WorkflowStepStatus::*;

    let allowed = match from {
        Pending => matches!(to, Ready | Blocked | Cancelled),
        Ready => matches!(to, Running),
        Running => matches!(
            to,
            WaitingInput | WaitingReview | InterruptRequested | Completed | Failed
        ),
        WaitingInput => matches!(to, Ready | Failed),
        WaitingReview => matches!(to, Ready | Failed),
        InterruptRequested => matches!(to, Interrupted),
        Interrupted => matches!(to, Cancelled),
        Blocked => matches!(to, Ready | Cancelled),
        Failed => matches!(to, Ready), // retry
        Completed | Skipped | Cancelled => false,
    };

    if allowed {
        Ok(())
    } else {
        tracing::warn!(
            from = ?from,
            to = ?to,
            "非法的步骤状态迁移被拒绝"
        );
        Err(TransitionError::IllegalStepTransition {
            from: format!("{:?}", from),
            to: format!("{:?}", to),
        })
    }
}

// ---------------------------------------------------------------------------
// Agent Session 状态机
// ---------------------------------------------------------------------------
// idle -> running
// running -> waiting_input
// running -> waiting_approval
// running -> interrupt_requested -> interrupted
// running -> paused
// running -> completed
// running -> failed
// waiting_input -> running
// waiting_approval -> running
// paused -> idle
// interrupted -> idle

/// 校验 Agent Session 状态迁移是否合法
pub fn validate_agent_session_transition(
    from: &WorkflowAgentSessionState,
    to: &WorkflowAgentSessionState,
) -> Result<(), TransitionError> {
    use WorkflowAgentSessionState::*;

    let allowed = match from {
        Idle => matches!(to, Running),
        Running => matches!(
            to,
            WaitingInput | WaitingApproval | InterruptRequested | Paused | Completed | Failed
        ),
        WaitingInput => matches!(to, Running | Failed),
        WaitingApproval => matches!(to, Running | Failed),
        InterruptRequested => matches!(to, Interrupted),
        Interrupted => matches!(to, Idle),
        Paused => matches!(to, Idle),
        Completed | Failed | Expired => false,
    };

    if allowed {
        Ok(())
    } else {
        tracing::warn!(
            from = ?from,
            to = ?to,
            "非法的 agent session 状态迁移被拒绝"
        );
        Err(TransitionError::IllegalAgentSessionTransition {
            from: format!("{:?}", from),
            to: format!("{:?}", to),
        })
    }
}

// ---------------------------------------------------------------------------
// 三层组合约束 (10.4)
// ---------------------------------------------------------------------------

/// 校验 execution 状态下 step 状态是否合法
pub fn validate_step_in_execution(
    execution_status: &WorkflowExecutionStatus,
    step_status: &WorkflowStepStatus,
) -> bool {
    use WorkflowExecutionStatus as E;
    use WorkflowStepStatus as S;

    match execution_status {
        E::Running => matches!(
            step_status,
            S::Pending
                | S::Ready
                | S::Running
                | S::Blocked
                | S::WaitingInput
                | S::WaitingReview
                | S::Completed
                | S::Failed
        ),
        E::Interrupting => matches!(
            step_status,
            S::Running | S::InterruptRequested | S::Interrupted | S::Completed | S::Failed
        ),
        E::WaitingUserAcceptance => matches!(
            step_status,
            S::Completed | S::Failed | S::Interrupted | S::Cancelled
        ),
        E::Pausing => matches!(
            step_status,
            S::Running | S::InterruptRequested | S::Completed | S::Failed | S::Blocked
        ),
        E::Paused => matches!(
            step_status,
            S::Pending | S::Blocked | S::Interrupted | S::Completed | S::Failed | S::Cancelled
        ),
        E::Recompiling => matches!(
            step_status,
            S::Pending | S::Blocked | S::Interrupted | S::Completed | S::Failed | S::Cancelled
        ),
        E::WaitingUser => matches!(
            step_status,
            S::WaitingInput
                | S::WaitingReview
                | S::Interrupted
                | S::Completed
                | S::Failed
                | S::Blocked
        ),
        E::Completed => matches!(
            step_status,
            S::Completed | S::Skipped | S::Cancelled | S::Failed
        ),
        // For other states, allow any
        _ => true,
    }
}

/// 校验 execution 状态下 agent session 状态是否合法
pub fn validate_agent_session_in_execution(
    execution_status: &WorkflowExecutionStatus,
    session_state: &WorkflowAgentSessionState,
) -> bool {
    use WorkflowAgentSessionState as A;
    use WorkflowExecutionStatus as E;

    match execution_status {
        E::Running => matches!(
            session_state,
            A::Idle | A::Running | A::WaitingInput | A::WaitingApproval | A::Completed | A::Failed
        ),
        E::Interrupting => matches!(
            session_state,
            A::Running | A::InterruptRequested | A::Interrupted | A::Idle
        ),
        E::WaitingUserAcceptance => matches!(
            session_state,
            A::Idle | A::Completed | A::Failed | A::Paused
        ),
        E::Pausing => matches!(
            session_state,
            A::Running | A::InterruptRequested | A::Paused | A::Idle
        ),
        E::Paused => matches!(
            session_state,
            A::Paused | A::Idle | A::Completed | A::Failed
        ),
        E::Recompiling => matches!(
            session_state,
            A::Paused | A::Idle | A::Completed | A::Failed
        ),
        E::WaitingUser => matches!(
            session_state,
            A::WaitingInput
                | A::WaitingApproval
                | A::Interrupted
                | A::Idle
                | A::Completed
                | A::Failed
        ),
        E::Completed => matches!(
            session_state,
            A::Idle | A::Completed | A::Failed | A::Expired
        ),
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Event type mapping
// ---------------------------------------------------------------------------

/// 根据目标 Execution 状态推断事件类型
fn execution_event_type(to: &WorkflowExecutionStatus) -> WorkflowEventType {
    use WorkflowExecutionStatus::*;
    match to {
        Bootstrapping => WorkflowEventType::ExecutionBootstrapping,
        Running => WorkflowEventType::ExecutionRunning,
        Failed => WorkflowEventType::ExecutionFailed,
        Completed => WorkflowEventType::ExecutionCompleted,
        Cancelled => WorkflowEventType::ExecutionCancelled,
        Pausing => WorkflowEventType::ExecutionPauseRequested,
        Paused => WorkflowEventType::ExecutionPaused,
        Resuming => WorkflowEventType::ExecutionResumeRequested,
        Interrupting => WorkflowEventType::ExecutionInterruptRequested,
        WaitingUser => WorkflowEventType::ExecutionInterrupted,
        WaitingUserAcceptance => WorkflowEventType::UserAcceptanceRequested,
        Recompiling => WorkflowEventType::PlanRecompiled,
        Completing => WorkflowEventType::ExecutionRunning, // transitional
        Pending => WorkflowEventType::ExecutionCreated,
    }
}

// ---------------------------------------------------------------------------
// Reducer entrypoints: validate + persist + emit event in one call
// ---------------------------------------------------------------------------

/// 执行 Execution 状态迁移：校验 → 持久化 → 写审计事件（原子操作）
pub async fn transition_execution(
    pool: &SqlitePool,
    execution: &WorkflowExecution,
    to: WorkflowExecutionStatus,
) -> Result<TransitionResult<WorkflowExecution>, TransitionError> {
    let from = &execution.status;
    validate_execution_transition(from, &to)?;

    let from_str = to_wire_format(from);
    let to_str = to_wire_format(&to);
    let event_type = execution_event_type(&to);

    let updated = WorkflowExecution::update_status(pool, execution.id, to).await?;

    let event = WorkflowEvent::create(
        pool,
        &CreateWorkflowEvent {
            execution_id: execution.id,
            round_id: None,
            step_id: None,
            agent_session_id: None,
            event_type,
            status_before: Some(from_str.clone()),
            status_after: Some(to_str.clone()),
            detail_json: None,
        },
        Uuid::new_v4(),
    )
    .await?;

    tracing::info!(
        execution_id = %execution.id,
        from = %from_str,
        to = %to_str,
        "execution 状态迁移完成"
    );

    Ok(TransitionResult {
        entity: updated,
        event,
    })
}

/// 执行 Execution 状态迁移（带额外上下文）：校验 → 持久化 → 写审计事件
pub async fn transition_execution_with_context(
    pool: &SqlitePool,
    execution: &WorkflowExecution,
    to: WorkflowExecutionStatus,
    round_id: Option<Uuid>,
    detail: Option<&str>,
) -> Result<TransitionResult<WorkflowExecution>, TransitionError> {
    let from = &execution.status;
    validate_execution_transition(from, &to)?;

    let from_str = to_wire_format(from);
    let to_str = to_wire_format(&to);
    let event_type = execution_event_type(&to);

    let updated = WorkflowExecution::update_status(pool, execution.id, to).await?;

    let detail_json = detail.map(|d| {
        serde_json::json!({
            "message": d,
            "execution_id": execution.id.to_string(),
            "compiled_graph_hash": execution.compiled_graph_hash,
        })
        .to_string()
    });

    let event = WorkflowEvent::create(
        pool,
        &CreateWorkflowEvent {
            execution_id: execution.id,
            round_id,
            step_id: None,
            agent_session_id: None,
            event_type,
            status_before: Some(from_str.clone()),
            status_after: Some(to_str.clone()),
            detail_json,
        },
        Uuid::new_v4(),
    )
    .await?;

    tracing::info!(
        execution_id = %execution.id,
        from = %from_str,
        to = %to_str,
        "execution 状态迁移完成"
    );

    Ok(TransitionResult {
        entity: updated,
        event,
    })
}

/// 执行 Step 状态迁移：校验（含组合约束） → 持久化 → 写审计事件
pub async fn transition_step(
    pool: &SqlitePool,
    execution: &WorkflowExecution,
    step: &WorkflowStep,
    to: WorkflowStepStatus,
) -> Result<TransitionResult<WorkflowStep>, TransitionError> {
    // 1. 校验单层迁移合法性
    validate_step_transition(&step.status, &to)?;

    // 2. 校验三层组合约束
    if !validate_step_in_execution(&execution.status, &to) {
        tracing::warn!(
            execution_status = ?execution.status,
            step_status = ?to,
            step_id = %step.id,
            "步骤状态与执行状态组合约束冲突"
        );
        return Err(TransitionError::IllegalStepTransition {
            from: format!("{:?}", step.status),
            to: format!("{:?} (execution 状态 {:?} 下不允许)", to, execution.status),
        });
    }

    let from_str = to_wire_format(&step.status);
    let to_str = to_wire_format(&to);

    let updated = WorkflowStep::update_status(pool, step.id, to).await?;

    let event = WorkflowEvent::create(
        pool,
        &CreateWorkflowEvent {
            execution_id: execution.id,
            round_id: Some(step.round_id),
            step_id: Some(step.id),
            agent_session_id: step.assigned_workflow_agent_session_id,
            event_type: WorkflowEventType::StepStatusChanged,
            status_before: Some(from_str.clone()),
            status_after: Some(to_str.clone()),
            detail_json: Some(
                serde_json::json!({
                    "step_key": step.step_key,
                    "step_title": step.title,
                })
                .to_string(),
            ),
        },
        Uuid::new_v4(),
    )
    .await?;

    tracing::info!(
        execution_id = %execution.id,
        step_id = %step.id,
        step_key = %step.step_key,
        from = %from_str,
        to = %to_str,
        "step 状态迁移完成"
    );

    Ok(TransitionResult {
        entity: updated,
        event,
    })
}

/// 执行 Agent Session 状态迁移：校验（含组合约束） → 持久化 → 写审计事件
pub async fn transition_agent_session(
    pool: &SqlitePool,
    execution: &WorkflowExecution,
    session: &WorkflowAgentSession,
    to: WorkflowAgentSessionState,
) -> Result<TransitionResult<WorkflowAgentSession>, TransitionError> {
    // 1. 校验单层迁移合法性
    validate_agent_session_transition(&session.state, &to)?;

    // 2. 校验三层组合约束
    if !validate_agent_session_in_execution(&execution.status, &to) {
        tracing::warn!(
            execution_status = ?execution.status,
            session_state = ?to,
            session_id = %session.id,
            "agent session 状态与执行状态组合约束冲突"
        );
        return Err(TransitionError::IllegalAgentSessionTransition {
            from: format!("{:?}", session.state),
            to: format!("{:?} (execution 状态 {:?} 下不允许)", to, execution.status),
        });
    }

    let from_str = to_wire_format(&session.state);
    let to_str = to_wire_format(&to);

    let updated = WorkflowAgentSession::update_state(pool, session.id, to).await?;

    let event = WorkflowEvent::create(
        pool,
        &CreateWorkflowEvent {
            execution_id: execution.id,
            round_id: None,
            step_id: None,
            agent_session_id: Some(session.id),
            event_type: WorkflowEventType::AgentSessionStateChanged,
            status_before: Some(from_str.clone()),
            status_after: Some(to_str.clone()),
            detail_json: None,
        },
        Uuid::new_v4(),
    )
    .await?;

    tracing::info!(
        execution_id = %execution.id,
        session_id = %session.id,
        from = %from_str,
        to = %to_str,
        "agent session 状态迁移完成"
    );

    Ok(TransitionResult {
        entity: updated,
        event,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Execution transition tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pending_to_bootstrapping() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Pending,
                &WorkflowExecutionStatus::Bootstrapping,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_bootstrapping_to_running() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Bootstrapping,
                &WorkflowExecutionStatus::Running,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_bootstrapping_to_failed() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Bootstrapping,
                &WorkflowExecutionStatus::Failed,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_running_to_pausing() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Running,
                &WorkflowExecutionStatus::Pausing,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_running_to_waiting_user_acceptance() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Running,
                &WorkflowExecutionStatus::WaitingUserAcceptance,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_waiting_user_acceptance_to_paused() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::WaitingUserAcceptance,
                &WorkflowExecutionStatus::Paused,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_paused_to_recompiling() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Paused,
                &WorkflowExecutionStatus::Recompiling,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_recompiling_to_resuming() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Recompiling,
                &WorkflowExecutionStatus::Resuming,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_resuming_to_running() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Resuming,
                &WorkflowExecutionStatus::Running,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_resuming_to_pausing_allowed() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Resuming,
                &WorkflowExecutionStatus::Pausing,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_completed_to_running_rejected() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Completed,
                &WorkflowExecutionStatus::Running,
            )
            .is_err()
        );
    }

    #[test]
    fn test_pending_to_running_rejected() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Pending,
                &WorkflowExecutionStatus::Running,
            )
            .is_err()
        );
    }

    #[test]
    fn test_failed_to_running_rejected() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Failed,
                &WorkflowExecutionStatus::Running,
            )
            .is_err()
        );
    }

    #[test]
    fn test_recompiling_only_from_paused() {
        // recompiling 只能从 paused 进入
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Running,
                &WorkflowExecutionStatus::Recompiling,
            )
            .is_err()
        );

        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::WaitingUserAcceptance,
                &WorkflowExecutionStatus::Recompiling,
            )
            .is_err()
        );
    }

    #[test]
    fn test_paused_to_cancelled() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Paused,
                &WorkflowExecutionStatus::Cancelled,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_running_to_waiting_user() {
        // Running -> WaitingUser: step requests user action (approval/permission/continue)
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Running,
                &WorkflowExecutionStatus::WaitingUser,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_waiting_user_to_failed() {
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::WaitingUser,
                &WorkflowExecutionStatus::Failed,
            )
            .is_ok()
        );
    }

    // -----------------------------------------------------------------------
    // Step transition tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_step_pending_to_ready() {
        assert!(
            validate_step_transition(&WorkflowStepStatus::Pending, &WorkflowStepStatus::Ready,)
                .is_ok()
        );
    }

    #[test]
    fn test_step_running_to_completed() {
        assert!(
            validate_step_transition(&WorkflowStepStatus::Running, &WorkflowStepStatus::Completed,)
                .is_ok()
        );
    }

    #[test]
    fn test_step_running_to_interrupt_requested() {
        assert!(
            validate_step_transition(
                &WorkflowStepStatus::Running,
                &WorkflowStepStatus::InterruptRequested,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_step_failed_to_ready_retry() {
        assert!(
            validate_step_transition(&WorkflowStepStatus::Failed, &WorkflowStepStatus::Ready,)
                .is_ok()
        );
    }

    #[test]
    fn test_step_completed_terminal() {
        assert!(
            validate_step_transition(&WorkflowStepStatus::Completed, &WorkflowStepStatus::Running,)
                .is_err()
        );
    }

    #[test]
    fn test_step_pending_to_running_rejected() {
        // Must go through ready first
        assert!(
            validate_step_transition(&WorkflowStepStatus::Pending, &WorkflowStepStatus::Running,)
                .is_err()
        );
    }

    #[test]
    fn test_step_waiting_review_to_failed() {
        assert!(
            validate_step_transition(
                &WorkflowStepStatus::WaitingReview,
                &WorkflowStepStatus::Failed,
            )
            .is_ok()
        );
    }

    // -----------------------------------------------------------------------
    // Agent Session transition tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_agent_idle_to_running() {
        assert!(
            validate_agent_session_transition(
                &WorkflowAgentSessionState::Idle,
                &WorkflowAgentSessionState::Running,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_agent_running_to_paused() {
        assert!(
            validate_agent_session_transition(
                &WorkflowAgentSessionState::Running,
                &WorkflowAgentSessionState::Paused,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_agent_paused_to_idle() {
        assert!(
            validate_agent_session_transition(
                &WorkflowAgentSessionState::Paused,
                &WorkflowAgentSessionState::Idle,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_agent_completed_terminal() {
        assert!(
            validate_agent_session_transition(
                &WorkflowAgentSessionState::Completed,
                &WorkflowAgentSessionState::Running,
            )
            .is_err()
        );
    }

    #[test]
    fn test_agent_waiting_approval_to_failed() {
        assert!(
            validate_agent_session_transition(
                &WorkflowAgentSessionState::WaitingApproval,
                &WorkflowAgentSessionState::Failed,
            )
            .is_ok()
        );
    }

    // -----------------------------------------------------------------------
    // 三层组合约束 tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_running_execution_allows_running_step() {
        assert!(validate_step_in_execution(
            &WorkflowExecutionStatus::Running,
            &WorkflowStepStatus::Running,
        ));
    }

    #[test]
    fn test_completed_execution_rejects_running_step() {
        assert!(!validate_step_in_execution(
            &WorkflowExecutionStatus::Completed,
            &WorkflowStepStatus::Running,
        ));
    }

    #[test]
    fn test_paused_execution_rejects_running_step() {
        assert!(!validate_step_in_execution(
            &WorkflowExecutionStatus::Paused,
            &WorkflowStepStatus::Running,
        ));
    }

    #[test]
    fn test_waiting_user_acceptance_allows_idle_session() {
        assert!(validate_agent_session_in_execution(
            &WorkflowExecutionStatus::WaitingUserAcceptance,
            &WorkflowAgentSessionState::Idle,
        ));
    }

    #[test]
    fn test_waiting_user_acceptance_rejects_running_session() {
        assert!(!validate_agent_session_in_execution(
            &WorkflowExecutionStatus::WaitingUserAcceptance,
            &WorkflowAgentSessionState::Running,
        ));
    }

    #[test]
    fn test_waiting_user_allows_waiting_review_step() {
        assert!(validate_step_in_execution(
            &WorkflowExecutionStatus::WaitingUser,
            &WorkflowStepStatus::WaitingReview,
        ));
    }

    #[test]
    fn test_waiting_user_allows_waiting_approval_session() {
        assert!(validate_agent_session_in_execution(
            &WorkflowExecutionStatus::WaitingUser,
            &WorkflowAgentSessionState::WaitingApproval,
        ));
    }

    // -----------------------------------------------------------------------
    // Replan path semantics regression test
    // -----------------------------------------------------------------------

    #[test]
    fn test_replan_path_waiting_user_acceptance_to_paused_to_recompiling_to_resuming_to_running() {
        // waiting_user_acceptance -> paused
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::WaitingUserAcceptance,
                &WorkflowExecutionStatus::Paused,
            )
            .is_ok()
        );

        // paused -> recompiling
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Paused,
                &WorkflowExecutionStatus::Recompiling,
            )
            .is_ok()
        );

        // recompiling -> resuming
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Recompiling,
                &WorkflowExecutionStatus::Resuming,
            )
            .is_ok()
        );

        // resuming -> running
        assert!(
            validate_execution_transition(
                &WorkflowExecutionStatus::Resuming,
                &WorkflowExecutionStatus::Running,
            )
            .is_ok()
        );
    }

    // -----------------------------------------------------------------------
    // Event type mapping tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_execution_event_type_mapping() {
        assert_eq!(
            execution_event_type(&WorkflowExecutionStatus::Bootstrapping),
            WorkflowEventType::ExecutionBootstrapping
        );
        assert_eq!(
            execution_event_type(&WorkflowExecutionStatus::Running),
            WorkflowEventType::ExecutionRunning
        );
        assert_eq!(
            execution_event_type(&WorkflowExecutionStatus::Failed),
            WorkflowEventType::ExecutionFailed
        );
        assert_eq!(
            execution_event_type(&WorkflowExecutionStatus::Completed),
            WorkflowEventType::ExecutionCompleted
        );
        assert_eq!(
            execution_event_type(&WorkflowExecutionStatus::Cancelled),
            WorkflowEventType::ExecutionCancelled
        );
        assert_eq!(
            execution_event_type(&WorkflowExecutionStatus::Paused),
            WorkflowEventType::ExecutionPaused
        );
        assert_eq!(
            execution_event_type(&WorkflowExecutionStatus::WaitingUserAcceptance),
            WorkflowEventType::UserAcceptanceRequested
        );
    }

    // -----------------------------------------------------------------------
    // Wire format serialization tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_wire_format_produces_snake_case() {
        // 多词枚举值必须序列化为 snake_case，与数据库和观测契约保持一致
        assert_eq!(
            to_wire_format(&WorkflowExecutionStatus::WaitingUserAcceptance),
            "waiting_user_acceptance"
        );
        assert_eq!(
            to_wire_format(&WorkflowExecutionStatus::WaitingUser),
            "waiting_user"
        );
        assert_eq!(
            to_wire_format(&WorkflowStepStatus::InterruptRequested),
            "interrupt_requested"
        );
        assert_eq!(
            to_wire_format(&WorkflowStepStatus::WaitingInput),
            "waiting_input"
        );
        assert_eq!(
            to_wire_format(&WorkflowStepStatus::WaitingReview),
            "waiting_review"
        );
        assert_eq!(
            to_wire_format(&WorkflowAgentSessionState::InterruptRequested),
            "interrupt_requested"
        );
        assert_eq!(
            to_wire_format(&WorkflowAgentSessionState::WaitingInput),
            "waiting_input"
        );
        assert_eq!(
            to_wire_format(&WorkflowAgentSessionState::WaitingApproval),
            "waiting_approval"
        );
    }

    #[test]
    fn test_wire_format_single_word_enums() {
        assert_eq!(to_wire_format(&WorkflowExecutionStatus::Running), "running");
        assert_eq!(to_wire_format(&WorkflowExecutionStatus::Paused), "paused");
        assert_eq!(to_wire_format(&WorkflowStepStatus::Pending), "pending");
        assert_eq!(to_wire_format(&WorkflowAgentSessionState::Idle), "idle");
    }
}
