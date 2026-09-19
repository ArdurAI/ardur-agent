//! The shared effect-bucket registry (#545 / GOV-06 / decision D1).
//!
//! ONE typed, versioned table maps the agent's native cost axes
//! (`ardur_core_types::CostTuple`: tokens_in / tokens_out / cents / wall_ms /
//! milli-attention) onto the **normative MIC effect classes**
//! (`read` / `write` / `network` / `exec` / `external_send` — verifier
//! contract §5.4, MD schema `effect_policies`, `lineage_budgets`).
//!
//! # Why one registry
//!
//! The convergence contract map's D1 review finding: a single bucket
//! vocabulary IS deterministic inside one implementation — the real failure
//! is **cross-surface drift**, where the MD author, the ObservedEvent emitter
//! and the ER adapter each invent their own bucket names (`tokens`,
//! `tool_exec`, `egress`, `cost` …) and portable MIC-State conformance breaks
//! the moment a second adapter exists (§6.5: if two implementations would
//! map the same observed value to different buckets, neither may claim
//! portable conformance).
//!
//! Every surface in this crate derives its vocabulary from THIS module:
//!
//! - the MD author keys `effect_policies` / `lineage_budgets` by
//!   [`REGISTRY_EFFECT_CLASSES`] and rejects budget keys outside it;
//! - the ER adapter's `budget_remaining` map is projected through
//!   [`EffectBucketRegistry::project_budget_remaining`];
//! - the ObservedEvent emitter path normalizes §6.2 side-effect spellings
//!   through [`normalize_effect_class`] (cross-crate agreement pinned by a
//!   test against `ardur-observed-events`' enum).
//!
//! # Native economics stay native
//!
//! `cents` and `wall_ms` are **economic axes**, not portable bucket names:
//! they feed the operator's cost gate untouched and contribute zero to every
//! normative bucket in v1. The issue is explicit — native cents/tokens
//! remain separate economic axes, not substitute names for the normative
//! portable buckets, and the owner-selected cost controls are not altered
//! here, only mapped.
//!
//! # Units and rounding
//!
//! Buckets count **steps** ([`EffectUnit::Steps`]). A native axis contributes
//! `floor(raw × numerator ÷ denominator)` steps — always **floor**, never
//! ceil: rounding up invents usage the caller did not incur. The v1 table:
//!
//! | class | native axis | scale | meaning |
//! |---|---|---|---|
//! | `read` | tokens_in | 1/1 | one billed input token = one read step |
//! | `write` | tokens_out | 1/1 | one billed output token = one write step |
//! | `exec` | milli_attention | 1/1000 | one whole unit of attention = one exec step |
//! | `network` | — | — | no native axis in v1; emitter-classified only |
//! | `external_send` | — | — | no native axis in v1; emitter-classified only |
//!
//! Reading `tokens_in`/`tokens_out` as read/write *steps* is the deliberate
//! v1 modeling choice: a model round is the unit of work this runtime
//! meters, and unbudgeted classes (network / external_send) can only fill
//! from emitter-classified tool steps, never from an invented axis
//! contribution.

use std::collections::BTreeMap;

use ardur_core_types::CostTuple;
use serde::{Deserialize, Serialize};

use crate::error::GovernanceError;

/// The five normative MIC effect classes, as a typed enum.
///
/// Wire spellings are the schema's snake_case values; [`EffectClass::to_string`]
/// renders them, so the enum cannot drift from the MD schema vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    /// Read-only consumption.
    Read,
    /// Local state mutation.
    Write,
    /// Network egress of control traffic.
    Network,
    /// Process/tool execution.
    Exec,
    /// Data sent to an external destination.
    ExternalSend,
}

impl EffectClass {
    /// The schema wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EffectClass::Read => "read",
            EffectClass::Write => "write",
            EffectClass::Network => "network",
            EffectClass::Exec => "exec",
            EffectClass::ExternalSend => "external_send",
        }
    }
}

impl std::fmt::Display for EffectClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The registry's class set in canonical order — the SAME order
/// [`crate::mission::EFFECT_CLASSES`] (and the MD schema's `contains`
/// assertions) use, re-typed so surfaces share one symbol, not five strings.
pub const REGISTRY_EFFECT_CLASSES: [EffectClass; 5] = [
    EffectClass::Read,
    EffectClass::Write,
    EffectClass::Network,
    EffectClass::Exec,
    EffectClass::ExternalSend,
];

/// A native `CostTuple` axis that can contribute to a bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CostAxis {
    /// Prompt/input tokens billed.
    TokensIn,
    /// Completion/output tokens billed.
    TokensOut,
    /// Whole US cents.
    Cents,
    /// Wall-clock milliseconds.
    WallMs,
    /// Milli-attention units.
    MilliAttention,
}

impl CostAxis {
    /// The `CostTuple` field spelling (descriptor self-description).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CostAxis::TokensIn => "tokens_in",
            CostAxis::TokensOut => "tokens_out",
            CostAxis::Cents => "cents",
            CostAxis::WallMs => "wall_ms",
            CostAxis::MilliAttention => "milli_attention",
        }
    }

    /// This axis's raw value on a tuple.
    #[must_use]
    pub fn value(self, cost: &CostTuple) -> u64 {
        match self {
            CostAxis::TokensIn => cost.tokens_in,
            CostAxis::TokensOut => cost.tokens_out,
            CostAxis::Cents => cost.cents,
            CostAxis::WallMs => cost.wall_ms,
            CostAxis::MilliAttention => cost.attention_score,
        }
    }
}

impl std::fmt::Display for CostAxis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a bucket counts. v1 counts steps only; the enum exists so a future
/// unit (e.g. bytes for network) is a deliberate descriptor change, not a
/// silent re-interpretation of existing numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectUnit {
    /// Count of discrete governed steps.
    Steps,
}

/// `floor(raw × numerator ÷ denominator)` — the only rounding the registry
/// performs. Floor, never ceil: a positive remainder is never invented into
/// a whole step of usage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AxisScale {
    /// Multiplier numerator (≥ 1).
    pub numerator: u64,
    /// Divisor denominator (≥ 1).
    pub denominator: u64,
}

impl AxisScale {
    /// `floor(raw × numerator ÷ denominator)` with no overflow (u128
    /// intermediate) and no ceiling — the two silent-widening failure modes.
    #[must_use]
    pub fn floor(self, raw: u64) -> u64 {
        u128::from(raw)
            .saturating_mul(u128::from(self.numerator))
            .checked_div(u128::from(self.denominator))
            .map_or(u64::MAX, |v| u64::try_from(v).unwrap_or(u64::MAX))
    }
}

/// One effect class's mapping descriptor: unit, contributing native axes,
/// and each axis's scale. `axes` empty means the class fills only from
/// emitter-classified steps in v1 (no native axis invents contribution).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectBucketDescriptor {
    /// The normative class.
    pub class: EffectClass,
    /// What the bucket counts.
    pub unit: EffectUnit,
    /// Native axis contributions, keyed by axis.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub axes: BTreeMap<CostAxis, AxisScale>,
}

/// Why a registry could not be built from descriptors.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistryError {
    /// Not exactly one descriptor per normative class.
    #[error("effect-bucket registry incomplete: missing class `{0}`")]
    MissingClass(String),
    /// More than one descriptor for a class.
    #[error("effect-bucket registry invalid: duplicate class `{0}`")]
    DuplicateClass(String),
    /// A descriptor carried a non-positive scale.
    #[error("effect-bucket registry invalid: class `{0}` has a non-positive scale")]
    InvalidScale(String),
}

/// The one shared registry. Construct it via [`effect_bucket_registry`] (the
/// pinned v1 table) or [`EffectBucketRegistry::from_descriptors`] (a versioned
/// alternative table — descriptor changes must carry a new version, never
/// mutate v1 in place).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectBucketRegistry {
    version: String,
    descriptors: BTreeMap<EffectClass, EffectBucketDescriptor>,
}

impl EffectBucketRegistry {
    /// Build a registry from descriptors, enforcing exactly-one-per-class
    /// over all five normative classes.
    ///
    /// # Errors
    ///
    /// [`RegistryError`] naming the first structural problem: a missing
    /// class, a duplicate, or a non-positive scale.
    pub fn from_descriptors(
        version: impl Into<String>,
        descriptors: Vec<EffectBucketDescriptor>,
    ) -> Result<Self, RegistryError> {
        let mut map = BTreeMap::new();
        for descriptor in descriptors {
            if map.insert(descriptor.class, descriptor.clone()).is_some() {
                return Err(RegistryError::DuplicateClass(descriptor.class.to_string()));
            }
            for scale in descriptor.axes.values() {
                if scale.numerator == 0 || scale.denominator == 0 {
                    return Err(RegistryError::InvalidScale(descriptor.class.to_string()));
                }
            }
        }
        // A renamed descriptor reads as DUPLICATE first (its new name now
        // collides), then the vacated slot reports MISSING — either order
        // refuses the table; the duplicate check above has already fired by
        // the time this loop runs, so the pair lands where it lands.
        for class in REGISTRY_EFFECT_CLASSES {
            if !map.contains_key(&class) {
                return Err(RegistryError::MissingClass(class.to_string()));
            }
        }
        Ok(Self {
            version: version.into(),
            descriptors: map,
        })
    }

    /// The pinned version string.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The classes in canonical order.
    #[must_use]
    pub fn classes(&self) -> Vec<EffectClass> {
        REGISTRY_EFFECT_CLASSES.to_vec()
    }

    /// One class's descriptor, keyed by wire spelling.
    #[must_use]
    pub fn get(&self, class: &str) -> Option<&EffectBucketDescriptor> {
        let typed = EffectClass::from_wire(class)?;
        self.descriptors.get(&typed)
    }

    /// All descriptors, registry order.
    #[must_use]
    pub fn descriptor_list(&self) -> Vec<EffectBucketDescriptor> {
        self.classes()
            .iter()
            .filter_map(|c| self.descriptors.get(c).cloned())
            .collect()
    }

    /// A self-describing summary: version + which native axes map to buckets
    /// and which stay economic.
    #[must_use]
    pub fn descriptor(&self) -> RegistryDescriptor {
        let mut mapped: Vec<String> = Vec::new();
        let mut economic: Vec<String> = Vec::new();
        for class in self.classes() {
            let Some(d) = self.descriptors.get(&class) else {
                continue;
            };
            for axis in d.axes.keys() {
                let name = axis.as_str().to_string();
                if !mapped.contains(&name) {
                    mapped.push(name);
                }
            }
        }
        for axis in [CostAxis::Cents, CostAxis::WallMs] {
            let name = axis.as_str().to_string();
            if !mapped.contains(&name) {
                economic.push(name);
            }
        }
        RegistryDescriptor {
            version: self.version.clone(),
            mapped_native_axes: mapped,
            economic_native_axes: economic,
        }
    }

    /// Project a native [`CostTuple`] into per-class bucket amounts.
    ///
    /// Deterministic by construction (§6.5): same tuple in, same map out.
    /// Axes not named by a descriptor contribute nothing — including the
    /// economic axes `cents` and `wall_ms`, which never move a bucket.
    #[must_use]
    pub fn project_cost_tuple(&self, cost: &CostTuple) -> BTreeMap<EffectClass, u64> {
        let mut buckets = BTreeMap::new();
        for class in self.classes() {
            let mut steps = 0_u64;
            if let Some(descriptor) = self.descriptors.get(&class) {
                for (axis, scale) in &descriptor.axes {
                    steps = steps.saturating_add(scale.floor(axis.value(cost)));
                }
            }
            buckets.insert(class, steps);
        }
        buckets
    }

    /// Normalize an observed class spelling into a typed bucket, or `None`
    /// when the spelling is not in the shared vocabulary. Never falls back
    /// to a default class (no default widening).
    ///
    /// Accepted spellings: the five normative names, plus the §6.2
    /// pre-normalization side-effect taxonomy (`none`, `internal_write`,
    /// `state_change`, `external_send`).
    #[must_use]
    pub fn normalize(&self, observed: &str) -> Option<EffectClass> {
        normalize_effect_class(observed)
    }

    /// Project per-class remaining budgets onto the ER `budget_remaining`
    /// wire map. Keys outside the registry are REJECTED (an adapter
    /// inventing a bucket name must fail, not silently drop or widen it);
    /// classes with no entry stay absent — no invented zeros.
    ///
    /// # Errors
    ///
    /// [`GovernanceError::InvalidClaim`] naming the unknown key.
    pub fn project_budget_remaining(
        &self,
        per_class: &BTreeMap<String, u64>,
    ) -> Result<BTreeMap<String, u64>, GovernanceError> {
        let mut out = BTreeMap::new();
        for (key, value) in per_class {
            let class = EffectClass::from_wire(key).ok_or_else(|| {
                GovernanceError::InvalidClaim(format!(
                    "budget_remaining key `{key}` is not an effect-bucket registry class"
                ))
            })?;
            out.insert(class.as_str().to_string(), *value);
        }
        Ok(out)
    }
}

/// Structural summary of a registry for self-description.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryDescriptor {
    /// The pinned version.
    pub version: String,
    /// Native axes with a bucket contribution.
    pub mapped_native_axes: Vec<String>,
    /// Native axes that stay economic (no bucket contribution).
    pub economic_native_axes: Vec<String>,
}

impl EffectClass {
    /// Parse a wire spelling; unknown spellings are `None` (fail-closed).
    #[must_use]
    pub fn from_wire(wire: &str) -> Option<Self> {
        match wire {
            "read" => Some(EffectClass::Read),
            "write" => Some(EffectClass::Write),
            "network" => Some(EffectClass::Network),
            "exec" => Some(EffectClass::Exec),
            "external_send" => Some(EffectClass::ExternalSend),
            _ => None,
        }
    }
}

/// The shared normalization used by every surface. Vocabulary (fixed):
///
/// - normative names are fixed points;
/// - `none` → `read` (a no-side-effect step is still a governed read of a
///   resource; budgeting it under `read` is the deterministic v1 choice);
/// - `internal_write` / `state_change` → `write`;
/// - `external_send` → `external_send` (fixed point);
/// - anything else — including the D1 adapter-invented `tokens`, `cost`,
///   `egress`, `tool_exec` — is `None`.
#[must_use]
pub fn normalize_effect_class(observed: &str) -> Option<EffectClass> {
    match observed {
        "read" | "none" => Some(EffectClass::Read),
        "write" | "internal_write" | "state_change" => Some(EffectClass::Write),
        "network" => Some(EffectClass::Network),
        "exec" => Some(EffectClass::Exec),
        "external_send" => Some(EffectClass::ExternalSend),
        _ => None,
    }
}

/// Project per-class remaining budgets onto the ER `budget_remaining` wire
/// map through the SHARED registry — the free-function form the ER adapter
/// path uses. See [`EffectBucketRegistry::project_budget_remaining`].
///
/// # Errors
///
/// [`GovernanceError::InvalidClaim`] naming any key outside the registry.
pub fn project_budget_remaining(
    per_class: &BTreeMap<String, u64>,
    registry: &EffectBucketRegistry,
) -> Result<BTreeMap<String, u64>, GovernanceError> {
    registry.project_budget_remaining(per_class)
}

/// The pinned v1 table (see the module docs for the mapping and rationale).
#[must_use]
pub fn effect_bucket_registry() -> EffectBucketRegistry {
    let axes = |pairs: &[(CostAxis, u64, u64)]| -> BTreeMap<CostAxis, AxisScale> {
        pairs
            .iter()
            .copied()
            .map(|(axis, numerator, denominator)| {
                (
                    axis,
                    AxisScale {
                        numerator,
                        denominator,
                    },
                )
            })
            .collect()
    };

    let descriptors = vec![
        EffectBucketDescriptor {
            class: EffectClass::Read,
            unit: EffectUnit::Steps,
            axes: axes(&[(CostAxis::TokensIn, 1, 1)]),
        },
        EffectBucketDescriptor {
            class: EffectClass::Write,
            unit: EffectUnit::Steps,
            axes: axes(&[(CostAxis::TokensOut, 1, 1)]),
        },
        EffectBucketDescriptor {
            class: EffectClass::Network,
            unit: EffectUnit::Steps,
            axes: BTreeMap::new(),
        },
        EffectBucketDescriptor {
            class: EffectClass::Exec,
            unit: EffectUnit::Steps,
            axes: axes(&[(CostAxis::MilliAttention, 1, 1_000)]),
        },
        EffectBucketDescriptor {
            class: EffectClass::ExternalSend,
            unit: EffectUnit::Steps,
            axes: BTreeMap::new(),
        },
    ];
    EffectBucketRegistry::from_descriptors(REGISTRY_VERSION, descriptors)
        .expect("the pinned v1 table satisfies its own invariants")
}

/// The version string pinned to the v1 table.
pub const REGISTRY_VERSION: &str = "effect-bucket-registry.v1";

// ---------------------------------------------------------------------------
// The ledger: admission reservation, commit/refund, failed effects, sibling
// escrow carves — with conservation enforced after every operation.
// ---------------------------------------------------------------------------

/// Why a ledger operation failed. Every variant names the class or
/// reservation involved, so a refusal is diagnosable, never a bare `false`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LedgerError {
    /// The requested amounts exceed the ceiling plus outstanding holds
    /// (§9.4: over-reserve is refused, never best-effort).
    #[error("over-reserve on `{class}`: requested {requested}, available {available}")]
    OverReserve {
        /// The class that refused.
        class: String,
        /// What was asked for.
        requested: u64,
        /// What was actually available.
        available: u64,
    },
    /// A class with no ceiling entry reads as ceiling zero (fail-closed).
    #[error("over-reserve on `{class}`: no ceiling declared, treated as zero")]
    NoCeiling {
        /// The class that refused.
        class: String,
    },
    /// The actuals exceeded the reservation (§9.4 again, at settle time).
    #[error("overrun on `{class}`: actual {actual} steps exceeds reservation {reserved}")]
    Overrun {
        /// The class that overran.
        class: String,
        /// Actual measured steps.
        actual: u64,
        /// The reserved hold.
        reserved: u64,
    },
    /// The reservation id is unknown or already settled (duplicate replay
    /// / restart protection).
    #[error("reservation `{0}` is unknown or already settled")]
    UnknownReservation(String),
}

impl LedgerError {
    /// The class a refusal names, when it names one.
    #[must_use]
    pub fn class(&self) -> Option<&str> {
        match self {
            LedgerError::OverReserve { class, .. }
            | LedgerError::NoCeiling { class }
            | LedgerError::Overrun { class, .. } => Some(class),
            LedgerError::UnknownReservation(_) => None,
        }
    }

    /// The reservation id a refusal names, when it names one.
    #[must_use]
    pub fn reservation_id(&self) -> Option<&str> {
        match self {
            LedgerError::UnknownReservation(id) => Some(id),
            _ => None,
        }
    }
}

/// A reservation's identity: caller-assigned, opaque to the ledger, and the
/// replay/restart key — settling consumes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    /// Caller-assigned unique id.
    pub id: String,
    /// Per-class reserved amounts.
    pub amounts: BTreeMap<EffectClass, u64>,
}

/// What a commit charged and refunded, per class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitOutcome {
    /// Steps actually charged (floored actuals, bounded by the hold).
    pub charged: BTreeMap<EffectClass, u64>,
    /// Steps released back to available (hold − charged).
    pub refunded: BTreeMap<EffectClass, u64>,
}

/// Per-class lineage budget ledger: ceiling, consumed, escrowed (children),
/// and outstanding holds, with the conservation invariant
/// `consumed + escrowed + holds + available == ceiling` audited on demand
/// and enforced on (de)serialize so corrupt state cannot load.
///
/// Restart-safe: `Serialize`/`Deserialize` round-trip the full state
/// including settled reservation ids, so a restarted process keeps both
/// balances AND replay protection. A state that violates conservation is
/// rejected at deserialize time (fail-closed).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EffectLedger {
    ceiling: BTreeMap<EffectClass, u64>,
    consumed: BTreeMap<EffectClass, u64>,
    escrowed: BTreeMap<EffectClass, u64>,
    holds: BTreeMap<EffectClass, u64>,
    live: BTreeMap<String, BTreeMap<EffectClass, u64>>,
    settled: std::collections::BTreeSet<String>,
    serial: u64,
}

fn next_serial() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl EffectLedger {
    /// New ledger from per-class ceilings. Missing classes are ceiling zero.
    #[must_use]
    pub fn new(ceilings: &BTreeMap<EffectClass, u64>) -> Self {
        let zeroed = |v: u64| {
            REGISTRY_EFFECT_CLASSES
                .iter()
                .map(|c| (*c, v))
                .collect::<BTreeMap<_, _>>()
        };
        let _ = zeroed;
        EffectLedger {
            ceiling: ceilings.clone(),
            consumed: BTreeMap::new(),
            escrowed: BTreeMap::new(),
            holds: BTreeMap::new(),
            live: BTreeMap::new(),
            settled: std::collections::BTreeSet::new(),
            serial: next_serial(),
        }
    }

    /// Remaining steps reservable in `class` right now:
    /// `ceiling − consumed − escrowed − holds`.
    #[must_use]
    pub fn available(&self, class: EffectClass) -> u64 {
        self.ceiling
            .get(&class)
            .copied()
            .unwrap_or(0)
            .saturating_sub(
                self.consumed.get(&class).copied().unwrap_or(0)
                    + self.escrowed.get(&class).copied().unwrap_or(0)
                    + self.holds.get(&class).copied().unwrap_or(0),
            )
    }

    /// Steps charged so far in `class`.
    #[must_use]
    pub fn consumed(&self, class: EffectClass) -> u64 {
        self.consumed.get(&class).copied().unwrap_or(0)
    }

    /// Steps escrowed to children in `class`.
    #[must_use]
    pub fn escrowed(&self, class: EffectClass) -> u64 {
        self.escrowed.get(&class).copied().unwrap_or(0)
    }

    /// Reserve admission amounts, or refuse with a named class (§9.4).
    /// Amounts of zero are legal (a read-only step still takes a hold of 0).
    pub fn reserve(
        &mut self,
        amounts: &BTreeMap<EffectClass, u64>,
    ) -> Result<Reservation, LedgerError> {
        for (&class, &amount) in amounts {
            if !self.ceiling.contains_key(&class) {
                return Err(LedgerError::NoCeiling {
                    class: class.to_string(),
                });
            }
            if amount > self.available(class) {
                return Err(LedgerError::OverReserve {
                    class: class.to_string(),
                    requested: amount,
                    available: self.available(class),
                });
            }
        }
        let id = format!("res-{}", self.serial);
        self.serial = self.serial.wrapping_add(1);
        for (&class, &amount) in amounts {
            *self.holds.entry(class).or_insert(0) += amount;
        }
        let reservation = Reservation {
            id,
            amounts: amounts.clone(),
        };
        self.live.insert(reservation.id.clone(), amounts.clone());
        Ok(reservation)
    }

    /// Settle a reservation against measured actuals (a native CostTuple
    /// projected by the SHARED registry — the same table the MD author
    /// declared under, so the ledger can never be charged in a vocabulary
    /// the MD did not declare).
    ///
    /// Charges `min(floored actuals, hold)`: a partial refund releases the
    /// unused hold; an actual exceeding the hold is a typed [`LedgerError::Overrun`]
    /// refused without mutating anything (no best-effort overspend).
    /// Actuals below one step floor to zero — honestly unbilled, with the
    /// whole hold refunded.
    pub fn commit(
        &mut self,
        reservation: &Reservation,
        actuals: &CostTuple,
    ) -> Result<CommitOutcome, LedgerError> {
        let held = self
            .live
            .get(&reservation.id)
            .ok_or_else(|| LedgerError::UnknownReservation(reservation.id.clone()))?;
        let projected = effect_bucket_registry().project_cost_tuple(actuals);
        let mut charged = BTreeMap::new();
        let mut refunded = BTreeMap::new();
        for (&class, &hold) in held {
            let actual = projected.get(&class).copied().unwrap_or(0);
            if actual > hold {
                return Err(LedgerError::Overrun {
                    class: class.to_string(),
                    actual,
                    reserved: hold,
                });
            }
            charged.insert(class, actual);
            refunded.insert(class, hold - actual);
        }
        for (&class, &charge) in &charged {
            *self.holds.entry(class).or_insert(0) -= charge + refunded[&class];
            *self.consumed.entry(class).or_insert(0) += charge;
        }
        self.live.remove(&reservation.id);
        self.settled.insert(reservation.id.clone());
        Ok(CommitOutcome { charged, refunded })
    }

    /// Release a reservation whose effect never ran: the FULL hold returns
    /// to available, nothing is consumed.
    pub fn fail(
        &mut self,
        reservation: &Reservation,
    ) -> Result<BTreeMap<EffectClass, u64>, LedgerError> {
        let held = self
            .live
            .get(&reservation.id)
            .ok_or_else(|| LedgerError::UnknownReservation(reservation.id.clone()))?;
        let refunded = held.clone();
        for (&class, &amount) in &refunded {
            *self.holds.entry(class).or_insert(0) -= amount;
        }
        self.live.remove(&reservation.id);
        self.settled.insert(reservation.id.clone());
        Ok(refunded)
    }

    /// Escrow amounts to a CHILD ledger (sibling allocation): the parent's
    /// `escrowed` rises, available falls, and the child is born with those
    /// amounts as its ceilings. Sum of children + parent remainder equals
    /// the parent's starting state at every point (§5.5/§9.4 escrow rights).
    pub fn carve(
        &mut self,
        amounts: &BTreeMap<EffectClass, u64>,
    ) -> Result<EffectLedger, LedgerError> {
        // Same admission rules as reserve: an over-draw cannot escrow, and a
        // class the parent's ceilings do not cover (absent, or zero with a
        // nonzero ask) cannot escrow either — the carve would mint a child
        // authority the parent never held.
        for (&class, &amount) in amounts {
            let ceiling = self.ceiling.get(&class).copied();
            if ceiling.is_none() || (amount > 0 && ceiling.is_none_or(|c| c == 0)) {
                return Err(LedgerError::NoCeiling {
                    class: class.to_string(),
                });
            }
            if amount > self.available(class) {
                return Err(LedgerError::OverReserve {
                    class: class.to_string(),
                    requested: amount,
                    available: self.available(class),
                });
            }
        }
        for (&class, &amount) in amounts {
            *self.escrowed.entry(class).or_insert(0) += amount;
        }
        Ok(EffectLedger::new(amounts))
    }

    /// Verify `consumed + escrowed + holds + available == ceiling` for every
    /// class, and that every live hold is fully backed. Errors name the
    /// class and the arithmetic — a corrupted ledger must be loud.
    pub fn audit(&self) -> Result<(), LedgerError> {
        for class in REGISTRY_EFFECT_CLASSES {
            let ceiling = self.ceiling.get(&class).copied().unwrap_or(0);
            let consumed = self.consumed.get(&class).copied().unwrap_or(0);
            let escrowed = self.escrowed.get(&class).copied().unwrap_or(0);
            let holds = self.holds.get(&class).copied().unwrap_or(0);
            if consumed
                .checked_add(escrowed)
                .and_then(|v| v.checked_add(holds))
                .and_then(|v| v.checked_add(self.available(class)))
                != Some(ceiling)
            {
                return Err(LedgerError::OverReserve {
                    class: class.to_string(),
                    requested: consumed + escrowed + holds,
                    available: ceiling,
                });
            }
        }
        // Every live reservation's amounts are exactly backed by holds.
        for (id, amounts) in &self.live {
            for (&class, &amount) in amounts {
                if self.holds.get(&class).copied().unwrap_or(0) < amount {
                    return Err(LedgerError::UnknownReservation(id.clone()));
                }
            }
        }
        Ok(())
    }
}

/// Deserialize runs the conservation audit — a derived impl would accept a
/// corrupt state (consumed above ceiling, holds without backing) that no
/// legal operation sequence can produce, silently continuing from invented
/// balances. Corruption must fail closed at load.
impl<'de> Deserialize<'de> for EffectLedger {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            ceiling: BTreeMap<EffectClass, u64>,
            consumed: BTreeMap<EffectClass, u64>,
            escrowed: BTreeMap<EffectClass, u64>,
            holds: BTreeMap<EffectClass, u64>,
            live: BTreeMap<String, BTreeMap<EffectClass, u64>>,
            settled: std::collections::BTreeSet<String>,
            #[serde(default = "next_serial")]
            serial: u64,
        }
        let raw = Raw::deserialize(deserializer)?;
        let ledger = EffectLedger {
            ceiling: raw.ceiling,
            consumed: raw.consumed,
            escrowed: raw.escrowed,
            holds: raw.holds,
            live: raw.live,
            settled: raw.settled,
            serial: raw.serial,
        };
        ledger.audit().map_err(|e| {
            serde::de::Error::custom(format!(
                "serialized ledger does not conserve ({e}); refusing to load corrupt state"
            ))
        })?;
        Ok(ledger)
    }
}
