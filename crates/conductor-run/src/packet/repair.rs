//! The repair packet, composed — master plan §6.5, §4.6.
//!
//! > **Repair packet** adds only: failing check IDs · the failure fingerprint ·
//! > a bounded log excerpt (first failing assertion + 40 lines, never the full
//! > log) · the diff of what the previous attempt changed · attempt ordinal and
//! > remaining budget · an explicit `do_not_retry` list of approaches already
//! > tried. That last field is what stops attempt 2 from being attempt 1 again.
//!
//! # "Adds only" is the whole specification, and S6 built only the addition
//!
//! [`crate::repair::packet`] produces exactly those six fields and says so: its
//! docs read *"everything else the agent needs is the implementation packet,
//! which S12 owns."* This module is that composition — the implementation packet
//! **plus** the repair fields, in one canonical document, so a repairing agent is
//! told the objective, the scope, the acceptance criteria, the decisions and the
//! verification commands *as well as* what went wrong last time.
//!
//! Without it a repair attempt received either nothing extra (before S12) or the
//! plain implementation packet (after S12's first half) — which is to say: the
//! same instruction as attempt 1, which is precisely the failure §6.5's last
//! sentence names.
//!
//! # Why the base is rebuilt rather than passed in
//!
//! [`build`] takes the store and derives the implementation half itself. A
//! caller-supplied base would let the two halves describe different runs, and the
//! repair half is *about* a specific run's history — a mismatch there would be a
//! packet that reads coherently and is wrong. Deriving both from one `run_id`
//! makes that unrepresentable.

use conductor_core::RunId;
use conductor_store::Store;
use serde_yaml::{Mapping, Value};

use super::implementation::{self, ImplementationPacket};
use super::{Emitted, PacketError};
use crate::repair::packet::RepairPacket as RepairFields;

/// §6.5's repair packet: the implementation packet plus what went wrong.
#[derive(Debug, Clone)]
pub struct ComposedRepairPacket {
    base: ImplementationPacket,
    added: RepairFields,
}

impl ComposedRepairPacket {
    /// The six fields §6.5 says this packet adds, as they were composed.
    pub fn added(&self) -> &RepairFields {
        &self.added
    }

    fn to_value(&self) -> Value {
        // Start from the implementation packet, so "adds only" is literally what
        // this is rather than a second document repeating most of it — the same
        // move [`super::continuation`] makes, for the same reason.
        let Value::Mapping(mut m) = self.base.to_value_for_continuation() else {
            unreachable!("an implementation packet is a mapping")
        };
        m.insert(Value::from("packet"), Value::from("repair"));

        // Serialized through `serde_yaml` from the S6 type rather than
        // re-listed field by field here: two spellings of the same six fields is
        // two things that can disagree, and S6's type is the one its own tests
        // pin. Its field order is fixed by its declaration, and
        // `super::canonical_bytes` sorts map keys anyway (§6.6).
        let added =
            serde_yaml::to_value(&self.added).unwrap_or_else(|_| Value::Mapping(Mapping::new()));
        m.insert(Value::from("repair"), added);
        Value::Mapping(m)
    }

    /// The canonical bytes, whatever their size.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        super::canonical_bytes(&self.to_value())
    }

    /// Canonicalize, bound-check and hash.
    pub fn emit(&self) -> Result<Emitted, PacketError> {
        super::emit(&self.to_value())
    }

    /// `blake3:<hex>` over the canonical bytes.
    pub fn hash(&self) -> super::PacketHash {
        super::PacketHash::from_bytes(&self.canonical_bytes())
    }

    /// The packet as YAML — what the repairing agent is handed.
    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(&super::render(&self.to_value())).unwrap_or_default()
    }
}

/// Compose §6.5's repair packet for one run.
///
/// `added` is what [`crate::repair::packet::build`] produced from the run's
/// durable observation history.
pub fn build(
    store: &mut Store,
    run_id: &RunId,
    added: &RepairFields,
) -> Result<ComposedRepairPacket, PacketError> {
    Ok(ComposedRepairPacket {
        base: implementation::build(store, run_id)?,
        added: added.clone(),
    })
}
