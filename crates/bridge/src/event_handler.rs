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

async fn apply_batch(
	store: &MailStore,
	local_store: &LocalStore,
	backend: &dyn MailBackend,
	sync_limit: usize,
	batch: &EntityUpdateBatch,
) {
	// 1) Bucket updates by what they affect.
	let mut folder_entry_lists: std::collections::HashSet<String> = Default::default();
	let mut mail_events: Vec<&EntityUpdateEvent> = Vec::new();
	for ev in &batch.updates {
		if ev.application != TUTANOTA_APP {
			continue;
		}
		match ev.type_id {
			MAIL_SET_ENTRY_TYPE_ID => {
				folder_entry_lists.insert(ev.instance_list_id.clone());
			},
			MAIL_TYPE_ID => mail_events.push(ev),
			_ => {},
		}
	}

	// 2) Any MailSetEntry CREATE/DELETE on a folder's entries list is the
	// canonical signal that the folder's contents changed. Re-running
	// `sync_folder` reuses the existing diff-and-update logic and is correct
	// for all of CREATE / DELETE / multi-move at once. Cheaper than tracking
	// per-entry id mappings; we can optimise later if it becomes a hot path.
	if !folder_entry_lists.is_empty() {
		let folders = store.list_folders().await;
		for folder in folders
			.iter()
			.filter(|f| folder_entry_lists.contains(&f.entries_list_id))
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
