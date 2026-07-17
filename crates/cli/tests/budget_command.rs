//! §2.1 Phase 1: a `/budget` command bound to a 1000c budget reports that
//! balance. The counter is seeded from a real `InMemoryBudgetStore` so the test
//! exercises the §11.14 budget surface, not just an atomic.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use ardur_cli::BudgetCommand;
use ardur_cost_gate::{BudgetStore, CostTuple, HolderId, InMemoryBudgetStore};
use ardur_runtime::{CommandBus, CommandContext, InMemoryCommandBus};

#[test]
fn budget_command_reports_initial_balance() {
    let holder = HolderId("cli-session".to_string());
    let store = InMemoryBudgetStore::new();
    store.set_balance(holder.clone(), CostTuple::cents(1000));

    // Read the provisioned balance back through the async store, then mirror it
    // into the shared counter the `/budget` command reads.
    let balance = tokio::runtime::Runtime::new()
        .expect("tokio runtime builds")
        .block_on(store.current_balance(&holder))
        .expect("the holder was provisioned");
    let remaining = Arc::new(AtomicU64::new(balance.cents));

    let mut bus = InMemoryCommandBus::new();
    bus.register_command("budget", Box::new(BudgetCommand::new(remaining)));

    let result = bus
        .dispatch(CommandContext {
            command: "budget".to_string(),
            args: String::new(),
        })
        .expect("/budget is registered");

    assert!(
        result.output.contains("1000"),
        "budget output should mention the 1000c balance, got: {}",
        result.output
    );
}
