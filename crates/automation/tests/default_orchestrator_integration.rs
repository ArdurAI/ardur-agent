use ardur_automation::{
    BundleHash, CedarExpression, DefaultTaskFlowOrchestrator, FlowControl, FlowNode, FlowStep,
    ParallelWait, ProviderId, Sha256Digest, StepDispatch, StepFailureKind, StepId, StepOutcome,
    TaskCreationRequest, TaskFlowDag, TaskFlowOrchestrator, ToolId,
};
use ardur_cap_token::{HolderId, VerifiedClaims};
use uuid::Uuid;

fn digest(byte: u8) -> Sha256Digest {
    Sha256Digest([byte; 32])
}

fn claims_with(allowlist: &[&str]) -> VerifiedClaims {
    VerifiedClaims {
        token_id: Uuid::now_v7(),
        audience: "ardur-automation".to_string(),
        subject: HolderId("spiffe://tenant/user".to_string()),
        expires_unix: 4_102_444_800,
        budget_remaining: 1_000,
        tool_allowlist: allowlist.iter().map(|value| (*value).to_string()).collect(),
        capabilities: Vec::new(),
    }
}

fn verified_claims() -> VerifiedClaims {
    claims_with(&["task.create", "tool.echo", "provider.stub", "task.override"])
}

fn step(name: &str, estimated_duration_ms: u32) -> FlowStep {
    FlowStep {
        step_id: StepId::new(),
        step_name: name.to_string(),
        dispatch: StepDispatch::ToolCall {
            tool_id: ToolId("tool.echo".to_string()),
            args_template: serde_json::json!({ "message": name }),
        },
        verify_via: None,
        estimated_cost_micro_usd: 10,
        estimated_duration_ms,
    }
}

fn sequence_dag(steps: Vec<FlowStep>) -> TaskFlowDag {
    TaskFlowDag {
        dag_hash: BundleHash(digest(9)),
        version: 1,
        root: FlowNode::Control(FlowControl::Sequence(
            steps.into_iter().map(FlowNode::Step).collect(),
        )),
        max_depth: 4,
        max_fanout: 4,
        invariants: Vec::new(),
        estimated_total_cost_micro_usd: 20,
    }
}

#[tokio::test]
async fn default_orchestrator_executes_sequence_dag_to_success() {
    let orchestrator = DefaultTaskFlowOrchestrator::new();
    let dag = sequence_dag(vec![
        step("plan", 5),
        FlowStep {
            dispatch: StepDispatch::ProviderCall {
                provider_id: ProviderId("stub".to_string()),
                request_template: serde_json::json!({ "prompt": "execute" }),
            },
            ..step("execute", 5)
        },
    ]);
    let step_ids: Vec<StepId> = match &dag.root {
        FlowNode::Control(FlowControl::Sequence(nodes)) => nodes
            .iter()
            .filter_map(|node| node.as_step().map(|step| step.step_id))
            .collect(),
        _ => unreachable!(),
    };

    let handle = orchestrator
        .create_task(
            &verified_claims(),
            TaskCreationRequest {
                description: "real sequence flow".to_string(),
                flow_dag: Some(dag),
            },
        )
        .await
        .expect("create task");
    let state = orchestrator
        .get_task_state(handle.task_id)
        .await
        .expect("state is stored");

    assert!(matches!(
        state.outcome,
        Some(ardur_automation::TaskOutcome::Succeeded)
    ));
    assert!(state.active_control.is_none());
    for step_id in step_ids {
        assert_eq!(
            state.step_outcomes.get(&step_id),
            Some(&StepOutcome::Succeeded)
        );
    }
}

#[tokio::test]
async fn default_orchestrator_records_timeout_error_path() {
    let orchestrator = DefaultTaskFlowOrchestrator::new().with_step_timeout_ms(10);
    let slow_step = step("slow", 25);
    let slow_step_id = slow_step.step_id;
    let handle = orchestrator
        .create_task(
            &verified_claims(),
            TaskCreationRequest {
                description: "timeout flow".to_string(),
                flow_dag: Some(sequence_dag(vec![slow_step])),
            },
        )
        .await
        .expect("timeout is recorded, not a create error");
    let state = orchestrator
        .get_task_state(handle.task_id)
        .await
        .expect("state is stored");

    assert!(matches!(
        state.outcome,
        Some(ardur_automation::TaskOutcome::Failed)
    ));
    assert!(matches!(
        state.step_outcomes.get(&slow_step_id),
        Some(StepOutcome::Failed {
            failure_kind: StepFailureKind::TimeoutExceeded
        })
    ));
}

#[tokio::test]
async fn default_orchestrator_rejects_invalid_empty_description() {
    let orchestrator = DefaultTaskFlowOrchestrator::new();
    let err = orchestrator
        .create_task(
            &verified_claims(),
            TaskCreationRequest {
                description: "   ".to_string(),
                flow_dag: None,
            },
        )
        .await
        .expect_err("empty description is invalid");
    assert!(err.to_string().contains("description must not be empty"));
}

#[tokio::test]
async fn default_orchestrator_rejects_step_dispatch_outside_cap_token_allowlist() {
    let orchestrator = DefaultTaskFlowOrchestrator::new();
    let err = orchestrator
        .create_task(
            &claims_with(&["task.create"]),
            TaskCreationRequest {
                description: "unauthorized dispatch".to_string(),
                flow_dag: Some(sequence_dag(vec![step("not allowed", 5)])),
            },
        )
        .await
        .expect_err("tool.echo is not in the cap-token allowlist");

    assert!(err.to_string().contains("tool.echo"));
    assert!(err.to_string().contains("cap-token"));
}

#[tokio::test]
async fn default_orchestrator_rejects_unsupported_or_empty_control_flow() {
    let orchestrator = DefaultTaskFlowOrchestrator::new();
    let cases = vec![
        (
            "empty sequence",
            FlowNode::Control(FlowControl::Sequence(Vec::new())),
        ),
        (
            "empty parallel all",
            FlowNode::Control(FlowControl::Parallel {
                branches: Vec::new(),
                wait: ParallelWait::All,
            }),
        ),
        (
            "anyn zero",
            FlowNode::Control(FlowControl::Parallel {
                branches: vec![FlowNode::Step(step("branch", 5))],
                wait: ParallelWait::AnyN(0),
            }),
        ),
        (
            "conditional without Cedar evaluator",
            FlowNode::Control(FlowControl::Conditional {
                predicate: CedarExpression(serde_json::json!({ "op": "eq", "value": true })),
                then_branch: Box::new(FlowNode::Step(step("then", 5))),
                else_branch: None,
            }),
        ),
    ];

    for (label, root) in cases {
        let err = orchestrator
            .create_task(
                &verified_claims(),
                TaskCreationRequest {
                    description: label.to_string(),
                    flow_dag: Some(TaskFlowDag {
                        dag_hash: BundleHash(digest(7)),
                        version: 1,
                        root,
                        max_depth: 4,
                        max_fanout: 4,
                        invariants: Vec::new(),
                        estimated_total_cost_micro_usd: 10,
                    }),
                },
            )
            .await
            .expect_err(&format!("{label} should be rejected fail-closed"));
        assert!(
            err.to_string().contains("unsupported")
                || err.to_string().contains("empty")
                || err.to_string().contains("AnyN"),
            "{label}: unexpected error {err}"
        );
    }
}

#[tokio::test]
async fn operator_override_rejects_steps_that_are_not_verification_failed() {
    let orchestrator = DefaultTaskFlowOrchestrator::new().with_step_timeout_ms(10);
    let slow_step = step("slow", 25);
    let slow_step_id = slow_step.step_id;
    let handle = orchestrator
        .create_task(
            &verified_claims(),
            TaskCreationRequest {
                description: "timeout flow".to_string(),
                flow_dag: Some(sequence_dag(vec![slow_step])),
            },
        )
        .await
        .expect("timeout is recorded, not a create error");

    let err = orchestrator
        .operator_override_verification(
            &verified_claims(),
            handle.task_id,
            slow_step_id,
            "operator saw enough evidence".to_string(),
        )
        .await
        .expect_err("only verification-failed steps may be overridden");

    assert!(err.to_string().contains("verification"));
}
