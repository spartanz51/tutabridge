//! Realtime event-bus handler.
//!
//! Consumes [`EventBusMessage`] batches emitted by the SDK's WebSocket
//! `EventBusClient` and applies them to `MailStore` (in-memory) and
//! `LocalStore` (encrypted SQLite + `.eml` files).
//!
//! The hot path — `MailSetEntry` CREATE/DELETE — is handled **without**
//! re-listing the affected folder over REST: a `MailSetEntry`'s element id
//! is a Tuta-defined encoding of `(receivedDate, mail_element_id)`, so we
//! recover the mail id directly via `tutasdk::mail_set_entry_id::deconstruct`
//! and either move the already-decrypted Mail between folders in memory
//! (when it's a MOVE between two cached folders) or ask the backend for
//! the single Mail (`load_mail`) when it is a brand-new arrival. The full
//! `sync_folder` only runs as a safety-net fallback (decode failure,
//! unknown folder, `load_mail` error).
//!
//! After each batch the `(group_id, batch_id)` pair is persisted so the
//! next reconnect can resume from there via `groupsToLastEventBatchIds`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use log::{debug, info, warn};
use tokio::sync::{mpsc, watch};
use tutasdk::event_bus::{EntityUpdateBatch, EntityUpdateEvent, EventBusMessage, Operation};
use tutasdk::{mail_set_entry_id, CustomId};

use crate::store::LocalStore;
use crate::sync::{sync_folder, MailStore, StoredMail};
use crate::tuta::{FolderInfo, MailBackend};

/// Application tag for Tuta's mail-side entities.
const TUTANOTA_APP: &str = "tutanota";
/// `Mail` entity (see `tuta-sdk/.../entities/generated/tutanota.rs`,
/// `impl Entity for Mail`).
const MAIL_TYPE_ID: i64 = 97;
/// `MailSetEntry` entity — placement of a mail inside a folder/MailSet.
const MAIL_SET_ENTRY_TYPE_ID: i64 = 1450;
/// `MailSet` entity — the folder itself (custom folders are created /
/// renamed / deleted by mutating MailSet entries).
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

/// Bucketed view of the mail-relevant entity updates inside a batch. Pure;
/// no I/O. Splitting `MailSetEntry` events into creates and deletes lets
/// us process them in the right order (CREATEs first — so a move can clone
/// the mail from the source folder before the DELETE removes it).
#[cfg_attr(test, derive(Debug))]
#[derive(Default)]
struct Bucketed<'a> {
	mail_set_entry_creates: Vec<&'a EntityUpdateEvent>,
	mail_set_entry_deletes: Vec<&'a EntityUpdateEvent>,
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
			MAIL_SET_ENTRY_TYPE_ID => match ev.operation {
				Operation::Create => out.mail_set_entry_creates.push(ev),
				Operation::Delete => out.mail_set_entry_deletes.push(ev),
				// The Tuta model treats `MailSetEntry` as immutable — only
				// CREATE / DELETE happen. Ignore other operations defensively.
				_ => {},
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
		mail_set_entry_creates,
		mail_set_entry_deletes,
		mail_events,
		folder_list_dirty,
	} = bucket_updates(&batch.updates);

	// A MailSet event means the user added / renamed / deleted a folder in
	// the webmail. Refresh the list first so a brand-new folder is known
	// before we try to apply MailSetEntry events that reference it.
	if folder_list_dirty {
		refresh_folder_list(store, local_store, backend).await;
	}

	// Snapshot the folder list once; the delta path matches events to
	// folders by `entries_list_id`.
	let folders = store.list_folders().await;
	let folder_by_entries: HashMap<&str, &FolderInfo> = folders
		.iter()
		.map(|f| (f.entries_list_id.as_str(), f))
		.collect();

	// Folders we could not handle precisely — fall back to a full
	// `sync_folder` at the end.
	let mut fallback_folders: HashSet<String> = HashSet::new();

	// 1) MailSetEntry CREATEs first. Doing creates *before* the matching
	// deletes lets a MOVE clone the already-decrypted Mail straight from
	// the source folder (still present in the cache at this point) — no
	// REST round-trip.
	for ev in &mail_set_entry_creates {
		apply_mail_set_entry_create(
			store,
			local_store,
			backend,
			&folder_by_entries,
			&mut fallback_folders,
			ev,
		)
		.await;
	}

	// 2) MailSetEntry DELETEs. Per Tuta's wire model these arrive paired
	// with the CREATEs (a MOVE = DELETE source + CREATE target in the same
	// batch); a lone DELETE means a trash / hard-delete.
	for ev in &mail_set_entry_deletes {
		apply_mail_set_entry_delete(store, local_store, &folder_by_entries, ev).await;
	}

	// 3) Mail-entity events — UPDATE (read/unread, subject, …) and DELETE.
	// CREATE on a Mail entity is paired with a MailSetEntry CREATE which
	// the loop above already handled.
	for ev in mail_events {
		apply_mail_event(store, local_store, backend, ev).await;
	}

	// 4) Safety net: for every folder we couldn't precisely apply (decode
	// failure, unknown folder, REST error during `load_mail`), re-run the
	// classic full sync so the user never silently misses a mail.
	for entries_list_id in &fallback_folders {
		let Some(folder) = folder_by_entries.get(entries_list_id.as_str()).copied() else {
			continue;
		};
		debug!(
			"Event bus: fallback full sync for {} (batch {})",
			folder.imap_path, batch.batch_id
		);
		if let Err(e) = sync_folder(store, local_store, backend, folder, sync_limit).await {
			warn!(
				"Event bus fallback sync failed for {}: {}",
				folder.imap_path, e
			);
		}
	}
}

async fn refresh_folder_list(
	store: &MailStore,
	local_store: &LocalStore,
	backend: &dyn MailBackend,
) {
	match backend.list_folders().await {
		Ok(folders) => {
			let known: HashSet<String> = folders.iter().map(|f| f.id.clone()).collect();
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

async fn apply_mail_set_entry_create(
	store: &MailStore,
	local_store: &LocalStore,
	backend: &dyn MailBackend,
	folder_by_entries: &HashMap<&str, &FolderInfo>,
	fallback_folders: &mut HashSet<String>,
	ev: &EntityUpdateEvent,
) {
	let custom = CustomId(ev.instance_id.clone());
	let mail_eid = match mail_set_entry_id::deconstruct(&custom) {
		Ok((_date, mail_id)) => mail_id.0,
		Err(e) => {
			warn!(
				"MailSetEntry CREATE id {:?} could not be decoded: {e} — falling back to full sync",
				ev.instance_id
			);
			fallback_folders.insert(ev.instance_list_id.clone());
			return;
		},
	};
	let Some(target_folder) = folder_by_entries.get(ev.instance_list_id.as_str()).copied() else {
		// Folder unknown — typically a newly created custom folder whose
		// `MailSet` event we have not yet processed. Falling back ensures
		// we discover it via `list_folders` on the next batch.
		fallback_folders.insert(ev.instance_list_id.clone());
		return;
	};

	// HIT path: the mail already lives in another cached folder (typical
	// MOVE between two known folders). Clone the StoredMail into the
	// target, allocate a fresh UID and persist.
	if let Some((source_folder, mut stored)) = store.find_mail_anywhere(&mail_eid).await {
		debug!(
			"Event bus: cloning mail {} from {} → {} (no REST)",
			mail_eid, source_folder, target_folder.imap_path
		);
		assign_uid_and_upsert(store, local_store, target_folder, &mail_eid, &mut stored).await;
		return;
	}

	// MISS path: never seen this mail. The MailSetEntry payload is
	// inline in `event.instance`; decrypting it gives us the referenced
	// Mail's full `(list_id, element_id)` directly, so we can ask the
	// backend for just that one mail. No global mail-list-id cache, no
	// `sync_folder` fallback needed in the typical case.
	let mail_id_tuple = resolve_mail_set_entry(backend, ev).await;
	let Some(mail_tuple) = mail_id_tuple else {
		// Inline decode failed AND we have no cached mail to clone — the
		// folder will be brought up-to-date by the safety-net sync below.
		fallback_folders.insert(ev.instance_list_id.clone());
		return;
	};
	let list_id = mail_tuple.list_id.to_string();
	let elem_id = mail_tuple.element_id.to_string();
	match backend.load_mail(&list_id, &elem_id).await {
		Ok(Some(mail)) => {
			debug!(
				"Event bus: targeted load_mail({}, {}) → {} (1 REST call)",
				list_id, elem_id, target_folder.imap_path
			);
			let mut stored = StoredMail {
				mail,
				details: None,
				rfc2822: None,
				uid: 0,
			};
			assign_uid_and_upsert(store, local_store, target_folder, &elem_id, &mut stored).await;
		},
		Ok(None) => {
			debug!("MailSetEntry CREATE: mail {} not found on server", elem_id);
		},
		Err(e) => {
			warn!(
				"MailSetEntry CREATE: load_mail({}, {}) failed: {e} — falling back",
				list_id, elem_id
			);
			fallback_folders.insert(ev.instance_list_id.clone());
		},
	}
}

/// Try to discover the Mail referenced by a `MailSetEntry` CREATE event
/// without a REST round-trip. Preferred path: decrypt `event.instance`
/// inline (the entry carries `mail: IdTupleGenerated`). Falls back to the
/// in-memory cache by entry-id-derived mail id — if neither works, the
/// caller queues a fallback `sync_folder`.
async fn resolve_mail_set_entry(
	backend: &dyn MailBackend,
	ev: &EntityUpdateEvent,
) -> Option<tutasdk::IdTupleGenerated> {
	if let Some(json) = ev.instance.as_deref() {
		match backend.decrypt_inline_mail_set_entry(json).await {
			Ok(Some(entry)) => return Some(entry.mail),
			Ok(None) => debug!("MailSetEntry inline: session key unresolved"),
			Err(e) => warn!("MailSetEntry inline decrypt failed: {e}"),
		}
	}
	None
}

async fn apply_mail_set_entry_delete(
	store: &MailStore,
	local_store: &LocalStore,
	folder_by_entries: &HashMap<&str, &FolderInfo>,
	ev: &EntityUpdateEvent,
) {
	let custom = CustomId(ev.instance_id.clone());
	let mail_eid = match mail_set_entry_id::deconstruct(&custom) {
		Ok((_date, mail_id)) => mail_id.0,
		Err(e) => {
			// We cannot identify *which* mail left the folder without the
			// decoded id; an upstream MailSetEntry CREATE may have queued a
			// fallback already, otherwise the next periodic interaction
			// (re-select, FETCH) will reconcile.
			warn!(
				"MailSetEntry DELETE id {:?} could not be decoded: {e}",
				ev.instance_id
			);
			return;
		},
	};
	let Some(source_folder) = folder_by_entries.get(ev.instance_list_id.as_str()).copied() else {
		return;
	};
	let removed = store
		.remove_mail_from_folder(&source_folder.id, &mail_eid)
		.await;
	if removed {
		debug!(
			"Event bus: removed mail {} from {} (no REST)",
			mail_eid, source_folder.imap_path
		);
	}
	// Drop the on-disk row + `.eml` only if no folder still holds the
	// mail. Multi-folder placement (rare with the current Tuta model) and
	// MOVE-within-batch (the matching CREATE ran first, so the target
	// folder still has it) are both preserved by this check.
	if !store.is_mail_anywhere(&mail_eid).await {
		if let Err(e) = local_store.delete_mail(&mail_eid) {
			warn!("Failed to delete cached mail {}: {}", mail_eid, e);
		}
	}
}

async fn apply_mail_event(
	store: &MailStore,
	local_store: &LocalStore,
	backend: &dyn MailBackend,
	ev: &EntityUpdateEvent,
) {
	match ev.operation {
		Operation::Delete => {
			if let Err(e) = local_store.delete_mail(&ev.instance_id) {
				warn!("Failed to delete cached mail {}: {}", ev.instance_id, e);
			}
			store.remove_mail_everywhere(&ev.instance_id).await;
		},
		Operation::Update => {
			// Prefer the inline-decrypt path: the encrypted Mail is already
			// in `event.instance`, no REST round-trip needed. Fall back to
			// `load_mail` if the payload is absent or its session key is in a
			// transient unresolvable state (e.g. post-reply attachment keys).
			let mail = resolve_mail(backend, ev).await;
			match mail {
				Some(mail) => {
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
				None => {
					// Disappeared / unresolvable — treat like a delete to
					// keep the cache consistent.
					let _ = local_store.delete_mail(&ev.instance_id);
					store.remove_mail_everywhere(&ev.instance_id).await;
				},
			}
		},
		Operation::Create | Operation::Other(_) => {},
	}
}

/// Decrypt-inline first, REST-fallback second. Centralises the policy so
/// every event-bus consumer takes the same fast path when the payload is
/// already inline, and the same safety-net otherwise.
async fn resolve_mail(
	backend: &dyn MailBackend,
	ev: &EntityUpdateEvent,
) -> Option<tutasdk::entities::generated::tutanota::Mail> {
	if let Some(json) = ev.instance.as_deref() {
		match backend.decrypt_inline_mail(json).await {
			Ok(Some(mail)) => {
				debug!(
					"Event bus: decrypted Mail {} inline (no REST)",
					ev.instance_id
				);
				return Some(mail);
			},
			Ok(None) => {
				debug!(
					"Event bus: Mail {} session key unresolvable, falling back to load_mail",
					ev.instance_id
				);
			},
			Err(e) => {
				warn!(
					"Event bus: decrypt_inline_mail({}) failed: {e} — falling back",
					ev.instance_id
				);
			},
		}
	}
	match backend
		.load_mail(&ev.instance_list_id, &ev.instance_id)
		.await
	{
		Ok(opt) => opt,
		Err(e) => {
			warn!("Event bus: load_mail({}) failed: {e}", ev.instance_id);
			None
		},
	}
}

/// Allocate a fresh UID in `target_folder`, stamp it on `stored`, then
/// upsert both `MailStore` and the `LocalStore` metadata row in one step.
async fn assign_uid_and_upsert(
	store: &MailStore,
	local_store: &LocalStore,
	target_folder: &FolderInfo,
	mail_eid: &str,
	stored: &mut StoredMail,
) {
	let uid = match local_store.allocate_folder_uids(&target_folder.id, &[mail_eid]) {
		Ok(map) => map.get(mail_eid).copied().unwrap_or(0),
		Err(e) => {
			warn!(
				"Failed to allocate UID for {} in {}: {e}",
				mail_eid, target_folder.imap_path
			);
			0
		},
	};
	stored.uid = uid;

	let meta = crate::sync::mail_to_metadata(&stored.mail, &target_folder.id, uid);
	if let Err(e) = local_store.upsert_mail_metadata(&meta) {
		warn!("Failed to persist {} in {}: {e}", mail_eid, target_folder.id);
	}

	store
		.upsert_mail_in_folder(&target_folder.id, stored.clone())
		.await;
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
		assert!(out.mail_set_entry_creates.is_empty());
		assert!(out.mail_set_entry_deletes.is_empty());
		assert!(out.mail_events.is_empty());
		assert!(!out.folder_list_dirty);
	}

	#[test]
	fn bucket_ignores_other_applications() {
		let updates = vec![ev("sys", 97, "L", "E", Operation::Create)];
		let out = bucket_updates(&updates);
		assert!(out.mail_set_entry_creates.is_empty());
		assert!(out.mail_set_entry_deletes.is_empty());
		assert!(out.mail_events.is_empty());
	}

	#[test]
	fn bucket_ignores_unknown_type_ids_in_tutanota() {
		// Unrelated tutanota entities (attachments, contacts, …) pass through.
		let updates = vec![ev("tutanota", 999, "L", "E", Operation::Update)];
		let out = bucket_updates(&updates);
		assert!(out.mail_set_entry_creates.is_empty());
		assert!(out.mail_set_entry_deletes.is_empty());
		assert!(out.mail_events.is_empty());
	}

	#[test]
	fn bucket_splits_mail_set_entry_creates_and_deletes() {
		let updates = vec![
			ev("tutanota", MAIL_SET_ENTRY_TYPE_ID, "inbox_entries", "e1", Operation::Create),
			ev("tutanota", MAIL_SET_ENTRY_TYPE_ID, "source_entries", "e2", Operation::Delete),
			ev("tutanota", MAIL_SET_ENTRY_TYPE_ID, "sent_entries", "e3", Operation::Create),
		];
		let out = bucket_updates(&updates);
		assert_eq!(out.mail_set_entry_creates.len(), 2);
		assert_eq!(out.mail_set_entry_deletes.len(), 1);
		// Order within each bucket is preserved (Tuta guarantees batch order).
		assert_eq!(out.mail_set_entry_creates[0].instance_id, "e1");
		assert_eq!(out.mail_set_entry_creates[1].instance_id, "e3");
		assert_eq!(out.mail_set_entry_deletes[0].instance_id, "e2");
	}

	#[test]
	fn bucket_ignores_mail_set_entry_update_operations() {
		// MailSetEntry is immutable per Tuta's model; UPDATE shouldn't
		// happen, but if one ever sneaks through we ignore it rather than
		// crash.
		let updates = vec![ev(
			"tutanota",
			MAIL_SET_ENTRY_TYPE_ID,
			"inbox_entries",
			"e1",
			Operation::Update,
		)];
		let out = bucket_updates(&updates);
		assert!(out.mail_set_entry_creates.is_empty());
		assert!(out.mail_set_entry_deletes.is_empty());
	}

	#[test]
	fn bucket_collects_mail_events_in_order() {
		let updates = vec![
			ev("tutanota", MAIL_TYPE_ID, "mailL", "m1", Operation::Update),
			ev("tutanota", MAIL_TYPE_ID, "mailL", "m2", Operation::Delete),
		];
		let out = bucket_updates(&updates);
		assert_eq!(out.mail_events.len(), 2);
		assert_eq!(out.mail_events[0].instance_id, "m1");
		assert_eq!(out.mail_events[1].operation, Operation::Delete);
	}

	#[test]
	fn bucket_marks_folder_list_dirty_on_mail_set_event() {
		let updates = vec![
			ev("tutanota", MAIL_SET_TYPE_ID, "folderL", "f1", Operation::Create),
			ev("tutanota", MAIL_SET_TYPE_ID, "folderL", "f2", Operation::Delete),
		];
		let out = bucket_updates(&updates);
		assert!(out.folder_list_dirty);
		assert!(out.mail_set_entry_creates.is_empty());
		assert!(out.mail_set_entry_deletes.is_empty());
		assert!(out.mail_events.is_empty());
	}

	#[test]
	fn bucket_mixed_batch() {
		let updates = vec![
			ev("tutanota", MAIL_SET_ENTRY_TYPE_ID, "inbox_entries", "e1", Operation::Create),
			ev("tutanota", MAIL_TYPE_ID, "mailL", "m1", Operation::Update),
			ev("sys", 42, "X", "Y", Operation::Create), // ignored
		];
		let out = bucket_updates(&updates);
		assert_eq!(out.mail_set_entry_creates.len(), 1);
		assert_eq!(out.mail_events.len(), 1);
		assert!(!out.folder_list_dirty);
	}
}
