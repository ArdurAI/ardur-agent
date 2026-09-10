//! Integration test for the `ardur-standing-goals` public surface: drives a
//! goal through its full lifecycle via `GoalRegistry`, the way a real
//! consumer (a scheduler loop, an admin surface) would — construct through
//! `StandingGoal::new`, mutate via the public transition methods, persist
//! through the registry, and verify serde round-trips and error paths.

use ardur_standing_goals::{Frequency, GoalRegistry, GoalStatus, StandingGoal, StandingGoalError};

#[test]
fn goal_moves_through_its_full_lifecycle_via_the_registry() {
    let registry = GoalRegistry::new();

    let goal = StandingGoal::new(
        "Nightly reconciliation",
        "Sweep orphan receipts",
        Frequency::Daily,
        "gnani",
    );
    let id = registry.create(goal).expect("create succeeds");

    // Fresh goal starts Active with a zeroed run history.
    let fetched = registry.get(&id).expect("get succeeds");
    assert_eq!(fetched.status, GoalStatus::Active);
    assert_eq!(fetched.run_count, 0);
    assert_eq!(fetched.success_rate(), 0.0);

    // A consumer records two runs (one failed) and writes the mutation back.
    let mut running = fetched;
    running.record_run(true);
    running.record_run(false);
    registry.update(running.clone()).expect("update succeeds");

    let after_runs = registry.get(&id).expect("get succeeds");
    assert_eq!(after_runs.run_count, 2);
    assert_eq!(after_runs.success_count, 1);
    assert_eq!(after_runs.success_rate(), 0.5);
    assert!(after_runs.last_run.is_some(), "record_run stamps last_run");

    // Pause, then resume, round-tripping through the registry each time.
    let mut paused = after_runs;
    paused.pause();
    registry.update(paused).expect("update succeeds");
    assert_eq!(
        registry.get(&id).expect("get succeeds").status,
        GoalStatus::Paused
    );
    assert_eq!(
        registry.list_by_status(GoalStatus::Active).unwrap().len(),
        0
    );
    assert_eq!(
        registry.list_by_status(GoalStatus::Paused).unwrap().len(),
        1
    );

    let mut resumed = registry.get(&id).unwrap();
    resumed.resume();
    registry.update(resumed).expect("update succeeds");
    assert_eq!(
        registry.get(&id).expect("get succeeds").status,
        GoalStatus::Active
    );

    // Mark completed, then remove — the terminal transitions.
    let mut completed = registry.get(&id).unwrap();
    completed.mark_completed();
    registry.update(completed).expect("update succeeds");
    assert_eq!(
        registry.get(&id).expect("get succeeds").status,
        GoalStatus::Completed
    );

    registry.remove(&id).expect("remove succeeds");
    assert!(matches!(
        registry.get(&id),
        Err(StandingGoalError::NotFound(_))
    ));
}

#[test]
fn registry_rejects_update_and_remove_of_an_unknown_goal() {
    let registry = GoalRegistry::new();
    let ghost = StandingGoal::new("Ghost", "Never created", Frequency::Weekly, "gnani");

    assert!(matches!(
        registry.update(ghost.clone()),
        Err(StandingGoalError::NotFound(id)) if id == ghost.id
    ));
    assert!(matches!(
        registry.remove(&ghost.id),
        Err(StandingGoalError::NotFound(id)) if id == ghost.id
    ));
}

#[test]
fn goal_survives_a_serde_json_round_trip() {
    let mut goal = StandingGoal::new(
        "Custom cadence goal",
        "Exercise the Custom frequency variant",
        Frequency::Custom("*/15 * * * *".to_string()),
        "gnani",
    );
    goal.metadata
        .insert("owner_team".to_string(), "platform".to_string());
    goal.record_run(true);

    let json = serde_json::to_string(&goal).expect("goal serializes");
    let restored: StandingGoal = serde_json::from_str(&json).expect("goal deserializes");

    assert_eq!(restored.id, goal.id);
    assert_eq!(restored.frequency, goal.frequency);
    assert_eq!(restored.run_count, goal.run_count);
    assert_eq!(restored.success_count, goal.success_count);
    assert_eq!(
        restored.metadata.get("owner_team"),
        Some(&"platform".to_string())
    );
}

#[test]
fn list_reflects_every_created_goal_regardless_of_status() {
    let registry = GoalRegistry::new();
    let statuses_seeded = [
        Frequency::Hourly,
        Frequency::Daily,
        Frequency::Weekly,
        Frequency::Monthly,
    ];
    for frequency in statuses_seeded {
        let goal = StandingGoal::new("Seeded", "Desc", frequency, "gnani");
        registry.create(goal).expect("create succeeds");
    }

    let all = registry.list().expect("list succeeds");
    assert_eq!(all.len(), 4);
    assert!(all.iter().all(|g| g.status == GoalStatus::Active));
}
