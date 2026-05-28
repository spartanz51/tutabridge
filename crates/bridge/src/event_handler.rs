//! Realtime event-bus handler.
//!
//! Consumes [`EventBusMessage`] batches emitted by the SDK's WebSocket
//! `EventBusClient` and applies them to `MailStore` (in-memory) and
//! `LocalStore` (encrypted SQLite + .eml files). After each batch the
//! `(group_id, batch_id)` pair is persisted so the next reconnect can
//! resume from there via `groupsToLastEventBatchIds`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use log::{debug, info, warn};
use tokio::sync::{mpsc, watch};
use tutasdk::event_bus::{EntityUpdateBatch, EntityUpdateEvent, EventBusMessage, Operation};

use crate::store::LocalStore;
use crate::sync::{sync_folder, MailStore};
use crate::tuta::MailBackend;

/// Application tag for Tuta's mail-side entities.
const TUTANOTA_APP: &str = "tutanota";
/// `Mail` entity (see `tuta-sdk/.../entities/generated/tutanota.rs`,
/// `impl Entity for Mail`).
const MAIL_TYPE_ID: i64 = 97;
/// `MailSetEntry` entity — placement of a mail inside a folder/MailSet.
const MAIL_SET_ENTRY_TYPE_ID: i64 = 1450;
/// `MailSet` entity — the folder itself (custom folders are created/renamed/
/// deleted by mutating MailSet entries).
const MAIL_SET_TYPE_ID: i64 = 429;

pub async fn run_event_handler(
	store: Arc<MailStore>,
	local_store: Arc<LocalStore>,
	backend: Arc<dyn MailBackend>,
	sync_limit: usize,
	last_batch_ids: Arc<Mutex<HashMap<String, String>>>,
	mut rx: mpsc::Receiver<EventBusMessage>,
	mut shutdown: watch::Receiver<bool>,
) {
	info!("Event handler started");
	loop {
		tokio::select! {
			biased;
			_ = shutdown.changed() => break,
			msg = rx.recv() => {
				let Some(msg) = msg else { break };
				process(&store, &local_store, &*backend, sync_limit, &last_batch_ids, msg).await;
			}
		}
	}
	info!("Event handler shutting down");
}

async fn process(
	store: &MailStore,
	local_store: &LocalStore,
	backend: &dyn MailBackend,
	sync_limit: usize,
	last_batch_ids: &Mutex<HashMap<String, String>>,
	msg: EventBusMessage,
) {
	let batch = match msg {
		EventBusMessage::EntityUpdate(b) => b,
		EventBusMessage::InitialSyncDone => {
			info!("Event bus initial sync done");
			return;
		},
		// Counter / leader / op-status / phishing / work-estimate / unknown:
		// nothing to do at this layer.
		_ => return,
	};

	apply_batch(store, local_store, backend, sync_limit, &batch).await;

	// Advance the in-memory catch-up state and persist it. The two must stay
	// in sync — the in-memory map drives the next reconnect's query string,
	// the on-disk row survives bridge restarts.
	{
		let mut ids = last_batch_ids.lock().unwrap();
		ids.insert(batch.group_id.clone(), batch.batch_id.clone());
	}
	if let Err(e) = local_store.set_event_bus_batch_id(&batch.group_id, &batch.batch_id) {
		warn!("Failed to persist last batch id for {}: {}", batch.group_id, e);
	}
}

/// Bucketed view of the mail-relevant entity updates inside a batch. The
/// routing decision (which folders need a resync, which mails need a
/// metadata refresh, whether the folder list itself changed) is pure and
/// has no I/O — that lets us test it in isolation.
#[cfg_attr(test, derive(Debug))]
#[derive(Default)]
struct Bucketed<'a> {
	/// `MailSetEntry.instance_list_id` for every CREATE / DELETE / UPDATE
	/// we received — i.e. the folders whose contents changed.
	folder_entry_lists: std::collections::HashSet<&'a str>,
	/// Mail-entity updates (read/unread, subject, delete, …).
	mail_events: Vec<&'a EntityUpdateEvent>,
	/// A `MailSet` entity event arrived — the folder list itself changed
	/// (custom folder created / renamed / deleted). Triggers a refresh of
	/// `store.list_folders()` and a prune of folders no longer on the server.
	folder_list_dirty: bool,
}

fn bucket_updates(updates: &[EntityUpdateEvent]) -> Bucketed<'_> {
	let mut out = Bucketed::default();
	for ev in updates {
		if ev.application != TUTANOTA_APP {
			continue;
		}
		match ev.type_id {
			MAIL_SET_ENTRY_TYPE_ID => {
				out.folder_entry_lists.insert(ev.instance_list_id.as_str());
			},
			MAIL_TYPE_ID => out.mail_events.push(ev),
			MAIL_SET_TYPE_ID => out.folder_list_dirty = true,
			_ => {},
		}
	}
	out
}

async fn apply_batch(
	store: &MailStore,
	local_store: &LocalStore,
	backend: &dyn MailBackend,
	sync_limit: usize,
	batch: &EntityUpdateBatch,
) {
	let Bucketed {
		folder_entry_lists,
		mail_events,
		folder_list_dirty,
	} = bucket_updates(&batch.updates);

	// A MailSet event means the user added / renamed / deleted a folder in
	// the webmail. Refresh the list first so the subsequent MailSetEntry
	// re-sync (below) sees any newly created folder, and prune folders that
	// disappeared from the server.
	if folder_list_dirty {
		match backend.list_folders().await {
			Ok(folders) => {
				let known: std::collections::HashSet<String> =
					folders.iter().map(|f| f.id.clone()).collect();
				store.set_folder_list(folders).await;
				let removed = store.prune_unknown_folders(&known).await;
				for fid in &removed {
					debug!("Event bus: folder {} removed", fid);
					match local_store.delete_folder_mails(fid) {
						Ok(ids) => {
							for eid in &ids {
								if let Err(e) = local_store.delete_eml(eid) {
									warn!("Failed to delete cached eml {}: {}", eid, e);
								}
							}
						},
						Err(e) => warn!("Failed to delete folder cache {}: {}", fid, e),
					}
				}
			},
			Err(e) => warn!("MailSet event: folder list refresh failed: {e}"),
		}
	}

	// Any MailSetEntry CREATE/DELETE on a folder's entries list is the
	// canonical signal that the folder's contents changed. Re-running
	// `sync_folder` reuses the existing diff-and-update logic and is correct
	// for all of CREATE / DELETE / multi-move at once. Cheaper than tracking
	// per-entry id mappings; we can optimise later if it becomes a hot path.
	if !folder_entry_lists.is_empty() {
		let folders = store.list_folders().await;
		for folder in folders
			.iter()
			.filter(|f| folder_entry_lists.contains(f.entries_list_id.as_str()))
		{
			debug!(
				"Event bus: re-syncing folder {} (batch {})",
				folder.imap_path, batch.batch_id
			);
			if let Err(e) = sync_folder(store, local_store, backend, folder, sync_limit).await {
				warn!(
					"Event bus folder sync failed for {}: {}",
					folder.imap_path, e
				);
			}
		}
	}

	// 3) Mail-entity events — UPDATE (read/unread, subject, …) and DELETE.
	// CREATE on a Mail entity is paired with a MailSetEntry CREATE, which is
	// already handled by the folder re-sync above; nothing to do here.
	for ev in mail_events {
		match ev.operation {
			Operation::Delete => {
				if let Err(e) = local_store.delete_mail(&ev.instance_id) {
					warn!("Failed to delete cached mail {}: {}", ev.instance_id, e);
				}
				store.remove_mail_everywhere(&ev.instance_id).await;
			},
			Operation::Update => {
				match backend
					.load_mail(&ev.instance_list_id, &ev.instance_id)
					.await
				{
					Ok(Some(mail)) => {
						store.refresh_mail_in_place(&mail).await;
						let mail_json = serde_json::to_string(&mail).unwrap_or_default();
						if let Err(e) = local_store.refresh_mail_fields(
							&ev.instance_id,
							&mail.subject,
							&mail.sender.name,
							&mail.sender.address,
							mail.unread,
							&mail_json,
						) {
							debug!("Could not refresh metadata for {}: {}", ev.instance_id, e);
						}
					},
					Ok(None) => {
						// Disappeared between event and our follow-up load —
						// treat like a delete.
						let _ = local_store.delete_mail(&ev.instance_id);
						store.remove_mail_everywhere(&ev.instance_id).await;
					},
					Err(e) => warn!("Mail UPDATE: failed to load {}: {}", ev.instance_id, e),
				}
			},
			Operation::Create | Operation::Other(_) => {},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn ev(app: &str, type_id: i64, list: &str, elem: &str, op: Operation) -> EntityUpdateEvent {
		EntityUpdateEvent {
			application: app.to_string(),
			type_id,
			instance_list_id: list.to_string(),
			instance_id: elem.to_string(),
			operation: op,
			instance: None,
			blob_instance: None,
		}
	}

	#[test]
	fn bucket_empty_batch() {
		let out = bucket_updates(&[]);
		assert!(out.folder_entry_lists.is_empty());
		assert!(out.mail_events.is_empty());
	}

	#[test]
	fn bucket_ignores_other_applications() {
		// `sys`-app events (e.g. group/user changes) must not affect mail buckets.
		let updates = vec![ev("sys", 97, "L", "E", Operation::Create)];
		let out = bucket_updates(&updates);
		assert!(out.folder_entry_lists.is_empty());
		assert!(out.mail_events.is_empty());
	}

	#[test]
	fn bucket_ignores_unknown_type_ids_in_tutanota() {
		// Unrelated tutanota entities (e.g. attachments, contacts) should pass through.
		let updates = vec![ev("tutanota", 999, "L", "E", Operation::Update)];
		let out = bucket_updates(&updates);
		assert!(out.folder_entry_lists.is_empty());
		assert!(out.mail_events.is_empty());
	}

	#[test]
	fn bucket_collects_mail_set_entry_lists() {
		// Same list appearing twice (CREATE + DELETE) should de-duplicate.
		let updates = vec![
			ev("tutanota", MAIL_SET_ENTRY_TYPE_ID, "inbox_entries", "e1", Operation::Create),
			ev("tutanota", MAIL_SET_ENTRY_TYPE_ID, "inbox_entries", "e2", Operation::Delete),
			ev("tutanota", MAIL_SET_ENTRY_TYPE_ID, "sent_entries", "e3", Operation::Create),
		];
		let out = bucket_updates(&updates);
		assert_eq!(out.folder_entry_lists.len(), 2);
		assert!(out.folder_entry_lists.contains("inbox_entries"));
		assert!(out.folder_entry_lists.contains("sent_entries"));
		assert!(out.mail_events.is_empty());
	}

	#[test]
	fn bucket_collects_mail_events_in_order() {
		let updates = vec![
			ev("tutanota", MAIL_TYPE_ID, "mailL", "m1", Operation::Update),
			ev("tutanota", MAIL_TYPE_ID, "mailL", "m2", Operation::Delete),
		];
		let out = bucket_updates(&updates);
		assert!(out.folder_entry_lists.is_empty());
		assert_eq!(out.mail_events.len(), 2);
		assert_eq!(out.mail_events[0].instance_id, "m1");
		assert_eq!(out.mail_events[1].operation, Operation::Delete);
	}

	#[test]
	fn bucket_mixed_batch() {
		let updates = vec![
			ev("tutanota", MAIL_SET_ENTRY_TYPE_ID, "inbox_entries", "e1", Operation::Create),
			ev("tutanota", MAIL_TYPE_ID, "mailL", "m1", Operation::Update),
			ev("sys", 42, "X", "Y", Operation::Create), // ignored
		];
		let out = bucket_updates(&updates);
		assert_eq!(out.folder_entry_lists.len(), 1);
		assert!(out.folder_entry_lists.contains("inbox_entries"));
		assert_eq!(out.mail_events.len(), 1);
		assert_eq!(out.mail_events[0].instance_id, "m1");
		assert!(!out.folder_list_dirty);
	}

	#[test]
	fn bucket_marks_folder_list_dirty_on_mail_set_event() {
		// Any CRUD on a MailSet (folder entity) flips the dirty flag once.
		let updates = vec![
			ev("tutanota", MAIL_SET_TYPE_ID, "folderL", "f1", Operation::Create),
			ev("tutanota", MAIL_SET_TYPE_ID, "folderL", "f2", Operation::Delete),
		];
		let out = bucket_updates(&updates);
		assert!(out.folder_list_dirty);
		// MailSet events themselves are not bucketed as mail/entry events.
		assert!(out.folder_entry_lists.is_empty());
		assert!(out.mail_events.is_empty());
	}
}
