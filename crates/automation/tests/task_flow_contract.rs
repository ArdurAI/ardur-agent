use ardur_automation::{
    BundleHash, FlowControl, FlowNode, FlowStep, MockTaskFlowOrchestrator, ProviderId,
    Sha256Digest, StepDispatch, StepId, TaskCreationRequest, TaskFlowDag, TaskFlowOrchestrator,
    TaskRecord, TaskStatus, ToolId,
};
use ardur_cap_token::{HolderId, VerifiedClaims};
use uuid::Uuid;

fn digest(byte: u8) -> Sha256Digest {
    Sha256Digest([byte; 32])
}

fn verified_claims() -> VerifiedClaims {
    VerifiedClaims {
        token_id: Uuid::now_v7(),
        audience: "ardur-automation".to_string(),
        subject: HolderId("spiffe://tenant/user".to_string()),
        expires_unix: 4_102_444_800,
        budget_remaining: 1_000,
        tool_allowlist: vec!["task.create".to_string()],
    }
}

fn sample_step(name: &str) -> FlowStep {
    FlowStep {
        step_id: StepId::new(),
        step_name: name.to_string(),
        dispatch: StepDispatch::ToolCall {
            tool_id: ToolId("tool.echo".to_string()),
            args_template: serde_json::json!({ "message": name }),
        },
        verify_via: None,
        estimated_cost_micro_usd: 10,
        estimated_duration_ms: 5,
    }
}

fn sample_dag() -> TaskFlowDag {
    TaskFlowDag {
        dag_hash: BundleHash(digest(7)),
        version: 1,
        root: FlowNode::Control(FlowControl::Sequence(vec![
            FlowNode::Step(sample_step("plan")),
            FlowNode::Step(FlowStep {
                dispatch: StepDispatch::ProviderCall {
                    provider_id: ProviderId("stub".to_string()),
                    request_template: serde_json::json!({ "prompt": "execute" }),
                },
                ..sample_step("execute")
            }),
        ])),
        max_depth: 4,
        max_fanout: 2,
        invariants: Vec::new(),
        estimated_total_cost_micro_usd: 20,
    }
}

#[test]
fn task_flow_dag_serializes_round_trip() {
    let dag = sample_dag();
    let encoded = serde_json::to_string(&dag).expect("serialize dag");
    let decoded: TaskFlowDag = serde_json::from_str(&encoded).expect("deserialize dag");

    assert_eq!(decoded, dag);
}

#[test]
fn task_record_defaults_flow_fields_for_single_step_records() {
    let raw = serde_json::json!({
        "task_id": Uuid::now_v7(),
        "mission_id": "mission.alpha",
        "description": "single-step task",
        "status": "pending",
        "created_at_ms": 1,
        "updated_at_ms": 1
    });

    let record: TaskRecord = serde_json::from_value(raw).expect("deserialize task record");

    assert_eq!(record.status, TaskStatus::Pending);
    assert!(record.flow_dag.is_none());
    assert!(record.flow_runtime_state.is_none());
    assert!(record.receipts.is_empty());
}

#[tokio::test]
async fn mock_orchestrator_tracks_create_get_and_cancel() {
    let orchestrator = MockTaskFlowOrchestrator::new();
    let token = verified_claims();
    let handle = orchestrator
        .create_task(
            &token,
            TaskCreationRequest {
                description: "two step flow".to_string(),
                flow_dag: Some(sample_dag()),
            },
        )
        .await
        .expect("create task");

    let running_state = orchestrator
        .get_task_state(handle.task_id)
        .await
        .expect("get created task");
    assert!(running_state.active_control.is_some());
    assert!(running_state.outcome.is_none());

    orchestrator
        .cancel_task(&token, handle.task_id)
        .await
        .expect("cancel task");

    let cancelled_state = orchestrator
        .get_task_state(handle.task_id)
        .await
        .expect("get cancelled task");
    assert!(matches!(
        cancelled_state.outcome,
        Some(ardur_automation::TaskOutcome::Cancelled)
    ));
}
