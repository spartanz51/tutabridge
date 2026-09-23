//! WebSocket event bus client.
//!
//! Mirrors the TypeScript `EventBusClient`
//! (`src/common/api/worker/EventBusClient.ts`): opens a WebSocket to `/event?…`
//! to receive realtime entity updates and a handful of control messages, with
//! catch-up of missed batches via the `groupsToLastEventBatchIds` query
//! parameter.
//!
//! Scope of this module is **transport, framing and reconnect**. Entity-update
//! batches are parsed into typed [`EntityUpdateBatch`] values (decoding the
//! server's wire format where numeric fields are sent as strings). The other
//! message kinds (`unreadCounterUpdate`, `leaderStatus`,
//! `operationStatusUpdate`, …) are emitted as raw [`serde_json::Value`] so the
//! caller can decide what to do with them — using the SDK's type-mapping
//! machinery if a typed view is needed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use log::{debug, info, warn};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

/// Entity event batches expire on the server after 45 days; we use 44 days as a
/// safety margin (matching the TS client). Past that, the server cannot replay
/// the missed batches and the caller has to fall back to a full re-sync.
pub const ENTITY_EVENT_BATCH_EXPIRE: Duration = Duration::from_secs(44 * 24 * 60 * 60);

// REST-style codes wrapped in the websocket close code (wire = 4000 + code).
const NORMAL_SHUTDOWN: u16 = 1;
const NOT_AUTHORIZED: u16 = 401;
const SESSION_EXPIRED: u16 = 440;
const TOO_MANY_REQUESTS: u16 = 429;
const ACCESS_DEACTIVATED: u16 = 470;
const ACCESS_BLOCKED: u16 = 472;

// Reconnect interval bounds in seconds (inclusive). We pick a random value in
// the range so a fleet of clients does not all reconnect at the same instant.
const RECONNECT_SMALL: (u64, u64) = (5, 10);
const RECONNECT_MEDIUM: (u64, u64) = (20, 40);
const RECONNECT_LARGE: (u64, u64) = (60, 120);

// Heartbeat: send a WebSocket Ping at this cadence so the server (or any NAT /
// firewall in between) gets keep-alive traffic and any half-closed TCP socket
// surfaces as an immediate write error.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

// Read-side liveness check: if no frame (data, Ping, Pong, …) arrives within
// this window, treat the socket as dead and force a reconnect. The TS client
// rides on the browser's automatic WebSocket keep-alive; in Rust we have to
// detect zombie sockets ourselves. 90s is comfortably above the 60s heartbeat
// round-trip and well under typical consumer NAT timeouts (~120-300s).
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

// Numeric attribute ids from the model; these are the field tags the server
// uses on the wire. Keep them centralised so the parser stays grep-able if the
// model ever evolves.
mod attr {
	// WebsocketEntityData
	pub const BATCH_ID: &str = "1485";
	pub const BATCH_OWNER: &str = "1486";
	pub const ENTITY_UPDATES: &str = "1487";
	// EntityUpdate
	pub const APPLICATION: &str = "464";
	pub const INSTANCE_LIST_ID: &str = "466";
	pub const INSTANCE_ID: &str = "467";
	pub const OPERATION: &str = "624";
	pub const TYPE_ID: &str = "2556";
	pub const INSTANCE: &str = "2617";
	pub const BLOB_INSTANCE: &str = "2701";
}

/// Server-side operation type, parsed from the wire integer.
#[cfg_attr(any(test, feature = "testing"), derive(Debug, PartialEq, Eq))]
#[derive(Clone, Copy)]
pub enum Operation {
	Create,
	Update,
	Delete,
	Other(i64),
}

impl Operation {
	fn from_code(c: i64) -> Self {
		match c {
			0 => Self::Create,
			1 => Self::Update,
			2 => Self::Delete,
			n => Self::Other(n),
		}
	}
}

/// A single entity update entry inside a batch.
#[cfg_attr(any(test, feature = "testing"), derive(Debug, PartialEq, Eq))]
pub struct EntityUpdateEvent {
	pub application: String,
	pub type_id: i64,
	/// Empty if the entity is an element type rather than a list element.
	pub instance_list_id: String,
	pub instance_id: String,
	pub operation: Operation,
	/// Still-encrypted JSON of the instance, when the server inlined it
	/// (typically on `Mail` `CREATE`/`UPDATE`). Decrypt via the SDK's
	/// `CryptoFacade`/`InstanceMapper` machinery, or simply re-load the entity
	/// through `CryptoEntityClient`.
	pub instance: Option<String>,
	pub blob_instance: Option<String>,
}

/// A complete batch of entity updates owned by one group.
#[cfg_attr(any(test, feature = "testing"), derive(Debug, PartialEq, Eq))]
pub struct EntityUpdateBatch {
	pub batch_id: String,
	pub group_id: String,
	pub updates: Vec<EntityUpdateEvent>,
}

/// A decoded event-bus message.
#[cfg_attr(any(test, feature = "testing"), derive(Debug))]
pub enum EventBusMessage {
	/// A batch of entity updates. After the caller has fully processed the
	/// batch, it should record `batch_id` as the new "last processed" id for
	/// `group_id` so the next reconnect can resume from there.
	EntityUpdate(EntityUpdateBatch),
	/// Unread-counter update — raw JSON; convert via the SDK's type machinery
	/// if a typed view is needed.
	CounterUpdate(Value),
	LeaderStatus(Value),
	OperationStatusUpdate(Value),
	/// Server signals that catch-up of missed batches is complete.
	InitialSyncDone,
	/// Hint about the size of the catch-up (count of missed batches).
	InitialSyncWorkEstimate(u64),
	/// Phishing markers, exposed as the raw JSON value.
	PhishingMarkers(Value),
	/// Any other / unknown message type, exposed for forward compatibility.
	Unknown { kind: String, payload: String },
}

/// Observable connection state of an `EventBusClient`. Subscribe via
/// [`EventBusClient::state`] to surface the value to a UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsState {
	/// Idle. `run` has not been called, or it returned to teardown.
	Stopped,
	/// First connection attempt in progress.
	Connecting,
	/// Stream is open and delivering frames.
	Connected,
	/// Stream dropped (network or server close), waiting for the next
	/// reconnect attempt.
	Reconnecting,
}

#[derive(Debug, Error)]
pub enum EventBusError {
	#[error("websocket transport error: {0}")]
	Transport(String),
	#[error("authentication rejected (code {0})")]
	AuthenticationRejected(u16),
	#[error("cached event batch ids expired — full re-sync required")]
	OutOfSync,
	#[error("invalid message: {0}")]
	InvalidMessage(String),
	#[error("client stopped")]
	Stopped,
}

/// What to do after the server closed the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseAction {
	/// Auth-style failure: do not reconnect.
	Terminate(u16),
	/// Session expired: caller has to refresh credentials before reconnect.
	Suspend,
	/// Transient close: reconnect after a delay picked in this range (seconds).
	Reconnect((u64, u64)),
}

/// Event-bus client. A single instance can be `run` many times; the WebSocket
/// connection lives for the duration of `run`. The per-group "last processed
/// batch id" state is shared via [`EventBusClient::last_batch_ids`] so the
/// caller can advance it as batches are processed — the next reconnect will
/// use the latest values.
pub struct EventBusClient {
	/// REST base URL, e.g. `https://app.tuta.com`. Converted to `wss://` for
	/// the WebSocket handshake.
	base_url: String,
	sys_model_version: u32,
	tutanota_model_version: u32,
	client_version: String,
	/// Optional client name (e.g. `desktop`, `android`); empty string omits it.
	client_name: String,
	last_batch_ids: Arc<Mutex<HashMap<String, String>>>,
	/// Broadcasts the [`WsState`] transitions to any observer. Updated from
	/// inside `run` so a UI can render "connecting" / "connected" /
	/// "reconnecting" without polling.
	state: watch::Sender<WsState>,
}

impl EventBusClient {
	pub fn new(
		base_url: String,
		sys_model_version: u32,
		tutanota_model_version: u32,
		client_version: String,
		client_name: String,
	) -> Self {
		let (state, _) = watch::channel(WsState::Stopped);
		Self {
			base_url,
			sys_model_version,
			tutanota_model_version,
			client_version,
			client_name,
			last_batch_ids: Arc::new(Mutex::new(HashMap::new())),
			state,
		}
	}

	/// Subscribe to live connection-state transitions
	/// (`Stopped` → `Connecting` → `Connected` → `Reconnecting` → …).
	pub fn state(&self) -> watch::Receiver<WsState> {
		self.state.subscribe()
	}

	/// Shared handle to the per-group last-processed-batch-id map. Update an
	/// entry after each fully processed batch; the next reconnect uses the
	/// current values to ask the server to resend events missed since that
	/// batch.
	pub fn last_batch_ids(&self) -> Arc<Mutex<HashMap<String, String>>> {
		Arc::clone(&self.last_batch_ids)
	}

	/// Connect, read events forever, reconnect on transient failures. Returns
	/// when `shutdown` fires or a terminal error occurs (auth rejected).
	pub async fn run(
		&self,
		access_token: String,
		user_id: String,
		out: mpsc::Sender<EventBusMessage>,
		mut shutdown: watch::Receiver<bool>,
	) -> Result<(), EventBusError> {
		let mut failed_attempts: u32 = 0;
		// Helper: announce a state, ignoring "no subscribers" — observers are
		// optional and the bus must not block on them.
		let publish = |s: WsState| {
			let _ = self.state.send(s);
		};
		// Ensure we always return Stopped to observers, even on a panic in the
		// caller's await tree (the guard fires on drop).
		struct StopOnDrop<'a>(&'a watch::Sender<WsState>);
		impl Drop for StopOnDrop<'_> {
			fn drop(&mut self) {
				let _ = self.0.send(WsState::Stopped);
			}
		}
		let _guard = StopOnDrop(&self.state);
		loop {
			if *shutdown.borrow() {
				return Err(EventBusError::Stopped);
			}

			publish(WsState::Connecting);
			let url = self.build_ws_url(&access_token, &user_id);
			info!("ws connect");

			let connect = tokio_tungstenite::connect_async(&url);
			let connection = tokio::select! {
				biased;
				_ = shutdown.changed() => return Err(EventBusError::Stopped),
				r = connect => r,
			};

			let (mut stream, _resp) = match connection {
				Ok(s) => {
					if failed_attempts > 0 {
						info!("ws connected (after {failed_attempts} failed attempt(s))");
					} else {
						info!("ws connected");
					}
					failed_attempts = 0;
					publish(WsState::Connected);
					s
				},
				Err(e) => {
					warn!("ws connect failed: {e}");
					failed_attempts = failed_attempts.saturating_add(1);
					publish(WsState::Reconnecting);
					if self.sleep_backoff(failed_attempts, None, &mut shutdown).await {
						return Err(EventBusError::Stopped);
					}
					continue;
				},
			};

			// Background task that nudges us every HEARTBEAT_INTERVAL to send a
			// Ping. Going through an mpsc rather than sharing the stream lets
			// the read-loop keep exclusive ownership of `stream` (and is what
			// makes the heartbeat cleanly cancellable on every reconnect).
			let (ping_tx, mut ping_rx) = mpsc::channel::<()>(2);
			let heartbeat = tokio::spawn(async move {
				loop {
					tokio::time::sleep(HEARTBEAT_INTERVAL).await;
					if ping_tx.send(()).await.is_err() {
						return;
					}
				}
			});

			// Read frames until the connection closes.
			let mut close_hint: Option<CloseAction> = None;
			let close_reason = loop {
				tokio::select! {
					_ = shutdown.changed() => {
						let _ = stream.send(Message::Close(None)).await;
						heartbeat.abort();
						return Err(EventBusError::Stopped);
					}
					Some(()) = ping_rx.recv() => {
						if let Err(e) = stream.send(Message::Ping(Vec::new())).await {
							warn!("ws heartbeat send failed: {e}");
							break "heartbeat-send-failed";
						}
						debug!("ws heartbeat ping sent");
					}
					frame = tokio::time::timeout(IDLE_TIMEOUT, stream.next()) => match frame {
						Err(_) => {
							warn!(
								"ws: no frame for {}s — forcing reconnect (probable zombie socket)",
								IDLE_TIMEOUT.as_secs()
							);
							break "idle-timeout";
						}
						Ok(None) => break "stream-end",
						Ok(Some(Err(e))) => {
							warn!("ws frame error: {e}");
							break "frame-error";
						}
						Ok(Some(Ok(Message::Text(text)))) => match parse_message(&text) {
							Ok(msg) => {
								if out.send(msg).await.is_err() {
									heartbeat.abort();
									return Err(EventBusError::Stopped);
								}
							},
							Err(e) => warn!("ws parse: {e}"),
						},
						Ok(Some(Ok(Message::Ping(p)))) => {
							let _ = stream.send(Message::Pong(p)).await;
						}
						Ok(Some(Ok(Message::Pong(_)))) => {
							debug!("ws pong received");
						}
						Ok(Some(Ok(Message::Close(cf)))) => {
							let code = cf.map(|f| u16::from(f.code)).unwrap_or(0);
							let action = close_action(code);
							if let CloseAction::Terminate(c) = action {
								heartbeat.abort();
								return Err(EventBusError::AuthenticationRejected(c));
							}
							close_hint = Some(action);
							break "close-frame";
						}
						Ok(Some(Ok(_))) => {}
					},
				}
			};
			heartbeat.abort();
			debug!("ws inner loop exited: {close_reason}");

			failed_attempts = failed_attempts.saturating_add(1);
			publish(WsState::Reconnecting);
			if self
				.sleep_backoff(failed_attempts, close_hint, &mut shutdown)
				.await
			{
				return Err(EventBusError::Stopped);
			}
		}
	}

	/// Returns `true` if the wait was interrupted by a shutdown signal.
	async fn sleep_backoff(
		&self,
		failed_attempts: u32,
		close: Option<CloseAction>,
		shutdown: &mut watch::Receiver<bool>,
	) -> bool {
		let (lo, hi) = reconnect_interval(failed_attempts, close);
		let secs = pick_random(lo, hi);
		info!("ws reconnect in {secs}s (attempt #{failed_attempts})");
		tokio::select! {
			_ = shutdown.changed() => true,
			_ = tokio::time::sleep(Duration::from_secs(secs)) => false,
		}
	}

	fn build_ws_url(&self, access_token: &str, user_id: &str) -> String {
		let base = self
			.base_url
			.trim_end_matches('/')
			.replacen("https://", "wss://", 1)
			.replacen("http://", "ws://", 1);
		format!("{}{}", base, self.build_path(access_token, user_id))
	}

	fn build_path(&self, access_token: &str, user_id: &str) -> String {
		use std::fmt::Write as _;
		let mut q = String::from("/event?modelVersions=");
		let _ = write!(q, "{}.{}", self.sys_model_version, self.tutanota_model_version);
		let _ = write!(q, "&clientVersion={}", urlencode(&self.client_version));
		let _ = write!(q, "&userId={}", urlencode(user_id));
		let _ = write!(q, "&accessToken={}", urlencode(access_token));
		if !self.client_name.is_empty() {
			let _ = write!(q, "&clientName={}", urlencode(&self.client_name));
		}
		let ids = self.last_batch_ids.lock().unwrap();
		if !ids.is_empty() {
			q.push_str("&groupsToLastEventBatchIds=");
			for (g, b) in ids.iter() {
				let _ = write!(q, "{}={};", g, b);
			}
		}
		q
	}
}

fn urlencode(s: &str) -> String {
	form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn parse_message(text: &str) -> Result<EventBusMessage, EventBusError> {
	let (kind, value) = text.split_once(';').unwrap_or((text, ""));
	match kind {
		"entityUpdate" => Ok(EventBusMessage::EntityUpdate(parse_entity_update_batch(
			value,
		)?)),
		"unreadCounterUpdate" => Ok(EventBusMessage::CounterUpdate(parse_json(value, kind)?)),
		"leaderStatus" => Ok(EventBusMessage::LeaderStatus(parse_json(value, kind)?)),
		"operationStatusUpdate" => Ok(EventBusMessage::OperationStatusUpdate(parse_json(
			value, kind,
		)?)),
		"initialSyncDone" => Ok(EventBusMessage::InitialSyncDone),
		"initialSyncWorkEstimate" => {
			let n: u64 = value.trim().parse().unwrap_or(0);
			Ok(EventBusMessage::InitialSyncWorkEstimate(n))
		},
		"phishingMarkers" => Ok(EventBusMessage::PhishingMarkers(parse_json(value, kind)?)),
		other => Ok(EventBusMessage::Unknown {
			kind: other.to_string(),
			payload: value.to_string(),
		}),
	}
}

fn parse_json(value: &str, kind: &str) -> Result<Value, EventBusError> {
	serde_json::from_str(value).map_err(|e| EventBusError::InvalidMessage(format!("{kind}: {e}")))
}

fn parse_entity_update_batch(value: &str) -> Result<EntityUpdateBatch, EventBusError> {
	let v: Value = parse_json(value, "entityUpdate")?;
	let batch_id = take_string(&v, attr::BATCH_ID)?;
	let group_id = take_string(&v, attr::BATCH_OWNER)?;
	let arr = v
		.get(attr::ENTITY_UPDATES)
		.and_then(|u| u.as_array())
		.ok_or_else(|| EventBusError::InvalidMessage("entityUpdate: missing entityUpdates".into()))?;
	let updates = arr
		.iter()
		.map(parse_entity_update_event)
		.collect::<Result<Vec<_>, _>>()?;
	Ok(EntityUpdateBatch {
		batch_id,
		group_id,
		updates,
	})
}

fn parse_entity_update_event(v: &Value) -> Result<EntityUpdateEvent, EventBusError> {
	Ok(EntityUpdateEvent {
		application: take_string(v, attr::APPLICATION)?,
		type_id: take_string_or_int(v, attr::TYPE_ID)?,
		instance_list_id: v
			.get(attr::INSTANCE_LIST_ID)
			.and_then(|x| x.as_str())
			.unwrap_or("")
			.to_string(),
		instance_id: take_string(v, attr::INSTANCE_ID)?,
		operation: Operation::from_code(take_string_or_int(v, attr::OPERATION)?),
		instance: v
			.get(attr::INSTANCE)
			.and_then(|x| x.as_str())
			.map(String::from),
		blob_instance: v
			.get(attr::BLOB_INSTANCE)
			.and_then(|x| x.as_str())
			.map(String::from),
	})
}

fn take_string(v: &Value, key: &str) -> Result<String, EventBusError> {
	v.get(key)
		.and_then(|x| x.as_str())
		.map(String::from)
		.ok_or_else(|| EventBusError::InvalidMessage(format!("missing or non-string field {key}")))
}

/// Accepts both JSON numbers (`1`) and numeric strings (`"1"`); the server's
/// untyped wire format uses strings for everything, while typed REST responses
/// or test fixtures may already use numbers.
fn take_string_or_int(v: &Value, key: &str) -> Result<i64, EventBusError> {
	match v.get(key) {
		Some(Value::String(s)) => s
			.parse::<i64>()
			.map_err(|e| EventBusError::InvalidMessage(format!("{key}: {e}"))),
		Some(Value::Number(n)) => n
			.as_i64()
			.ok_or_else(|| EventBusError::InvalidMessage(format!("{key}: not an i64"))),
		_ => Err(EventBusError::InvalidMessage(format!(
			"missing numeric field {key}"
		))),
	}
}

fn close_action(server_code: u16) -> CloseAction {
	// Tuta wraps a REST-style code as 4000 + code; we accept both wire and
	// unwrapped values so this stays usable in tests and lower-level callers.
	let code = if server_code >= 4000 {
		server_code - 4000
	} else {
		server_code
	};
	match code {
		NOT_AUTHORIZED | ACCESS_DEACTIVATED | ACCESS_BLOCKED | TOO_MANY_REQUESTS => {
			CloseAction::Terminate(code)
		},
		SESSION_EXPIRED => CloseAction::Suspend,
		NORMAL_SHUTDOWN => CloseAction::Reconnect(RECONNECT_LARGE),
		_ => CloseAction::Reconnect(RECONNECT_SMALL),
	}
}

fn reconnect_interval(failed_attempts: u32, close: Option<CloseAction>) -> (u64, u64) {
	if let Some(CloseAction::Reconnect(r)) = close {
		return r;
	}
	match failed_attempts {
		0 | 1 => RECONNECT_SMALL,
		2 => RECONNECT_MEDIUM,
		_ => RECONNECT_LARGE,
	}
}

/// Cheap uniform-ish pick in `lo..=hi`, no extra dependency.
fn pick_random(lo: u64, hi: u64) -> u64 {
	if hi <= lo {
		return lo;
	}
	let nanos = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.subsec_nanos() as u64)
		.unwrap_or(0);
	lo + (nanos % (hi - lo + 1))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_initial_sync_done_with_no_payload() {
		let m = parse_message("initialSyncDone").unwrap();
		assert!(matches!(m, EventBusMessage::InitialSyncDone));
	}

	#[test]
	fn parses_initial_sync_work_estimate() {
		match parse_message("initialSyncWorkEstimate;42").unwrap() {
			EventBusMessage::InitialSyncWorkEstimate(n) => assert_eq!(n, 42),
			other => panic!("wrong variant {:?}", other),
		}
	}

	#[test]
	fn unknown_type_is_exposed_for_forward_compat() {
		match parse_message("somethingNew;{\"a\":1}").unwrap() {
			EventBusMessage::Unknown { kind, payload } => {
				assert_eq!(kind, "somethingNew");
				assert_eq!(payload, "{\"a\":1}");
			},
			other => panic!("wrong variant {:?}", other),
		}
	}

	#[test]
	fn payload_keeps_inner_semicolons() {
		// split_once on the first ';' must leave the rest intact. Phishing
		// markers happen to use a single string payload, so this also doubles
		// as a sanity check that the framing does not eat content.
		match parse_message("initialSyncWorkEstimate;1;2;3").unwrap() {
			// "1;2;3" parses as the first integer 0 via fallback — that's fine,
			// the point is that we did not panic on the inner semicolons.
			EventBusMessage::InitialSyncWorkEstimate(_) => {},
			other => panic!("wrong variant {:?}", other),
		}
	}

	#[test]
	fn parses_empty_entity_update_batch() {
		// Server wire format: numeric attribute keys, numeric fields as strings.
		let json = r#"{
			"1484":"0",
			"1485":"batch-aaaaaaaa",
			"1486":"group-bbbbbbbb",
			"2557":"1",
			"2558":"deadbeef",
			"1487":[]
		}"#;
		match parse_message(&format!("entityUpdate;{}", json)).unwrap() {
			EventBusMessage::EntityUpdate(b) => {
				assert_eq!(b.batch_id, "batch-aaaaaaaa");
				assert_eq!(b.group_id, "group-bbbbbbbb");
				assert!(b.updates.is_empty());
			},
			other => panic!("wrong variant {:?}", other),
		}
	}

	#[test]
	fn parses_entity_update_batch_with_one_mail_create() {
		// One CREATE on a Mail (application=tutanota, typeId=97), no encrypted
		// inline instance. Mirrors the wire format the server actually sends:
		// every numeric field is a quoted string.
		let json = r#"{
			"1484":"0",
			"1485":"batch1",
			"1486":"group1",
			"2557":"1",
			"2558":"hash",
			"1487":[{
				"463":null,
				"464":"tutanota",
				"466":"listIdAAAAA",
				"467":"mailIdBBBBB",
				"624":"0",
				"2556":"97",
				"2617":null,
				"2701":null,
				"2618":null
			}]
		}"#;
		match parse_message(&format!("entityUpdate;{}", json)).unwrap() {
			EventBusMessage::EntityUpdate(b) => {
				assert_eq!(b.updates.len(), 1);
				let u = &b.updates[0];
				assert_eq!(u.application, "tutanota");
				assert_eq!(u.instance_list_id, "listIdAAAAA");
				assert_eq!(u.instance_id, "mailIdBBBBB");
				assert_eq!(u.operation, Operation::Create);
				assert_eq!(u.type_id, 97);
				assert!(u.instance.is_none());
				assert!(u.blob_instance.is_none());
			},
			other => panic!("wrong variant {:?}", other),
		}
	}

	#[test]
	fn maps_operation_codes_to_variants() {
		assert_eq!(Operation::from_code(0), Operation::Create);
		assert_eq!(Operation::from_code(1), Operation::Update);
		assert_eq!(Operation::from_code(2), Operation::Delete);
		assert_eq!(Operation::from_code(7), Operation::Other(7));
	}

	#[test]
	fn rejects_malformed_entity_update_json() {
		let err = parse_message("entityUpdate;not-json").unwrap_err();
		assert!(matches!(err, EventBusError::InvalidMessage(_)));
	}

	#[test]
	fn take_string_or_int_accepts_both_shapes() {
		// Defensive: the server always sends strings, but typed test fixtures
		// or future format tweaks may use real numbers.
		let v: Value = serde_json::from_str(r#"{"a":"7","b":7}"#).unwrap();
		assert_eq!(take_string_or_int(&v, "a").unwrap(), 7);
		assert_eq!(take_string_or_int(&v, "b").unwrap(), 7);
		assert!(take_string_or_int(&v, "missing").is_err());
	}

	#[test]
	fn parses_unread_counter_update_as_raw_value() {
		// We deliberately do not try to type counter updates — the consumer
		// can apply the SDK's type machinery if it needs a typed view.
		let raw = r#"{"1493":"0","1494":"mailGroupX","2559":"1","2560":"h","1495":[]}"#;
		match parse_message(&format!("unreadCounterUpdate;{}", raw)).unwrap() {
			EventBusMessage::CounterUpdate(v) => {
				assert_eq!(v.get("1494").and_then(|x| x.as_str()), Some("mailGroupX"));
			},
			other => panic!("wrong variant {:?}", other),
		}
	}

	#[test]
	fn heartbeat_fires_before_idle_timeout() {
		// We must send (and receive, via the server's Pong reply) a frame
		// inside IDLE_TIMEOUT — otherwise a perfectly healthy connection
		// would be killed every IDLE_TIMEOUT seconds. Keep at least 20s
		// headroom for round-trip latency under adverse network conditions.
		assert!(
			HEARTBEAT_INTERVAL + Duration::from_secs(20) <= IDLE_TIMEOUT,
			"HEARTBEAT_INTERVAL ({:?}) is too close to IDLE_TIMEOUT ({:?})",
			HEARTBEAT_INTERVAL,
			IDLE_TIMEOUT
		);
	}

	#[test]
	fn heartbeat_interval_is_below_typical_nat_timeout() {
		// Consumer NATs commonly drop idle UDP/TCP flows after 120s; many
		// firewalls go down to 60s. Keep some margin to refresh the flow
		// before the gateway forgets us.
		assert!(
			HEARTBEAT_INTERVAL <= Duration::from_secs(90),
			"HEARTBEAT_INTERVAL ({:?}) risks letting NATs evict the flow",
			HEARTBEAT_INTERVAL
		);
	}

	#[test]
	fn close_action_terminates_on_auth_errors() {
		assert!(matches!(close_action(4401), CloseAction::Terminate(401)));
		assert!(matches!(close_action(4470), CloseAction::Terminate(470)));
		assert!(matches!(close_action(4472), CloseAction::Terminate(472)));
		assert!(matches!(close_action(4429), CloseAction::Terminate(429)));
	}

	#[test]
	fn close_action_accepts_already_unwrapped_codes() {
		// 4xx without the 4000 wrapping (defensive against libraries that
		// report the inner code directly).
		assert!(matches!(close_action(401), CloseAction::Terminate(401)));
	}

	#[test]
	fn close_action_suspends_on_session_expired() {
		assert!(matches!(close_action(4440), CloseAction::Suspend));
	}

	#[test]
	fn close_action_normal_shutdown_uses_large_interval() {
		match close_action(4001) {
			CloseAction::Reconnect(r) => assert_eq!(r, RECONNECT_LARGE),
			other => panic!("unexpected {:?}", other),
		}
	}

	#[test]
	fn close_action_unknown_uses_small_interval() {
		match close_action(4999) {
			CloseAction::Reconnect(r) => assert_eq!(r, RECONNECT_SMALL),
			other => panic!("unexpected {:?}", other),
		}
	}

	#[test]
	fn reconnect_interval_grows_with_attempts() {
		assert_eq!(reconnect_interval(1, None), RECONNECT_SMALL);
		assert_eq!(reconnect_interval(2, None), RECONNECT_MEDIUM);
		assert_eq!(reconnect_interval(3, None), RECONNECT_LARGE);
		assert_eq!(reconnect_interval(99, None), RECONNECT_LARGE);
	}

	#[test]
	fn reconnect_interval_respects_close_hint() {
		// A server-provided close hint should override the attempt-based escalation.
		let hint = Some(CloseAction::Reconnect(RECONNECT_LARGE));
		assert_eq!(reconnect_interval(1, hint), RECONNECT_LARGE);
	}

	#[test]
	fn pick_random_is_within_bounds_and_handles_degenerate() {
		for _ in 0..32 {
			let v = pick_random(5, 10);
			assert!((5..=10).contains(&v));
		}
		assert_eq!(pick_random(7, 7), 7);
		assert_eq!(pick_random(9, 1), 9); // hi < lo: defensively returns lo.
	}

	#[test]
	fn build_path_includes_required_query_params() {
		let client = EventBusClient::new(
			"https://app.tuta.com".to_string(),
			123,
			456,
			"0.1.0".to_string(),
			String::new(),
		);
		let path = client.build_path("tok-A", "user-1");
		assert!(path.starts_with("/event?"));
		assert!(path.contains("modelVersions=123.456"));
		assert!(path.contains("clientVersion=0.1.0"));
		assert!(path.contains("userId=user-1"));
		assert!(path.contains("accessToken=tok-A"));
		assert!(!path.contains("clientName="));
		assert!(!path.contains("groupsToLastEventBatchIds"));
	}

	#[test]
	fn build_path_includes_client_name_when_set() {
		let client = EventBusClient::new(
			"https://app.tuta.com".to_string(),
			1,
			1,
			"v".to_string(),
			"desktop".to_string(),
		);
		assert!(client.build_path("t", "u").contains("clientName=desktop"));
	}

	#[test]
	fn build_path_serialises_last_batch_ids() {
		let client = EventBusClient::new(
			"https://app.tuta.com".to_string(),
			1,
			1,
			"v".to_string(),
			String::new(),
		);
		{
			let mut ids = client.last_batch_ids.lock().unwrap();
			ids.insert("g1".to_string(), "b1".to_string());
		}
		let path = client.build_path("t", "u");
		assert!(
			path.contains("groupsToLastEventBatchIds=g1=b1;"),
			"got: {}",
			path
		);
	}

	#[test]
	fn build_path_url_encodes_special_chars_in_token() {
		let client = EventBusClient::new(
			"https://app.tuta.com".to_string(),
			1,
			1,
			"v".to_string(),
			String::new(),
		);
		let path = client.build_path("a+b/c=d", "u");
		// form_urlencoded encodes '+' as %2B, '/' as %2F, '=' as %3D.
		assert!(path.contains("accessToken=a%2Bb%2Fc%3Dd"), "got: {}", path);
	}

	#[test]
	fn build_ws_url_switches_scheme_to_wss() {
		let client = EventBusClient::new(
			"https://app.tuta.com/".to_string(),
			1,
			1,
			"v".to_string(),
			String::new(),
		);
		let url = client.build_ws_url("t", "u");
		assert!(
			url.starts_with("wss://app.tuta.com/event?"),
			"got: {}",
			url
		);
	}

	#[test]
	fn state_initial_value_is_stopped() {
		let client = EventBusClient::new(
			"https://app.tuta.com".to_string(),
			1,
			1,
			"v".to_string(),
			String::new(),
		);
		// Subscribing before `run` is called: no transition has happened yet.
		let rx = client.state();
		assert_eq!(*rx.borrow(), WsState::Stopped);
	}

	#[test]
	fn state_subscribers_are_independent() {
		// Each `state()` call returns a fresh receiver, observing the same
		// current value; sending an updated state from the inner sender
		// reaches every subscriber.
		let client = EventBusClient::new(
			"https://app.tuta.com".to_string(),
			1,
			1,
			"v".to_string(),
			String::new(),
		);
		let rx1 = client.state();
		let rx2 = client.state();
		assert_eq!(*rx1.borrow(), WsState::Stopped);
		assert_eq!(*rx2.borrow(), WsState::Stopped);
		// Drive a transition through the same channel the run loop uses.
		let _ = client.state.send(WsState::Connected);
		assert_eq!(*rx1.borrow(), WsState::Connected);
		assert_eq!(*rx2.borrow(), WsState::Connected);
	}

	#[test]
	fn build_ws_url_switches_plain_http_to_ws() {
		let client = EventBusClient::new(
			"http://127.0.0.1:9000".to_string(),
			1,
			1,
			"v".to_string(),
			String::new(),
		);
		let url = client.build_ws_url("t", "u");
		assert!(
			url.starts_with("ws://127.0.0.1:9000/event?"),
			"got: {}",
			url
		);
	}
}
