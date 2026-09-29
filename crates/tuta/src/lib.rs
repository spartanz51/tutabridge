//! Bridge-specific extensions composed from the public API of Tuta's Rust
//! SDK: the event bus client, the folder tree, mail operations and the
//! `MailSetEntry` id codec.
use tutasdk::{date, entities, util, CustomId, GeneratedId};
pub mod event_bus;
#[cfg(test)]
mod fixtures;
pub mod folder_system;
pub mod mail;
pub mod mail_set_entry_id;
