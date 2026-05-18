use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use db::models::workflow_types::*;
use sha2::{Digest, Sha256};

use super::workflow_validator::{self, ValidationResult};

/// Compile errors produced by workflow plan compilation.
#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    #[error("Plan validation failed: {0}")]
    ValidationFailed(String),
    #[error("Compilation failed: {0}")]
    CompileError(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParallelWorkspaceConflict {
    pub wave_index: usize,
    pub workspace_path: String,
    pub members: Vec<ParallelWorkspaceMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParallelWorkspaceMember {
    pub agent_id: String,
    pub step_keys: Vec<String>,
}

/// Compiler that converts workflow plan JSON into an executable compiled graph.
pub struct WorkflowCompiler;

impl WorkflowCompiler {
    /// Parse and compile a workflow plan from a JSON string.
    pub fn compile_from_json(
        json_str: &str,
        valid_agent_ids: &[String],
    ) -> Result<CompiledGraph, CompileError> {
        let plan: WorkflowPlanJson = serde_json::from_str(json_str)
            .map_err(|e| CompileError::ValidationFailed(format!("Failed to parse JSON: {}", e)))?;

        Self::compile(&plan, valid_agent_ids)
    }

    /// Compile a workflow plan into a compiled graph.
    pub fn compile(
        plan: &WorkflowPlanJson,
        valid_agent_ids: &[String],
    ) -> Result<CompiledGraph, CompileError> {
        // 1. Run full validation.
        let validation = workflow_validator::validate_plan(plan, valid_agent_ids);
        if !validation.is_valid {
            let error_messages: Vec<String> = validation
                .errors
                .iter()
                .map(|e| format!("[{}] {}", e.field, e.message))
                .collect();
            return Err(CompileError::ValidationFailed(error_messages.join("; ")));
        }
        // 2. Compile nodes into CompiledStep.
        let default_retry = plan.globals.as_ref().map(|g| g.default_retry).unwrap_or(1);

        let mut steps: Vec<CompiledStep> = Vec::with_capacity(plan.nodes.len());
        let topo_order = Self::topological_sort(plan);

        for (order, node_id) in topo_order.iter().enumerate() {
            let node = plan.nodes.iter().find(|n| n.id == *node_id).unwrap();
            let step_type = match node.data.step_type.as_str() {
                "task" => WorkflowStepType::Task,
                "review" => WorkflowStepType::Review,
                "result" => WorkflowStepType::Result,
                other => {
                    return Err(CompileError::CompileError(format!(
                        "Unknown step type: {}",
                        other
                    )));
                }
            };

            steps.push(CompiledStep {
                step_key: node.id.clone(),
                step_type,
                title: node.data.title.clone(),
                instructions: node.data.instructions.clone(),
                assigned_agent_id: node.data.agent_id.clone(),
                acceptance: node.data.acceptance.clone(),
                outputs: node.data.outputs.clone(),
                interruptible: node.data.interruptible,
                max_retry: node.data.max_retry.unwrap_or(default_retry),
                display_order: order as i32,
                loop_key: None,
                review_scope: node.data.review_scope.clone(),
            });
        }

        // Build loops from explicit reviewScope declarations and back-patch loop_key.
        let discovered = Self::discover_loops_from_graph(plan, default_retry)?;
        let loops = if discovered.is_empty() {
            None
        } else {
            for loop_def in &discovered {
                for step in &mut steps {
                    if step.step_key == loop_def.review_step_key
                        || loop_def.member_step_keys.contains(&step.step_key)
                    {
                        step.loop_key = Some(loop_def.loop_key.clone());
                    }
                }
            }
            Some(discovered)
        };

        // 3. Compile edges into CompiledEdge.
        let edges: Vec<CompiledEdge> = plan
            .edges
            .iter()
            .map(|e| {
                let kind = e
                    .data
                    .as_ref()
                    .map(|d| match d.kind.as_str() {
                        "soft" => WorkflowEdgeKind::Soft,
                        _ => WorkflowEdgeKind::Hard,
                    })
                    .unwrap_or(WorkflowEdgeKind::Hard);

                CompiledEdge {
                    edge_id: e.id.clone(),
                    from_step_key: e.source.clone(),
                    to_step_key: e.target.clone(),
                    edge_kind: kind,
                }
            })
            .collect();

        // 4. Compute initial ready steps: nodes without incoming edges.
        let targets: HashSet<&str> = plan.edges.iter().map(|e| e.target.as_str()).collect();
        let ready_step_keys: Vec<String> = plan
            .nodes
            .iter()
            .filter(|n| !targets.contains(n.id.as_str()))
            .map(|n| n.id.clone())
            .collect();

        // 5. Compute deterministic hashes.
        let plan_hash = Self::compute_hash(plan);
        let compiled_graph_hash = Self::compute_compiled_hash(&steps, &edges, loops.as_deref());

        Ok(CompiledGraph {
            plan_hash,
            compiled_graph_hash,
            steps,
            edges,
            ready_step_keys,
            loops,
        })
    }

    /// Validate only, without compiling.
    pub fn validate_only(plan: &WorkflowPlanJson, valid_agent_ids: &[String]) -> ValidationResult {
        workflow_validator::validate_plan(plan, valid_agent_ids)
    }

    /// Finds pre-run task waves where multiple agents can run in parallel while
    /// sharing the same effective workspace.
    ///
    /// The `agent_workspace_by_id` map is keyed by workflow `agentId`. Callers
    /// should pass already-resolved effective workspaces, including any session
    /// default fallback.
    pub fn find_parallel_workspace_conflicts(
        graph: &CompiledGraph,
        agent_workspace_by_id: &HashMap<String, String>,
    ) -> Vec<ParallelWorkspaceConflict> {
        let step_by_key: HashMap<&str, &CompiledStep> = graph
            .steps
            .iter()
            .map(|step| (step.step_key.as_str(), step))
            .collect();
        let mut in_degree: BTreeMap<&str, usize> = graph
            .steps
            .iter()
            .map(|step| (step.step_key.as_str(), 0))
            .collect();
        let mut adjacency: BTreeMap<&str, Vec<&str>> = graph
            .steps
            .iter()
            .map(|step| (step.step_key.as_str(), Vec::new()))
            .collect();

        for edge in &graph.edges {
            let from = edge.from_step_key.as_str();
            let to = edge.to_step_key.as_str();
            if !step_by_key.contains_key(from) || !step_by_key.contains_key(to) {
                continue;
            }
            adjacency.entry(from).or_default().push(to);
            *in_degree.entry(to).or_default() += 1;
        }

        let mut ready: BTreeSet<&str> = in_degree
            .iter()
            .filter_map(|(step_key, degree)| (*degree == 0).then_some(*step_key))
            .collect();
        let mut conflicts = Vec::new();
        let mut wave_index = 0;

        while !ready.is_empty() {
            let wave = ready.iter().copied().collect::<Vec<_>>();
            let mut workspace_members: BTreeMap<String, BTreeMap<String, Vec<String>>> =
                BTreeMap::new();

            for step_key in &wave {
                let Some(step) = step_by_key.get(step_key) else {
                    continue;
                };
                if step.step_type != WorkflowStepType::Task {
                    continue;
                }
                let Some(agent_id) = step.assigned_agent_id.as_ref() else {
                    continue;
                };
                let Some(workspace_path) = agent_workspace_by_id
                    .get(agent_id)
                    .and_then(|path| normalize_workspace_key(path))
                else {
                    continue;
                };
                workspace_members
                    .entry(workspace_path)
                    .or_default()
                    .entry(agent_id.clone())
                    .or_default()
                    .push(step.step_key.clone());
            }

            for (workspace_path, members_by_agent) in workspace_members {
                if members_by_agent.len() <= 1 {
                    continue;
                }
                conflicts.push(ParallelWorkspaceConflict {
                    wave_index,
                    workspace_path,
                    members: members_by_agent
                        .into_iter()
                        .map(|(agent_id, step_keys)| ParallelWorkspaceMember {
                            agent_id,
                            step_keys,
                        })
                        .collect(),
                });
            }

            let mut next_ready = BTreeSet::new();
            for step_key in wave {
                if let Some(neighbors) = adjacency.get(step_key) {
                    for neighbor in neighbors {
                        if let Some(degree) = in_degree.get_mut(neighbor) {
                            *degree = degree.saturating_sub(1);
                            if *degree == 0 {
                                next_ready.insert(*neighbor);
                            }
                        }
                    }
                }
            }

            ready = next_ready;
            wave_index += 1;
        }

        conflicts
    }

    /// Compute the deterministic hash for plan JSON.
    pub fn compute_hash(plan: &WorkflowPlanJson) -> String {
        // Use canonical JSON serialization to preserve determinism.
        let canonical = serde_json::to_string(plan).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Compute the deterministic hash for compiled output, covering all fields that affect scheduling and behavior.
    fn compute_compiled_hash(
        steps: &[CompiledStep],
        edges: &[CompiledEdge],
        loops: Option<&[CompiledLoopDef]>,
    ) -> String {
        let mut hasher = Sha256::new();
        for step in steps {
            hasher.update(step.step_key.as_bytes());
            hasher.update(format!("{:?}", step.step_type).as_bytes());
            hasher.update(step.title.as_bytes());
            hasher.update(step.instructions.as_bytes());
            hasher.update(step.assigned_agent_id.as_deref().unwrap_or("").as_bytes());
            if let Some(ref acceptance) = step.acceptance {
                for a in acceptance {
                    hasher.update(a.as_bytes());
                }
            }
            if let Some(ref outputs) = step.outputs {
                for o in outputs {
                    hasher.update(o.as_bytes());
                }
            }
            hasher.update(if step.interruptible { &[1u8] } else { &[0u8] });
            hasher.update(step.max_retry.to_le_bytes());
            hasher.update(step.display_order.to_le_bytes());
            hasher.update(step.loop_key.as_deref().unwrap_or("").as_bytes());
            if let Some(ref review_scope) = step.review_scope {
                for step_key in review_scope {
                    hasher.update(step_key.as_bytes());
                }
            }
        }
        for edge in edges {
            hasher.update(edge.edge_id.as_bytes());
            hasher.update(edge.from_step_key.as_bytes());
            hasher.update(edge.to_step_key.as_bytes());
            hasher.update(format!("{:?}", edge.edge_kind).as_bytes());
        }
        if let Some(loops) = loops {
            for loop_def in loops {
                hasher.update(loop_def.loop_key.as_bytes());
                for member_step_key in &loop_def.member_step_keys {
                    hasher.update(member_step_key.as_bytes());
                }
                hasher.update(loop_def.review_step_key.as_bytes());
                for review_scope_step_key in &loop_def.review_scope_step_keys {
                    hasher.update(review_scope_step_key.as_bytes());
                }
                hasher.update(loop_def.max_retry.to_le_bytes());
                hasher.update(if loop_def.user_review_required {
                    &[1u8]
                } else {
                    &[0u8]
                });
            }
        }
        format!("{:x}", hasher.finalize())
    }

    /// Build loops from explicit review scopes.
    ///
    /// `data.reviewScope` is the source of truth for loop membership. A review node without a
    /// non-empty reviewScope is treated as a plain review step, not as a retry loop.
    fn discover_loops_from_graph(
        plan: &WorkflowPlanJson,
        default_retry: u32,
    ) -> Result<Vec<CompiledLoopDef>, CompileError> {
        let node_by_id: HashMap<&str, &WorkflowPlanNode> = plan
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node))
            .collect();

        let mut loops = Vec::new();
        let mut claimed_nodes: HashMap<String, String> = HashMap::new();

        for node in &plan.nodes {
            if node.data.step_type != "review" {
                continue;
            }

            let Some(review_scope) = node.data.review_scope.clone() else {
                continue;
            };
            if review_scope.is_empty() {
                continue;
            }

            let loop_key = format!("loop-{}", node.id);
            let member_step_keys =
                Self::validate_review_scope(plan, &node.id, &review_scope, &node_by_id)?;

            for member_key in &member_step_keys {
                if let Some(existing_loop) = claimed_nodes.get(member_key) {
                    return Err(CompileError::CompileError(format!(
                        "Invalid reviewScope: task node '{}' is declared by both loop '{}' and loop '{}'. The runtime model requires each task to belong to at most one review loop. Remove this node from one reviewScope or split it into two separate task nodes.",
                        member_key, existing_loop, loop_key
                    )));
                }
            }
            if let Some(existing_loop) = claimed_nodes.get(&node.id) {
                return Err(CompileError::CompileError(format!(
                    "Invalid reviewScope: review node '{}' is declared by both loop '{}' and loop '{}'. A review node can only be the review node for one loop.",
                    node.id, existing_loop, loop_key
                )));
            }

            for member_key in &member_step_keys {
                claimed_nodes.insert(member_key.clone(), loop_key.clone());
            }
            claimed_nodes.insert(node.id.clone(), loop_key.clone());

            let max_retry = node.data.max_retry.unwrap_or(default_retry);
            loops.push(CompiledLoopDef {
                loop_key,
                member_step_keys,
                review_step_key: node.id.clone(),
                review_scope_step_keys: review_scope,
                max_retry,
                user_review_required: true,
            });
        }

        Ok(loops)
    }

    fn validate_review_scope(
        plan: &WorkflowPlanJson,
        review_step_key: &str,
        review_scope: &[String],
        node_by_id: &HashMap<&str, &WorkflowPlanNode>,
    ) -> Result<Vec<String>, CompileError> {
        let mut outgoing: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in &plan.edges {
            outgoing
                .entry(edge.source.as_str())
                .or_default()
                .push(edge.target.as_str());
        }
        for targets in outgoing.values_mut() {
            targets.sort();
        }

        let mut member_step_keys = Vec::with_capacity(review_scope.len());
        let mut member_seen = HashSet::new();
        let mut scope_path_tasks = Vec::new();
        let mut errors = Vec::new();
        for scope_key in review_scope {
            if !member_seen.insert(scope_key.as_str()) {
                errors.push(format!(
                    "Invalid reviewScope: review node '{}' declares node '{}' more than once. Remove the duplicate entry.",
                    review_step_key, scope_key
                ));
                continue;
            }

            let Some(scope_node) = node_by_id.get(scope_key.as_str()) else {
                errors.push(format!(
                    "Invalid reviewScope: review node '{}' references missing node '{}'. Use an existing task node id.",
                    review_step_key, scope_key
                ));
                continue;
            };
            if scope_node.data.step_type != "task" {
                errors.push(format!(
                    "Invalid reviewScope: review node '{}' includes node '{}' with type '{}', but reviewScope can only include task nodes.",
                    review_step_key, scope_key, scope_node.data.step_type
                ));
                continue;
            }

            let path_tasks = match Self::task_nodes_on_paths_to_review(
                scope_key,
                review_step_key,
                &outgoing,
                node_by_id,
            ) {
                Ok(path_tasks) => path_tasks,
                Err(CompileError::CompileError(message)) => {
                    errors.push(message);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if path_tasks.is_empty() {
                errors.push(format!(
                    "Invalid reviewScope: node '{}' in review node '{}' is not a predecessor of that review node. The graph has no directed path from '{}' to '{}'.",
                    review_step_key, scope_key, scope_key, review_step_key
                ));
                continue;
            }

            member_step_keys.push(scope_key.clone());
            scope_path_tasks.push((scope_key, path_tasks));
        }

        let member_set: HashSet<&str> = member_step_keys.iter().map(String::as_str).collect();
        for (scope_key, path_tasks) in scope_path_tasks {
            for path_task in path_tasks {
                if !member_set.contains(path_task.as_str()) {
                    errors.push(format!(
                        "Invalid reviewScope: review node '{}' includes '{}', but the path from '{}' to '{}' also passes through task node '{}'. To keep retry state consistent, also add '{}' to this reviewScope or adjust the dependency edges.",
                        review_step_key,
                        scope_key,
                        scope_key,
                        review_step_key,
                        path_task,
                        path_task
                    ));
                }
            }
        }

        if !errors.is_empty() {
            return Err(CompileError::CompileError(errors.join("; ")));
        }

        Ok(member_step_keys)
    }

    fn task_nodes_on_paths_to_review(
        start_step_key: &str,
        review_step_key: &str,
        outgoing: &HashMap<&str, Vec<&str>>,
        node_by_id: &HashMap<&str, &WorkflowPlanNode>,
    ) -> Result<HashSet<String>, CompileError> {
        let mut reaches_review = false;
        let mut path_tasks = HashSet::new();
        let mut visited: HashSet<&str> = HashSet::new();
        let mut stack = vec![start_step_key];

        if !Self::can_reach_step(start_step_key, review_step_key, outgoing) {
            return Ok(HashSet::new());
        }

        while let Some(step_key) = stack.pop() {
            if step_key == review_step_key {
                reaches_review = true;
                continue;
            }
            if !visited.insert(step_key) {
                continue;
            }

            let Some(node) = node_by_id.get(step_key) else {
                continue;
            };
            match node.data.step_type.as_str() {
                "task" => {
                    path_tasks.insert(step_key.to_string());
                }
                "review" => {
                    return Err(CompileError::CompileError(format!(
                        "Invalid reviewScope: the path from node '{}' to review node '{}' passes through another review node '{}'. Loops cannot cross review boundaries; declare only the task nodes directly retried by this review node.",
                        start_step_key, review_step_key, step_key
                    )));
                }
                _ => {}
            }

            if let Some(targets) = outgoing.get(step_key) {
                for target in targets.iter().rev() {
                    if Self::can_reach_step(target, review_step_key, outgoing) {
                        stack.push(*target);
                    }
                }
            }
        }

        if reaches_review {
            Ok(path_tasks)
        } else {
            Ok(HashSet::new())
        }
    }

    fn can_reach_step(
        start_step_key: &str,
        target_step_key: &str,
        outgoing: &HashMap<&str, Vec<&str>>,
    ) -> bool {
        let mut visited: HashSet<&str> = HashSet::new();
        let mut stack = vec![start_step_key];

        while let Some(step_key) = stack.pop() {
            if step_key == target_step_key {
                return true;
            }
            if !visited.insert(step_key) {
                continue;
            }

            if let Some(targets) = outgoing.get(step_key) {
                for target in targets {
                    stack.push(*target);
                }
            }
        }

        false
    }

    /// Topological sort using Kahn's algorithm. Returns ordered node ids.
    fn topological_sort(plan: &WorkflowPlanJson) -> Vec<String> {
        let node_ids: Vec<&str> = plan.nodes.iter().map(|n| n.id.as_str()).collect();
        let node_set: HashSet<&str> = node_ids.iter().copied().collect();

        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut in_degree: HashMap<&str, usize> = HashMap::new();

        for &id in &node_ids {
            adj.entry(id).or_default();
            in_degree.entry(id).or_insert(0);
        }

        for edge in &plan.edges {
            if node_set.contains(edge.source.as_str()) && node_set.contains(edge.target.as_str()) {
                adj.entry(edge.source.as_str())
                    .or_default()
                    .push(edge.target.as_str());
                *in_degree.entry(edge.target.as_str()).or_insert(0) += 1;
            }
        }

        // Use a sorted queue for deterministic output.
        let mut queue: Vec<&str> = in_degree
            .iter()
            .filter(|entry| *entry.1 == 0)
            .map(|entry| *entry.0)
            .collect();
        queue.sort();

        let mut result = Vec::with_capacity(node_ids.len());

        while let Some(node) = queue.pop() {
            // pop from sorted → take last for determinism (reverse sorted)
            result.push(node.to_string());
            let mut next_ready = Vec::new();
            if let Some(neighbors) = adj.get(node) {
                for &neighbor in neighbors {
                    let deg = in_degree.get_mut(neighbor).unwrap();
                    *deg -= 1;
                    if *deg == 0 {
                        next_ready.push(neighbor);
                    }
                }
            }
            next_ready.sort();
            // Insert in sorted order so pop() gives deterministic results
            for n in next_ready.into_iter().rev() {
                queue.push(n);
            }
            queue.sort();
        }

        result
    }
}

fn normalize_workspace_key(path: &str) -> Option<String> {
    let mut normalized = path.trim().replace('\\', "/");
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    if normalized.is_empty() {
        return None;
    }
    #[cfg(windows)]
    {
        normalized = normalized.to_ascii_lowercase();
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan_json() -> &'static str {
        r#"{
            "version": 1,
            "title": "测试计划",
            "goal": "测试编译",
            "agents": { "lead": "lead-agent", "available": ["agent-1", "agent-2"] },
            "nodes": [
                {
                    "id": "task_1", "type": "workflowStep",
                    "position": { "x": 0, "y": 0 },
                    "data": { "stepType": "task", "agentId": "agent-1", "title": "任务1", "instructions": "做事1" }
                },
                {
                    "id": "task_2", "type": "workflowStep",
                    "position": { "x": 240, "y": 0 },
                    "data": { "stepType": "task", "agentId": "agent-2", "title": "任务2", "instructions": "做事2" }
                },
                {
                    "id": "result", "type": "workflowStep",
                    "position": { "x": 120, "y": 140 },
                    "data": { "stepType": "result", "title": "最终结果", "instructions": "汇总" }
                }
            ],
            "edges": [
                { "id": "task_1->result", "source": "task_1", "target": "result" },
                { "id": "task_2->result", "source": "task_2", "target": "result" }
            ]
        }"#
    }

    fn agents() -> Vec<String> {
        vec!["lead-agent".into(), "agent-1".into(), "agent-2".into()]
    }

    fn loop_plan_json() -> String {
        serde_json::json!({
            "version": 1,
            "title": "回路测试计划",
            "goal": "验证回路编译",
            "agents": { "lead": "lead-agent", "available": ["agent-1", "agent-2"] },
            "nodes": [
                {
                    "id": "draft", "type": "workflowStep",
                    "position": { "x": 0, "y": 0 },
                    "data": { "stepType": "task", "agentId": "agent-1", "title": "起草", "instructions": "产出初稿" }
                },
                {
                    "id": "revise", "type": "workflowStep",
                    "position": { "x": 200, "y": 0 },
                    "data": { "stepType": "task", "agentId": "agent-2", "title": "修订", "instructions": "补充细节" }
                },
                {
                    "id": "review", "type": "workflowStep",
                    "position": { "x": 400, "y": 0 },
                    "data": { "stepType": "review", "title": "审核", "instructions": "审核回路结果", "reviewScope": ["draft", "revise"], "maxRetry": 2 }
                },
                {
                    "id": "result", "type": "workflowStep",
                    "position": { "x": 600, "y": 0 },
                    "data": { "stepType": "result", "title": "最终结果", "instructions": "汇总" }
                }
            ],
            "edges": [
                { "id": "draft->revise", "source": "draft", "target": "revise" },
                { "id": "revise->review", "source": "revise", "target": "review" },
                { "id": "draft->review", "source": "draft", "target": "review" },
                { "id": "review->result", "source": "review", "target": "result" }
            ]
        })
        .to_string()
    }
    #[test]
    fn test_compile_success() {
        let graph = WorkflowCompiler::compile_from_json(sample_plan_json(), &agents()).unwrap();
        assert_eq!(graph.steps.len(), 3);
        assert_eq!(graph.edges.len(), 2);
        assert!(!graph.plan_hash.is_empty());
        assert!(!graph.compiled_graph_hash.is_empty());
    }

    #[test]
    fn test_ready_steps() {
        let graph = WorkflowCompiler::compile_from_json(sample_plan_json(), &agents()).unwrap();
        // task_1 and task_2 have no incoming edges
        assert!(graph.ready_step_keys.contains(&"task_1".to_string()));
        assert!(graph.ready_step_keys.contains(&"task_2".to_string()));
        assert!(!graph.ready_step_keys.contains(&"result".to_string()));
    }

    #[test]
    fn detects_parallel_task_members_sharing_workspace_before_run() {
        let graph = WorkflowCompiler::compile_from_json(sample_plan_json(), &agents()).unwrap();
        let workspaces = HashMap::from([
            ("agent-1".to_string(), "/workspace/project".to_string()),
            ("agent-2".to_string(), "/workspace/project/".to_string()),
        ]);

        let conflicts = WorkflowCompiler::find_parallel_workspace_conflicts(&graph, &workspaces);

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].wave_index, 0);
        assert_eq!(conflicts[0].workspace_path, "/workspace/project");
        assert_eq!(
            conflicts[0].members,
            vec![
                ParallelWorkspaceMember {
                    agent_id: "agent-1".to_string(),
                    step_keys: vec!["task_1".to_string()],
                },
                ParallelWorkspaceMember {
                    agent_id: "agent-2".to_string(),
                    step_keys: vec!["task_2".to_string()],
                },
            ]
        );
    }

    #[test]
    fn ignores_same_workspace_members_when_tasks_are_serial() {
        let plan = serde_json::json!({
            "version": 1,
            "title": "Serial task test",
            "goal": "Serial task members share workspace but cannot run together",
            "agents": { "lead": "lead-agent", "available": ["agent-1", "agent-2"] },
            "nodes": [
                { "id": "a", "type": "workflowStep", "position": { "x": 0, "y": 0 }, "data": { "stepType": "task", "agentId": "agent-1", "title": "A", "instructions": "A" } },
                { "id": "b", "type": "workflowStep", "position": { "x": 200, "y": 0 }, "data": { "stepType": "task", "agentId": "agent-2", "title": "B", "instructions": "B" } },
                { "id": "result", "type": "workflowStep", "position": { "x": 400, "y": 0 }, "data": { "stepType": "result", "title": "Result", "instructions": "Result" } }
            ],
            "edges": [
                { "id": "a->b", "source": "a", "target": "b" },
                { "id": "b->result", "source": "b", "target": "result" }
            ]
        })
        .to_string();
        let graph = WorkflowCompiler::compile_from_json(&plan, &agents()).unwrap();
        let workspaces = HashMap::from([
            ("agent-1".to_string(), "/workspace/project".to_string()),
            ("agent-2".to_string(), "/workspace/project".to_string()),
        ]);

        let conflicts = WorkflowCompiler::find_parallel_workspace_conflicts(&graph, &workspaces);

        assert!(conflicts.is_empty());
    }

    #[test]
    fn test_deterministic_hash() {
        let graph1 = WorkflowCompiler::compile_from_json(sample_plan_json(), &agents()).unwrap();
        let graph2 = WorkflowCompiler::compile_from_json(sample_plan_json(), &agents()).unwrap();
        assert_eq!(graph1.plan_hash, graph2.plan_hash);
        assert_eq!(graph1.compiled_graph_hash, graph2.compiled_graph_hash);
    }

    #[test]
    fn test_compile_invalid_json() {
        let result = WorkflowCompiler::compile_from_json("not json", &agents());
        assert!(result.is_err());
    }

    #[test]
    fn test_compile_invalid_plan() {
        let json = r#"{ "version": 1, "title": "", "goal": "test", "agents": {"lead": "x", "available": []}, "nodes": [], "edges": [] }"#;
        let result = WorkflowCompiler::compile_from_json(json, &["x".into()]);
        assert!(result.is_err());
    }

    #[test]
    fn test_topological_order() {
        let graph = WorkflowCompiler::compile_from_json(sample_plan_json(), &agents()).unwrap();
        // result must come after task_1 and task_2
        let result_pos = graph
            .steps
            .iter()
            .position(|s| s.step_key == "result")
            .unwrap();
        let task1_pos = graph
            .steps
            .iter()
            .position(|s| s.step_key == "task_1")
            .unwrap();
        let task2_pos = graph
            .steps
            .iter()
            .position(|s| s.step_key == "task_2")
            .unwrap();
        assert!(result_pos > task1_pos);
        assert!(result_pos > task2_pos);
    }

    #[test]
    fn test_compile_loops_from_review_scope() {
        let graph = WorkflowCompiler::compile_from_json(&loop_plan_json(), &agents()).unwrap();
        let loops = graph.loops.expect("explicit review scope loops");

        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].loop_key, "loop-review");
        let mut member_keys = loops[0].member_step_keys.clone();
        member_keys.sort();
        assert_eq!(member_keys, vec!["draft", "revise"]);
        assert_eq!(loops[0].review_step_key, "review");
        assert_eq!(loops[0].review_scope_step_keys, vec!["draft", "revise"]);
        assert_eq!(loops[0].max_retry, 2);
        assert!(loops[0].user_review_required);

        for step in &graph.steps {
            if step.step_key == "draft" || step.step_key == "revise" || step.step_key == "review" {
                assert_eq!(step.loop_key, Some("loop-review".to_string()));
            } else {
                assert_eq!(step.loop_key, None);
            }
        }
    }

    #[test]
    fn test_compile_rejects_non_predecessor_review_scope() {
        let mut invalid: serde_json::Value = serde_json::from_str(&loop_plan_json()).unwrap();
        // "result" is not a predecessor task of review node
        invalid["nodes"][2]["data"]["reviewScope"] = serde_json::json!(["result"]);
        let result = WorkflowCompiler::compile_from_json(&invalid.to_string(), &agents());

        assert!(
            matches!(result, Err(CompileError::CompileError(message)) if message.contains("reviewScope"))
        );
    }

    #[test]
    fn test_compile_rejects_shared_scope_across_loops() {
        let invalid = serde_json::json!({
            "version": 1,
            "title": "共享前置节点",
            "goal": "验证节点不能属于多个回路",
            "agents": { "lead": "lead-agent", "available": ["agent-1", "agent-2"] },
            "nodes": [
                { "id": "a1", "type": "workflowStep", "position": { "x": 0, "y": 0 }, "data": { "stepType": "task", "agentId": "agent-1", "title": "A1", "instructions": "A1" } },
                { "id": "a_review", "type": "workflowStep", "position": { "x": 0, "y": 100 }, "data": { "stepType": "review", "title": "A Review", "instructions": "review", "reviewScope": ["a1"] } },
                { "id": "b_review", "type": "workflowStep", "position": { "x": 200, "y": 100 }, "data": { "stepType": "review", "title": "B Review", "instructions": "review", "reviewScope": ["a1"] } },
                { "id": "result", "type": "workflowStep", "position": { "x": 400, "y": 50 }, "data": { "stepType": "result", "title": "Result", "instructions": "汇总" } }
            ],
            "edges": [
                { "id": "a1->a_review", "source": "a1", "target": "a_review" },
                { "id": "a1->b_review", "source": "a1", "target": "b_review" },
                { "id": "a_review->result", "source": "a_review", "target": "result" },
                { "id": "b_review->result", "source": "b_review", "target": "result" }
            ]
        })
        .to_string();
        let result = WorkflowCompiler::compile_from_json(&invalid, &agents());

        assert!(
            matches!(result, Err(CompileError::CompileError(message)) if message.contains("declared by both"))
        );
    }

    #[test]
    fn test_review_without_scope_does_not_create_loop() {
        let plan = serde_json::json!({
            "version": 1,
            "title": "Plain review test",
            "goal": "Review without scope is not a loop",
            "agents": { "lead": "lead-agent", "available": ["agent-1", "agent-2"] },
            "nodes": [
                { "id": "a", "type": "workflowStep", "position": { "x": 0, "y": 0 }, "data": { "stepType": "task", "agentId": "agent-1", "title": "A", "instructions": "A" } },
                { "id": "b", "type": "workflowStep", "position": { "x": 200, "y": 0 }, "data": { "stepType": "review", "title": "B", "instructions": "review" } },
                { "id": "result", "type": "workflowStep", "position": { "x": 400, "y": 0 }, "data": { "stepType": "result", "title": "Result", "instructions": "result" } }
            ],
            "edges": [
                { "id": "a->b", "source": "a", "target": "b" },
                { "id": "b->result", "source": "b", "target": "result" }
            ]
        })
        .to_string();

        let graph = WorkflowCompiler::compile_from_json(&plan, &agents()).unwrap();
        assert!(graph.loops.is_none());
    }

    #[test]
    fn test_compile_rejects_missing_intermediate_scope_task() {
        let mut invalid: serde_json::Value = serde_json::from_str(&loop_plan_json()).unwrap();
        invalid["nodes"][2]["data"]["reviewScope"] = serde_json::json!(["draft"]);
        let result = WorkflowCompiler::compile_from_json(&invalid.to_string(), &agents());

        assert!(
            matches!(result, Err(CompileError::CompileError(message)) if message.contains("revise"))
        );
    }

    #[test]
    fn test_compile_reports_all_review_scope_errors() {
        let invalid = serde_json::json!({
            "version": 1,
            "title": "Invalid review scope",
            "goal": "Collect all review scope errors",
            "agents": { "lead": "lead-agent", "available": ["agent-1", "agent-2"] },
            "nodes": [
                { "id": "draft", "type": "workflowStep", "position": { "x": 0, "y": 0 }, "data": { "stepType": "task", "agentId": "agent-1", "title": "Draft", "instructions": "Draft" } },
                { "id": "revise", "type": "workflowStep", "position": { "x": 200, "y": 0 }, "data": { "stepType": "task", "agentId": "agent-2", "title": "Revise", "instructions": "Revise" } },
                { "id": "side", "type": "workflowStep", "position": { "x": 200, "y": 100 }, "data": { "stepType": "task", "agentId": "agent-2", "title": "Side", "instructions": "Side" } },
                { "id": "review", "type": "workflowStep", "position": { "x": 400, "y": 0 }, "data": { "stepType": "review", "title": "Review", "instructions": "Review", "reviewScope": ["draft", "draft", "missing", "review", "side"] } },
                { "id": "result", "type": "workflowStep", "position": { "x": 600, "y": 0 }, "data": { "stepType": "result", "title": "Result", "instructions": "Result" } }
            ],
            "edges": [
                { "id": "draft->revise", "source": "draft", "target": "revise" },
                { "id": "revise->review", "source": "revise", "target": "review" },
                { "id": "review->result", "source": "review", "target": "result" },
                { "id": "side->result", "source": "side", "target": "result" }
            ]
        })
        .to_string();
        let result = WorkflowCompiler::compile_from_json(&invalid, &agents());

        let Err(CompileError::CompileError(message)) = result else {
            panic!("expected aggregated reviewScope errors");
        };
        assert!(message.contains("more than once"));
        assert!(message.contains("missing"));
        assert!(message.contains("type 'review'"));
        assert!(message.contains("not a predecessor"));
        assert!(message.contains("revise"));
    }

    #[test]
    fn test_no_loops_without_review_nodes() {
        // Plans without review nodes should have no loops
        let graph = WorkflowCompiler::compile_from_json(sample_plan_json(), &agents()).unwrap();
        assert!(graph.loops.is_none());
    }
}
