//! ardur-runtime — the §1.0 runtime foundation: the chat runtime, the typed
//! command bus, and the Session/Turn domain types every other crate plugs
//! into.
//!
//! Plan family: §1.0 (`plans/1.0-phase-one-runtime-foundation-plan.md`).
//!
//! PHASE 0: contracts only. No implementation bodies — every trait method is
//! `unimplemented!()`. The public trait surface is FROZEN against §1.0;
//! widening it is a §0.0 amendment. Bodies land in §1.0 Phase 1 (the first
//! real implementation work after this scaffold).
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::future::Future;

use anyhow::Result;
use uuid::Uuid;

/// A message submitted by the user to begin or continue a turn.
#[derive(Clone, Debug)]
pub struct UserMessage(pub String);

/// The stable, time-ordered identifier of a single turn (UUIDv7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TurnId(pub Uuid);

/// The stable, time-ordered identifier of a session (UUIDv7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(pub Uuid);

/// A typed command dispatched through the [`CommandBus`]. The concrete
/// command variants land in §1.0 Phase 1.
#[derive(Clone, Debug, Default)]
pub struct Command {
    // TODO(§1.0 Phase 1): the typed command envelope (verb + payload + scope).
}

/// The receipt returned by the bus acknowledging a dispatched [`Command`].
#[derive(Clone, Debug, Default)]
pub struct CommandReceipt {
    // TODO(§1.0 Phase 1): accepted/rejected + the emitted receipt hash.
}

/// A single request/response cycle within a [`Session`].
#[derive(Clone, Debug)]
pub struct Turn {
    /// The turn's stable identifier.
    pub id: TurnId,
    // TODO(§1.0 Phase 1): request, response, receipts, cost.
}

/// A conversational session: a stable id plus its ordered turns. The cap-token
/// binding and the full history model are added in §1.0 Phase 1.
#[derive(Clone, Debug)]
pub struct Session {
    /// The session's stable identifier.
    pub id: SessionId,
    /// The turns recorded in this session, in order.
    pub turns: Vec<Turn>,
}

/// The interactive chat runtime: submit a user message to start a turn, or
/// cancel an in-flight one.
pub trait ChatRuntime {
    /// Submit a user message, returning the id of the turn it begins.
    ///
    /// This is a required method: its `impl Future` return type cannot be
    /// satisfied by an `unimplemented!()` default body, so the contract is
    /// expressed as a bare signature rather than a stub.
    fn submit(&self, message: UserMessage) -> impl Future<Output = Result<TurnId>>;
    /// Cancel an in-flight turn by id.
    fn cancel(&self, turn: TurnId) -> Result<()> {
        let _ = turn;
        unimplemented!("Phase 0 contract — body lands in §1.0 Phase 1")
    }
}

/// The typed command bus: dispatches a [`Command`] and returns its receipt.
pub trait CommandBus {
    /// Dispatch a command, returning the bus's acknowledgement receipt.
    fn dispatch(&self, command: Command) -> Result<CommandReceipt> {
        let _ = command;
        unimplemented!("Phase 0 contract — body lands in §1.0 Phase 1")
    }
}
