//! Wire journal format + migrations (kimi protocol 1.0 → 1.5).

pub mod record;
pub mod migration;
pub mod journal;

pub use record::{
    create_wire_metadata_record, is_wire_metadata_record, is_wire_record, op_to_wire_record,
    wire_record_to_payload, WireMetadataRecord, WireRecord, AGENT_WIRE_RECORD_KEY,
};
pub use migration::{
    migrate_wire_records, resolve_wire_migrations, WIRE_PROTOCOL_VERSION,
};
pub use journal::WireJournal;
