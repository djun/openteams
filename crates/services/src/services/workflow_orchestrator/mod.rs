//! Workflow Orchestrator 骨架
//!
//! Phase 1a 职责：
//! - command handler: 接收 bootstrap 命令
//! - state reducer: 集中管理执行/步骤/agent session 状态迁移
//! - scheduler loop 骨架: 接口预留
//! - event projector: 审计事件由 reducer 自动写入

pub mod reducer;

use std::collections::HashMap;

use chrono::Utc;
use db::{
    DBService,
    models::{
        chat_agent::ChatAgent,
        chat_message::{ChatMessage, ChatSenderType},
        chat_session::ChatSession,
        chat_session_agent::ChatSessionAgent,
        chat_work_item::{ChatWorkItem, ChatWorkItemType},
        workflow_agent_session::{CreateWorkflowAgentSession, WorkflowAgentSession},
        workflow_event::{CreateWorkflowEvent, WorkflowEvent},
        workflow_execution::{CreateWorkflowExecution, WorkflowExecution},
        workflow_plan::{CreateWorkflowPlan, WorkflowPlan},
        workflow_plan_revision::{CreateWorkflowPlanRevision, WorkflowPlanRevision},
        workflow_round::{CreateWorkflowRound, WorkflowRound},
        workflow_step::{CreateWorkflowStep, WorkflowStep},
        workflow_step_edge::{CreateWorkflowStepEdge, WorkflowStepEdge},
        workflow_transcript::{CreateWorkflowTranscript, WorkflowTranscript},
        workflow_types::*,
    },
};
use sqlx::SqlitePool;
use uuid::Uuid;

use super::{
    chat,
    chat_runner::{ChatRunner, ChatRunnerError},
    workflow_compiler::WorkflowCompiler,
    workflow_runtime::{
        SummaryPayload, WorkflowRuntimeError, WorkflowStepProtocolMessage, WorkflowStepRunResult,
        build_step_execution_prompt, build_workflow_card_projection, parse_summary_payload,
        predecessor_summaries, run_workflow_agent_prompt,
    },
};

/// Orchestrator 错误
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("数据库错误: {0}")]
    Database(#[from] sqlx::Error),
    #[error("编译错误: {0}")]
    Compile(#[from] super::workflow_compiler::CompileError),
    #[error("运行时错误: {0}")]
    Runtime(#[from] WorkflowRuntimeError),
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("聊天服务错误: {0}")]
    Chat(#[from] super::chat::ChatServiceError),
    #[error("聊天运行器错误: {0}")]
    ChatRunner(#[from] ChatRunnerError),
    #[error("状态迁移非法: {0}")]
    IllegalTransition(String),
    #[error("未找到资源: {0}")]
    NotFound(String),
}

impl From<reducer::TransitionError> for OrchestratorError {
    fn from(e: reducer::TransitionError) -> Self {
        OrchestratorError::IllegalTransition(e.to_string())
    }
}

/// Orchestrator 是 workflow mode 的核心调度组件
pub struct WorkflowOrchestrator;

impl WorkflowOrchestrator {
    // -----------------------------------------------------------------------
    // Command Handler: bootstrap
    // -----------------------------------------------------------------------

    /// 从一个已校验的 plan revision 创建 execution 并 bootstrap
    ///
    /// 流程:
    /// 1. 创建 execution (pending)
    /// 2. 通过 reducer 迁移到 bootstrapping
    /// 3. 编译 plan → compiled graph
    /// 4. 创建 agent sessions, round, steps, edges
    /// 5. 通过 reducer 迁移 ready steps
    /// 6. 通过 reducer 迁移到 running 或 failed
    ///
    /// `agent_id_map`: 将 plan JSON 中的 string agent_id 映射到实际的 session_agent UUID
    pub async fn bootstrap_execution(
        pool: &SqlitePool,
        plan: &WorkflowPlan,
        revision: &WorkflowPlanRevision,
        lead_session_agent_id: Option<Uuid>,
        valid_agent_ids: &[String],
        agent_id_map: &HashMap<String, Uuid>,
    ) -> Result<BootstrapResult, OrchestratorError> {
        let execution_id = Uuid::new_v4();

        // 1. 创建 execution (pending)
        let execution = WorkflowExecution::create(
            pool,
            &CreateWorkflowExecution {
                session_id: plan.session_id,
                plan_id: plan.id,
                active_revision_id: Some(revision.id),
                lead_session_agent_id,
                title: plan.title.clone(),
            },
            execution_id,
        )
        .await?;

        // 2. 通过 reducer 迁移到 bootstrapping（校验 + 持久化 + 审计事件）
        let tr =
            reducer::transition_execution(pool, &execution, WorkflowExecutionStatus::Bootstrapping)
                .await?;
        let execution = tr.entity;

        // 3. 编译 plan
        let compiled =
            match WorkflowCompiler::compile_from_json(&revision.plan_json, valid_agent_ids) {
                Ok(graph) => graph,
                Err(e) => {
                    // bootstrapping -> failed（通过 reducer，含审计事件）
                    let tr = reducer::transition_execution_with_context(
                        pool,
                        &execution,
                        WorkflowExecutionStatus::Failed,
                        None,
                        Some(&format!("编译失败: {}", e)),
                    )
                    .await?;

                    return Ok(BootstrapResult {
                        execution: tr.entity,
                        round: None,
                        steps: vec![],
                        edges: vec![],
                        agent_sessions: vec![],
                        events: vec![],
                        failed: true,
                        failure_reason: Some(format!("{}", e)),
                    });
                }
            };

        // 4. 更新 compiled graph hash（数据字段更新，非状态迁移）
        let execution = WorkflowExecution::update_compiled_graph_hash(
            pool,
            execution.id,
            &compiled.compiled_graph_hash,
            revision.id,
        )
        .await?;

        // 5. 创建 round
        let round_id = Uuid::new_v4();
        let round = WorkflowRound::create(
            pool,
            &CreateWorkflowRound {
                execution_id: execution.id,
                round_index: 1,
                source_revision_id: Some(revision.id),
            },
            round_id,
        )
        .await?;

        // 更新 execution 的 active round（数据字段更新，非状态迁移）
        let execution =
            WorkflowExecution::update_active_round(pool, execution.id, round.id, 1).await?;

        // 6. 创建 workflow agent sessions（去重：每个 agent 只创建一个 session）
        let mut agent_session_map: HashMap<String, Uuid> = HashMap::new();
        let mut created_agent_sessions = Vec::new();
        let mut lead_workflow_agent_session_id = None;

        if let Some(lead_session_agent_id) = lead_session_agent_id {
            let ws = WorkflowAgentSession::create(
                pool,
                &CreateWorkflowAgentSession {
                    workflow_execution_id: execution.id,
                    session_agent_id: lead_session_agent_id,
                    role: WorkflowAgentSessionRole::Lead,
                },
                Uuid::new_v4(),
            )
            .await?;
            lead_workflow_agent_session_id = Some(ws.id);
            created_agent_sessions.push(ws);
        }

        for compiled_step in &compiled.steps {
            if let Some(ref agent_id_str) = compiled_step.assigned_agent_id {
                if agent_session_map.contains_key(agent_id_str) {
                    continue;
                }
                if let Some(&session_agent_uuid) = agent_id_map.get(agent_id_str) {
                    if lead_session_agent_id == Some(session_agent_uuid) {
                        if let Some(lead_workflow_agent_session_id) = lead_workflow_agent_session_id
                        {
                            agent_session_map
                                .insert(agent_id_str.clone(), lead_workflow_agent_session_id);
                        }
                        continue;
                    }
                    let role = if lead_session_agent_id == Some(session_agent_uuid) {
                        WorkflowAgentSessionRole::Lead
                    } else {
                        WorkflowAgentSessionRole::Worker
                    };
                    let ws_id = Uuid::new_v4();
                    let ws = WorkflowAgentSession::create(
                        pool,
                        &CreateWorkflowAgentSession {
                            workflow_execution_id: execution.id,
                            session_agent_id: session_agent_uuid,
                            role,
                        },
                        ws_id,
                    )
                    .await?;
                    agent_session_map.insert(agent_id_str.clone(), ws.id);
                    created_agent_sessions.push(ws);
                }
            }
        }

        // 7. 创建 steps 并绑定 agent session
        let mut step_id_map: HashMap<String, Uuid> = HashMap::new();
        let mut created_steps = Vec::new();

        for compiled_step in &compiled.steps {
            let step_id = Uuid::new_v4();
            step_id_map.insert(compiled_step.step_key.clone(), step_id);

            let assigned_ws_id = compiled_step
                .assigned_agent_id
                .as_ref()
                .and_then(|aid| agent_session_map.get(aid))
                .copied()
                .or(lead_workflow_agent_session_id);

            let step = WorkflowStep::create(
                pool,
                &CreateWorkflowStep {
                    execution_id: execution.id,
                    round_id: round.id,
                    compiled_revision_id: Some(revision.id),
                    step_key: compiled_step.step_key.clone(),
                    step_type: compiled_step.step_type.clone(),
                    title: compiled_step.title.clone(),
                    instructions: compiled_step.instructions.clone(),
                    assigned_workflow_agent_session_id: assigned_ws_id,
                    max_retry: compiled_step.max_retry as i32,
                    round_index: 1,
                    display_order: compiled_step.display_order,
                },
                step_id,
            )
            .await?;

            created_steps.push(step);
        }

        // 8. 创建 edges
        let mut created_edges = Vec::new();
        for compiled_edge in &compiled.edges {
            let from_id = step_id_map
                .get(&compiled_edge.from_step_key)
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!(
                        "步骤 {} 未找到",
                        compiled_edge.from_step_key
                    ))
                })?;
            let to_id = step_id_map.get(&compiled_edge.to_step_key).ok_or_else(|| {
                OrchestratorError::NotFound(format!("步骤 {} 未找到", compiled_edge.to_step_key))
            })?;

            let edge = WorkflowStepEdge::create(
                pool,
                &CreateWorkflowStepEdge {
                    execution_id: execution.id,
                    compiled_revision_id: Some(revision.id),
                    from_step_id: *from_id,
                    to_step_id: *to_id,
                    edge_kind: compiled_edge.edge_kind.clone(),
                },
                Uuid::new_v4(),
            )
            .await?;

            created_edges.push(edge);
        }

        // 9. 将无前驱的 step 标记为 ready（通过 reducer，含组合约束校验 + 审计事件）
        for ready_key in &compiled.ready_step_keys {
            if let Some(&step_id) = step_id_map.get(ready_key) {
                let step = created_steps.iter().find(|s| s.id == step_id);
                if let Some(step) = step {
                    let tr =
                        reducer::transition_step(pool, &execution, step, WorkflowStepStatus::Ready)
                            .await?;
                    // 更新 created_steps 中的 step 状态
                    if let Some(s) = created_steps.iter_mut().find(|s| s.id == step_id) {
                        *s = tr.entity;
                    }
                }
            }
        }

        // 10. 通过 reducer 迁移到 running（校验 + 持久化 + 审计事件）
        let tr = reducer::transition_execution_with_context(
            pool,
            &execution,
            WorkflowExecutionStatus::Running,
            Some(round.id),
            None,
        )
        .await?;
        let execution = tr.entity;
        // 设置 started_at 时间戳（数据字段更新，非状态迁移）
        let execution = WorkflowExecution::set_started(pool, execution.id).await?;

        // 写入 round 启动事件（非状态迁移事件，由 orchestrator 直接写入）
        WorkflowEvent::create(
            pool,
            &CreateWorkflowEvent {
                execution_id: execution.id,
                round_id: Some(round.id),
                step_id: None,
                agent_session_id: None,
                event_type: WorkflowEventType::RoundStarted,
                status_before: None,
                status_after: Some("running".to_string()),
                detail_json: None,
            },
            Uuid::new_v4(),
        )
        .await?;

        let events = WorkflowEvent::find_by_execution(pool, execution.id).await?;

        Ok(BootstrapResult {
            execution,
            round: Some(round),
            steps: created_steps,
            edges: created_edges,
            agent_sessions: created_agent_sessions,
            events,
            failed: false,
            failure_reason: None,
        })
    }

    // -----------------------------------------------------------------------
    // Scheduler Loop 骨架 (Phase 1b 实现)
    // -----------------------------------------------------------------------

    /// Phase 1b: 唤醒调度循环，找到 ready steps 并触发 agent run
    pub async fn wake_scheduler(
        db: &DBService,
        chat_runner: &ChatRunner,
        execution_id: Uuid,
    ) -> Result<(), OrchestratorError> {
        let pool = &db.pool;
        let mut execution = WorkflowExecution::find_by_id(pool, execution_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("execution {} 未找到", execution_id))
            })?;

        loop {
            let plan = WorkflowPlan::find_by_id(pool, execution.plan_id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!("plan {} 未找到", execution.plan_id))
                })?;
            let revision_id = execution.active_revision_id.ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "execution {} 缺少 active revision",
                    execution.id
                ))
            })?;
            let revision = WorkflowPlanRevision::find_by_id(pool, revision_id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!("revision {} 未找到", revision_id))
                })?;
            let session = ChatSession::find_by_id(pool, execution.session_id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!("session {} 未找到", execution.session_id))
                })?;
            let session_agents = ChatSessionAgent::find_all_for_session(pool, session.id).await?;
            let workflow_agent_sessions =
                WorkflowAgentSession::find_by_execution(pool, execution.id).await?;
            let steps = WorkflowStep::find_by_execution(pool, execution.id).await?;
            let edges = WorkflowStepEdge::find_by_execution(pool, execution.id).await?;
            let agents = load_agents_for_session(pool, &session_agents).await?;

            if steps
                .iter()
                .all(|step| step.status == WorkflowStepStatus::Completed)
            {
                for workflow_session in &workflow_agent_sessions {
                    if workflow_session.state == WorkflowAgentSessionState::Running {
                        reducer::transition_agent_session(
                            pool,
                            &execution,
                            workflow_session,
                            WorkflowAgentSessionState::Completed,
                        )
                        .await?;
                    }
                }
                let completing = reducer::transition_execution(
                    pool,
                    &execution,
                    WorkflowExecutionStatus::Completing,
                )
                .await?;
                execution = completing.entity;
                let completed = reducer::transition_execution(
                    pool,
                    &execution,
                    WorkflowExecutionStatus::Completed,
                )
                .await?;
                execution = WorkflowExecution::set_completed(pool, completed.entity.id).await?;
                Self::persist_completion_work_items(
                    pool,
                    chat_runner,
                    &execution,
                    &steps,
                    &workflow_agent_sessions,
                    &session_agents,
                    &agents,
                )
                .await?;
                Self::refresh_workflow_card(
                    pool,
                    chat_runner,
                    &execution,
                    &plan,
                    &revision,
                    &session_agents,
                    &agents,
                    None,
                )
                .await?;
                return Ok(());
            }

            let mut ready_promotions = Vec::new();
            for step in &steps {
                if step.status != WorkflowStepStatus::Pending {
                    continue;
                }

                let blocked = edges
                    .iter()
                    .filter(|edge| edge.to_step_id == step.id)
                    .any(|edge| {
                        steps
                            .iter()
                            .find(|candidate| candidate.id == edge.from_step_id)
                            .map(|candidate| candidate.status != WorkflowStepStatus::Completed)
                            .unwrap_or(true)
                    });

                if !blocked {
                    ready_promotions.push(step.id);
                }
            }

            let mut current_steps = steps;
            for step_id in ready_promotions {
                if let Some(step) = current_steps
                    .iter()
                    .find(|step| step.id == step_id)
                    .cloned()
                {
                    let transitioned = reducer::transition_step(
                        pool,
                        &execution,
                        &step,
                        WorkflowStepStatus::Ready,
                    )
                    .await?;
                    if let Some(existing) = current_steps.iter_mut().find(|item| item.id == step_id)
                    {
                        *existing = transitioned.entity;
                    }
                }
            }

            // Collect ready steps: one per workflow agent session for parallel execution.
            // When multiple ready steps share the same agent session, pick the one
            // with the lowest display_order (preserving the original sequential priority).
            let mut ready_by_session: HashMap<Uuid, WorkflowStep> = HashMap::new();
            for step in current_steps
                .iter()
                .filter(|s| s.status == WorkflowStepStatus::Ready)
            {
                let ws =
                    resolve_step_workflow_session(&execution, &workflow_agent_sessions, step)?;
                ready_by_session
                    .entry(ws.id)
                    .and_modify(|existing| {
                        if step.display_order < existing.display_order {
                            *existing = step.clone();
                        }
                    })
                    .or_insert(step.clone());
            }
            let parallel_steps: Vec<WorkflowStep> =
                ready_by_session.into_values().collect();

            if parallel_steps.is_empty() {
                // No ready steps found. Check if we're stuck (not all completed, no promotable pending).
                let all_completed = current_steps
                    .iter()
                    .all(|s| s.status == WorkflowStepStatus::Completed);
                let has_waiting = current_steps.iter().any(|s| {
                    s.status == WorkflowStepStatus::WaitingInput
                        || s.status == WorkflowStepStatus::WaitingReview
                });

                if !all_completed && !has_waiting {
                    // Execution is stuck: steps are interrupted/failed/blocked with no path forward
                    let stuck_reason = "No runnable steps remain and not all steps completed";
                    reducer::transition_execution_with_context(
                        pool,
                        &execution,
                        WorkflowExecutionStatus::Failed,
                        execution.active_round_id,
                        Some(stuck_reason),
                    )
                    .await?;
                    Self::refresh_workflow_card(
                        pool,
                        chat_runner,
                        &execution,
                        &plan,
                        &revision,
                        &session_agents,
                        &agents,
                        Some(stuck_reason.to_string()),
                    )
                    .await?;
                } else {
                    Self::refresh_workflow_card(
                        pool,
                        chat_runner,
                        &execution,
                        &plan,
                        &revision,
                        &session_agents,
                        &agents,
                        None,
                    )
                    .await?;
                }
                return Ok(());
            }

            // --- Parallel execution: run all selected steps concurrently ---
            let step_futures: Vec<_> = parallel_steps
                .iter()
                .map(|step| {
                    Self::prepare_and_run_step(
                        db,
                        pool,
                        &execution,
                        step,
                        &workflow_agent_sessions,
                        &session,
                        &session_agents,
                        &agents,
                        &plan,
                        &current_steps,
                        &edges,
                    )
                })
                .collect();
            let outcomes = futures::future::join_all(step_futures).await;

            // Process outcomes: collect parked/failed results
            let mut any_parked = false;
            let mut first_failure: Option<String> = None;
            for outcome in outcomes {
                match outcome? {
                    StepOutcome::Completed => {}
                    StepOutcome::Parked => {
                        any_parked = true;
                    }
                    StepOutcome::Failed(reason) => {
                        if first_failure.is_none() {
                            first_failure = Some(reason);
                        }
                    }
                }
            }

            // Execution-level state transitions based on aggregated outcomes
            if let Some(failure_reason) = first_failure {
                // Fail all non-terminal agent sessions
                let latest_sessions =
                    WorkflowAgentSession::find_by_execution(pool, execution.id).await?;
                for ws in &latest_sessions {
                    if matches!(
                        ws.state,
                        WorkflowAgentSessionState::Running
                            | WorkflowAgentSessionState::WaitingApproval
                            | WorkflowAgentSessionState::WaitingInput
                    ) {
                        let _ = reducer::transition_agent_session(
                            pool,
                            &execution,
                            ws,
                            WorkflowAgentSessionState::Failed,
                        )
                        .await;
                    }
                }
                execution = reducer::transition_execution_with_context(
                    pool,
                    &execution,
                    WorkflowExecutionStatus::Failed,
                    execution.active_round_id,
                    Some(&failure_reason),
                )
                .await?
                .entity;
                Self::refresh_workflow_card(
                    pool,
                    chat_runner,
                    &execution,
                    &plan,
                    &revision,
                    &session_agents,
                    &agents,
                    Some(failure_reason),
                )
                .await?;
                return Ok(());
            }

            if any_parked {
                // At least one step is waiting for user action
                execution = reducer::transition_execution_with_context(
                    pool,
                    &execution,
                    WorkflowExecutionStatus::WaitingUser,
                    execution.active_round_id,
                    None,
                )
                .await?
                .entity;
                Self::refresh_workflow_card(
                    pool,
                    chat_runner,
                    &execution,
                    &plan,
                    &revision,
                    &session_agents,
                    &agents,
                    None,
                )
                .await?;
                return Ok(());
            }

            // All steps in this batch completed — refresh card and continue loop
            Self::refresh_workflow_card(
                pool,
                chat_runner,
                &execution,
                &plan,
                &revision,
                &session_agents,
                &agents,
                None,
            )
            .await?;

            execution = WorkflowExecution::find_by_id(pool, execution.id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!("execution {} 未找到", execution.id))
                })?;
        }
    }
}

fn resolve_step_workflow_session<'a>(
    execution: &WorkflowExecution,
    workflow_sessions: &'a [WorkflowAgentSession],
    step: &WorkflowStep,
) -> Result<&'a WorkflowAgentSession, OrchestratorError> {
    if let Some(workflow_session_id) = step.assigned_workflow_agent_session_id {
        return workflow_sessions
            .iter()
            .find(|session| session.id == workflow_session_id)
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "workflow agent session {} 未找到",
                    workflow_session_id
                ))
            });
    }

    let lead_session_agent_id = execution.lead_session_agent_id.ok_or_else(|| {
        OrchestratorError::NotFound(format!(
            "execution {} 缺少 lead session agent",
            execution.id
        ))
    })?;

    workflow_sessions
        .iter()
        .find(|session| session.session_agent_id == lead_session_agent_id)
        .ok_or_else(|| {
            OrchestratorError::NotFound(format!(
                "execution {} 的 lead workflow session 未找到",
                execution.id
            ))
        })
}

async fn load_agents_for_session(
    pool: &SqlitePool,
    session_agents: &[ChatSessionAgent],
) -> Result<Vec<ChatAgent>, OrchestratorError> {
    let mut agents = Vec::new();
    for session_agent in session_agents {
        let agent = ChatAgent::find_by_id(pool, session_agent.agent_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("agent {} 未找到", session_agent.agent_id))
            })?;
        agents.push(agent);
    }
    Ok(agents)
}

impl WorkflowOrchestrator {
    fn parse_step_output_message(
        execution_id: Uuid,
        step: &WorkflowStep,
        raw_output: &str,
    ) -> Result<WorkflowStepProtocolMessage, OrchestratorError> {
        super::workflow_runtime::parse_step_protocol_output(
            execution_id,
            &step.step_key,
            raw_output,
        )
        .map_err(OrchestratorError::from)
    }

    fn step_message_error(message: String, content: Option<String>) -> OrchestratorError {
        OrchestratorError::Runtime(WorkflowRuntimeError::Validation(
            content
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!("{message}: {value}"))
                .unwrap_or(message),
        ))
    }

    pub async fn refresh_workflow_card(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        plan: &WorkflowPlan,
        revision: &WorkflowPlanRevision,
        session_agents: &[ChatSessionAgent],
        agents: &[ChatAgent],
        error_message: Option<String>,
    ) -> Result<(), OrchestratorError> {
        let Some(message_id) = execution.workflow_card_message_id else {
            return Ok(());
        };

        let message = ChatMessage::find_by_id(pool, message_id)
            .await?
            .ok_or_else(|| OrchestratorError::NotFound(format!("message {} 未找到", message_id)))?;
        let workflow_sessions = WorkflowAgentSession::find_by_execution(pool, execution.id).await?;
        let steps = WorkflowStep::find_by_execution(pool, execution.id).await?;
        let edges = WorkflowStepEdge::find_by_execution(pool, execution.id).await?;

        let projection = build_workflow_card_projection(
            execution,
            plan,
            revision,
            &steps,
            &edges,
            &workflow_sessions,
            session_agents,
            agents,
            error_message,
        )?;
        let mut meta = message.meta.0.clone();
        meta["card_type"] = serde_json::json!("workflow_execution");
        meta["workflow_card"] = serde_json::to_value(&projection)?;

        let updated =
            ChatMessage::update_content_and_meta(pool, message.id, "Workflow execution", meta)
                .await?;
        chat_runner.emit_message_updated(updated.session_id, updated);
        Ok(())
    }

    pub async fn refresh_execution_projection(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution_id: Uuid,
        error_message: Option<String>,
    ) -> Result<WorkflowExecution, OrchestratorError> {
        let execution = WorkflowExecution::find_by_id(pool, execution_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("execution {} 未找到", execution_id))
            })?;
        let plan = WorkflowPlan::find_by_id(pool, execution.plan_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("plan {} 未找到", execution.plan_id))
            })?;
        let revision_id = execution.active_revision_id.ok_or_else(|| {
            OrchestratorError::NotFound(format!("execution {} 缺少 active revision", execution.id))
        })?;
        let revision = WorkflowPlanRevision::find_by_id(pool, revision_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("revision {} 未找到", revision_id))
            })?;
        let session_agents =
            ChatSessionAgent::find_all_for_session(pool, execution.session_id).await?;
        let agents = load_agents_for_session(pool, &session_agents).await?;

        Self::refresh_workflow_card(
            pool,
            chat_runner,
            &execution,
            &plan,
            &revision,
            &session_agents,
            &agents,
            error_message,
        )
        .await?;

        Ok(execution)
    }

    async fn park_for_user_action(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        step: &WorkflowStep,
        workflow_session: &WorkflowAgentSession,
        entry_type: &str,
        content: &str,
        description: Option<String>,
        waiting_step_status: WorkflowStepStatus,
        waiting_agent_state: WorkflowAgentSessionState,
        extra_meta: Option<serde_json::Value>,
    ) -> Result<WorkflowTranscript, OrchestratorError> {
        let waiting_step = reducer::transition_step(pool, execution, step, waiting_step_status)
            .await?
            .entity;
        let waiting_session = reducer::transition_agent_session(
            pool,
            execution,
            workflow_session,
            waiting_agent_state,
        )
        .await?
        .entity;
        let waiting_execution = reducer::transition_execution_with_context(
            pool,
            execution,
            WorkflowExecutionStatus::WaitingUser,
            Some(waiting_step.round_id),
            Some(content),
        )
        .await?
        .entity;

        let mut meta_json = serde_json::json!({
            "description": description,
            "resolved": false,
        });
        if let Some(extra_meta) = extra_meta
            && let Some(extra_meta_obj) = extra_meta.as_object()
            && let Some(meta_json_obj) = meta_json.as_object_mut()
        {
            for (key, value) in extra_meta_obj {
                meta_json_obj.insert(key.clone(), value.clone());
            }
        }
        let meta_json = meta_json.to_string();

        let transcript = Self::write_transcript(
            pool,
            waiting_execution.id,
            Some(waiting_step.round_id),
            Some(waiting_session.id),
            Some(waiting_step.id),
            "control",
            entry_type,
            content,
            Some(&meta_json),
        )
        .await?;

        Self::refresh_execution_projection(pool, chat_runner, waiting_execution.id, None).await?;

        Ok(transcript)
    }

    fn merge_transcript_meta(
        existing_meta_json: Option<&str>,
        updates: serde_json::Value,
    ) -> String {
        let mut meta = existing_meta_json
            .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if !meta.is_object() {
            meta = serde_json::json!({});
        }
        let meta_object = meta.as_object_mut().expect("meta object");

        if let Some(update_object) = updates.as_object() {
            for (key, value) in update_object {
                meta_object.insert(key.clone(), value.clone());
            }
        }

        meta.to_string()
    }

    pub async fn resolve_transcript_action(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        transcript_id: Uuid,
        resolved_action: &str,
        input_text: Option<&str>,
    ) -> Result<ResolvedTranscriptAction, OrchestratorError> {
        let transcript = WorkflowTranscript::find_by_id(pool, transcript_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("transcript {} 未找到", transcript_id))
            })?;
        let execution = WorkflowExecution::find_by_id(pool, transcript.execution_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("execution {} 未找到", transcript.execution_id))
            })?;

        if execution.status != WorkflowExecutionStatus::WaitingUser {
            return Err(OrchestratorError::IllegalTransition(format!(
                "execution {} is {:?}, expected waiting_user",
                execution.id, execution.status
            )));
        }

        let step_id = transcript.step_id.ok_or_else(|| {
            OrchestratorError::NotFound(format!("transcript {} 缺少 step_id", transcript.id))
        })?;
        let workflow_agent_session_id = transcript.workflow_agent_session_id.ok_or_else(|| {
            OrchestratorError::NotFound(format!(
                "transcript {} 缺少 workflow_agent_session_id",
                transcript.id
            ))
        })?;

        let step = WorkflowStep::find_by_id(pool, step_id)
            .await?
            .ok_or_else(|| OrchestratorError::NotFound(format!("step {} 未找到", step_id)))?;
        let workflow_session = WorkflowAgentSession::find_by_id(pool, workflow_agent_session_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "workflow agent session {} 未找到",
                    workflow_agent_session_id
                ))
            })?;

        let existing_meta: serde_json::Value = transcript
            .meta_json
            .as_deref()
            .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if matches!(
            existing_meta.get("resolved"),
            Some(serde_json::Value::Bool(true))
        ) {
            return Err(OrchestratorError::IllegalTransition(format!(
                "transcript {} already resolved",
                transcript.id
            )));
        }

        let resolution_kind = match (transcript.entry_type.as_str(), resolved_action) {
            ("approval_request", "approved")
            | ("permission_request", "granted")
            | ("continue_confirmation", "continued")
            | ("input_request", "submitted") => TranscriptResolution::Resume,
            ("approval_request", "rejected") => {
                TranscriptResolution::Fail("Approval rejected by user.".to_string())
            }
            ("permission_request", "denied") => {
                TranscriptResolution::Fail("Permission denied by user.".to_string())
            }
            ("input_request", action) => {
                return Err(OrchestratorError::IllegalTransition(format!(
                    "unsupported action '{}' for input request",
                    action
                )));
            }
            ("continue_confirmation", action) => {
                return Err(OrchestratorError::IllegalTransition(format!(
                    "unsupported action '{}' for continue confirmation",
                    action
                )));
            }
            (entry_type, action) => {
                return Err(OrchestratorError::IllegalTransition(format!(
                    "unsupported action '{}' for transcript type '{}'",
                    action, entry_type
                )));
            }
        };

        let input_text = input_text
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if transcript.entry_type == "input_request" && input_text.is_none() {
            return Err(OrchestratorError::IllegalTransition(
                "input request requires non-empty input_text".to_string(),
            ));
        }

        let updated_meta_json = Self::merge_transcript_meta(
            transcript.meta_json.as_deref(),
            serde_json::json!({
                "resolved": true,
                "resolved_action": resolved_action,
                "resolved_at": Utc::now().to_rfc3339(),
                "input_text": input_text,
            }),
        );
        let updated_transcript =
            WorkflowTranscript::update_meta_json(pool, transcript.id, &updated_meta_json).await?;

        let decision_notice = if let Some(input_text) = input_text.as_deref() {
            input_text.to_string()
        } else {
            format!("User {} {}", resolved_action, transcript.content.trim())
        };

        match resolution_kind {
            TranscriptResolution::Resume => {
                let resumed_execution = reducer::transition_execution(
                    pool,
                    &execution,
                    WorkflowExecutionStatus::Running,
                )
                .await?
                .entity;
                let resumed_step = reducer::transition_step(
                    pool,
                    &resumed_execution,
                    &step,
                    WorkflowStepStatus::Ready,
                )
                .await?
                .entity;
                let resumed_session = reducer::transition_agent_session(
                    pool,
                    &resumed_execution,
                    &workflow_session,
                    WorkflowAgentSessionState::Running,
                )
                .await?
                .entity;

                let resolution_meta = serde_json::json!({
                    "source_transcript_id": updated_transcript.id,
                    "action": resolved_action,
                })
                .to_string();
                Self::write_transcript(
                    pool,
                    resumed_execution.id,
                    Some(resumed_step.round_id),
                    Some(resumed_session.id),
                    Some(resumed_step.id),
                    "user",
                    "message",
                    &decision_notice,
                    Some(&resolution_meta),
                )
                .await?;

                Self::refresh_execution_projection(pool, chat_runner, resumed_execution.id, None)
                    .await?;

                Ok(ResolvedTranscriptAction {
                    transcript: updated_transcript,
                    execution: resumed_execution,
                    should_wake_scheduler: true,
                })
            }
            TranscriptResolution::Fail(failure_reason) => {
                let failed_execution = reducer::transition_execution_with_context(
                    pool,
                    &execution,
                    WorkflowExecutionStatus::Failed,
                    transcript.round_id,
                    Some(&failure_reason),
                )
                .await?
                .entity;
                let recorded_step = WorkflowStep::record_execution_result(
                    pool,
                    step.id,
                    Uuid::new_v4(),
                    Some(
                        serde_json::to_string(&SummaryPayload {
                            summary: failure_reason.clone(),
                            content: Some(transcript.content.clone()),
                            outputs: vec![],
                        })
                        .unwrap_or_else(|_| failure_reason.clone()),
                    ),
                )
                .await?;
                let failed_step = reducer::transition_step(
                    pool,
                    &failed_execution,
                    &recorded_step,
                    WorkflowStepStatus::Failed,
                )
                .await?
                .entity;
                let failed_session = reducer::transition_agent_session(
                    pool,
                    &failed_execution,
                    &workflow_session,
                    WorkflowAgentSessionState::Failed,
                )
                .await?
                .entity;

                let resolution_meta = serde_json::json!({
                    "source_transcript_id": updated_transcript.id,
                    "action": resolved_action,
                    "status": "failed",
                })
                .to_string();
                Self::write_transcript(
                    pool,
                    failed_execution.id,
                    Some(failed_step.round_id),
                    Some(failed_session.id),
                    Some(failed_step.id),
                    "user",
                    "message",
                    &decision_notice,
                    Some(&resolution_meta),
                )
                .await?;

                Self::refresh_execution_projection(
                    pool,
                    chat_runner,
                    failed_execution.id,
                    Some(failure_reason),
                )
                .await?;

                Ok(ResolvedTranscriptAction {
                    transcript: updated_transcript,
                    execution: failed_execution,
                    should_wake_scheduler: false,
                })
            }
        }
    }

    async fn persist_completion_work_items(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        steps: &[WorkflowStep],
        workflow_sessions: &[WorkflowAgentSession],
        session_agents: &[ChatSessionAgent],
        agents: &[ChatAgent],
    ) -> Result<(), OrchestratorError> {
        if !ChatWorkItem::find_by_run_id(pool, execution.id)
            .await?
            .is_empty()
        {
            return Ok(());
        }

        let result_step = steps
            .iter()
            .find(|step| step.step_type == WorkflowStepType::Result)
            .ok_or_else(|| {
                OrchestratorError::NotFound("workflow result step 未找到".to_string())
            })?;
        let payload =
            parse_summary_payload(result_step.summary_text.as_deref()).ok_or_else(|| {
                OrchestratorError::Runtime(WorkflowRuntimeError::Validation(
                    "workflow result step 缺少可持久化的完成摘要".to_string(),
                ))
            })?;
        let workflow_session =
            resolve_step_workflow_session(execution, workflow_sessions, result_step)?;
        let session_agent = session_agents
            .iter()
            .find(|item| item.id == workflow_session.session_agent_id)
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "session agent {} 未找到",
                    workflow_session.session_agent_id
                ))
            })?;
        let agent = agents
            .iter()
            .find(|item| item.id == session_agent.agent_id)
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("agent {} 未找到", session_agent.agent_id))
            })?;

        let conclusion = match payload.content.as_deref().map(str::trim) {
            Some(content) if !content.is_empty() && content != payload.summary.trim() => {
                format!("{}\n\n{}", payload.summary, content)
            }
            _ => payload.summary.clone(),
        };

        chat_runner
            .persist_work_item(
                execution.session_id,
                session_agent.id,
                agent.id,
                execution.id,
                &agent.name,
                ChatWorkItemType::Conclusion,
                conclusion,
            )
            .await?;

        for output in payload.outputs {
            let output = output.trim();
            if output.is_empty() {
                continue;
            }

            chat_runner
                .persist_work_item(
                    execution.session_id,
                    session_agent.id,
                    agent.id,
                    execution.id,
                    &agent.name,
                    ChatWorkItemType::Artifact,
                    format!("`{output}`"),
                )
                .await?;
        }

        Ok(())
    }

    pub async fn create_workflow_plan_and_card(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        session: &ChatSession,
        source_message_id: Option<Uuid>,
        lead_session_agent: &ChatSessionAgent,
        plan_json: &str,
    ) -> Result<(WorkflowPlan, WorkflowPlanRevision, ChatMessage), OrchestratorError> {
        let parsed_plan: WorkflowPlanJson = serde_json::from_str(plan_json)?;
        let plan_hash = WorkflowCompiler::compute_hash(&parsed_plan);
        let plan_schema_version = parsed_plan
            .plan_schema_version()
            .map_err(|err| OrchestratorError::Runtime(WorkflowRuntimeError::Validation(err)))?;
        let plan = WorkflowPlan::create(
            pool,
            &CreateWorkflowPlan {
                session_id: session.id,
                source_message_id,
                created_by_session_agent_id: Some(lead_session_agent.id),
                title: parsed_plan.title.clone(),
                summary_text: Some(parsed_plan.goal.clone()),
                plan_json: plan_json.to_string(),
                plan_schema_version,
                plan_hash: plan_hash.clone(),
                validation_status: WorkflowValidationStatus::Valid,
                validation_errors_json: None,
            },
            Uuid::new_v4(),
        )
        .await?;
        let plan = WorkflowPlan::update_status(pool, plan.id, WorkflowPlanStatus::Ready).await?;
        let revision = WorkflowPlanRevision::create(
            pool,
            &CreateWorkflowPlanRevision {
                plan_id: plan.id,
                revision_no: 1,
                edited_by: WorkflowRevisionEditor::Lead,
                editor_session_agent_id: Some(lead_session_agent.id),
                reason: Some("generate-plan-and-run".to_string()),
                plan_json: plan_json.to_string(),
                plan_hash,
                validation_status: WorkflowValidationStatus::Valid,
                validation_errors_json: None,
            },
            Uuid::new_v4(),
        )
        .await?;
        let message = chat::create_message(
            pool,
            session.id,
            ChatSenderType::System,
            None,
            "Workflow execution".to_string(),
            Some(serde_json::json!({
                "card_type": "workflow_execution"
            })),
        )
        .await?;
        chat_runner.emit_message_new(message.session_id, message.clone());
        Ok((plan, revision, message))
    }

    /// Create a plan in `ready` state and a preview card (no execution created).
    /// Used by the `workflow_generate` -> plan_generation pipeline.
    pub async fn create_workflow_plan_preview_card(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        session: &ChatSession,
        source_message_id: Option<Uuid>,
        lead_session_agent: &ChatSessionAgent,
        plan_json: &str,
    ) -> Result<(WorkflowPlan, WorkflowPlanRevision, ChatMessage), OrchestratorError> {
        let parsed_plan: WorkflowPlanJson = serde_json::from_str(plan_json)?;
        let plan_hash = WorkflowCompiler::compute_hash(&parsed_plan);
        let plan_schema_version = parsed_plan
            .plan_schema_version()
            .map_err(|err| OrchestratorError::Runtime(WorkflowRuntimeError::Validation(err)))?;
        let plan = WorkflowPlan::create(
            pool,
            &CreateWorkflowPlan {
                session_id: session.id,
                source_message_id,
                created_by_session_agent_id: Some(lead_session_agent.id),
                title: parsed_plan.title.clone(),
                summary_text: Some(parsed_plan.goal.clone()),
                plan_json: plan_json.to_string(),
                plan_schema_version,
                plan_hash: plan_hash.clone(),
                validation_status: WorkflowValidationStatus::Valid,
                validation_errors_json: None,
            },
            Uuid::new_v4(),
        )
        .await?;
        let plan = WorkflowPlan::update_status(pool, plan.id, WorkflowPlanStatus::Ready).await?;
        let revision = WorkflowPlanRevision::create(
            pool,
            &CreateWorkflowPlanRevision {
                plan_id: plan.id,
                revision_no: 1,
                edited_by: WorkflowRevisionEditor::Lead,
                editor_session_agent_id: Some(lead_session_agent.id),
                reason: Some("workflow_generate".to_string()),
                plan_json: plan_json.to_string(),
                plan_hash,
                validation_status: WorkflowValidationStatus::Valid,
                validation_errors_json: None,
            },
            Uuid::new_v4(),
        )
        .await?;

        // Build preview projection
        let session_agents = ChatSessionAgent::find_all_for_session(pool, session.id).await?;
        let agents = load_agents_for_session(pool, &session_agents).await?;
        let agent_views: Vec<super::workflow_runtime::WorkflowCardAgent> = session_agents
            .iter()
            .filter_map(|sa| {
                let agent = agents.iter().find(|a| a.id == sa.agent_id)?;
                Some(super::workflow_runtime::WorkflowCardAgent {
                    session_agent_id: sa.id.to_string(),
                    workflow_agent_session_id: None,
                    agent_id: agent.id.to_string(),
                    name: agent.name.clone(),
                })
            })
            .collect();

        let step_views: Vec<super::workflow_runtime::WorkflowCardStep> = parsed_plan
            .nodes
            .iter()
            .map(|n| {
                let step_type_str = if n.data.step_type.is_empty() {
                    "task".to_string()
                } else {
                    n.data.step_type.to_lowercase()
                };
                super::workflow_runtime::WorkflowCardStep {
                    id: n.id.clone(),
                    step_key: n.id.clone(),
                    title: n.data.title.clone(),
                    step_type: step_type_str,
                    status: "pending".to_string(),
                    agent_name: n.data.agent_id.clone(),
                    summary_text: None,
                }
            })
            .collect();

        let preview = super::workflow_runtime::WorkflowCardProjection {
            execution_id: None,
            plan_id: plan.id.to_string(),
            revision_id: revision.id.to_string(),
            title: plan.title.clone(),
            goal: plan
                .summary_text
                .clone()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| plan.title.clone()),
            state: super::workflow_runtime::WorkflowCardState::PreviewReady,
            execution_status: "preview".to_string(),
            error_message: None,
            completed_step_count: 0,
            total_step_count: parsed_plan.nodes.len(),
            result_summary: None,
            outputs: Vec::new(),
            agents: agent_views,
            steps: step_views,
            plan: parsed_plan,
            started_at: None,
            completed_at: None,
            validation_errors: None,
        };

        let card_meta = serde_json::json!({
            "card_type": "workflow_plan",
            "workflow_plan_id": plan.id,
            "active_revision_id": revision.id,
            "display_state": "preview_ready",
            "workflow_card": serde_json::to_value(&preview)?,
        });

        // Single-card contract: reuse existing workflow card if present
        let existing_card_id = Self::find_session_workflow_card_message_id(pool, session.id).await;
        let message = if let Some(existing_id) = existing_card_id {
            let updated = ChatMessage::update_content_and_meta(
                pool,
                existing_id,
                "Workflow Plan",
                card_meta.clone(),
            )
            .await?;
            chat_runner.emit_message_updated(updated.session_id, updated.clone());
            updated
        } else {
            let msg = chat::create_message(
                pool,
                session.id,
                ChatSenderType::System,
                None,
                "Workflow Plan".to_string(),
                Some(card_meta),
            )
            .await?;
            chat_runner.emit_message_new(msg.session_id, msg.clone());
            msg
        };

        // Update plan with the card message id for later reference (e.g. execute_plan)
        let plan = WorkflowPlan::update_workflow_card_message_id(pool, plan.id, message.id).await?;

        Ok((plan, revision, message))
    }

    /// Find the existing workflow card message in this session by looking at
    /// plans that already have a `workflow_card_message_id`.
    pub async fn find_session_workflow_card_message_id(
        pool: &SqlitePool,
        session_id: Uuid,
    ) -> Option<Uuid> {
        let plans = WorkflowPlan::find_by_session(pool, session_id)
            .await
            .unwrap_or_default();
        for plan in &plans {
            if let Some(card_msg_id) = plan.workflow_card_message_id {
                return Some(card_msg_id);
            }
        }
        None
    }

    // -----------------------------------------------------------------------
    // Execute API: create execution from an existing ready plan (idempotent)
    // -----------------------------------------------------------------------

    /// Execute a plan that is in `ready` status.
    /// Idempotent: if an active execution already exists for this plan, returns it.
    pub async fn execute_plan(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        plan_id: Uuid,
    ) -> Result<BootstrapResult, OrchestratorError> {
        let plan = WorkflowPlan::find_by_id(pool, plan_id)
            .await?
            .ok_or_else(|| OrchestratorError::NotFound(format!("plan {} 未找到", plan_id)))?;

        if plan.status != WorkflowPlanStatus::Ready {
            return Err(OrchestratorError::IllegalTransition(format!(
                "plan {} status is {:?}, expected Ready",
                plan_id, plan.status
            )));
        }

        // Idempotent check: if active execution exists for this plan, return early
        let active_executions =
            WorkflowExecution::find_active_by_session(pool, plan.session_id).await?;
        for existing in &active_executions {
            if existing.plan_id == plan_id {
                // Already executing this plan
                let steps = WorkflowStep::find_by_execution(pool, existing.id).await?;
                let edges = WorkflowStepEdge::find_by_execution(pool, existing.id).await?;
                let agent_sessions =
                    WorkflowAgentSession::find_by_execution(pool, existing.id).await?;
                let round = existing.active_round_id.and_then(|_| {
                    // We can't do async in and_then easily, so skip
                    None::<WorkflowRound>
                });
                let events = WorkflowEvent::find_by_execution(pool, existing.id).await?;
                return Ok(BootstrapResult {
                    execution: existing.clone(),
                    round,
                    steps,
                    edges,
                    agent_sessions,
                    events,
                    failed: false,
                    failure_reason: None,
                });
            }
        }
        if !active_executions.is_empty() {
            return Err(OrchestratorError::IllegalTransition(
                "another workflow execution is already active in this session".to_string(),
            ));
        }

        let revision = WorkflowPlanRevision::find_latest_by_plan(pool, plan_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("plan {} 缺少 revision", plan_id))
            })?;

        let session = ChatSession::find_by_id(pool, plan.session_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("session {} 未找到", plan.session_id))
            })?;
        let session_agents = ChatSessionAgent::find_all_for_session(pool, session.id).await?;
        let agents = load_agents_for_session(pool, &session_agents).await?;

        let lead_session_agent_id = plan
            .created_by_session_agent_id
            .or_else(|| session_agents.first().map(|sa| sa.id));

        let valid_agent_ids: Vec<String> = agents.iter().map(|a| a.id.to_string()).collect();
        let agent_id_map: HashMap<String, Uuid> = session_agents
            .iter()
            .map(|sa| (sa.agent_id.to_string(), sa.id))
            .collect();

        let bootstrap = Self::bootstrap_execution(
            pool,
            &plan,
            &revision,
            lead_session_agent_id,
            &valid_agent_ids,
            &agent_id_map,
        )
        .await?;

        // Update the workflow card if it exists
        if let Some(card_msg_id) = plan.workflow_card_message_id {
            let _ = WorkflowExecution::update_workflow_card_message_id(
                pool,
                bootstrap.execution.id,
                card_msg_id,
            )
            .await;

            // Refresh the card to show running state
            Self::refresh_workflow_card(
                pool,
                chat_runner,
                &bootstrap.execution,
                &plan,
                &revision,
                &session_agents,
                &agents,
                bootstrap.failure_reason.clone(),
            )
            .await?;
        }

        Ok(bootstrap)
    }

    // -----------------------------------------------------------------------
    // Pause / Interrupt controls
    // -----------------------------------------------------------------------

    /// Pause all running steps in the execution.
    pub async fn pause_all(
        pool: &SqlitePool,
        execution_id: Uuid,
    ) -> Result<WorkflowExecution, OrchestratorError> {
        let execution = WorkflowExecution::find_by_id(pool, execution_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("execution {} 未找到", execution_id))
            })?;

        if execution.status != WorkflowExecutionStatus::Running {
            return Err(OrchestratorError::IllegalTransition(format!(
                "cannot pause: execution is {:?}",
                execution.status
            )));
        }

        // Transition to pausing
        let tr = reducer::transition_execution(pool, &execution, WorkflowExecutionStatus::Pausing)
            .await?;

        // For MVP, immediately go to paused (no async convergence needed yet)
        let tr = reducer::transition_execution(pool, &tr.entity, WorkflowExecutionStatus::Paused)
            .await?;

        Ok(tr.entity)
    }

    /// Interrupt a specific step.
    /// After interruption, checks if the execution has any remaining runnable steps.
    /// If not, transitions execution to Failed to avoid being stuck in Running forever.
    pub async fn interrupt_step(
        pool: &SqlitePool,
        execution_id: Uuid,
        step_id: Uuid,
    ) -> Result<WorkflowStep, OrchestratorError> {
        let execution = WorkflowExecution::find_by_id(pool, execution_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("execution {} 未找到", execution_id))
            })?;

        let step = WorkflowStep::find_by_id(pool, step_id)
            .await?
            .ok_or_else(|| OrchestratorError::NotFound(format!("step {} 未找到", step_id)))?;

        if step.status != WorkflowStepStatus::Running {
            return Err(OrchestratorError::IllegalTransition(format!(
                "cannot interrupt: step is {:?}",
                step.status
            )));
        }

        let tr = reducer::transition_step(
            pool,
            &execution,
            &step,
            WorkflowStepStatus::InterruptRequested,
        )
        .await?;

        // For MVP, immediately transition to interrupted
        let tr = reducer::transition_step(
            pool,
            &execution,
            &tr.entity,
            WorkflowStepStatus::Interrupted,
        )
        .await?;
        let interrupted_step = tr.entity;

        // Check if the execution is now stuck: no running/ready/pending-promotable steps remain
        if execution.status == WorkflowExecutionStatus::Running {
            let all_steps = WorkflowStep::find_by_execution(pool, execution_id).await?;
            let edges = WorkflowStepEdge::find_by_execution(pool, execution_id).await?;
            let has_runnable = all_steps.iter().any(|s| {
                s.status == WorkflowStepStatus::Running || s.status == WorkflowStepStatus::Ready
            });
            let has_promotable_pending = all_steps.iter().any(|s| {
                if s.status != WorkflowStepStatus::Pending {
                    return false;
                }
                // A pending step is promotable if all predecessors are completed
                !edges
                    .iter()
                    .filter(|e| e.to_step_id == s.id)
                    .any(|e| {
                        all_steps
                            .iter()
                            .find(|candidate| candidate.id == e.from_step_id)
                            .map(|candidate| candidate.status != WorkflowStepStatus::Completed)
                            .unwrap_or(true)
                    })
            });

            if !has_runnable && !has_promotable_pending {
                // Execution cannot make further progress — fail it
                reducer::transition_execution_with_context(
                    pool,
                    &execution,
                    WorkflowExecutionStatus::Failed,
                    Some(interrupted_step.round_id),
                    Some(&format!(
                        "Step \"{}\" was interrupted and no remaining steps can proceed",
                        interrupted_step.title
                    )),
                )
                .await?;
            }
        }

        Ok(interrupted_step)
    }
}

/// Bootstrap 结果
#[derive(Debug)]
pub struct BootstrapResult {
    pub execution: WorkflowExecution,
    pub round: Option<WorkflowRound>,
    pub steps: Vec<WorkflowStep>,
    pub edges: Vec<WorkflowStepEdge>,
    pub agent_sessions: Vec<WorkflowAgentSession>,
    pub events: Vec<WorkflowEvent>,
    pub failed: bool,
    pub failure_reason: Option<String>,
}

#[derive(Debug)]
pub struct ResolvedTranscriptAction {
    pub transcript: WorkflowTranscript,
    pub execution: WorkflowExecution,
    pub should_wake_scheduler: bool,
}

enum TranscriptResolution {
    Resume,
    Fail(String),
}

/// Outcome of a single step execution within the scheduler.
/// Step-level state transitions are handled internally; execution-level
/// transitions are deferred to the caller.
enum StepOutcome {
    /// Step completed with a final result
    Completed,
    /// Step parked waiting for user action (approval/permission/continue/input)
    Parked,
    /// Step failed with the given error message
    Failed(String),
}

impl WorkflowOrchestrator {
    /// Execute a single step: resolve context, transition to Running, run agent
    /// prompt, process the result. Returns a StepOutcome without modifying
    /// execution-level state (the caller handles execution transitions).
    async fn prepare_and_run_step(
        db: &DBService,
        pool: &SqlitePool,
        execution: &WorkflowExecution,
        step: &WorkflowStep,
        workflow_agent_sessions: &[WorkflowAgentSession],
        session: &ChatSession,
        session_agents: &[ChatSessionAgent],
        agents: &[ChatAgent],
        plan: &WorkflowPlan,
        current_steps: &[WorkflowStep],
        edges: &[WorkflowStepEdge],
    ) -> Result<StepOutcome, OrchestratorError> {
        let workflow_session =
            resolve_step_workflow_session(execution, workflow_agent_sessions, step)?;
        let session_agent = session_agents
            .iter()
            .find(|item| item.id == workflow_session.session_agent_id)
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "session agent {} 未找到",
                    workflow_session.session_agent_id
                ))
            })?;
        let agent = agents
            .iter()
            .find(|item| item.id == session_agent.agent_id)
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("agent {} 未找到", session_agent.agent_id))
            })?;

        if workflow_session.state == WorkflowAgentSessionState::Idle {
            reducer::transition_agent_session(
                pool,
                execution,
                workflow_session,
                WorkflowAgentSessionState::Running,
            )
            .await?;
        }

        let running_step =
            reducer::transition_step(pool, execution, step, WorkflowStepStatus::Running)
                .await?
                .entity;

        let _ = Self::write_transcript(
            pool,
            execution.id,
            running_step.round_id.into(),
            Some(workflow_session.id),
            Some(running_step.id),
            "system",
            "message",
            &format!(
                "Step \"{}\" started (assigned to {})",
                running_step.title, session_agent.agent_id
            ),
            None,
        )
        .await;

        let dependency_summaries = predecessor_summaries(&running_step, current_steps, edges);
        let step_transcript_context = WorkflowTranscript::find_by_step(pool, running_step.id)
            .await?
            .into_iter()
            .map(|transcript| {
                format!(
                    "- [{}:{}] {}",
                    transcript.sender_type, transcript.entry_type, transcript.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let workflow_goal = plan
            .summary_text
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| plan.title.clone());
        let prompt = build_step_execution_prompt(
            execution,
            &workflow_goal,
            &running_step,
            &dependency_summaries,
            Some(&step_transcript_context),
        );

        let protocol_message = match run_workflow_agent_prompt(
            db,
            session,
            agent,
            session_agent,
            &prompt,
        )
        .await
        {
            Ok(raw_output) => {
                match Self::parse_step_output_message(execution.id, &running_step, &raw_output) {
                    Ok(message) => message,
                    Err(err) => {
                        let failed_step = WorkflowStep::record_execution_result(
                            pool,
                            running_step.id,
                            Uuid::new_v4(),
                            Some(
                                serde_json::to_string(&SummaryPayload {
                                    summary: err.to_string(),
                                    content: Some(raw_output),
                                    outputs: vec![],
                                })
                                .unwrap_or_else(|_| err.to_string()),
                            ),
                        )
                        .await?;
                        reducer::transition_step(
                            pool,
                            execution,
                            &failed_step,
                            WorkflowStepStatus::Failed,
                        )
                        .await?;
                        let _ = Self::write_transcript(
                            pool,
                            execution.id,
                            failed_step.round_id.into(),
                            Some(workflow_session.id),
                            Some(failed_step.id),
                            "system",
                            "message",
                            &format!("Step \"{}\" failed: {}", failed_step.title, err),
                            None,
                        )
                        .await;
                        return Ok(StepOutcome::Failed(err.to_string()));
                    }
                }
            }
            Err(err) => {
                let failed_step = WorkflowStep::record_execution_result(
                    pool,
                    running_step.id,
                    Uuid::new_v4(),
                    Some(
                        serde_json::to_string(&SummaryPayload {
                            summary: err.to_string(),
                            content: None,
                            outputs: vec![],
                        })
                        .unwrap_or_else(|_| err.to_string()),
                    ),
                )
                .await?;
                reducer::transition_step(
                    pool,
                    execution,
                    &failed_step,
                    WorkflowStepStatus::Failed,
                )
                .await?;
                let _ = Self::write_transcript(
                    pool,
                    execution.id,
                    failed_step.round_id.into(),
                    Some(workflow_session.id),
                    Some(failed_step.id),
                    "system",
                    "message",
                    &format!("Step \"{}\" failed: {}", failed_step.title, err),
                    None,
                )
                .await;
                return Ok(StepOutcome::Failed(err.to_string()));
            }
        };

        match protocol_message {
            WorkflowStepProtocolMessage::ApprovalRequest {
                title, description, ..
            } => {
                reducer::transition_step(
                    pool,
                    execution,
                    &running_step,
                    WorkflowStepStatus::WaitingReview,
                )
                .await?;
                reducer::transition_agent_session(
                    pool,
                    execution,
                    workflow_session,
                    WorkflowAgentSessionState::WaitingApproval,
                )
                .await?;
                let meta_json = serde_json::json!({
                    "description": description,
                    "resolved": false,
                })
                .to_string();
                Self::write_transcript(
                    pool,
                    execution.id,
                    Some(running_step.round_id),
                    Some(workflow_session.id),
                    Some(running_step.id),
                    "control",
                    "approval_request",
                    &title,
                    Some(&meta_json),
                )
                .await?;
                Ok(StepOutcome::Parked)
            }
            WorkflowStepProtocolMessage::PermissionRequest {
                title, description, ..
            } => {
                reducer::transition_step(
                    pool,
                    execution,
                    &running_step,
                    WorkflowStepStatus::WaitingReview,
                )
                .await?;
                reducer::transition_agent_session(
                    pool,
                    execution,
                    workflow_session,
                    WorkflowAgentSessionState::WaitingApproval,
                )
                .await?;
                let meta_json = serde_json::json!({
                    "description": description,
                    "resolved": false,
                })
                .to_string();
                Self::write_transcript(
                    pool,
                    execution.id,
                    Some(running_step.round_id),
                    Some(workflow_session.id),
                    Some(running_step.id),
                    "control",
                    "permission_request",
                    &title,
                    Some(&meta_json),
                )
                .await?;
                Ok(StepOutcome::Parked)
            }
            WorkflowStepProtocolMessage::ContinueConfirmation {
                message,
                description,
                ..
            } => {
                reducer::transition_step(
                    pool,
                    execution,
                    &running_step,
                    WorkflowStepStatus::WaitingInput,
                )
                .await?;
                reducer::transition_agent_session(
                    pool,
                    execution,
                    workflow_session,
                    WorkflowAgentSessionState::WaitingInput,
                )
                .await?;
                let meta_json = serde_json::json!({
                    "description": description,
                    "resolved": false,
                })
                .to_string();
                Self::write_transcript(
                    pool,
                    execution.id,
                    Some(running_step.round_id),
                    Some(workflow_session.id),
                    Some(running_step.id),
                    "control",
                    "continue_confirmation",
                    &message,
                    Some(&meta_json),
                )
                .await?;
                Ok(StepOutcome::Parked)
            }
            WorkflowStepProtocolMessage::InputRequest {
                prompt,
                description,
                placeholder,
                ..
            } => {
                reducer::transition_step(
                    pool,
                    execution,
                    &running_step,
                    WorkflowStepStatus::WaitingInput,
                )
                .await?;
                reducer::transition_agent_session(
                    pool,
                    execution,
                    workflow_session,
                    WorkflowAgentSessionState::WaitingInput,
                )
                .await?;
                let meta_json = serde_json::json!({
                    "description": description,
                    "resolved": false,
                    "placeholder": placeholder,
                })
                .to_string();
                Self::write_transcript(
                    pool,
                    execution.id,
                    Some(running_step.round_id),
                    Some(workflow_session.id),
                    Some(running_step.id),
                    "control",
                    "input_request",
                    &prompt,
                    Some(&meta_json),
                )
                .await?;
                Ok(StepOutcome::Parked)
            }
            WorkflowStepProtocolMessage::Error {
                message, content, ..
            } => {
                let err = Self::step_message_error(message, content);
                let failed_step = WorkflowStep::record_execution_result(
                    pool,
                    running_step.id,
                    Uuid::new_v4(),
                    Some(
                        serde_json::to_string(&SummaryPayload {
                            summary: err.to_string(),
                            content: None,
                            outputs: vec![],
                        })
                        .unwrap_or_else(|_| err.to_string()),
                    ),
                )
                .await?;
                reducer::transition_step(
                    pool,
                    execution,
                    &failed_step,
                    WorkflowStepStatus::Failed,
                )
                .await?;
                let _ = Self::write_transcript(
                    pool,
                    execution.id,
                    failed_step.round_id.into(),
                    Some(workflow_session.id),
                    Some(failed_step.id),
                    "system",
                    "message",
                    &format!("Step \"{}\" failed: {}", failed_step.title, err),
                    None,
                )
                .await;
                Ok(StepOutcome::Failed(err.to_string()))
            }
            WorkflowStepProtocolMessage::FinalResult {
                summary,
                content,
                outputs,
                ..
            } => {
                let execution_result = WorkflowStepRunResult {
                    run_id: Uuid::new_v4(),
                    summary,
                    content,
                    outputs,
                };
                let recorded_step = WorkflowStep::record_execution_result(
                    pool,
                    running_step.id,
                    execution_result.run_id,
                    Some(
                        serde_json::to_string(&SummaryPayload {
                            summary: execution_result.summary.clone(),
                            content: Some(execution_result.content.clone()),
                            outputs: execution_result.outputs.clone(),
                        })
                        .unwrap_or_else(|_| execution_result.summary.clone()),
                    ),
                )
                .await?;
                reducer::transition_step(
                    pool,
                    execution,
                    &recorded_step,
                    WorkflowStepStatus::Completed,
                )
                .await?;
                let _ = Self::write_transcript(
                    pool,
                    execution.id,
                    recorded_step.round_id.into(),
                    Some(workflow_session.id),
                    Some(recorded_step.id),
                    "agent",
                    "message",
                    &execution_result.summary,
                    Some(
                        &serde_json::to_string(&SummaryPayload {
                            summary: execution_result.summary.clone(),
                            content: Some(execution_result.content.clone()),
                            outputs: execution_result.outputs.clone(),
                        })
                        .unwrap_or_default(),
                    ),
                )
                .await;
                Ok(StepOutcome::Completed)
            }
        }
    }

    pub async fn write_transcript(
        pool: &SqlitePool,
        execution_id: Uuid,
        round_id: Option<Uuid>,
        workflow_agent_session_id: Option<Uuid>,
        step_id: Option<Uuid>,
        sender_type: &str,
        entry_type: &str,
        content: &str,
        meta_json: Option<&str>,
    ) -> Result<WorkflowTranscript, OrchestratorError> {
        WorkflowTranscript::create(
            pool,
            &CreateWorkflowTranscript {
                execution_id,
                round_id,
                workflow_agent_session_id,
                step_id,
                sender_type: sender_type.to_string(),
                entry_type: entry_type.to_string(),
                content: content.to_string(),
                meta_json: meta_json.map(String::from),
            },
            Uuid::new_v4(),
        )
        .await
        .map_err(OrchestratorError::Database)
    }

    pub async fn resolve_transcript(
        pool: &SqlitePool,
        transcript_id: Uuid,
        resolved_action: &str,
    ) -> Result<WorkflowTranscript, OrchestratorError> {
        let _transcript = WorkflowTranscript::find_by_id(pool, transcript_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("transcript {} 未找到", transcript_id))
            })?;
        let meta = serde_json::json!({
            "resolved": true,
            "resolved_action": resolved_action,
        });
        WorkflowTranscript::update_meta_json(pool, transcript_id, &meta.to_string())
            .await
            .map_err(OrchestratorError::Database)
    }
}
