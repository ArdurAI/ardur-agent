//! smoke — compiles + passes on stable Rust. Asserts the public contract
//! surface exists and is name-stable. No behavior is exercised at Phase 0
//! (the future returned by `submit` is never awaited).
use ardur_runtime::{
    ChatRuntime, Command, CommandBus, CommandReceipt, Session, SessionId, Turn, TurnId, UserMessage,
};
use uuid::Uuid;

struct Dummy;

impl ChatRuntime for Dummy {
    // `async fn` satisfies the trait's `-> impl Future<…>` declaration.
    async fn submit(&self, message: UserMessage) -> anyhow::Result<TurnId> {
        let _ = message;
        unimplemented!("smoke never awaits this future")
    }
}

impl CommandBus for Dummy {}

#[test]
fn public_surface_is_name_stable() {
    let _msg = UserMessage("hi".into());
    let _turn = Turn {
        id: TurnId(Uuid::nil()),
    };
    let _session = Session {
        id: SessionId(Uuid::nil()),
        turns: Vec::new(),
    };

    // CommandBus is object-safe; ChatRuntime (RPITIT) is not, so bind it as a
    // concrete value to prove `Dummy: ChatRuntime`.
    let runtime = Dummy;
    let _bus: &dyn CommandBus = &runtime;
    fn assert_chat_runtime<T: ChatRuntime>(_t: &T) {}
    assert_chat_runtime(&runtime);

    let _command: Option<Command> = None;
    let _receipt: Option<CommandReceipt> = None;
}
