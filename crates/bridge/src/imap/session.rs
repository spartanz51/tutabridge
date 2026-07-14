use log::{debug, info};
use std::sync::Arc;
use tutasdk::entities::generated::tutanota::{Mail, MailAddress, MailDetails, TutanotaFile};

use crate::imap::search::{self, MsgView};
use crate::mail::mail_to_rfc2822;
use crate::mail::rfc2822::{extract_headers, format_address, format_internal_date};
use crate::store::LocalStore;
use crate::sync::MailStore;
use crate::tuta::{FolderInfo, MailBackend};

#[derive(Debug, Clone, PartialEq)]
enum State {
    NotAuthenticated,
    Authenticated,
    Selected,
    Logout,
}

struct CachedMail {
    mail: Mail,
    details: Option<MailDetails>,
    rfc2822: Option<String>,
    /// `rfc2822` always carries the real headers, but for messages outside the
    /// body-prefetch window it holds a placeholder body. `body_loaded` tracks
    /// whether the *body* is final: `true` once we have real details (or have
    /// confirmed the message has no body source), `false` while a body FETCH
    /// still needs to pull it on demand.
    body_loaded: bool,
    uid: u32,
    deleted: bool,
}

pub struct ImapSession {
    store: Arc<MailStore>,
    backend: Arc<dyn MailBackend>,
    /// The on-disk store, used for full-text body search. `None` in unit tests
    /// (body terms then simply don't match).
    local_store: Option<Arc<LocalStore>>,
    state: State,
    selected_folder: Option<FolderInfo>,
    mails: Vec<CachedMail>,
    uid_next: u32,
    idle_tag: Option<String>,
    auth_tag: Option<String>,
    password_hash: Option<String>,
}

impl ImapSession {
    pub fn new(
        store: Arc<MailStore>,
        backend: Arc<dyn MailBackend>,
        password_hash: Option<String>,
        local_store: Option<Arc<LocalStore>>,
    ) -> Self {
        Self {
            store,
            backend,
            local_store,
            state: State::NotAuthenticated,
            selected_folder: None,
            mails: Vec::new(),
            uid_next: 1,
            idle_tag: None,
            auth_tag: None,
            password_hash,
        }
    }

    pub fn is_logout(&self) -> bool {
        self.state == State::Logout
    }

    pub fn is_idle(&self) -> bool {
        self.idle_tag.is_some()
    }

    pub fn is_awaiting_auth(&self) -> bool {
        self.auth_tag.is_some()
    }

    pub fn handle_auth_response(&mut self, line: &str) -> Vec<String> {
        let tag = match self.auth_tag.take() {
            Some(t) => t,
            None => return vec![],
        };

        let decoded =
            match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, line.trim()) {
                Ok(d) => d,
                Err(_) => {
                    return vec![format!(
                        "{} NO [AUTHENTICATIONFAILED] Invalid base64\r\n",
                        tag
                    )];
                }
            };

        // PLAIN format: \0authcid\0password (authzid is empty)
        let parts: Vec<&[u8]> = decoded.splitn(3, |&b| b == 0).collect();
        let password = if parts.len() == 3 {
            String::from_utf8_lossy(parts[2]).to_string()
        } else if parts.len() == 2 {
            String::from_utf8_lossy(parts[1]).to_string()
        } else {
            return vec![format!(
                "{} NO [AUTHENTICATIONFAILED] Invalid PLAIN data\r\n",
                tag
            )];
        };

        if let Some(ref expected) = self.password_hash {
            if password != *expected {
                return vec![format!(
                    "{} NO [AUTHENTICATIONFAILED] Invalid credentials\r\n",
                    tag
                )];
            }
        }

        self.state = State::Authenticated;
        info!("IMAP client authenticated via AUTHENTICATE PLAIN");
        vec![format!("{} OK AUTHENTICATE completed\r\n", tag)]
    }

    pub fn end_idle(&mut self) -> Vec<String> {
        if let Some(tag) = self.idle_tag.take() {
            vec![format!("{} OK IDLE terminated\r\n", tag)]
        } else {
            vec![]
        }
    }

    pub async fn check_new_mail(&mut self) -> Vec<String> {
        let folder = match self.selected_folder.clone() {
            Some(f) => f,
            None => return vec![],
        };

        // Compare the new authoritative list against our session view by
        // element id. Count-only diffing is not enough — a MOVE between two
        // folders that share the same `sync_limit` window keeps the count
        // constant while swapping one mail for another. We must detect set
        // changes and signal them to the client (EXPUNGE for departed
        // mails, EXISTS for the new count so the client refetches).
        let new_list = self.store.get_folder(&folder.id).await;
        let new_eids: std::collections::HashSet<String> = new_list
            .iter()
            .filter_map(|m| m.mail._id.as_ref().map(|id| id.element_id.to_string()))
            .collect();

        // EXPUNGE seqnos must reference the message numbers as they were
        // **before** this update, so compute them against `self.mails`.
        let mut expunges: Vec<u32> = Vec::new();
        for (idx, m) in self.mails.iter().enumerate() {
            if let Some(eid) = m.mail._id.as_ref().map(|id| id.element_id.to_string()) {
                if !new_eids.contains(&eid) {
                    expunges.push((idx as u32) + 1);
                }
            }
        }

        let old_eids: std::collections::HashSet<String> = self
            .mails
            .iter()
            .filter_map(|m| m.mail._id.as_ref().map(|id| id.element_id.to_string()))
            .collect();
        let added = new_list
            .iter()
            .filter(|m| {
                m.mail
                    ._id
                    .as_ref()
                    .map(|id| !old_eids.contains(&id.element_id.to_string()))
                    .unwrap_or(false)
            })
            .count();

        if expunges.is_empty() && added == 0 && new_list.len() == self.mails.len() {
            return vec![];
        }

        // Apply the new view, then notify. RFC 3501: EXPUNGEs go in
        // descending seqno order so subsequent values are not shifted.
        let _ = self.refresh_mails(&folder.id).await;

        let mut responses = Vec::with_capacity(expunges.len() + 1);
        expunges.sort_unstable_by(|a, b| b.cmp(a));
        for s in expunges {
            responses.push(format!("* {} EXPUNGE\r\n", s));
        }
        responses.push(format!("* {} EXISTS\r\n", self.mails.len()));
        responses
    }

    pub async fn handle_command(&mut self, line: &str) -> Vec<String> {
        let (tag, cmd, args) = parse_command(line);

        match cmd.to_uppercase().as_str() {
            "CAPABILITY" => self.cmd_capability(&tag),
            "NOOP" => vec![format!("{} OK NOOP completed\r\n", tag)],
            "LOGOUT" => self.cmd_logout(&tag),
            "LOGIN" => self.cmd_login(&tag, &args),
            "AUTHENTICATE" => {
                if args.trim().eq_ignore_ascii_case("PLAIN") {
                    self.auth_tag = Some(tag.clone());
                    vec!["+ \r\n".to_string()]
                } else {
                    vec![format!("{} NO Unsupported mechanism\r\n", tag)]
                }
            }
            "LIST" => self.cmd_list(&tag, &args).await,
            "LSUB" => self.cmd_list(&tag, &args).await,
            "SELECT" => self.cmd_select(&tag, &args).await,
            "EXAMINE" => self.cmd_select(&tag, &args).await,
            "STATUS" => self.cmd_status(&tag, &args).await,
            "FETCH" => self.cmd_fetch(&tag, &args, false).await,
            "UID" => self.cmd_uid(&tag, &args).await,
            "CLOSE" => self.cmd_close(&tag),
            "EXPUNGE" => self.cmd_expunge(&tag).await,
            "SEARCH" => self.cmd_search(&tag, &args, false),
            "STORE" => self.cmd_store(&tag, &args, false).await,
            "MOVE" => self.cmd_move(&tag, &args, false).await,
            "COPY" => self.cmd_copy(&tag),
            "IDLE" => {
                self.idle_tag = Some(tag.clone());
                vec!["+ idling\r\n".to_string()]
            }
            "NAMESPACE" => vec![
                "* NAMESPACE ((\"\" \"/\")) NIL NIL\r\n".to_string(),
                format!("{} OK NAMESPACE completed\r\n", tag),
            ],
            _ => vec![format!("{} BAD Unknown command\r\n", tag)],
        }
    }

    /// `true` if `mailbox` resolves to the Sent folder. Tuta saves sent mail
    /// server-side, so an `APPEND` there is a no-op (see `handle_append` in the
    /// connection loop), which is what lets a client's "save to Sent" succeed
    /// without creating a duplicate.
    pub(crate) async fn append_targets_sent(&self, mailbox: &str) -> bool {
        let name = super::utf7::decode(mailbox).unwrap_or_else(|| mailbox.to_string());
        self.store
            .folder_by_imap_path(&name)
            .await
            .map(|f| f.special_use.as_deref() == Some("\\Sent"))
            .unwrap_or(false)
    }

    fn cmd_capability(&self, tag: &str) -> Vec<String> {
        vec![
            "* CAPABILITY IMAP4rev1 AUTH=PLAIN IDLE NAMESPACE UIDPLUS MOVE\r\n".to_string(),
            format!("{} OK CAPABILITY completed\r\n", tag),
        ]
    }

    fn cmd_logout(&mut self, tag: &str) -> Vec<String> {
        self.state = State::Logout;
        vec![
            "* BYE TutaBridge signing off\r\n".to_string(),
            format!("{} OK LOGOUT completed\r\n", tag),
        ]
    }

    fn cmd_login(&mut self, tag: &str, args: &str) -> Vec<String> {
        if let Some(ref expected) = self.password_hash {
            let (_, password) = parse_login_args(args);
            if password != *expected {
                return vec![format!(
                    "{} NO [AUTHENTICATIONFAILED] Invalid credentials\r\n",
                    tag
                )];
            }
        }
        self.state = State::Authenticated;
        info!("IMAP client authenticated (bridge session)");
        vec![format!("{} OK LOGIN completed\r\n", tag)]
    }

    async fn cmd_list(&self, tag: &str, args: &str) -> Vec<String> {
        if self.state == State::NotAuthenticated {
            return vec![format!("{} NO Not authenticated\r\n", tag)];
        }

        let mut responses = Vec::new();

        if args.trim() == "\"\" \"\"" || args.trim().is_empty() {
            responses.push("* LIST (\\Noselect) \"/\" \"\"\r\n".to_string());
            responses.push(format!("{} OK LIST completed\r\n", tag));
            return responses;
        }

        for folder in self.store.list_folders().await {
            let flags = folder.special_use.as_deref().unwrap_or("");
            responses.push(format!(
                "* LIST ({}) \"/\" \"{}\"\r\n",
                flags,
                super::utf7::encode(&folder.imap_path)
            ));
        }
        responses.push(format!("{} OK LIST completed\r\n", tag));

        responses
    }

    async fn cmd_select(&mut self, tag: &str, args: &str) -> Vec<String> {
        if self.state == State::NotAuthenticated {
            return vec![format!("{} NO Not authenticated\r\n", tag)];
        }

        let raw_name = args.trim().trim_matches('"');
        let folder_name = super::utf7::decode(raw_name).unwrap_or_else(|| raw_name.to_string());
        let folder = match self.store.folder_by_imap_path(&folder_name).await {
            Some(f) => f,
            None => {
                return vec![format!(
                    "{} NO [NONEXISTENT] Mailbox does not exist\r\n",
                    tag
                )];
            }
        };
        let folder_id = folder.id.clone();
        self.selected_folder = Some(folder);
        self.state = State::Selected;

        match self.refresh_mails(&folder_id).await {
            Ok(()) => {
                let count = self.mails.len();
                let first_unseen = self
                    .mails
                    .iter()
                    .position(|m| m.mail.unread)
                    .map(|i| i + 1)
                    .unwrap_or(0);

                let mut resp = vec![
                    format!("* {} EXISTS\r\n", count),
                    "* 0 RECENT\r\n".to_string(),
                    "* FLAGS (\\Seen \\Answered \\Flagged \\Deleted \\Draft)\r\n".to_string(),
                    "* OK [PERMANENTFLAGS (\\Seen \\Flagged)] Limited\r\n".to_string(),
                    "* OK [UIDVALIDITY 1] UIDs valid\r\n".to_string(),
                    format!("* OK [UIDNEXT {}] Predicted next UID\r\n", self.uid_next),
                ];
                if first_unseen > 0 {
                    resp.push(format!("* OK [UNSEEN {}] First unseen\r\n", first_unseen));
                }
                resp.push(format!("{} OK [READ-WRITE] SELECT completed\r\n", tag));
                resp
            }
            Err(e) => {
                log::error!("Failed to load mails for {}: {}", folder_name, e);
                vec![
                    "* 0 EXISTS\r\n".to_string(),
                    "* 0 RECENT\r\n".to_string(),
                    format!("{} OK [READ-WRITE] SELECT completed\r\n", tag),
                ]
            }
        }
    }

    async fn cmd_status(&self, tag: &str, args: &str) -> Vec<String> {
        if self.state == State::NotAuthenticated {
            return vec![format!("{} NO Not authenticated\r\n", tag)];
        }
        let (raw_name, _) = parse_imap_token(args.trim());
        let folder_name = super::utf7::decode(&raw_name).unwrap_or_else(|| raw_name.clone());
        let stored = match self.store.folder_by_imap_path(&folder_name).await {
            Some(folder) => self.store.get_folder(&folder.id).await,
            None => Vec::new(),
        };
        let count = stored.len();
        let unseen = stored.iter().filter(|m| m.mail.unread).count();
        vec![
            format!(
                "* STATUS \"{}\" (MESSAGES {} UNSEEN {} RECENT 0 UIDNEXT {} UIDVALIDITY 1)\r\n",
                raw_name, count, unseen, self.uid_next
            ),
            format!("{} OK STATUS completed\r\n", tag),
        ]
    }

    async fn cmd_fetch(&mut self, tag: &str, args: &str, uid_mode: bool) -> Vec<String> {
        if self.state != State::Selected {
            return vec![format!("{} NO No mailbox selected\r\n", tag)];
        }

        let (seq_set, items) = parse_fetch_args(args);
        let indices = self.resolve_sequence_set(&seq_set, uid_mode);

        let mut responses = Vec::new();

        for idx in indices {
            if idx >= self.mails.len() {
                continue;
            }

            // Make sure we have a renderable body before answering a body
            // FETCH. The mailbox lists every message, but only the newest
            // `sync_limit` bodies are pre-warmed — older ones are fetched
            // here, on demand, the first time a client opens them.
            if needs_body(&items) && !self.mails[idx].body_loaded {
                let elem_id = self.mails[idx]
                    .mail
                    ._id
                    .as_ref()
                    .map(|id| id.element_id.to_string());

                if self.mails[idx].details.is_some() {
                    // Have details already, just render the envelope.
                    let rfc = mail_to_rfc2822(
                        &self.mails[idx].mail,
                        self.mails[idx].details.as_ref(),
                        &[],
                    );
                    self.mails[idx].rfc2822 = Some(rfc);
                    self.mails[idx].body_loaded = true;
                } else if let Some((details, rfc)) = match (&elem_id, &self.selected_folder) {
                    // The syncer may have filled it in since our snapshot.
                    (Some(eid), Some(folder)) => self.store.get_details(&folder.id, eid).await,
                    _ => None,
                } {
                    self.mails[idx].details = Some(details);
                    self.mails[idx].rfc2822 = Some(rfc);
                    self.mails[idx].body_loaded = true;
                } else {
                    // Out of the prefetch window: fetch the body on demand,
                    // unless this mail is in a post-failure cooldown (do not
                    // re-hit a throttled server on every client redraw).
                    if let Some(eid) = &elem_id {
                        if self.store.body_fetch_on_cooldown(eid) {
                            return vec![format!(
                                "{} NO [UNAVAILABLE] message body temporarily unavailable, try again later\r\n",
                                tag
                            )];
                        }
                    }
                    let mail = self.mails[idx].mail.clone();
                    match self.backend.load_mail_details(&mail).await {
                        Ok(Some(details)) => {
                            let atts = self
                                .backend
                                .load_attachments(&mail)
                                .await
                                .unwrap_or_default();
                            let refs: Vec<(&TutanotaFile, &[u8])> =
                                atts.iter().map(|(f, d)| (f, d.as_slice())).collect();
                            let rfc = mail_to_rfc2822(&mail, Some(&details), &refs);
                            // Share into the in-memory store so other
                            // connections (and stats) see it too.
                            if let (Some(eid), Some(folder)) = (&elem_id, &self.selected_folder) {
                                self.store
                                    .update_mail_details(
                                        &folder.id,
                                        eid,
                                        details.clone(),
                                        rfc.clone(),
                                        false,
                                    )
                                    .await;
                            }
                            self.mails[idx].details = Some(details);
                            self.mails[idx].rfc2822 = Some(rfc);
                            self.mails[idx].body_loaded = true;
                        }
                        Ok(None) => {
                            // Legacy mail with no body reference — headers only.
                            // Mark loaded so we don't re-hit the network on
                            // every body FETCH of a message that has no body.
                            self.mails[idx].body_loaded = true;
                            debug!("uid={} has no body source", self.mails[idx].uid);
                        }
                        Err(e) => {
                            log::warn!(
                                "On-demand body fetch failed for uid={}: {e}",
                                self.mails[idx].uid
                            );
                            if let Some(eid) = &elem_id {
                                self.store.mark_body_fetch_failed(eid);
                            }
                            return vec![format!(
                                "{} NO [UNAVAILABLE] could not fetch message body, try again later\r\n",
                                tag
                            )];
                        }
                    }
                }
            }

            let cached = &self.mails[idx];
            let seq = idx + 1;
            let resp = build_fetch_response(seq, cached, &items, uid_mode);
            responses.push(resp);
        }

        let cmd_name = if uid_mode { "UID FETCH" } else { "FETCH" };
        responses.push(format!("{} OK {} completed\r\n", tag, cmd_name));
        responses
    }

    fn cmd_search(&self, tag: &str, args: &str, uid_mode: bool) -> Vec<String> {
        if self.state != State::Selected {
            return vec![format!("{} NO No mailbox selected\r\n", tag)];
        }

        let query = search::parse(args);
        let ctx = self.resolve_body_search(&query);

        let ids: Vec<u32> = self
            .mails
            .iter()
            .enumerate()
            .filter(|(i, cached)| {
                let view = Self::build_search_view(*i, cached);
                search::matches(&query, &view, &ctx)
            })
            .map(|(i, cached)| if uid_mode { cached.uid } else { (i + 1) as u32 })
            .collect();

        let id_str = ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(" ");

        let cmd = if uid_mode { "UID SEARCH" } else { "SEARCH" };
        vec![
            format!("* SEARCH {}\r\n", id_str),
            format!("{} OK {} completed\r\n", tag, cmd),
        ]
    }

    /// Project a cached message onto the fields IMAP SEARCH compares. Derived
    /// strings (From / To / Cc / Bcc / headers) are owned by the [`MsgView`];
    /// the cheap ones (subject, body) borrow straight from `cached`.
    fn build_search_view(idx: usize, cached: &CachedMail) -> MsgView<'_> {
        let headers = cached
            .rfc2822
            .as_deref()
            .map(extract_headers)
            .unwrap_or_default();

        // To/Cc/Bcc come from details when the body is loaded; otherwise only
        // the envelope `firstRecipient` (rendered as To) is available.
        let (to, cc, bcc) = match cached.details.as_ref() {
            Some(d) => (
                join_addrs(&d.recipients.toRecipients),
                join_addrs(&d.recipients.ccRecipients),
                join_addrs(&d.recipients.bccRecipients),
            ),
            None => (
                cached
                    .mail
                    .firstRecipient
                    .as_ref()
                    .map(format_address)
                    .unwrap_or_default(),
                String::new(),
                String::new(),
            ),
        };

        let sent_ms = cached
            .details
            .as_ref()
            .map(|d| d.sentDate.as_millis())
            .unwrap_or_else(|| cached.mail.receivedDate.as_millis());

        let element_id = cached
            .mail
            ._id
            .as_ref()
            .map(|id| id.element_id.0.as_str())
            .unwrap_or("");

        MsgView {
            seq: (idx + 1) as u32,
            uid: cached.uid,
            element_id,
            subject: &cached.mail.subject,
            from: format_address(&cached.mail.sender),
            to,
            cc,
            bcc,
            headers,
            date_ms: cached.mail.receivedDate.as_millis(),
            sent_ms,
            unread: cached.mail.unread,
            deleted: cached.deleted,
            size: cached.rfc2822.as_ref().map(|r| r.len() as u64).unwrap_or(0),
        }
    }

    /// Resolve the `BODY`/`TEXT` terms of a query against the full-text index,
    /// once per distinct term. With no local store (unit tests) the context is
    /// empty, so body terms match nothing.
    fn resolve_body_search(&self, query: &search::SearchKey) -> search::SearchContext {
        let mut ctx = search::SearchContext::empty();
        let Some(local_store) = &self.local_store else {
            return ctx;
        };
        for term in search::collect_body_terms(query) {
            match local_store.search_body(&term) {
                Ok(ids) => {
                    ctx.body_hits.insert(term, ids.into_iter().collect());
                }
                Err(e) => log::warn!("FTS body search failed for {term:?}: {e}"),
            }
        }
        ctx
    }

    async fn cmd_store(&mut self, tag: &str, args: &str, uid_mode: bool) -> Vec<String> {
        if self.state != State::Selected {
            return vec![format!("{} NO No mailbox selected\r\n", tag)];
        }

        let args_upper = args.to_uppercase();
        let (seq_set, _) = parse_store_args(args);
        let indices = self.resolve_sequence_set(&seq_set, uid_mode);
        let adding = args_upper.contains("+FLAGS");
        let removing = args_upper.contains("-FLAGS");

        if args_upper.contains("\\SEEN") {
            for idx in &indices {
                if *idx < self.mails.len() {
                    self.mails[*idx].mail.unread = !adding;
                }
            }

            let mail_ids: Vec<_> = indices
                .iter()
                .filter_map(|&i| self.mails.get(i))
                .filter_map(|m| m.mail._id.clone())
                .collect();

            if !mail_ids.is_empty() {
                if let Err(e) = self.backend.set_unread_status(mail_ids, !adding).await {
                    log::warn!("Failed to update read status on server: {}", e);
                }
            }
        }

        if args_upper.contains("\\DELETED") {
            for idx in &indices {
                if *idx < self.mails.len() {
                    self.mails[*idx].deleted = if removing { false } else { true };
                }
            }
        }

        let cmd = if uid_mode { "UID STORE" } else { "STORE" };
        vec![format!("{} OK {} completed\r\n", tag, cmd)]
    }

    async fn cmd_uid(&mut self, tag: &str, args: &str) -> Vec<String> {
        if self.state != State::Selected {
            return vec![format!("{} NO No mailbox selected\r\n", tag)];
        }

        let parts: Vec<&str> = args.splitn(2, ' ').collect();
        let subcmd = parts.first().map(|s| s.to_uppercase()).unwrap_or_default();
        let subargs = parts.get(1).unwrap_or(&"");

        match subcmd.as_str() {
            "FETCH" => self.cmd_fetch(tag, subargs, true).await,
            "SEARCH" => self.cmd_search(tag, subargs, true),
            "STORE" => self.cmd_store(tag, subargs, true).await,
            "MOVE" => self.cmd_move(tag, subargs, true).await,
            "COPY" => self.cmd_copy(tag),
            _ => vec![format!("{} BAD Unknown UID subcommand\r\n", tag)],
        }
    }

    fn cmd_close(&mut self, tag: &str) -> Vec<String> {
        self.state = State::Authenticated;
        self.selected_folder = None;
        self.mails.clear();
        vec![format!("{} OK CLOSE completed\r\n", tag)]
    }

    async fn cmd_expunge(&mut self, tag: &str) -> Vec<String> {
        if self.state != State::Selected {
            return vec![format!("{} NO No mailbox selected\r\n", tag)];
        }

        let deleted_ids: Vec<_> = self
            .mails
            .iter()
            .filter(|m| m.deleted)
            .filter_map(|m| m.mail._id.clone())
            .collect();

        if !deleted_ids.is_empty() {
            if let Err(e) = self.backend.trash_mails(deleted_ids).await {
                log::warn!("Failed to trash mails: {}", e);
            }
        }

        let mut responses = Vec::new();
        let mut seq = 1u32;
        let mut i = 0;
        while i < self.mails.len() {
            if self.mails[i].deleted {
                responses.push(format!("* {} EXPUNGE\r\n", seq));
                self.mails.remove(i);
            } else {
                seq += 1;
                i += 1;
            }
        }

        responses.push(format!("{} OK EXPUNGE completed\r\n", tag));
        responses
    }

    /// MOVE / UID MOVE (RFC 6851): move messages to another mailbox. Tuta
    /// folders are exclusive, so this maps directly to a server-side move; the
    /// messages are then expunged from the source view.
    async fn cmd_move(&mut self, tag: &str, args: &str, uid_mode: bool) -> Vec<String> {
        let label = if uid_mode { "UID MOVE" } else { "MOVE" };
        if self.state != State::Selected {
            return vec![format!("{} NO No mailbox selected\r\n", tag)];
        }

        let (seq_set, rest) = args.trim().split_once(' ').unwrap_or((args.trim(), ""));
        let (raw_name, _) = parse_imap_token(rest.trim());
        let folder_name = super::utf7::decode(&raw_name).unwrap_or_else(|| raw_name.clone());
        let target = match self.store.folder_by_imap_path(&folder_name).await {
            Some(f) => f,
            None => return vec![format!("{} NO [TRYCREATE] Mailbox does not exist\r\n", tag)],
        };

        let indices = self.resolve_sequence_set(seq_set, uid_mode);
        let mail_ids: Vec<_> = indices
            .iter()
            .filter_map(|&i| self.mails.get(i))
            .filter_map(|m| m.mail._id.clone())
            .collect::<Vec<_>>();

        if mail_ids.is_empty() {
            return vec![format!("{} OK {} completed\r\n", tag, label)];
        }

        let moved: std::collections::HashSet<String> = mail_ids
            .iter()
            .map(|id| id.element_id.to_string())
            .collect();

        if let Err(e) = self.backend.move_mails(mail_ids, &target).await {
            log::warn!("{} to {} failed: {}", label, folder_name, e);
            return vec![format!("{} NO {} failed\r\n", tag, label)];
        }

        // Expunge the moved messages from the source view (RFC 6851).
        let mut responses = Vec::new();
        let mut seq = 1u32;
        let mut i = 0;
        while i < self.mails.len() {
            let is_moved = self.mails[i]
                .mail
                ._id
                .as_ref()
                .map(|id| moved.contains(&id.element_id.to_string()))
                .unwrap_or(false);
            if is_moved {
                responses.push(format!("* {} EXPUNGE\r\n", seq));
                self.mails.remove(i);
            } else {
                seq += 1;
                i += 1;
            }
        }
        responses.push(format!("{} OK {} completed\r\n", tag, label));
        responses
    }

    /// COPY is not supported: Tuta folders are exclusive (no duplication).
    /// Clients should use MOVE, which we advertise.
    fn cmd_copy(&self, tag: &str) -> Vec<String> {
        vec![format!(
            "{} NO [CANNOT] COPY is not supported; use MOVE\r\n",
            tag
        )]
    }

    async fn refresh_mails(&mut self, folder_id: &str) -> Result<(), String> {
        let stored = self.store.get_folder(folder_id).await;

        // Carry over already-loaded details/rfc for this session.
        let old_cache: std::collections::HashMap<String, (Option<MailDetails>, Option<String>)> =
            self.mails
                .iter()
                .filter_map(|m| {
                    let eid = m.mail._id.as_ref()?.element_id.to_string();
                    Some((eid, (m.details.clone(), m.rfc2822.clone())))
                })
                .collect();

        self.mails.clear();
        // UIDs are stable and assigned by the store; sort ascending so message
        // sequence order matches UID order, as IMAP clients expect.
        let mut stored = stored;
        stored.sort_by_key(|m| m.uid);

        for sm in stored {
            let elem_id = sm.mail._id.as_ref().map(|id| id.element_id.to_string());
            let (old_details, old_rfc) = elem_id
                .as_ref()
                .and_then(|eid| old_cache.get(eid))
                .cloned()
                .unwrap_or((None, None));

            let uid = sm.uid;
            if uid >= self.uid_next {
                self.uid_next = uid + 1;
            }

            let details = sm.details.or(old_details);
            // The body is final if we have real details, or a previously
            // rendered rfc2822 (from the store or this session) — those always
            // carry a real body. With none of those, the rfc2822 we render
            // below has a placeholder body, so the body still needs an
            // on-demand fetch the first time a client opens the message.
            let body_loaded = details.is_some() || sm.rfc2822.is_some() || old_rfc.is_some();
            let rfc2822 = sm
                .rfc2822
                .or(old_rfc)
                .unwrap_or_else(|| mail_to_rfc2822(&sm.mail, details.as_ref(), &[]));

            self.mails.push(CachedMail {
                mail: sm.mail,
                details,
                rfc2822: Some(rfc2822),
                body_loaded,
                uid,
                deleted: false,
            });
        }

        debug!(
            "Refreshed {} mails for {} from store",
            self.mails.len(),
            folder_id
        );
        Ok(())
    }

    fn resolve_sequence_set(&self, seq_set: &str, uid_mode: bool) -> Vec<usize> {
        let max = if uid_mode {
            self.mails.iter().map(|m| m.uid).max().unwrap_or(0)
        } else {
            self.mails.len() as u32
        };

        let mut result = Vec::new();
        for part in seq_set.split(',') {
            let part = part.trim();
            if let Some((start, end)) = part.split_once(':') {
                let s = parse_seq_num(start, max);
                let e = parse_seq_num(end, max);
                let (lo, hi) = if s <= e { (s, e) } else { (e, s) };
                for n in lo..=hi {
                    if let Some(idx) = self.seq_to_index(n, uid_mode) {
                        result.push(idx);
                    }
                }
            } else {
                let n = parse_seq_num(part, max);
                if let Some(idx) = self.seq_to_index(n, uid_mode) {
                    result.push(idx);
                }
            }
        }
        result.sort();
        result.dedup();
        result
    }

    fn seq_to_index(&self, num: u32, uid_mode: bool) -> Option<usize> {
        if uid_mode {
            self.mails.iter().position(|m| m.uid == num)
        } else if num >= 1 && (num as usize) <= self.mails.len() {
            Some((num - 1) as usize)
        } else {
            None
        }
    }
}

/// Join a recipient list into a single comparable string for SEARCH.
fn join_addrs(addrs: &[MailAddress]) -> String {
    addrs
        .iter()
        .map(format_address)
        .collect::<Vec<_>>()
        .join(", ")
}

fn build_fetch_response(seq: usize, cached: &CachedMail, items: &str, uid_mode: bool) -> String {
    let items_upper = items.to_uppercase();
    let mut parts = Vec::new();

    if uid_mode || items_upper.contains("UID") {
        parts.push(format!("UID {}", cached.uid));
    }

    if items_upper.contains("FLAGS") {
        let mut flags = Vec::new();
        if !cached.mail.unread {
            flags.push("\\Seen");
        }
        if cached.deleted {
            flags.push("\\Deleted");
        }
        parts.push(format!("FLAGS ({})", flags.join(" ")));
    }

    if items_upper.contains("INTERNALDATE") {
        let date = format_internal_date(cached.mail.receivedDate.as_millis());
        parts.push(format!("INTERNALDATE \"{}\"", date));
    }

    if items_upper.contains("RFC822.SIZE") {
        let size = cached.rfc2822.as_ref().map(|r| r.len()).unwrap_or(0);
        parts.push(format!("RFC822.SIZE {}", size));
    }

    if items_upper.contains("ENVELOPE") {
        parts.push(format!("ENVELOPE {}", build_envelope(cached)));
    }

    if items_upper.contains("BODYSTRUCTURE") {
        let bs = cached
            .rfc2822
            .as_ref()
            .map(|r| crate::mail::compute_bodystructure(r))
            .unwrap_or_else(|| {
                "(\"TEXT\" \"HTML\" (\"CHARSET\" \"UTF-8\") NIL NIL \"BASE64\" 0 0)".to_owned()
            });
        parts.push(format!("BODYSTRUCTURE {}", bs));
    }

    if items_upper.contains("BODY[]") || items_upper.contains("BODY.PEEK[]") {
        if let Some(ref rfc) = cached.rfc2822 {
            parts.push(format!("BODY[] {{{}}}\r\n{}", rfc.len(), rfc));
        }
    } else if let Some((section, fields, negate)) = parse_header_fields_section(items) {
        if let Some(ref rfc) = cached.rfc2822 {
            let headers = filter_header_fields(rfc, &fields, negate);
            parts.push(format!(
                "BODY[{}] {{{}}}\r\n{}",
                section,
                headers.len(),
                headers
            ));
        }
    } else if items_upper.contains("HEADER") {
        // Bare BODY[HEADER] / BODY.PEEK[HEADER] / RFC822.HEADER.
        if let Some(ref rfc) = cached.rfc2822 {
            let headers = extract_headers(rfc);
            let item = if items_upper.contains("RFC822.HEADER") {
                "RFC822.HEADER"
            } else {
                "BODY[HEADER]"
            };
            parts.push(format!("{} {{{}}}\r\n{}", item, headers.len(), headers));
        }
    }

    let has_rfc822_full = items_upper
        .split_whitespace()
        .any(|t| t.trim_matches(|c| c == '(' || c == ')') == "RFC822");
    if has_rfc822_full {
        if let Some(ref rfc) = cached.rfc2822 {
            parts.push(format!("RFC822 {{{}}}\r\n{}", rfc.len(), rfc));
        }
    }

    format!("* {} FETCH ({})\r\n", seq, parts.join(" "))
}

/// Parse `HEADER.FIELDS (...)` / `HEADER.FIELDS.NOT (...)` out of a FETCH item
/// list. Returns the section echoed as the client sent it, the field names
/// uppercased for matching, and whether the match is negated.
///
/// Clients match FETCH data items to their pending request by the literal
/// section string (RFC 3501 §7.4.2), so the response must echo the requested
/// field list — answering with a rewritten list makes strict clients (Apple
/// Mail) drop the data on the floor.
fn parse_header_fields_section(items: &str) -> Option<(String, Vec<String>, bool)> {
    let upper = items.to_uppercase();
    let start = upper.find("HEADER.FIELDS")?;
    let negate = upper[start..].starts_with("HEADER.FIELDS.NOT");
    let open = start + upper[start..].find('(')?;
    let close = open + upper[open..].find(')')?;
    let keyword = if negate {
        "HEADER.FIELDS.NOT"
    } else {
        "HEADER.FIELDS"
    };
    let section = format!("{} {}", keyword, &items[open..=close]);
    let fields = upper[open + 1..close]
        .split_whitespace()
        .map(|f| f.trim_matches('"').to_string())
        .collect();
    Some((section, fields, negate))
}

/// Header lines matching (or, when negated, not matching) `fields` — with
/// continuation lines kept attached to their header — terminated by the blank
/// line that delimits a HEADER.FIELDS section.
fn filter_header_fields(rfc: &str, fields: &[String], negate: bool) -> String {
    let header_len = rfc.find("\r\n\r\n").map(|p| p + 2).unwrap_or(rfc.len());
    let mut out = String::new();
    let mut keep = false;
    for line in rfc[..header_len].split_inclusive("\r\n") {
        if !line.starts_with(' ') && !line.starts_with('\t') {
            let name = line.split(':').next().unwrap_or("").trim().to_uppercase();
            keep = fields.iter().any(|f| *f == name) != negate;
        }
        if keep {
            out.push_str(line);
        }
    }
    out.push_str("\r\n");
    out
}

fn build_envelope(cached: &CachedMail) -> String {
    let mail = &cached.mail;
    let date = format_internal_date(mail.receivedDate.as_millis());
    let subject = imap_string(&mail.subject);

    let from = format_envelope_addr(&mail.sender);
    let to = cached
        .details
        .as_ref()
        .map(|d| {
            d.recipients
                .toRecipients
                .iter()
                .map(format_envelope_addr)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .or_else(|| mail.firstRecipient.as_ref().map(format_envelope_addr))
        .unwrap_or_else(|| "NIL".to_string());

    let msg_id = mail
        ._id
        .as_ref()
        .map(|id| format!("<{}.{}@tutabridge.local>", id.list_id, id.element_id))
        .unwrap_or_default();
    let msg_id = imap_string(&msg_id);

    format!("(\"{date}\" {subject} ({from}) ({from}) ({from}) ({to}) NIL NIL NIL {msg_id})")
}

/// Encode a string as an IMAP nstring. Safe 7-bit text becomes a quoted string;
/// a value holding CR, LF, or 8-bit bytes becomes a server-side literal
/// ({N}CRLF<octets>), since a quoted string may not contain any of those. An
/// empty value becomes "". Without this, a mail subject or sender name with an
/// embedded newline produces a malformed FETCH response and the client cannot
/// parse the message list (an empty mailbox in Thunderbird).
fn imap_string(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".to_string();
    }
    if s.bytes().any(|b| b == b'\r' || b == b'\n' || b >= 0x80) {
        format!("{{{}}}\r\n{}", s.len(), s)
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn format_envelope_addr(addr: &tutasdk::entities::generated::tutanota::MailAddress) -> String {
    let (user, domain) = addr.address.split_once('@').unwrap_or((&addr.address, ""));
    let name = if addr.name.is_empty() {
        "NIL".to_string()
    } else {
        imap_string(&addr.name)
    };
    format!(
        "({} NIL {} {})",
        name,
        imap_string(user),
        imap_string(domain)
    )
}

fn parse_seq_num(s: &str, max: u32) -> u32 {
    if s == "*" {
        max
    } else {
        s.parse::<u32>().unwrap_or(0)
    }
}

fn parse_fetch_args(args: &str) -> (String, String) {
    if let Some(paren_start) = args.find('(') {
        let seq = args[..paren_start].trim().to_string();
        let items = args[paren_start..].trim().to_string();
        (seq, items)
    } else {
        let parts: Vec<&str> = args.splitn(2, ' ').collect();
        let seq = parts.first().unwrap_or(&"").to_string();
        let items = parts.get(1).unwrap_or(&"").to_string();
        (seq, items)
    }
}

fn parse_store_args(args: &str) -> (String, String) {
    let parts: Vec<&str> = args.splitn(2, ' ').collect();
    let seq = parts.first().unwrap_or(&"").to_string();
    let rest = parts.get(1).unwrap_or(&"").to_string();
    (seq, rest)
}

/// Whether a FETCH item list requires the real message body to be loaded.
///
/// Only full-body content does: `BODY[]` / `BODY.PEEK[]`, a numbered MIME part
/// (`BODY[1]`), `BODY[TEXT]`, or a standalone `RFC822` / `RFC822.TEXT`. Items the
/// bridge answers from metadata alone must NOT trigger an on-demand fetch:
/// `ENVELOPE`, `BODY[HEADER...]`, `BODYSTRUCTURE`, `RFC822.SIZE`, `FLAGS`, etc.
/// Treating header/envelope requests as body requests made the client's
/// list-building (one such fetch per message) download the entire mailbox.
/// Whether a FETCH item list requires the real message body to be loaded.
///
/// The bridge serves the full message for `BODY[]` / `BODY.PEEK[]` and a
/// standalone `RFC822`; only those need the body. Envelope, header, size and
/// structure items (`ENVELOPE`, `BODY[HEADER...]`, `RFC822.SIZE`,
/// `BODYSTRUCTURE`, `FLAGS`, ...) are answered from metadata and must NOT
/// trigger an on-demand fetch. Treating header/envelope requests as body
/// requests made the client's list-building (one such fetch per message)
/// download the entire mailbox.
fn needs_body(items: &str) -> bool {
    let u = items.to_uppercase();
    if u.contains("BODY[]") || u.contains("BODY.PEEK[]") {
        return true;
    }
    // Standalone RFC822 (a full message), but not RFC822.SIZE / .HEADER / .TEXT.
    u.split(|c: char| c == '(' || c == ')' || c.is_whitespace())
        .any(|token| token == "RFC822")
}

fn parse_login_args(args: &str) -> (String, String) {
    let args = args.trim();
    let (user, rest) = parse_imap_token(args);
    let (pass, _) = parse_imap_token(rest.trim_start());
    (user, pass)
}

fn parse_imap_token(s: &str) -> (String, &str) {
    if s.starts_with('"') {
        let mut result = String::new();
        let mut chars = s[1..].char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    if let Some((_, escaped)) = chars.next() {
                        result.push(escaped);
                    }
                }
                '"' => return (result, &s[i + 2..]),
                _ => result.push(c),
            }
        }
        (result, "")
    } else {
        let end = s.find(char::is_whitespace).unwrap_or(s.len());
        (
            s[..end].to_string(),
            if end < s.len() { &s[end..] } else { "" },
        )
    }
}

fn parse_command(line: &str) -> (String, String, String) {
    let parts: Vec<&str> = line.splitn(3, ' ').collect();
    let tag = parts.first().unwrap_or(&"*").to_string();
    let cmd = parts.get(1).unwrap_or(&"").to_string();
    let args = parts.get(2).unwrap_or(&"").to_string();
    (tag, cmd, args)
}

/// A parsed `APPEND` command: the tag, the target mailbox, and the size of the
/// trailing `{N}` message literal the client is about to send.
pub(crate) struct AppendRequest {
    pub tag: String,
    pub mailbox: String,
    pub literal_size: usize,
}

/// Parse an `APPEND` command line. Returns `None` if it is not a well-formed
/// APPEND (so the caller falls back to normal command handling). The literal
/// reading itself happens at the socket level, since the session is line based.
pub(crate) fn parse_append(line: &str) -> Option<AppendRequest> {
    let (tag, cmd, args) = parse_command(line);
    if !cmd.eq_ignore_ascii_case("APPEND") {
        return None;
    }
    let mailbox = append_first_token(&args)?;
    let literal_size = append_literal_size(&args)?;
    Some(AppendRequest {
        tag,
        mailbox,
        literal_size,
    })
}

/// The first argument token of an APPEND (the mailbox), honoring quoting.
fn append_first_token(args: &str) -> Option<String> {
    let s = args.trim_start();
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    } else {
        let end = s.find(char::is_whitespace).unwrap_or(s.len());
        (end > 0).then(|| s[..end].to_string())
    }
}

/// The size declared by the trailing `{N}` (or non-sync `{N+}`) literal.
fn append_literal_size(args: &str) -> Option<usize> {
    let open = args.rfind('{')?;
    let close = args[open..].find('}')? + open;
    args[open + 1..close]
        .trim_end_matches('+')
        .parse::<usize>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use tutasdk::date::DateTime;
    use tutasdk::entities::generated::tutanota::MailAddress;
    use tutasdk::GeneratedId;
    use tutasdk::IdTupleGenerated;

    fn test_id(s: &str) -> GeneratedId {
        GeneratedId(s.to_string())
    }

    fn make_test_mail(uid: u32, subject: &str, unread: bool) -> CachedMail {
        CachedMail {
            mail: Mail {
                _id: Some(IdTupleGenerated::new(
                    test_id("mail_list"),
                    test_id("mail_elem"),
                )),
                _permissions: test_id("perm1"),
                _format: 0,
                _ownerEncSessionKey: None,
                subject: subject.to_string(),
                receivedDate: DateTime::from_millis(1735130245000),
                state: 2,
                unread,
                confidential: false,
                replyType: 0,
                _ownerGroup: None,
                differentEnvelopeSender: None,
                listUnsubscribe: false,
                movedTime: None,
                phishingStatus: 0,
                authStatus: None,
                method: 0,
                recipientCount: 1,
                encryptionAuthStatus: None,
                _ownerKeyVersion: None,
                processingState: 0,
                processNeeded: false,
                sendAt: None,
                serverClassificationData: None,
                _kdfNonce: None,
                sender: MailAddress {
                    _id: None,
                    name: "Sender".to_string(),
                    address: "sender@tuta.com".to_string(),
                    contact: None,
                    _errors: Default::default(),
                },
                attachments: vec![],
                conversationEntry: IdTupleGenerated::new(
                    test_id("conv_list"),
                    test_id("conv_elem"),
                ),
                firstRecipient: Some(MailAddress {
                    _id: None,
                    name: "Recip".to_string(),
                    address: "recip@test.com".to_string(),
                    contact: None,
                    _errors: Default::default(),
                }),
                mailDetails: None,
                mailDetailsDraft: None,
                bucketKey: None,
                sets: vec![],
                clientSpamClassifierResult: None,
                _errors: Default::default(),
            },
            details: None,
            rfc2822: Some("Date: Wed, 25 Dec 2024 12:30:45 +0000\r\nFrom: sender@tuta.com\r\nSubject: Test\r\n\r\nBody\r\n".to_string()),
            body_loaded: true,
            uid,
            deleted: false,
        }
    }

    // --- imap_string / ENVELOPE encoding ---

    #[test]
    fn imap_string_quotes_plain_ascii() {
        assert_eq!(imap_string("Hello world"), "\"Hello world\"");
    }

    #[test]
    fn imap_string_escapes_quote_and_backslash() {
        assert_eq!(imap_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn imap_string_empty_is_empty_quotes() {
        assert_eq!(imap_string(""), "\"\"");
    }

    #[test]
    fn imap_string_uses_literal_for_newline() {
        // A quoted string may not contain CR/LF, so it must become a literal.
        let s = "La\nselection";
        assert_eq!(imap_string(s), format!("{{{}}}\r\n{}", s.len(), s));
    }

    #[test]
    fn imap_string_uses_literal_for_8bit() {
        let s = "sélection"; // UTF-8 'é' is 8-bit
        assert_eq!(imap_string(s), format!("{{{}}}\r\n{}", s.len(), s));
    }

    #[test]
    fn build_envelope_newline_subject_uses_literal() {
        // Regression: a newline in the subject used to be emitted as a quoted
        // string, producing a malformed FETCH response that broke the client's
        // parse of the message list (an empty mailbox in Thunderbird).
        let subj = "La\nselection Courtois - Le bien du mois";
        let env = build_envelope(&make_test_mail(1, subj, false));
        assert!(
            env.contains(&format!("{{{}}}\r\n{}", subj.len(), subj)),
            "subject must be a literal, got: {env}"
        );
        assert!(
            !env.contains("\"La"),
            "subject must not be a quoted string holding the newline: {env}"
        );
    }

    // --- parse_command ---

    #[test]
    fn test_parse_command_full() {
        let (tag, cmd, args) = parse_command("A001 LOGIN user pass");
        assert_eq!(tag, "A001");
        assert_eq!(cmd, "LOGIN");
        assert_eq!(args, "user pass");
    }

    #[test]
    fn test_parse_command_no_args() {
        let (tag, cmd, args) = parse_command("A002 NOOP");
        assert_eq!(tag, "A002");
        assert_eq!(cmd, "NOOP");
        assert_eq!(args, "");
    }

    #[test]
    fn test_parse_command_tag_only() {
        let (tag, cmd, args) = parse_command("A003");
        assert_eq!(tag, "A003");
        assert_eq!(cmd, "");
        assert_eq!(args, "");
    }

    #[test]
    fn test_parse_command_empty() {
        let (tag, cmd, args) = parse_command("");
        assert_eq!(tag, "");
        assert_eq!(cmd, "");
        assert_eq!(args, "");
    }

    // --- parse_fetch_args ---

    #[test]
    fn test_parse_fetch_args_with_parens() {
        let (seq, items) = parse_fetch_args("1:* (FLAGS UID)");
        assert_eq!(seq, "1:*");
        assert_eq!(items, "(FLAGS UID)");
    }

    #[test]
    fn test_parse_fetch_args_without_parens() {
        let (seq, items) = parse_fetch_args("1 FLAGS");
        assert_eq!(seq, "1");
        assert_eq!(items, "FLAGS");
    }

    #[test]
    fn test_parse_fetch_args_complex() {
        let (seq, items) = parse_fetch_args("1:5 (BODY.PEEK[HEADER.FIELDS (DATE FROM)])");
        assert_eq!(seq, "1:5");
        assert_eq!(items, "(BODY.PEEK[HEADER.FIELDS (DATE FROM)])");
    }

    // --- parse_store_args ---

    #[test]
    fn test_parse_store_args() {
        let (seq, rest) = parse_store_args("1:3 +FLAGS (\\Seen)");
        assert_eq!(seq, "1:3");
        assert_eq!(rest, "+FLAGS (\\Seen)");
    }

    // --- parse_seq_num ---

    #[test]
    fn test_parse_seq_num_number() {
        assert_eq!(parse_seq_num("42", 100), 42);
    }

    #[test]
    fn test_parse_seq_num_star() {
        assert_eq!(parse_seq_num("*", 100), 100);
    }

    #[test]
    fn test_parse_seq_num_invalid() {
        assert_eq!(parse_seq_num("abc", 100), 0);
    }

    #[test]
    fn test_parse_seq_num_zero() {
        assert_eq!(parse_seq_num("0", 100), 0);
    }

    // --- needs_body ---

    #[test]
    fn test_needs_body() {
        assert!(needs_body("(BODY[])"));
        assert!(needs_body("(BODY.PEEK[])"));
        assert!(needs_body("(RFC822)"));
        assert!(!needs_body("(ENVELOPE)"));
        assert!(needs_body("(FLAGS BODY[])"));
        assert!(!needs_body("(FLAGS)"));
        assert!(!needs_body("(FLAGS UID INTERNALDATE)"));
        assert!(!needs_body("(RFC822.SIZE)"));
        assert!(!needs_body("(BODYSTRUCTURE)"));
    }

    // --- build_fetch_response ---

    #[test]
    fn test_build_fetch_response_flags_only() {
        let cached = make_test_mail(1, "Test", false);
        let resp = build_fetch_response(1, &cached, "(FLAGS)", false);
        assert_eq!(resp, "* 1 FETCH (FLAGS (\\Seen))\r\n");
    }

    #[test]
    fn test_build_fetch_response_flags_unread() {
        let cached = make_test_mail(1, "Test", true);
        let resp = build_fetch_response(1, &cached, "(FLAGS)", false);
        assert_eq!(resp, "* 1 FETCH (FLAGS ())\r\n");
    }

    #[test]
    fn test_build_fetch_response_uid_mode() {
        let cached = make_test_mail(42, "Test", false);
        let resp = build_fetch_response(3, &cached, "(FLAGS)", true);
        assert!(resp.contains("UID 42"));
        assert!(resp.contains("FLAGS (\\Seen)"));
    }

    #[test]
    fn test_build_fetch_response_internaldate() {
        let cached = make_test_mail(1, "Test", false);
        let resp = build_fetch_response(1, &cached, "(INTERNALDATE)", false);
        assert!(resp.contains("INTERNALDATE \"25-Dec-2024 12:37:25 +0000\""));
    }

    #[test]
    fn test_build_fetch_response_rfc822_size() {
        let cached = make_test_mail(1, "Test", false);
        let resp = build_fetch_response(1, &cached, "(RFC822.SIZE)", false);
        let rfc_len = cached.rfc2822.as_ref().unwrap().len();
        assert!(resp.contains(&format!("RFC822.SIZE {}", rfc_len)));
    }

    #[test]
    fn test_build_fetch_response_envelope() {
        let cached = make_test_mail(1, "Hello World", false);
        let resp = build_fetch_response(1, &cached, "(ENVELOPE)", false);
        assert!(resp.contains("ENVELOPE"));
        assert!(resp.contains("Hello World"));
        assert!(resp.contains("\"sender\" \"tuta.com\""));
    }

    #[test]
    fn test_build_fetch_response_body_peek() {
        let cached = make_test_mail(1, "Test", false);
        let resp = build_fetch_response(1, &cached, "(BODY.PEEK[])", false);
        assert!(resp.contains("BODY[] {"));
        assert!(resp.contains("From: sender@tuta.com"));
    }

    #[test]
    fn test_build_fetch_response_rfc822_full() {
        let cached = make_test_mail(1, "Test", false);
        let resp = build_fetch_response(1, &cached, "(RFC822)", false);
        assert!(resp.contains("RFC822 {"));
        assert!(resp.contains("From: sender@tuta.com"));
    }

    // --- format_envelope_addr ---

    #[test]
    fn test_format_envelope_addr_with_name() {
        let addr = MailAddress {
            _id: None,
            name: "John Doe".to_string(),
            address: "john@example.com".to_string(),
            contact: None,
            _errors: Default::default(),
        };
        assert_eq!(
            format_envelope_addr(&addr),
            "(\"John Doe\" NIL \"john\" \"example.com\")"
        );
    }

    #[test]
    fn test_format_envelope_addr_no_name() {
        let addr = MailAddress {
            _id: None,
            name: "".to_string(),
            address: "john@example.com".to_string(),
            contact: None,
            _errors: Default::default(),
        };
        assert_eq!(
            format_envelope_addr(&addr),
            "(NIL NIL \"john\" \"example.com\")"
        );
    }

    #[test]
    fn test_format_envelope_addr_no_domain() {
        let addr = MailAddress {
            _id: None,
            name: "".to_string(),
            address: "localonly".to_string(),
            contact: None,
            _errors: Default::default(),
        };
        assert_eq!(format_envelope_addr(&addr), "(NIL NIL \"localonly\" \"\")");
    }

    #[test]
    fn test_format_envelope_addr_quotes_in_name() {
        let addr = MailAddress {
            _id: None,
            name: "John \"JD\" Doe".to_string(),
            address: "john@example.com".to_string(),
            contact: None,
            _errors: Default::default(),
        };
        let result = format_envelope_addr(&addr);
        // Inner quotes are backslash-escaped inside the IMAP quoted string.
        assert_eq!(
            result,
            "(\"John \\\"JD\\\" Doe\" NIL \"john\" \"example.com\")"
        );
    }

    // --- extract_headers (via rfc2822 module) ---

    #[test]
    fn test_extract_headers_from_rfc() {
        let rfc = "Date: Mon, 01 Jan 2024\r\nFrom: a@b.com\r\n\r\nBody content";
        let headers = extract_headers(rfc);
        assert_eq!(headers, "Date: Mon, 01 Jan 2024\r\nFrom: a@b.com\r\n\r\n");
    }

    // --- parse_command edge cases ---

    #[test]
    fn test_parse_command_with_extra_spaces() {
        let (tag, cmd, args) = parse_command("A1 FETCH 1:* (FLAGS UID)");
        assert_eq!(tag, "A1");
        assert_eq!(cmd, "FETCH");
        assert_eq!(args, "1:* (FLAGS UID)");
    }

    // --- build_fetch_response: combined items ---

    #[test]
    fn test_build_fetch_response_multiple_items() {
        let cached = make_test_mail(5, "Multi", true);
        let resp = build_fetch_response(3, &cached, "(FLAGS UID INTERNALDATE)", false);
        assert!(resp.starts_with("* 3 FETCH ("));
        assert!(resp.contains("FLAGS ()"));
        assert!(resp.contains("UID 5"));
        assert!(resp.contains("INTERNALDATE"));
    }

    // --- needs_body: RFC822 not matching RFC822.HEADER etc ---

    #[test]
    fn test_needs_body_rfc822_variants() {
        assert!(needs_body("(RFC822)"));
        assert!(!needs_body("(RFC822.SIZE)"));
        assert!(!needs_body("(RFC822.HEADER)"));
        assert!(!needs_body("(RFC822.TEXT)"));
        assert!(needs_body("(RFC822.SIZE RFC822)"));
        assert!(needs_body("(FLAGS RFC822)"));
    }

    // --- parse_fetch_args: edge cases ---

    #[test]
    fn test_parse_fetch_args_star_range() {
        let (seq, items) = parse_fetch_args("1:* (FLAGS)");
        assert_eq!(seq, "1:*");
        assert_eq!(items, "(FLAGS)");
    }

    // --- parse_seq_num: large numbers ---

    #[test]
    fn test_parse_seq_num_large_number() {
        assert_eq!(parse_seq_num("999999", 100), 999999);
    }

    #[test]
    fn test_parse_seq_num_negative() {
        assert_eq!(parse_seq_num("-1", 100), 0);
    }

    // --- build_envelope: subject with quotes ---

    #[test]
    fn test_build_envelope_subject_with_quotes() {
        let cached = make_test_mail(1, "Re: \"Important\" stuff", false);
        let resp = build_fetch_response(1, &cached, "(ENVELOPE)", false);
        assert!(resp.contains("\\\"Important\\\""));
        assert!(!resp.contains("\"\"Important\"\""));
    }

    // --- HEADER.FIELDS: section echo and field filtering ---

    #[test]
    fn header_fields_response_echoes_requested_list_verbatim() {
        let cached = make_test_mail(1, "Test", false);
        // Apple Mail-style request: long list, lowercase, includes fields the
        // message doesn't have. The response section must echo this list.
        let resp = build_fetch_response(
            1,
            &cached,
            "(BODY.PEEK[HEADER.FIELDS (date subject from x-priority x-universally-unique-identifier)])",
            false,
        );
        assert!(
            resp.contains(
                "BODY[HEADER.FIELDS (date subject from x-priority x-universally-unique-identifier)]"
            ),
            "section must echo the client's field list: {resp}"
        );
        assert!(resp.contains("Subject: Test"));
        assert!(resp.contains("From: sender@tuta.com"));
    }

    #[test]
    fn header_fields_filters_to_requested_fields() {
        let rfc = "Date: Wed, 25 Dec 2024 12:30:45 +0000\r\nFrom: a@b.c\r\nX-Long: first\r\n\tcontinued\r\nSubject: Hi\r\n\r\nBody\r\n";
        let got = filter_header_fields(rfc, &["SUBJECT".into(), "X-LONG".into()], false);
        assert_eq!(got, "X-Long: first\r\n\tcontinued\r\nSubject: Hi\r\n\r\n");
        // Negated: everything except the listed fields.
        let not = filter_header_fields(rfc, &["X-LONG".into(), "DATE".into()], true);
        assert_eq!(not, "From: a@b.c\r\nSubject: Hi\r\n\r\n");
    }

    #[test]
    fn parse_header_fields_section_variants() {
        let (section, fields, negate) =
            parse_header_fields_section("(BODY.PEEK[HEADER.FIELDS (From To)])").unwrap();
        assert_eq!(section, "HEADER.FIELDS (From To)");
        assert_eq!(fields, vec!["FROM".to_string(), "TO".to_string()]);
        assert!(!negate);

        let (section, fields, negate) =
            parse_header_fields_section("(BODY[HEADER.FIELDS.NOT (received)])").unwrap();
        assert_eq!(section, "HEADER.FIELDS.NOT (received)");
        assert_eq!(fields, vec!["RECEIVED".to_string()]);
        assert!(negate);

        assert!(parse_header_fields_section("(FLAGS UID)").is_none());
    }

    #[test]
    fn bare_header_fetch_returns_all_headers() {
        let cached = make_test_mail(1, "Test", false);
        let resp = build_fetch_response(1, &cached, "(BODY.PEEK[HEADER])", false);
        assert!(resp.contains("BODY[HEADER] {"), "got: {resp}");
        assert!(resp.contains("Subject: Test"));
        let resp = build_fetch_response(1, &cached, "(RFC822.HEADER)", false);
        assert!(resp.contains("RFC822.HEADER {"), "got: {resp}");
    }

    // --- needs_body: only real body content triggers an on-demand fetch ---

    #[test]
    fn needs_body_skips_metadata_and_header_fetches() {
        // List-building items are answered from metadata, no fetch.
        assert!(!needs_body("(FLAGS UID)"));
        assert!(!needs_body("(ENVELOPE)"));
        assert!(!needs_body("(FLAGS UID RFC822.SIZE ENVELOPE)"));
        assert!(!needs_body(
            "(BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT DATE)])"
        ));
        assert!(!needs_body("(UID RFC822.SIZE BODYSTRUCTURE FLAGS)"));
        assert!(!needs_body("(RFC822.HEADER)"));
        assert!(!needs_body("(BODY.PEEK[1.MIME])"));
    }

    #[test]
    fn needs_body_triggers_for_body_content() {
        assert!(needs_body("(BODY[])"));
        assert!(needs_body("(BODY.PEEK[])"));
        assert!(needs_body("(RFC822)"));
        assert!(needs_body("(FLAGS BODY.PEEK[])"));
        assert!(needs_body("BODY[]"));
    }

    // =================================================================
    // Integration tests with MockBackend
    // =================================================================

    use crate::mail::ParsedMessage;
    use crate::sync::MailStore;
    use crate::tuta::MailBackend;
    use std::sync::Mutex;
    use tutasdk::entities::generated::tutanota::{Body, Recipients};

    struct MockBackend {
        mails: Mutex<Vec<Mail>>,
        details: Mutex<std::collections::HashMap<String, MailDetails>>,
        trashed: Mutex<Vec<IdTupleGenerated>>,
        unread_calls: Mutex<Vec<(Vec<IdTupleGenerated>, bool)>>,
        sent: Mutex<Vec<ParsedMessage>>,
        moved: Mutex<Vec<(Vec<IdTupleGenerated>, String)>>,
        fail_details: Mutex<bool>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                mails: Mutex::new(Vec::new()),
                details: Mutex::new(std::collections::HashMap::new()),
                trashed: Mutex::new(Vec::new()),
                unread_calls: Mutex::new(Vec::new()),
                sent: Mutex::new(Vec::new()),
                moved: Mutex::new(Vec::new()),
                fail_details: Mutex::new(false),
            }
        }

        fn set_fail_details(&self, v: bool) {
            *self.fail_details.lock().unwrap() = v;
        }

        fn with_mails(mails: Vec<Mail>) -> Self {
            let m = Self::new();
            *m.mails.lock().unwrap() = mails;
            m
        }

        fn add_details(&self, element_id: &str, details: MailDetails) {
            self.details
                .lock()
                .unwrap()
                .insert(element_id.to_string(), details);
        }
    }

    use crate::sync::StoredMail;
    use crate::tuta::FolderInfo;
    use tutasdk::folder_system::MailSetKind;

    fn inbox_folder() -> FolderInfo {
        FolderInfo {
            id: "inbox".to_string(),
            list_id: "folders".to_string(),
            entries_list_id: "inbox_entries".to_string(),
            kind: MailSetKind::Inbox,
            imap_path: "INBOX".to_string(),
            special_use: None,
        }
    }

    #[tokio::test]
    async fn uid_move_moves_to_target_and_expunges() {
        let m1 = make_mail("e1", "one", false);
        let m2 = make_mail("e2", "two", false);
        let backend = Arc::new(MockBackend::with_mails(vec![m1.clone(), m2.clone()]));
        let store = MailStore::new();
        let target = FolderInfo {
            id: "cust".to_string(),
            list_id: "folders".to_string(),
            entries_list_id: "cust_entries".to_string(),
            kind: MailSetKind::Custom,
            imap_path: "Work".to_string(),
            special_use: None,
        };
        store.set_folder_list(vec![inbox_folder(), target]).await;
        store
            .set_folder(
                "inbox",
                vec![
                    StoredMail {
                        mail: m1,
                        details: None,
                        rfc2822: None,
                        uid: 1,
                        attachments_pending: false,
                    },
                    StoredMail {
                        mail: m2,
                        details: None,
                        rfc2822: None,
                        uid: 2,
                        attachments_pending: false,
                    },
                ],
            )
            .await;
        let mut session = ImapSession::new(store, backend.clone(), None, None);
        session.handle_command("a LOGIN u p").await;
        session.handle_command("b SELECT INBOX").await;

        let resp = session.handle_command("c UID MOVE 1 Work").await;
        assert!(
            resp.iter().any(|r| r.contains("EXPUNGE")),
            "expected EXPUNGE, got {resp:?}"
        );
        assert!(resp.last().unwrap().contains("OK UID MOVE"));

        let moved = backend.moved.lock().unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].1, "cust");
        assert_eq!(moved[0].0.len(), 1);
    }

    #[tokio::test]
    async fn on_demand_body_fetch_failure_returns_no_and_cooldowns() {
        let mail = make_mail("ed", "subj", false);
        let backend = Arc::new(MockBackend::with_mails(vec![mail.clone()]));
        backend.set_fail_details(true);
        let store = MailStore::new();
        store.set_folder_list(vec![inbox_folder()]).await;
        store
            .set_folder(
                "inbox",
                vec![StoredMail {
                    mail,
                    details: None,
                    rfc2822: None,
                    uid: 1,
                    attachments_pending: false,
                }],
            )
            .await;
        let mut session = ImapSession::new(store.clone(), backend.clone(), None, None);
        session.handle_command("a LOGIN u p").await;
        session.handle_command("b SELECT INBOX").await;

        // A failed on-demand body fetch returns NO [UNAVAILABLE] (not a fake
        // body) and arms the shared cooldown.
        let resp = session.handle_command("c UID FETCH 1 (BODY[])").await;
        assert!(
            resp.last().unwrap().contains("NO [UNAVAILABLE]"),
            "expected NO on failure, got {resp:?}"
        );
        assert!(store.body_fetch_on_cooldown("ed"));

        // While cooling down, a repeat fetch is short-circuited to NO without
        // touching the backend, even though it would now succeed.
        backend.set_fail_details(false);
        let resp2 = session.handle_command("d UID FETCH 1 (BODY[])").await;
        assert!(
            resp2.last().unwrap().contains("NO [UNAVAILABLE]"),
            "cooldown should keep returning NO, got {resp2:?}"
        );
    }

    #[tokio::test]
    async fn copy_is_rejected() {
        let backend = Arc::new(MockBackend::with_mails(vec![]));
        let (_store, mut session) = make_session(backend).await;
        session.handle_command("a LOGIN u p").await;
        session.handle_command("b SELECT INBOX").await;
        let resp = session.handle_command("c UID COPY 1 Work").await;
        assert!(
            resp[0].contains("NO"),
            "COPY should be rejected, got {resp:?}"
        );
    }

    #[test]
    fn parse_append_extracts_mailbox_and_size() {
        let r = parse_append("a1 APPEND \"Sent\" (\\Seen) {310}").unwrap();
        assert_eq!(r.tag, "a1");
        assert_eq!(r.mailbox, "Sent");
        assert_eq!(r.literal_size, 310);

        // Bare (unquoted) mailbox, no flags.
        let r = parse_append("x APPEND Drafts {42}").unwrap();
        assert_eq!(r.mailbox, "Drafts");
        assert_eq!(r.literal_size, 42);

        // Non-synchronizing literal {N+}.
        let r = parse_append("y APPEND \"Sent\" {7+}").unwrap();
        assert_eq!(r.literal_size, 7);

        // Not an APPEND, or missing the literal.
        assert!(parse_append("z SELECT INBOX").is_none());
        assert!(parse_append("z APPEND Sent").is_none());
    }

    #[tokio::test]
    async fn append_targets_sent_only_for_the_sent_folder() {
        let backend = Arc::new(MockBackend::with_mails(vec![]));
        let store = MailStore::new();
        let sent = FolderInfo {
            id: "sent".into(),
            list_id: "folders".into(),
            entries_list_id: "sent_entries".into(),
            kind: MailSetKind::Sent,
            imap_path: "Sent".into(),
            special_use: Some("\\Sent".into()),
        };
        store.set_folder_list(vec![inbox_folder(), sent]).await;
        let session = ImapSession::new(store, backend, None, None);

        assert!(session.append_targets_sent("Sent").await);
        assert!(!session.append_targets_sent("INBOX").await);
        assert!(!session.append_targets_sent("Nonexistent").await);
    }

    async fn populate_store(store: &MailStore, mails: &[Mail]) {
        store.set_folder_list(vec![inbox_folder()]).await;
        let stored: Vec<StoredMail> = mails
            .iter()
            .enumerate()
            .map(|(i, m)| StoredMail {
                mail: m.clone(),
                details: None,
                rfc2822: None,
                uid: (i + 1) as u32,
                attachments_pending: false,
            })
            .collect();
        store.set_folder("inbox", stored).await;
    }

    async fn make_session(backend: Arc<MockBackend>) -> (Arc<MailStore>, ImapSession) {
        let store = MailStore::new();
        let mails = backend.mails.lock().unwrap().clone();
        populate_store(&store, &mails).await;
        let session = ImapSession::new(store.clone(), backend, None, None);
        (store, session)
    }

    #[async_trait::async_trait]
    impl MailBackend for MockBackend {
        async fn load_mail_ids_for_folder(
            &self,
            _folder: &FolderInfo,
            _limit: usize,
        ) -> Result<Vec<Mail>, String> {
            Ok(self.mails.lock().unwrap().clone())
        }
        async fn load_mail(
            &self,
            _list_id: &str,
            element_id: &str,
        ) -> Result<Option<Mail>, String> {
            Ok(self
                .mails
                .lock()
                .unwrap()
                .iter()
                .find(|m| {
                    m._id
                        .as_ref()
                        .map(|id| id.element_id.to_string())
                        .as_deref()
                        == Some(element_id)
                })
                .cloned())
        }
        async fn decrypt_inline_mail(&self, _json: &str) -> Result<Option<Mail>, String> {
            // Tests stub: the inline path is exercised by handler tests with
            // an explicit recorded payload, not through the mock's body.
            Ok(None)
        }
        async fn decrypt_inline_mail_set_entry(
            &self,
            _json: &str,
        ) -> Result<Option<tutasdk::entities::generated::tutanota::MailSetEntry>, String> {
            Ok(None)
        }
        async fn decrypt_inline_mail_details_blob(
            &self,
            _json: &str,
        ) -> Result<Option<MailDetails>, String> {
            Ok(None)
        }
        async fn load_mail_details(&self, mail: &Mail) -> Result<Option<MailDetails>, String> {
            if *self.fail_details.lock().unwrap() {
                return Err("simulated Too Many Requests".to_string());
            }
            let key = mail
                ._id
                .as_ref()
                .map(|id| id.element_id.to_string())
                .unwrap_or_default();
            Ok(self.details.lock().unwrap().get(&key).cloned())
        }
        async fn load_attachments(
            &self,
            _mail: &Mail,
        ) -> Result<
            Vec<(
                tutasdk::entities::generated::tutanota::TutanotaFile,
                Vec<u8>,
            )>,
            String,
        > {
            // The mock has no attachment fixtures; the IMAP-session tests
            // only exercise body/header paths.
            Ok(Vec::new())
        }
        async fn list_folders(&self) -> Result<Vec<FolderInfo>, String> {
            Ok(vec![inbox_folder()])
        }
        async fn set_unread_status(
            &self,
            mail_ids: Vec<IdTupleGenerated>,
            unread: bool,
        ) -> Result<(), String> {
            self.unread_calls.lock().unwrap().push((mail_ids, unread));
            Ok(())
        }
        async fn trash_mails(&self, mail_ids: Vec<IdTupleGenerated>) -> Result<(), String> {
            self.trashed.lock().unwrap().extend(mail_ids);
            Ok(())
        }
        async fn move_mails(
            &self,
            mail_ids: Vec<IdTupleGenerated>,
            target: &FolderInfo,
        ) -> Result<(), String> {
            self.moved
                .lock()
                .unwrap()
                .push((mail_ids, target.id.clone()));
            Ok(())
        }
        async fn send_mail(&self, msg: &ParsedMessage) -> Result<(), String> {
            self.sent.lock().unwrap().push(msg.clone());
            Ok(())
        }
    }

    fn make_mail(element_id: &str, subject: &str, unread: bool) -> Mail {
        Mail {
            _id: Some(IdTupleGenerated::new(test_id("list1"), test_id(element_id))),
            _permissions: test_id("perm1"),
            _format: 0,
            _ownerEncSessionKey: None,
            subject: subject.to_string(),
            receivedDate: DateTime::from_millis(1735130245000),
            state: 2,
            unread,
            confidential: false,
            replyType: 0,
            _ownerGroup: None,
            differentEnvelopeSender: None,
            listUnsubscribe: false,
            movedTime: None,
            phishingStatus: 0,
            authStatus: None,
            method: 0,
            recipientCount: 1,
            encryptionAuthStatus: None,
            _ownerKeyVersion: None,
            processingState: 0,
            processNeeded: false,
            sendAt: None,
            serverClassificationData: None,
            _kdfNonce: None,
            sender: MailAddress {
                _id: None,
                name: "Sender".to_string(),
                address: "sender@tuta.com".to_string(),
                contact: None,
                _errors: Default::default(),
            },
            attachments: vec![],
            conversationEntry: IdTupleGenerated::new(test_id("conv_list"), test_id("conv_elem")),
            firstRecipient: Some(MailAddress {
                _id: None,
                name: "Recip".to_string(),
                address: "recip@test.com".to_string(),
                contact: None,
                _errors: Default::default(),
            }),
            mailDetails: None,
            mailDetailsDraft: None,
            bucketKey: None,
            sets: vec![],
            clientSpamClassifierResult: None,
            _errors: Default::default(),
        }
    }

    fn make_details(body_html: &str) -> MailDetails {
        MailDetails {
            _id: None,
            sentDate: DateTime::from_millis(1735130245000),
            authStatus: 0,
            replyTos: vec![],
            recipients: Recipients {
                _id: None,
                toRecipients: vec![MailAddress {
                    _id: None,
                    name: "Recip".to_string(),
                    address: "recip@test.com".to_string(),
                    contact: None,
                    _errors: Default::default(),
                }],
                ccRecipients: vec![],
                bccRecipients: vec![],
            },
            headers: None,
            body: Body {
                _id: None,
                text: Some(body_html.to_string()),
                compressedText: None,
                _errors: Default::default(),
            },
        }
    }

    // --- Full IMAP session integration tests ---

    #[tokio::test]
    async fn test_full_login_select_fetch_sequence() {
        let m1 = make_mail("m1", "First mail", true);
        let m2 = make_mail("m2", "Second mail", false);
        let d1 = make_details("<p>Body 1</p>");
        let d2 = make_details("<p>Body 2</p>");

        let backend = Arc::new(MockBackend::with_mails(vec![m1.clone(), m2.clone()]));
        let store = MailStore::new();
        let rfc1 = crate::mail::mail_to_rfc2822(&m1, Some(&d1), &[]);
        let rfc2 = crate::mail::mail_to_rfc2822(&m2, Some(&d2), &[]);
        store.set_folder_list(vec![inbox_folder()]).await;
        store
            .set_folder(
                "inbox",
                vec![
                    StoredMail {
                        mail: m1,
                        details: Some(d1),
                        rfc2822: Some(rfc1),
                        uid: 1,
                        attachments_pending: false,
                    },
                    StoredMail {
                        mail: m2,
                        details: Some(d2),
                        rfc2822: Some(rfc2),
                        uid: 2,
                        attachments_pending: false,
                    },
                ],
            )
            .await;
        let mut session = ImapSession::new(store, backend, None, None);

        // LOGIN
        let resp = session.handle_command("A001 LOGIN user pass").await;
        assert!(resp[0].contains("OK LOGIN"));

        // LIST
        let resp = session.handle_command("A002 LIST \"\" \"*\"").await;
        assert!(resp.iter().any(|r| r.contains("INBOX")));
        assert!(resp.last().unwrap().contains("OK LIST"));

        // SELECT INBOX
        let resp = session.handle_command("A003 SELECT INBOX").await;
        assert!(resp.iter().any(|r| r.contains("* 2 EXISTS")));
        assert!(resp.iter().any(|r| r.contains("UIDNEXT")));
        assert!(resp.last().unwrap().contains("OK"));

        // FETCH FLAGS
        let resp = session.handle_command("A004 FETCH 1:* (FLAGS UID)").await;
        assert!(resp[0].contains("* 1 FETCH"));
        assert!(resp[0].contains("FLAGS ()")); // unread → no \Seen
        assert!(resp[1].contains("* 2 FETCH"));
        assert!(resp[1].contains("FLAGS (\\Seen)")); // read → \Seen
        assert!(resp.last().unwrap().contains("OK FETCH"));

        // UID FETCH with BODY
        let resp = session
            .handle_command("A005 UID FETCH 1 (BODY.PEEK[])")
            .await;
        assert!(resp[0].contains("BODY[]"));
        // Body is base64-encoded HTML, verify the literal is present
        let b64_body = base64::engine::general_purpose::STANDARD.encode(b"<p>Body 1</p>");
        assert!(resp[0].contains(&b64_body));
    }

    #[tokio::test]
    async fn test_store_seen_flag_calls_backend() {
        let backend = Arc::new(MockBackend::with_mails(vec![make_mail(
            "m1",
            "Unread mail",
            true,
        )]));
        let store = MailStore::new();
        populate_store(&store, &backend.mails.lock().unwrap()).await;
        let mut session = ImapSession::new(store, backend.clone(), None, None);

        session.handle_command("A001 LOGIN user pass").await;
        session.handle_command("A002 SELECT INBOX").await;

        // Mark as read
        let resp = session.handle_command("A003 STORE 1 +FLAGS (\\Seen)").await;
        assert!(resp[0].contains("OK"));

        // Verify backend was called
        let calls = backend.unread_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, false); // unread=false → marking as read

        // Verify local state updated
        let resp = session.handle_command("A004 FETCH 1 (FLAGS)").await;
        assert!(resp[0].contains("\\Seen"));
    }

    #[tokio::test]
    async fn test_delete_and_expunge() {
        let backend = Arc::new(MockBackend::with_mails(vec![
            make_mail("m1", "Mail 1", false),
            make_mail("m2", "Mail 2", false),
            make_mail("m3", "Mail 3", false),
        ]));
        let store = MailStore::new();
        populate_store(&store, &backend.mails.lock().unwrap()).await;
        let mut session = ImapSession::new(store, backend.clone(), None, None);

        session.handle_command("A001 LOGIN user pass").await;
        session.handle_command("A002 SELECT INBOX").await;

        // Mark mail 2 as deleted
        session
            .handle_command("A003 STORE 2 +FLAGS (\\Deleted)")
            .await;

        // Verify deleted flag in FETCH
        let resp = session.handle_command("A004 FETCH 2 (FLAGS)").await;
        assert!(resp[0].contains("\\Deleted"));

        // EXPUNGE
        let resp = session.handle_command("A005 EXPUNGE").await;
        assert!(resp.iter().any(|r| r.contains("* 2 EXPUNGE")));
        assert!(resp.last().unwrap().contains("OK EXPUNGE"));

        // Verify backend was called
        let trashed = backend.trashed.lock().unwrap();
        assert_eq!(trashed.len(), 1);

        // Verify only 2 mails remain
        let resp = session.handle_command("A006 FETCH 1:* (FLAGS UID)").await;
        let fetch_lines: Vec<_> = resp
            .iter()
            .filter(|r| r.contains("* ") && r.contains("FETCH"))
            .collect();
        assert_eq!(fetch_lines.len(), 2);
    }

    #[tokio::test]
    async fn test_uid_stability_across_refresh() {
        let backend = Arc::new(MockBackend::with_mails(vec![
            make_mail("m1", "First", false),
            make_mail("m2", "Second", false),
        ]));
        let store = MailStore::new();
        populate_store(&store, &backend.mails.lock().unwrap()).await;
        let mut session = ImapSession::new(store.clone(), backend.clone(), None, None);

        session.handle_command("A001 LOGIN user pass").await;
        session.handle_command("A002 SELECT INBOX").await;

        // Get initial UIDs
        let resp = session.handle_command("A003 FETCH 1:* (UID)").await;
        let uid1_first = extract_uid(&resp[0]);
        let uid2_first = extract_uid(&resp[1]);

        // Add a new mail, update both backend and store, re-SELECT
        {
            let mut mails = backend.mails.lock().unwrap();
            mails.push(make_mail("m3", "Third", true));
        }
        populate_store(&store, &backend.mails.lock().unwrap()).await;
        session.handle_command("A004 SELECT INBOX").await;

        // Get UIDs again
        let resp = session.handle_command("A005 FETCH 1:* (UID)").await;
        let uid1_second = extract_uid(&resp[0]);
        let uid2_second = extract_uid(&resp[1]);
        let uid3 = extract_uid(&resp[2]);

        // UIDs for existing mails must be stable
        assert_eq!(uid1_first, uid1_second, "UID for m1 changed after refresh");
        assert_eq!(uid2_first, uid2_second, "UID for m2 changed after refresh");
        // New mail gets a new UID
        assert!(uid3 > uid2_second, "New mail UID should be higher");
    }

    fn extract_uid(line: &str) -> u32 {
        let uid_pos = line.find("UID ").unwrap();
        let rest = &line[uid_pos + 4..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        rest[..end].parse().unwrap()
    }

    #[tokio::test]
    async fn test_idle_end_with_done() {
        let backend = Arc::new(MockBackend::with_mails(vec![make_mail(
            "m1", "Test", false,
        )]));
        let (_store, mut session) = make_session(backend).await;

        session.handle_command("A001 LOGIN user pass").await;
        session.handle_command("A002 SELECT INBOX").await;

        // Enter IDLE
        let resp = session.handle_command("A003 IDLE").await;
        assert!(resp[0].contains("+ idling"));
        assert!(session.is_idle());

        // End IDLE
        let resp = session.end_idle();
        assert!(resp[0].contains("A003 OK IDLE terminated"));
        assert!(!session.is_idle());
    }

    #[tokio::test]
    async fn test_search_unseen() {
        let backend = Arc::new(MockBackend::with_mails(vec![
            make_mail("m1", "Read", false),
            make_mail("m2", "Unread", true),
            make_mail("m3", "Also read", false),
        ]));
        let (_store, mut session) = make_session(backend).await;

        session.handle_command("A001 LOGIN user pass").await;
        session.handle_command("A002 SELECT INBOX").await;

        let resp = session.handle_command("A003 SEARCH UNSEEN").await;
        assert!(resp[0].contains("* SEARCH 2"));
        assert!(!resp[0].contains("1"));
        assert!(!resp[0].contains("3"));
    }

    #[tokio::test]
    async fn test_uid_search_unseen() {
        let backend = Arc::new(MockBackend::with_mails(vec![
            make_mail("m1", "Read", false),
            make_mail("m2", "Unread", true),
        ]));
        let (_store, mut session) = make_session(backend).await;

        session.handle_command("A001 LOGIN user pass").await;
        session.handle_command("A002 SELECT INBOX").await;

        let resp = session.handle_command("A003 UID SEARCH UNSEEN").await;
        // Should return UID of the unread mail, not sequence number
        let search_line = &resp[0];
        assert!(search_line.contains("* SEARCH"));
        // UID 2 (second mail)
        assert!(search_line.contains("2"));
    }

    #[tokio::test]
    async fn test_search_subject_and_from() {
        let backend = Arc::new(MockBackend::with_mails(vec![
            make_mail("m1", "Invoice March", false),
            make_mail("m2", "Holiday photos", true),
            make_mail("m3", "Invoice April", false),
        ]));
        let (_store, mut session) = make_session(backend).await;
        session.handle_command("A001 LOGIN user pass").await;
        session.handle_command("A002 SELECT INBOX").await;

        // SUBJECT matches only the two invoices (1 and 3), case-insensitively.
        let resp = session
            .handle_command(r#"A003 SEARCH SUBJECT "invoice""#)
            .await;
        assert_eq!(resp[0], "* SEARCH 1 3\r\n");

        // Combined with UNSEEN, nothing matches (both invoices are read).
        let resp = session
            .handle_command(r#"A004 SEARCH SUBJECT "invoice" UNSEEN"#)
            .await;
        assert_eq!(resp[0], "* SEARCH \r\n");

        // FROM matches the shared sender on every message.
        let resp = session
            .handle_command(r#"A005 SEARCH FROM "sender@tuta.com""#)
            .await;
        assert_eq!(resp[0], "* SEARCH 1 2 3\r\n");

        // A non-matching subject returns an empty set (the regression that
        // motivated real SEARCH: it used to return everything).
        let resp = session
            .handle_command(r#"A006 SEARCH SUBJECT "nonexistent""#)
            .await;
        assert_eq!(resp[0], "* SEARCH \r\n");
    }

    #[tokio::test]
    async fn test_not_authenticated_rejects_commands() {
        let backend = Arc::new(MockBackend::new());
        let (_store, mut session) = make_session(backend).await;

        let resp = session.handle_command("A001 SELECT INBOX").await;
        assert!(resp[0].contains("NO Not authenticated"));

        let resp = session.handle_command("A001 LIST \"\" \"*\"").await;
        assert!(resp[0].contains("NO Not authenticated"));
    }

    #[tokio::test]
    async fn test_no_mailbox_selected_rejects_fetch() {
        let backend = Arc::new(MockBackend::new());
        let (_store, mut session) = make_session(backend).await;

        session.handle_command("A001 LOGIN user pass").await;

        let resp = session.handle_command("A002 FETCH 1 (FLAGS)").await;
        assert!(resp[0].contains("NO No mailbox selected"));
    }

    #[tokio::test]
    async fn test_close_resets_state() {
        let backend = Arc::new(MockBackend::with_mails(vec![make_mail(
            "m1", "Test", false,
        )]));
        let (_store, mut session) = make_session(backend).await;

        session.handle_command("A001 LOGIN user pass").await;
        session.handle_command("A002 SELECT INBOX").await;
        session.handle_command("A003 CLOSE").await;

        // Should reject FETCH after CLOSE
        let resp = session.handle_command("A004 FETCH 1 (FLAGS)").await;
        assert!(resp[0].contains("NO No mailbox selected"));
    }

    #[tokio::test]
    async fn test_expunge_sequence_numbers_shift() {
        let backend = Arc::new(MockBackend::with_mails(vec![
            make_mail("m1", "Mail 1", false),
            make_mail("m2", "Mail 2", false),
            make_mail("m3", "Mail 3", false),
            make_mail("m4", "Mail 4", false),
        ]));
        let (_store, mut session) = make_session(backend).await;

        session.handle_command("A001 LOGIN user pass").await;
        session.handle_command("A002 SELECT INBOX").await;

        // Delete mails 1 and 3
        session
            .handle_command("A003 STORE 1 +FLAGS (\\Deleted)")
            .await;
        session
            .handle_command("A004 STORE 3 +FLAGS (\\Deleted)")
            .await;

        let resp = session.handle_command("A005 EXPUNGE").await;

        // Mail 1 is expunged at seq 1, then mail 3 becomes seq 2
        let expunge_lines: Vec<_> = resp
            .iter()
            .filter(|r| r.starts_with("* ") && r.contains("EXPUNGE"))
            .collect();
        assert_eq!(expunge_lines.len(), 2);
        assert!(expunge_lines[0].contains("* 1 EXPUNGE"));
        assert!(expunge_lines[1].contains("* 2 EXPUNGE"));
    }

    #[tokio::test]
    async fn test_logout() {
        let backend = Arc::new(MockBackend::new());
        let (_store, mut session) = make_session(backend).await;

        let resp = session.handle_command("A001 LOGOUT").await;
        assert!(resp.iter().any(|r| r.contains("BYE")));
        assert!(session.is_logout());
    }

    #[tokio::test]
    async fn test_namespace() {
        let backend = Arc::new(MockBackend::new());
        let (_store, mut session) = make_session(backend).await;

        let resp = session.handle_command("A001 NAMESPACE").await;
        assert!(resp[0].contains("NAMESPACE"));
        assert!(resp[0].contains("((\"\" \"/\"))"));
    }

    #[tokio::test]
    async fn test_capability() {
        let backend = Arc::new(MockBackend::new());
        let (_store, mut session) = make_session(backend).await;

        let resp = session.handle_command("A001 CAPABILITY").await;
        assert!(resp[0].contains("IMAP4rev1"));
        assert!(resp[0].contains("IDLE"));
    }

    // --- check_new_mail: push diff to the IDLE'd client ---

    async fn session_on_inbox(mails: Vec<Mail>) -> (Arc<MailStore>, ImapSession) {
        let backend = Arc::new(MockBackend::with_mails(mails.clone()));
        let (store, mut session) = make_session(backend).await;
        populate_store(&store, &mails).await;
        session.handle_command("a LOGIN u p").await;
        session.handle_command("b SELECT INBOX").await;
        (store, session)
    }

    #[tokio::test]
    async fn check_new_mail_quiet_when_set_unchanged() {
        // Same mails, same order, same count → nothing to tell the client.
        let m1 = make_mail("e1", "one", false);
        let (_store, mut session) = session_on_inbox(vec![m1]).await;
        let resp = session.check_new_mail().await;
        assert!(resp.is_empty(), "expected no notifications, got {resp:?}");
    }

    #[tokio::test]
    async fn check_new_mail_pushes_exists_on_growth() {
        let m1 = make_mail("e1", "one", false);
        let (store, mut session) = session_on_inbox(vec![m1.clone()]).await;
        let m2 = make_mail("e2", "two", false);
        store
            .set_folder(
                "inbox",
                vec![
                    StoredMail {
                        mail: m1,
                        details: None,
                        rfc2822: None,
                        uid: 1,
                        attachments_pending: false,
                    },
                    StoredMail {
                        mail: m2,
                        details: None,
                        rfc2822: None,
                        uid: 2,
                        attachments_pending: false,
                    },
                ],
            )
            .await;
        let resp = session.check_new_mail().await;
        // No EXPUNGE — only an added mail. EXISTS reports the new total.
        assert!(!resp.iter().any(|r| r.contains("EXPUNGE")), "got {resp:?}");
        assert!(
            resp.iter().any(|r| r.contains("* 2 EXISTS")),
            "got {resp:?}"
        );
    }

    #[tokio::test]
    async fn check_new_mail_pushes_expunge_on_removal() {
        let m1 = make_mail("e1", "one", false);
        let m2 = make_mail("e2", "two", false);
        let (store, mut session) = session_on_inbox(vec![m1.clone(), m2.clone()]).await;
        // Drop m2 from the store — simulates a server-side delete arriving via
        // the event bus.
        store
            .set_folder(
                "inbox",
                vec![StoredMail {
                    mail: m1,
                    details: None,
                    rfc2822: None,
                    uid: 1,
                    attachments_pending: false,
                }],
            )
            .await;
        let resp = session.check_new_mail().await;
        // m2 was seqno 2 in the session view.
        assert!(
            resp.iter().any(|r| r.contains("* 2 EXPUNGE")),
            "got {resp:?}"
        );
        assert!(
            resp.iter().any(|r| r.contains("* 1 EXISTS")),
            "got {resp:?}"
        );
    }

    #[tokio::test]
    async fn check_new_mail_pushes_swap_when_count_unchanged() {
        // Regression test for the MOVE-between-folders bug: a fresh mail
        // arrived (event bus) and an old one fell out of the sync_limit
        // window. Total count stays the same, but the set changed — we must
        // still notify so the client refetches.
        let old1 = make_mail("e1", "one", false);
        let old2 = make_mail("e2", "two", false);
        let (store, mut session) = session_on_inbox(vec![old1.clone(), old2.clone()]).await;
        let new3 = make_mail("e3", "three", false);
        store
            .set_folder(
                "inbox",
                vec![
                    StoredMail {
                        mail: old1,
                        details: None,
                        rfc2822: None,
                        uid: 1,
                        attachments_pending: false,
                    },
                    StoredMail {
                        mail: new3,
                        details: None,
                        rfc2822: None,
                        uid: 3,
                        attachments_pending: false,
                    },
                ],
            )
            .await;
        let resp = session.check_new_mail().await;
        // e2 was at seqno 2 — must EXPUNGE it. Then EXISTS reports the
        // (unchanged) count so the client refetches and discovers e3.
        assert!(
            resp.iter().any(|r| r.contains("* 2 EXPUNGE")),
            "got {resp:?}"
        );
        assert!(
            resp.iter().any(|r| r.contains("* 2 EXISTS")),
            "got {resp:?}"
        );
    }

    #[tokio::test]
    async fn check_new_mail_expunges_in_descending_seqno() {
        // RFC 3501: when several mails are expunged, seqnos must arrive in
        // descending order so subsequent ones don't shift incorrectly.
        let m1 = make_mail("e1", "1", false);
        let m2 = make_mail("e2", "2", false);
        let m3 = make_mail("e3", "3", false);
        let (store, mut session) = session_on_inbox(vec![m1.clone(), m2, m3]).await;
        // Only m1 survives; m2 and m3 are gone.
        store
            .set_folder(
                "inbox",
                vec![StoredMail {
                    mail: m1,
                    details: None,
                    rfc2822: None,
                    uid: 1,
                    attachments_pending: false,
                }],
            )
            .await;
        let resp = session.check_new_mail().await;
        let expunge_idx_3 = resp
            .iter()
            .position(|r| r.contains("* 3 EXPUNGE"))
            .expect("missing 3");
        let expunge_idx_2 = resp
            .iter()
            .position(|r| r.contains("* 2 EXPUNGE"))
            .expect("missing 2");
        assert!(
            expunge_idx_3 < expunge_idx_2,
            "EXPUNGE 3 must come before EXPUNGE 2, got {resp:?}",
        );
    }
}
