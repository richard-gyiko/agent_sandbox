//! Digital twin service for Gmail API v1.
//!
//! Implements a simplified Gmail mailbox with messages, threads, labels, and
//! attachments.  Messages are stored as structured fields (not full MIME trees)
//! but V1 mimicry routes synthesise the Gmail `payload` structure so SDK
//! clients see familiar JSON shapes.

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, patch, post, put},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use twin_service::{
    AssertionResult, DiscoveryMeta, DiscoveryMethod, DiscoveryResource,
    ResolvedActorId, SharedTwinState, StateInspectable, StateNode,
    TimelineActionResult, TwinError, TwinService, TwinSnapshot, state_inspection_routes,
};

mod generated;
use generated::*;

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

type MessageId = String;
type ThreadId = String;
type LabelId = String;
type AttachmentId = String;
type GmailState = SharedTwinState<GmailTwinService>;

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GmailMessage {
    pub id: MessageId,
    pub thread_id: ThreadId,
    pub label_ids: Vec<LabelId>,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    pub snippet: String,
    pub internal_date: u64,
    pub size_estimate: u64,
    pub attachments: Vec<AttachmentRef>,
    pub history_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub attachment_id: AttachmentId,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GmailThread {
    pub id: ThreadId,
    pub history_id: u64,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GmailLabel {
    pub id: LabelId,
    pub name: String,
    pub label_type: LabelType,
    pub message_list_visibility: Visibility,
    pub label_list_visibility: LabelVisibility,
    pub color: Option<LabelColor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LabelType {
    System,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    Show,
    Hide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LabelVisibility {
    LabelShow,
    LabelShowIfUnread,
    LabelHide,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelColor {
    pub text_color: String,
    pub background_color: String,
}

/// Which fields to include in a message response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageFormat {
    Full,
    Metadata,
    Minimal,
}

impl MessageFormat {
    fn from_str(s: &str) -> Self {
        match s {
            "metadata" => Self::Metadata,
            "minimal" => Self::Minimal,
            "raw" => Self::Minimal, // raw not supported; degrade to minimal
            _ => Self::Full,
        }
    }
}

// ---------------------------------------------------------------------------
// Request / Response enums (decouple domain logic from HTTP)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum GmailRequest {
    // Messages
    ListMessages {
        actor_id: String,
        label_ids: Vec<String>,
        max_results: u32,
        page_token: Option<String>,
        q: Option<String>,
    },
    GetMessage {
        actor_id: String,
        message_id: String,
        format: MessageFormat,
    },
    SendMessage {
        actor_id: String,
        to: Vec<String>,
        cc: Vec<String>,
        bcc: Vec<String>,
        subject: String,
        body: String,
        thread_id: Option<String>,
        attachments: Vec<(String, String, Vec<u8>)>, // (filename, mime_type, data)
    },
    InsertMessage {
        actor_id: String,
        raw_message: SeedMessage,
    },
    ModifyMessage {
        actor_id: String,
        message_id: String,
        add_label_ids: Vec<String>,
        remove_label_ids: Vec<String>,
    },
    TrashMessage {
        actor_id: String,
        message_id: String,
    },
    UntrashMessage {
        actor_id: String,
        message_id: String,
    },
    DeleteMessage {
        actor_id: String,
        message_id: String,
    },

    // Threads
    ListThreads {
        actor_id: String,
        label_ids: Vec<String>,
        max_results: u32,
        page_token: Option<String>,
    },
    GetThread {
        actor_id: String,
        thread_id: String,
        format: MessageFormat,
    },
    ModifyThread {
        actor_id: String,
        thread_id: String,
        add_label_ids: Vec<String>,
        remove_label_ids: Vec<String>,
    },
    TrashThread {
        actor_id: String,
        thread_id: String,
    },
    UntrashThread {
        actor_id: String,
        thread_id: String,
    },
    DeleteThread {
        actor_id: String,
        thread_id: String,
    },

    // Labels
    ListLabels {
        actor_id: String,
    },
    GetLabel {
        actor_id: String,
        label_id: String,
    },
    CreateLabel {
        actor_id: String,
        name: String,
        message_list_visibility: Option<String>,
        label_list_visibility: Option<String>,
    },
    UpdateLabel {
        actor_id: String,
        label_id: String,
        name: Option<String>,
        message_list_visibility: Option<String>,
        label_list_visibility: Option<String>,
    },
    DeleteLabel {
        actor_id: String,
        label_id: String,
    },

    // Attachments
    GetAttachment {
        actor_id: String,
        message_id: String,
        attachment_id: String,
    },

    // Profile
    GetProfile {
        actor_id: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub enum GmailResponse {
    Message(GmailMessage),
    MessageList {
        messages: Vec<(MessageId, ThreadId)>,
        next_page_token: Option<String>,
        result_size_estimate: u32,
    },
    Thread {
        thread: GmailThread,
        messages: Vec<GmailMessage>,
    },
    ThreadList {
        threads: Vec<ThreadSummary>,
        next_page_token: Option<String>,
        result_size_estimate: u32,
    },
    Label(GmailLabel),
    LabelList(Vec<GmailLabel>),
    Attachment {
        data: Vec<u8>,
        size: u64,
    },
    Profile {
        email: String,
        messages_total: u64,
        threads_total: u64,
        history_id: u64,
    },
    Ok,
    Deleted,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadSummary {
    pub id: ThreadId,
    pub snippet: String,
    pub history_id: u64,
}

// ---------------------------------------------------------------------------
// Service struct
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, TwinSnapshot)]
pub struct GmailTwinService {
    messages: BTreeMap<MessageId, GmailMessage>,
    threads: BTreeMap<ThreadId, GmailThread>,
    labels: BTreeMap<LabelId, GmailLabel>,
    #[twin_snapshot(encode = "base64")]
    attachments: BTreeMap<AttachmentId, Vec<u8>>,
    next_id: u64,
    next_history_id: u64,
}

impl Default for GmailTwinService {
    fn default() -> Self {
        let mut labels = BTreeMap::new();
        let system_labels = [
            "INBOX",
            "SENT",
            "DRAFT",
            "TRASH",
            "SPAM",
            "UNREAD",
            "STARRED",
            "IMPORTANT",
            "CATEGORY_PERSONAL",
            "CATEGORY_SOCIAL",
            "CATEGORY_PROMOTIONS",
            "CATEGORY_UPDATES",
            "CATEGORY_FORUMS",
        ];
        for id in &system_labels {
            labels.insert(
                id.to_string(),
                GmailLabel {
                    id: id.to_string(),
                    name: id.to_string(),
                    label_type: LabelType::System,
                    message_list_visibility: Visibility::Show,
                    label_list_visibility: LabelVisibility::LabelShow,
                    color: None,
                },
            );
        }
        Self {
            messages: BTreeMap::new(),
            threads: BTreeMap::new(),
            labels,
            attachments: BTreeMap::new(),
            next_id: 1,
            next_history_id: 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

impl GmailTwinService {
    fn new_message_id(&mut self) -> MessageId {
        let id = format!("msg_{}", self.next_id);
        self.next_id += 1;
        id
    }

    fn new_thread_id(&mut self) -> ThreadId {
        let id = format!("thread_{}", self.next_id);
        self.next_id += 1;
        id
    }

    fn new_label_id(&mut self) -> LabelId {
        let id = format!("Label_{}", self.next_id);
        self.next_id += 1;
        id
    }

    fn new_attachment_id(&mut self) -> AttachmentId {
        let id = format!("att_{}", self.next_id);
        self.next_id += 1;
        id
    }

    fn next_history_id(&mut self) -> u64 {
        let id = self.next_history_id;
        self.next_history_id += 1;
        id
    }

    /// Ensure next_id is past all seeded IDs.
    fn bump_next_id(&mut self) {
        let extract_num = |s: &str| -> u64 {
            s.rsplit('_')
                .next()
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0)
        };

        let mut max_id: u64 = 0;
        for id in self.messages.keys() {
            max_id = max_id.max(extract_num(id));
        }
        for id in self.threads.keys() {
            max_id = max_id.max(extract_num(id));
        }
        for id in self.labels.keys() {
            max_id = max_id.max(extract_num(id));
        }
        for id in self.attachments.keys() {
            max_id = max_id.max(extract_num(id));
        }
        if max_id >= self.next_id {
            self.next_id = max_id + 1;
        }
    }

    fn make_snippet(body: &str) -> String {
        let truncated: String = body.chars().take(100).collect();
        if body.len() > 100 {
            format!("{truncated}...")
        } else {
            truncated
        }
    }

    fn estimate_size(msg: &GmailMessage) -> u64 {
        let mut size: u64 = 0;
        size += msg.from.len() as u64;
        for r in &msg.to {
            size += r.len() as u64;
        }
        size += msg.subject.len() as u64;
        if let Some(ref t) = msg.body_text {
            size += t.len() as u64;
        }
        if let Some(ref h) = msg.body_html {
            size += h.len() as u64;
        }
        for att in &msg.attachments {
            size += att.size;
        }
        size
    }

    /// Find or create a thread for a message.
    fn find_or_create_thread(
        &mut self,
        explicit_thread_id: Option<&str>,
        subject: &str,
    ) -> ThreadId {
        // 1. Explicit thread_id provided and exists
        if let Some(tid) = explicit_thread_id {
            if self.threads.contains_key(tid) {
                return tid.to_string();
            }
        }

        // 2. Match by subject (simplified threading)
        let normalized = subject
            .trim_start_matches("Re: ")
            .trim_start_matches("RE: ")
            .trim_start_matches("re: ")
            .trim_start_matches("Fwd: ")
            .trim_start_matches("FWD: ");
        for msg in self.messages.values() {
            let msg_norm = msg
                .subject
                .trim_start_matches("Re: ")
                .trim_start_matches("RE: ")
                .trim_start_matches("re: ")
                .trim_start_matches("Fwd: ")
                .trim_start_matches("FWD: ");
            if msg_norm == normalized && !normalized.is_empty() {
                return msg.thread_id.clone();
            }
        }

        // 3. Create a new thread
        let tid = self.new_thread_id();
        let hid = self.next_history_id();
        self.threads.insert(
            tid.clone(),
            GmailThread {
                id: tid.clone(),
                history_id: hid,
                snippet: String::new(),
            },
        );
        tid
    }

    /// Update thread snippet from its most recent message.
    fn update_thread_snippet(&mut self, thread_id: &str) {
        let latest = self
            .messages
            .values()
            .filter(|m| m.thread_id == thread_id)
            .max_by_key(|m| m.internal_date);
        if let Some(msg) = latest {
            let snippet = msg.snippet.clone();
            let hid = msg.history_id;
            if let Some(thread) = self.threads.get_mut(thread_id) {
                thread.snippet = snippet;
                thread.history_id = hid;
            }
        }
    }

    /// Count messages with a given label.
    fn label_message_count(&self, label_id: &str) -> (u64, u64) {
        let mut total = 0u64;
        let mut unread = 0u64;
        for msg in self.messages.values() {
            if msg.label_ids.iter().any(|l| l == label_id) {
                total += 1;
                if msg.label_ids.iter().any(|l| l == "UNREAD") {
                    unread += 1;
                }
            }
        }
        (total, unread)
    }

    /// Count unique threads that have at least one message with a given label.
    fn label_thread_count(&self, label_id: &str) -> (u64, u64) {
        let mut thread_ids = std::collections::BTreeSet::new();
        let mut unread_thread_ids = std::collections::BTreeSet::new();
        for msg in self.messages.values() {
            if msg.label_ids.iter().any(|l| l == label_id) {
                thread_ids.insert(&msg.thread_id);
                if msg.label_ids.iter().any(|l| l == "UNREAD") {
                    unread_thread_ids.insert(&msg.thread_id);
                }
            }
        }
        (thread_ids.len() as u64, unread_thread_ids.len() as u64)
    }

    fn current_timestamp_ms() -> u64 {
        // For deterministic testing, use a fixed "now" offset from next_id.
        // In a real server we'd use std::time, but for a twin we want reproducibility.
        1_700_000_000_000 // fixed epoch for determinism
    }
}

// ---------------------------------------------------------------------------
// Gmail query filter (supports the `q` parameter on messages.list)
// ---------------------------------------------------------------------------

/// Parsed representation of a Gmail `q` search query.
#[derive(Debug, Default)]
struct GmailQueryFilter {
    /// Labels that must be present (from `in:sent`, `in:inbox`, etc.)
    include_labels: Vec<String>,
    /// Labels that must NOT be present (from `-label:NAME`)
    exclude_labels: Vec<String>,
    /// Only messages on or after this timestamp (ms), from `after:YYYY/MM/DD`
    after_ms: Option<u64>,
    /// Only messages strictly before this timestamp (ms), from `before:YYYY/MM/DD`
    before_ms: Option<u64>,
    /// Sender must contain this substring (from `from:ADDRESS`)
    from: Option<String>,
    /// Subject must contain this substring (from `subject:TEXT`)
    subject: Option<String>,
}

/// Parse a Gmail-style `q` query string into a structured filter.
///
/// Supported tokens:
///   `in:sent`, `in:inbox`, `in:trash`        — label inclusion
///   `-label:LABEL_NAME`                      — label exclusion
///   `after:YYYY/MM/DD`                       — messages on or after date
///   `before:YYYY/MM/DD`                      — messages strictly before date
///   `from:ADDRESS`                           — sender substring match
///   `subject:TEXT`                            — subject substring match
///
/// Unknown tokens are silently ignored.
fn parse_gmail_query(q: &str) -> GmailQueryFilter {
    let mut filter = GmailQueryFilter::default();
    let mut chars = q.chars().peekable();
    let mut tokens: Vec<String> = Vec::new();

    // Tokenise: split on whitespace but keep `-label:X` as one token
    while chars.peek().is_some() {
        // skip whitespace
        while chars.peek().map_or(false, |c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut token = String::new();
        while chars.peek().map_or(false, |c| !c.is_whitespace()) {
            token.push(chars.next().unwrap());
        }
        if !token.is_empty() {
            tokens.push(token);
        }
    }

    for token in &tokens {
        if let Some(mailbox) = token.strip_prefix("in:") {
            let label = match mailbox.to_uppercase().as_str() {
                "SENT" => "SENT".to_string(),
                "INBOX" => "INBOX".to_string(),
                "TRASH" => "TRASH".to_string(),
                "DRAFT" | "DRAFTS" => "DRAFT".to_string(),
                "SPAM" => "SPAM".to_string(),
                "STARRED" => "STARRED".to_string(),
                "UNREAD" => "UNREAD".to_string(),
                other => other.to_uppercase(),
            };
            filter.include_labels.push(label);
        } else if let Some(label_name) = token.strip_prefix("-label:") {
            filter.exclude_labels.push(label_name.to_string());
        } else if let Some(date_str) = token.strip_prefix("after:") {
            if let Some(ms) = parse_date_to_ms(date_str) {
                filter.after_ms = Some(ms);
            }
        } else if let Some(date_str) = token.strip_prefix("before:") {
            if let Some(ms) = parse_date_to_ms(date_str) {
                filter.before_ms = Some(ms);
            }
        } else if let Some(addr) = token.strip_prefix("from:") {
            filter.from = Some(addr.to_lowercase());
        } else if let Some(subj) = token.strip_prefix("subject:") {
            filter.subject = Some(subj.to_lowercase());
        }
        // Unknown tokens are silently ignored
    }

    filter
}

/// Parse a date string like `YYYY/MM/DD` or `YYYY-MM-DD` into epoch milliseconds (UTC midnight).
fn parse_date_to_ms(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split(|c| c == '/' || c == '-').collect();
    if parts.len() != 3 {
        return None;
    }
    let year: i64 = parts[0].parse().ok()?;
    let month: i64 = parts[1].parse().ok()?;
    let day: i64 = parts[2].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || year < 1970 {
        return None;
    }
    // Simplified date-to-epoch: compute days since 1970-01-01 then multiply by 86400 * 1000.
    // Uses the algorithm from https://howardhinnant.github.io/date_algorithms.html
    let m = month;
    let y = if m <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let doy = ((153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1) as u64;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as u64 * 146097 + doe - 719468;
    // Wrapping note: this will be correct for dates from 1970 onwards
    Some(days * 86_400_000)
}

/// Check whether a message matches all criteria in a GmailQueryFilter.
fn message_matches_query(msg: &GmailMessage, filter: &GmailQueryFilter) -> bool {
    // include_labels: message must have ALL specified labels
    for label in &filter.include_labels {
        if !msg.label_ids.iter().any(|l| l.eq_ignore_ascii_case(label)) {
            return false;
        }
    }
    // exclude_labels: message must NOT have any of these labels
    for label in &filter.exclude_labels {
        if msg.label_ids.iter().any(|l| l.eq_ignore_ascii_case(label)) {
            return false;
        }
    }
    // after: internal_date >= after_ms
    if let Some(after) = filter.after_ms {
        if msg.internal_date < after {
            return false;
        }
    }
    // before: internal_date < before_ms
    if let Some(before) = filter.before_ms {
        if msg.internal_date >= before {
            return false;
        }
    }
    // from: case-insensitive substring match on sender
    if let Some(ref from) = filter.from {
        if !msg.from.to_lowercase().contains(from) {
            return false;
        }
    }
    // subject: case-insensitive substring match on subject
    if let Some(ref subject) = filter.subject {
        if !msg.subject.to_lowercase().contains(subject) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Core domain logic
// ---------------------------------------------------------------------------

impl GmailTwinService {
    pub fn handle(&mut self, request: GmailRequest) -> Result<GmailResponse, TwinError> {
        match request {
            // ----- Messages -----
            GmailRequest::ListMessages {
                actor_id: _,
                label_ids,
                max_results,
                page_token,
                q,
            } => {
                // Parse the q parameter into a structured filter
                let query_filter = q.as_deref().map(parse_gmail_query);

                let mut msgs: Vec<&GmailMessage> = if label_ids.is_empty() {
                    self.messages.values().collect()
                } else {
                    self.messages
                        .values()
                        .filter(|m| {
                            label_ids.iter().all(|l| m.label_ids.contains(l))
                        })
                        .collect()
                };

                // Apply q-based filtering if present
                if let Some(ref filter) = query_filter {
                    msgs.retain(|m| message_matches_query(m, filter));
                }
                // Sort by internal_date descending (newest first)
                msgs.sort_by(|a, b| b.internal_date.cmp(&a.internal_date));

                let total = msgs.len() as u32;
                let offset = page_token
                    .as_deref()
                    .and_then(|t| t.strip_prefix("offset:"))
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or(0);
                let limit = max_results.min(500).max(1) as usize;
                let page: Vec<(MessageId, ThreadId)> = msgs
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|m| (m.id.clone(), m.thread_id.clone()))
                    .collect();
                let next_offset = offset + page.len();
                let next_page_token = if next_offset < msgs.len() {
                    Some(format!("offset:{next_offset}"))
                } else {
                    None
                };
                Ok(GmailResponse::MessageList {
                    messages: page,
                    next_page_token,
                    result_size_estimate: total,
                })
            }

            GmailRequest::GetMessage {
                actor_id: _,
                message_id,
                format: _,
            } => {
                let msg = self
                    .messages
                    .get(&message_id)
                    .cloned()
                    .ok_or_else(|| TwinError::Operation(format!("message not found: {message_id}")))?;
                Ok(GmailResponse::Message(msg))
            }

            GmailRequest::SendMessage {
                actor_id,
                to,
                cc,
                bcc,
                subject,
                body,
                thread_id,
                attachments,
            } => {
                let tid = self.find_or_create_thread(thread_id.as_deref(), &subject);
                let mid = self.new_message_id();
                let hid = self.next_history_id();
                let snippet = Self::make_snippet(&body);

                let mut att_refs = Vec::new();
                for (filename, mime_type, data) in attachments {
                    let att_id = self.new_attachment_id();
                    let size = data.len() as u64;
                    self.attachments.insert(att_id.clone(), data);
                    att_refs.push(AttachmentRef {
                        attachment_id: att_id,
                        filename,
                        mime_type,
                        size,
                    });
                }

                let msg = GmailMessage {
                    id: mid.clone(),
                    thread_id: tid.clone(),
                    label_ids: vec!["SENT".to_string()],
                    from: format!("{actor_id}@twin.local"),
                    to,
                    cc,
                    bcc,
                    subject,
                    body_text: Some(body),
                    body_html: None,
                    snippet,
                    internal_date: Self::current_timestamp_ms() + hid,
                    size_estimate: 0,
                    attachments: att_refs,
                    history_id: hid,
                };
                let size = Self::estimate_size(&msg);
                let mut msg = msg;
                msg.size_estimate = size;

                self.messages.insert(mid, msg.clone());
                self.update_thread_snippet(&tid);
                Ok(GmailResponse::Message(msg))
            }

            GmailRequest::InsertMessage {
                actor_id: _,
                raw_message,
            } => {
                let mid = if raw_message.id.is_empty() {
                    self.new_message_id()
                } else {
                    raw_message.id.clone()
                };
                let tid = self.find_or_create_thread(
                    if raw_message.thread_id.is_empty() {
                        None
                    } else {
                        Some(&raw_message.thread_id)
                    },
                    &raw_message.subject,
                );
                let hid = self.next_history_id();
                let snippet = Self::make_snippet(
                    raw_message.body.as_deref().unwrap_or(""),
                );
                let label_ids = if raw_message.label_ids.is_empty() {
                    vec!["INBOX".to_string(), "UNREAD".to_string()]
                } else {
                    raw_message.label_ids.clone()
                };

                let mut att_refs = Vec::new();
                for seed_att in &raw_message.attachments {
                    let att_id = seed_att
                        .attachment_id
                        .clone()
                        .unwrap_or_else(|| self.new_attachment_id());
                    let data = BASE64.decode(&seed_att.content).map_err(|e| {
                        TwinError::Operation(format!(
                            "failed to decode attachment base64 for {}: {e}",
                            seed_att.filename
                        ))
                    })?;
                    let size = data.len() as u64;
                    self.attachments.insert(att_id.clone(), data);
                    att_refs.push(AttachmentRef {
                        attachment_id: att_id,
                        filename: seed_att.filename.clone(),
                        mime_type: seed_att.mime_type.clone(),
                        size,
                    });
                }

                let msg = GmailMessage {
                    id: mid.clone(),
                    thread_id: tid.clone(),
                    label_ids,
                    from: raw_message.from,
                    to: raw_message.to,
                    cc: raw_message.cc.unwrap_or_default(),
                    bcc: raw_message.bcc.unwrap_or_default(),
                    subject: raw_message.subject,
                    body_text: raw_message.body.clone(),
                    body_html: raw_message.body_html,
                    snippet,
                    internal_date: raw_message
                        .timestamp_ms
                        .unwrap_or(Self::current_timestamp_ms() + hid),
                    size_estimate: 0,
                    attachments: att_refs,
                    history_id: hid,
                };
                let size = Self::estimate_size(&msg);
                let mut msg = msg;
                msg.size_estimate = size;

                self.messages.insert(mid, msg.clone());
                self.update_thread_snippet(&tid);
                Ok(GmailResponse::Message(msg))
            }

            GmailRequest::ModifyMessage {
                actor_id: _,
                message_id,
                add_label_ids,
                remove_label_ids,
            } => {
                let msg = self.messages.get_mut(&message_id).ok_or_else(|| {
                    TwinError::Operation(format!("message not found: {message_id}"))
                })?;
                for lid in &remove_label_ids {
                    msg.label_ids.retain(|l| l != lid);
                }
                for lid in &add_label_ids {
                    if !msg.label_ids.contains(lid) {
                        msg.label_ids.push(lid.clone());
                    }
                }
                msg.history_id = self.next_history_id;
                self.next_history_id += 1;
                let msg = msg.clone();
                Ok(GmailResponse::Message(msg))
            }

            GmailRequest::TrashMessage {
                actor_id: _,
                message_id,
            } => {
                let msg = self.messages.get_mut(&message_id).ok_or_else(|| {
                    TwinError::Operation(format!("message not found: {message_id}"))
                })?;
                if !msg.label_ids.contains(&"TRASH".to_string()) {
                    msg.label_ids.push("TRASH".to_string());
                }
                msg.label_ids.retain(|l| l != "INBOX");
                msg.history_id = self.next_history_id;
                self.next_history_id += 1;
                let msg = msg.clone();
                Ok(GmailResponse::Message(msg))
            }

            GmailRequest::UntrashMessage {
                actor_id: _,
                message_id,
            } => {
                let msg = self.messages.get_mut(&message_id).ok_or_else(|| {
                    TwinError::Operation(format!("message not found: {message_id}"))
                })?;
                msg.label_ids.retain(|l| l != "TRASH");
                if !msg.label_ids.contains(&"INBOX".to_string()) {
                    msg.label_ids.push("INBOX".to_string());
                }
                msg.history_id = self.next_history_id;
                self.next_history_id += 1;
                let msg = msg.clone();
                Ok(GmailResponse::Message(msg))
            }

            GmailRequest::DeleteMessage {
                actor_id: _,
                message_id,
            } => {
                if !self.messages.contains_key(&message_id) {
                    return Err(TwinError::Operation(format!(
                        "message not found: {message_id}"
                    )));
                }
                let msg = self.messages.remove(&message_id).unwrap();
                // Remove attachments
                for att in &msg.attachments {
                    self.attachments.remove(&att.attachment_id);
                }
                // Clean up thread if empty
                let thread_id = msg.thread_id.clone();
                let thread_has_msgs = self
                    .messages
                    .values()
                    .any(|m| m.thread_id == thread_id);
                if !thread_has_msgs {
                    self.threads.remove(&thread_id);
                } else {
                    self.update_thread_snippet(&thread_id);
                }
                Ok(GmailResponse::Deleted)
            }

            // ----- Threads -----
            GmailRequest::ListThreads {
                actor_id: _,
                label_ids,
                max_results,
                page_token,
            } => {
                // Collect threads that have at least one message matching all label_ids
                let mut matching_thread_ids: Vec<&ThreadId> = if label_ids.is_empty() {
                    self.threads.keys().collect()
                } else {
                    let mut tids = std::collections::BTreeSet::new();
                    for msg in self.messages.values() {
                        if label_ids.iter().all(|l| msg.label_ids.contains(l)) {
                            tids.insert(&msg.thread_id);
                        }
                    }
                    tids.into_iter().collect()
                };
                // Sort by thread history_id descending
                matching_thread_ids.sort_by(|a, b| {
                    let ha = self.threads.get(*a).map(|t| t.history_id).unwrap_or(0);
                    let hb = self.threads.get(*b).map(|t| t.history_id).unwrap_or(0);
                    hb.cmp(&ha)
                });

                let total = matching_thread_ids.len() as u32;
                let offset = page_token
                    .as_deref()
                    .and_then(|t| t.strip_prefix("offset:"))
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or(0);
                let limit = max_results.min(500).max(1) as usize;
                let page: Vec<ThreadSummary> = matching_thread_ids
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .filter_map(|tid| {
                        self.threads.get(*tid).map(|t| ThreadSummary {
                            id: t.id.clone(),
                            snippet: t.snippet.clone(),
                            history_id: t.history_id,
                        })
                    })
                    .collect();
                let next_offset = offset + page.len();
                let next_page_token = if next_offset < matching_thread_ids.len() {
                    Some(format!("offset:{next_offset}"))
                } else {
                    None
                };
                Ok(GmailResponse::ThreadList {
                    threads: page,
                    next_page_token,
                    result_size_estimate: total,
                })
            }

            GmailRequest::GetThread {
                actor_id: _,
                thread_id,
                format: _,
            } => {
                let thread = self
                    .threads
                    .get(&thread_id)
                    .cloned()
                    .ok_or_else(|| TwinError::Operation(format!("thread not found: {thread_id}")))?;
                let mut msgs: Vec<GmailMessage> = self
                    .messages
                    .values()
                    .filter(|m| m.thread_id == thread_id)
                    .cloned()
                    .collect();
                msgs.sort_by_key(|m| m.internal_date);
                Ok(GmailResponse::Thread {
                    thread,
                    messages: msgs,
                })
            }

            GmailRequest::ModifyThread {
                actor_id: _,
                thread_id,
                add_label_ids,
                remove_label_ids,
            } => {
                if !self.threads.contains_key(&thread_id) {
                    return Err(TwinError::Operation(format!(
                        "thread not found: {thread_id}"
                    )));
                }
                let hid = self.next_history_id();
                for msg in self.messages.values_mut() {
                    if msg.thread_id == thread_id {
                        for lid in &remove_label_ids {
                            msg.label_ids.retain(|l| l != lid);
                        }
                        for lid in &add_label_ids {
                            if !msg.label_ids.contains(lid) {
                                msg.label_ids.push(lid.clone());
                            }
                        }
                        msg.history_id = hid;
                    }
                }
                if let Some(thread) = self.threads.get_mut(&thread_id) {
                    thread.history_id = hid;
                }
                let thread = self.threads.get(&thread_id).unwrap().clone();
                let msgs: Vec<GmailMessage> = self
                    .messages
                    .values()
                    .filter(|m| m.thread_id == thread_id)
                    .cloned()
                    .collect();
                Ok(GmailResponse::Thread {
                    thread,
                    messages: msgs,
                })
            }

            GmailRequest::TrashThread {
                actor_id: _,
                thread_id,
            } => {
                if !self.threads.contains_key(&thread_id) {
                    return Err(TwinError::Operation(format!(
                        "thread not found: {thread_id}"
                    )));
                }
                let hid = self.next_history_id();
                for msg in self.messages.values_mut() {
                    if msg.thread_id == thread_id {
                        if !msg.label_ids.contains(&"TRASH".to_string()) {
                            msg.label_ids.push("TRASH".to_string());
                        }
                        msg.label_ids.retain(|l| l != "INBOX");
                        msg.history_id = hid;
                    }
                }
                if let Some(thread) = self.threads.get_mut(&thread_id) {
                    thread.history_id = hid;
                }
                let thread = self.threads.get(&thread_id).unwrap().clone();
                let msgs: Vec<GmailMessage> = self
                    .messages
                    .values()
                    .filter(|m| m.thread_id == thread_id)
                    .cloned()
                    .collect();
                Ok(GmailResponse::Thread {
                    thread,
                    messages: msgs,
                })
            }

            GmailRequest::UntrashThread {
                actor_id: _,
                thread_id,
            } => {
                if !self.threads.contains_key(&thread_id) {
                    return Err(TwinError::Operation(format!(
                        "thread not found: {thread_id}"
                    )));
                }
                let hid = self.next_history_id();
                for msg in self.messages.values_mut() {
                    if msg.thread_id == thread_id {
                        msg.label_ids.retain(|l| l != "TRASH");
                        if !msg.label_ids.contains(&"INBOX".to_string()) {
                            msg.label_ids.push("INBOX".to_string());
                        }
                        msg.history_id = hid;
                    }
                }
                if let Some(thread) = self.threads.get_mut(&thread_id) {
                    thread.history_id = hid;
                }
                let thread = self.threads.get(&thread_id).unwrap().clone();
                let msgs: Vec<GmailMessage> = self
                    .messages
                    .values()
                    .filter(|m| m.thread_id == thread_id)
                    .cloned()
                    .collect();
                Ok(GmailResponse::Thread {
                    thread,
                    messages: msgs,
                })
            }

            GmailRequest::DeleteThread {
                actor_id: _,
                thread_id,
            } => {
                if !self.threads.contains_key(&thread_id) {
                    return Err(TwinError::Operation(format!(
                        "thread not found: {thread_id}"
                    )));
                }
                // Remove all messages in the thread
                let msg_ids: Vec<MessageId> = self
                    .messages
                    .values()
                    .filter(|m| m.thread_id == thread_id)
                    .map(|m| m.id.clone())
                    .collect();
                for mid in msg_ids {
                    if let Some(msg) = self.messages.remove(&mid) {
                        for att in &msg.attachments {
                            self.attachments.remove(&att.attachment_id);
                        }
                    }
                }
                self.threads.remove(&thread_id);
                Ok(GmailResponse::Deleted)
            }

            // ----- Labels -----
            GmailRequest::ListLabels { actor_id: _ } => {
                let labels: Vec<GmailLabel> = self.labels.values().cloned().collect();
                Ok(GmailResponse::LabelList(labels))
            }

            GmailRequest::GetLabel {
                actor_id: _,
                label_id,
            } => {
                let label = self
                    .labels
                    .get(&label_id)
                    .cloned()
                    .ok_or_else(|| TwinError::Operation(format!("label not found: {label_id}")))?;
                Ok(GmailResponse::Label(label))
            }

            GmailRequest::CreateLabel {
                actor_id: _,
                name,
                message_list_visibility,
                label_list_visibility,
            } => {
                // Check for duplicate name
                if self.labels.values().any(|l| l.name == name) {
                    return Err(TwinError::Operation(format!(
                        "label already exists: {name}"
                    )));
                }
                let id = self.new_label_id();
                let label = GmailLabel {
                    id: id.clone(),
                    name,
                    label_type: LabelType::User,
                    message_list_visibility: match message_list_visibility.as_deref() {
                        Some("hide") => Visibility::Hide,
                        _ => Visibility::Show,
                    },
                    label_list_visibility: match label_list_visibility.as_deref() {
                        Some("labelHide") => LabelVisibility::LabelHide,
                        Some("labelShowIfUnread") => LabelVisibility::LabelShowIfUnread,
                        _ => LabelVisibility::LabelShow,
                    },
                    color: None,
                };
                self.labels.insert(id, label.clone());
                Ok(GmailResponse::Label(label))
            }

            GmailRequest::UpdateLabel {
                actor_id: _,
                label_id,
                name,
                message_list_visibility,
                label_list_visibility,
            } => {
                let label = self.labels.get_mut(&label_id).ok_or_else(|| {
                    TwinError::Operation(format!("label not found: {label_id}"))
                })?;
                if label.label_type == LabelType::System {
                    return Err(TwinError::Operation(
                        "cannot modify system label".to_string(),
                    ));
                }
                if let Some(n) = name {
                    label.name = n;
                }
                if let Some(v) = message_list_visibility {
                    label.message_list_visibility = match v.as_str() {
                        "hide" => Visibility::Hide,
                        _ => Visibility::Show,
                    };
                }
                if let Some(v) = label_list_visibility {
                    label.label_list_visibility = match v.as_str() {
                        "labelHide" => LabelVisibility::LabelHide,
                        "labelShowIfUnread" => LabelVisibility::LabelShowIfUnread,
                        _ => LabelVisibility::LabelShow,
                    };
                }
                let label = label.clone();
                Ok(GmailResponse::Label(label))
            }

            GmailRequest::DeleteLabel {
                actor_id: _,
                label_id,
            } => {
                let label = self.labels.get(&label_id).ok_or_else(|| {
                    TwinError::Operation(format!("label not found: {label_id}"))
                })?;
                if label.label_type == LabelType::System {
                    return Err(TwinError::Operation(
                        "cannot delete system label".to_string(),
                    ));
                }
                self.labels.remove(&label_id);
                // Remove label from all messages
                for msg in self.messages.values_mut() {
                    msg.label_ids.retain(|l| l != &label_id);
                }
                Ok(GmailResponse::Deleted)
            }

            // ----- Attachments -----
            GmailRequest::GetAttachment {
                actor_id: _,
                message_id,
                attachment_id,
            } => {
                let msg = self
                    .messages
                    .get(&message_id)
                    .ok_or_else(|| TwinError::Operation(format!("message not found: {message_id}")))?;
                let att_ref = msg
                    .attachments
                    .iter()
                    .find(|a| a.attachment_id == attachment_id)
                    .ok_or_else(|| {
                        TwinError::Operation(format!("attachment not found: {attachment_id}"))
                    })?;
                let data = self
                    .attachments
                    .get(&attachment_id)
                    .ok_or_else(|| {
                        TwinError::Operation(format!("attachment data not found: {attachment_id}"))
                    })?
                    .clone();
                Ok(GmailResponse::Attachment {
                    size: att_ref.size,
                    data,
                })
            }

            // ----- Profile -----
            GmailRequest::GetProfile { actor_id } => {
                Ok(GmailResponse::Profile {
                    email: format!("{actor_id}@twin.local"),
                    messages_total: self.messages.len() as u64,
                    threads_total: self.threads.len() as u64,
                    history_id: self.next_history_id,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// State inspection
// ---------------------------------------------------------------------------

impl StateInspectable for GmailTwinService {
    fn inspect_state(&self) -> Vec<StateNode> {
        let mut nodes = Vec::new();

        // Threads as top-level nodes
        for thread in self.threads.values() {
            let (msg_count, _) = {
                let count = self
                    .messages
                    .values()
                    .filter(|m| m.thread_id == thread.id)
                    .count();
                (count, 0)
            };
            let mut props = BTreeMap::new();
            props.insert(
                "message_count".to_string(),
                serde_json::Value::Number(serde_json::Number::from(msg_count as u64)),
            );
            props.insert(
                "snippet".to_string(),
                serde_json::Value::String(thread.snippet.clone()),
            );
            nodes.push(StateNode {
                id: thread.id.clone(),
                label: thread.snippet.chars().take(50).collect::<String>(),
                kind: "thread".to_string(),
                parent_id: None,
                properties: props,
            });
        }

        // Messages as children of threads
        for msg in self.messages.values() {
            let mut props = BTreeMap::new();
            props.insert(
                "from".to_string(),
                serde_json::Value::String(msg.from.clone()),
            );
            props.insert(
                "to".to_string(),
                serde_json::json!(msg.to),
            );
            props.insert(
                "subject".to_string(),
                serde_json::Value::String(msg.subject.clone()),
            );
            props.insert(
                "label_ids".to_string(),
                serde_json::json!(msg.label_ids),
            );
            props.insert(
                "has_attachments".to_string(),
                serde_json::Value::Bool(!msg.attachments.is_empty()),
            );
            nodes.push(StateNode {
                id: msg.id.clone(),
                label: msg.subject.clone(),
                kind: "message".to_string(),
                parent_id: Some(msg.thread_id.clone()),
                properties: props,
            });
        }

        // Labels as top-level nodes
        for label in self.labels.values() {
            let (msgs_total, msgs_unread) = self.label_message_count(&label.id);
            let mut props = BTreeMap::new();
            props.insert(
                "type".to_string(),
                serde_json::Value::String(match label.label_type {
                    LabelType::System => "system".to_string(),
                    LabelType::User => "user".to_string(),
                }),
            );
            props.insert(
                "messages_total".to_string(),
                serde_json::Value::Number(serde_json::Number::from(msgs_total)),
            );
            props.insert(
                "messages_unread".to_string(),
                serde_json::Value::Number(serde_json::Number::from(msgs_unread)),
            );
            nodes.push(StateNode {
                id: label.id.clone(),
                label: label.name.clone(),
                kind: "label".to_string(),
                parent_id: None,
                properties: props,
            });
        }

        nodes
    }

    fn inspect_node(&self, id: &str) -> Option<StateNode> {
        // Check threads
        if let Some(thread) = self.threads.get(id) {
            let msg_count = self
                .messages
                .values()
                .filter(|m| m.thread_id == thread.id)
                .count();
            let mut props = BTreeMap::new();
            props.insert(
                "message_count".to_string(),
                serde_json::Value::Number(serde_json::Number::from(msg_count as u64)),
            );
            props.insert(
                "snippet".to_string(),
                serde_json::Value::String(thread.snippet.clone()),
            );
            return Some(StateNode {
                id: thread.id.clone(),
                label: thread.snippet.chars().take(50).collect::<String>(),
                kind: "thread".to_string(),
                parent_id: None,
                properties: props,
            });
        }

        // Check messages
        if let Some(msg) = self.messages.get(id) {
            let mut props = BTreeMap::new();
            props.insert("from".to_string(), serde_json::Value::String(msg.from.clone()));
            props.insert("to".to_string(), serde_json::json!(msg.to));
            props.insert("subject".to_string(), serde_json::Value::String(msg.subject.clone()));
            props.insert("label_ids".to_string(), serde_json::json!(msg.label_ids));
            props.insert(
                "has_attachments".to_string(),
                serde_json::Value::Bool(!msg.attachments.is_empty()),
            );
            return Some(StateNode {
                id: msg.id.clone(),
                label: msg.subject.clone(),
                kind: "message".to_string(),
                parent_id: Some(msg.thread_id.clone()),
                properties: props,
            });
        }

        // Check labels
        if let Some(label) = self.labels.get(id) {
            let (msgs_total, msgs_unread) = self.label_message_count(&label.id);
            let mut props = BTreeMap::new();
            props.insert(
                "type".to_string(),
                serde_json::Value::String(match label.label_type {
                    LabelType::System => "system".to_string(),
                    LabelType::User => "user".to_string(),
                }),
            );
            props.insert(
                "messages_total".to_string(),
                serde_json::Value::Number(serde_json::Number::from(msgs_total)),
            );
            props.insert(
                "messages_unread".to_string(),
                serde_json::Value::Number(serde_json::Number::from(msgs_unread)),
            );
            return Some(StateNode {
                id: label.id.clone(),
                label: label.name.clone(),
                kind: "label".to_string(),
                parent_id: None,
                properties: props,
            });
        }

        None
    }
}

// ---------------------------------------------------------------------------
// V1 helpers
// ---------------------------------------------------------------------------

/// Use base64url (no padding) for Gmail API data fields, matching the real API.
fn base64url_encode(data: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    URL_SAFE_NO_PAD.encode(data)
}

fn gmail_message_to_v1(msg: &GmailMessage, format: MessageFormat) -> V1Message {
    let payload = match format {
        MessageFormat::Minimal => None,
        _ => {
            let mut headers = vec![
                V1Header {
                    name: "From".to_string(),
                    value: msg.from.clone(),
                },
                V1Header {
                    name: "To".to_string(),
                    value: msg.to.join(", "),
                },
                V1Header {
                    name: "Subject".to_string(),
                    value: msg.subject.clone(),
                },
            ];
            if !msg.cc.is_empty() {
                headers.push(V1Header {
                    name: "Cc".to_string(),
                    value: msg.cc.join(", "),
                });
            }
            if !msg.bcc.is_empty() {
                headers.push(V1Header {
                    name: "Bcc".to_string(),
                    value: msg.bcc.join(", "),
                });
            }

            let include_body = format == MessageFormat::Full;
            let mut parts = Vec::new();
            let mut part_idx = 0u32;

            if let Some(ref text) = msg.body_text {
                parts.push(V1Part {
                    part_id: part_idx.to_string(),
                    mime_type: "text/plain".to_string(),
                    filename: None,
                    headers: vec![V1Header {
                        name: "Content-Type".to_string(),
                        value: "text/plain; charset=\"UTF-8\"".to_string(),
                    }],
                    body: V1Body {
                        size: text.len() as u64,
                        data: if include_body {
                            Some(base64url_encode(text.as_bytes()))
                        } else {
                            None
                        },
                        attachment_id: None,
                    },
                });
                part_idx += 1;
            }

            if let Some(ref html) = msg.body_html {
                parts.push(V1Part {
                    part_id: part_idx.to_string(),
                    mime_type: "text/html".to_string(),
                    filename: None,
                    headers: vec![V1Header {
                        name: "Content-Type".to_string(),
                        value: "text/html; charset=\"UTF-8\"".to_string(),
                    }],
                    body: V1Body {
                        size: html.len() as u64,
                        data: if include_body {
                            Some(base64url_encode(html.as_bytes()))
                        } else {
                            None
                        },
                        attachment_id: None,
                    },
                });
                part_idx += 1;
            }

            for att in &msg.attachments {
                parts.push(V1Part {
                    part_id: part_idx.to_string(),
                    mime_type: att.mime_type.clone(),
                    filename: Some(att.filename.clone()),
                    headers: vec![V1Header {
                        name: "Content-Type".to_string(),
                        value: att.mime_type.clone(),
                    }],
                    body: V1Body {
                        size: att.size,
                        data: None, // attachments use attachmentId
                        attachment_id: Some(att.attachment_id.clone()),
                    },
                });
                part_idx += 1;
            }
            let _ = part_idx; // suppress unused warning

            let top_mime = if msg.attachments.is_empty() {
                if msg.body_html.is_some() && msg.body_text.is_some() {
                    "multipart/alternative"
                } else if msg.body_text.is_some() {
                    "text/plain"
                } else if msg.body_html.is_some() {
                    "text/html"
                } else {
                    "text/plain"
                }
            } else {
                "multipart/mixed"
            };

            Some(V1Payload {
                mime_type: top_mime.to_string(),
                headers,
                body: V1Body {
                    size: 0,
                    data: None,
                    attachment_id: None,
                },
                parts,
            })
        }
    };

    V1Message {
        id: msg.id.clone(),
        thread_id: msg.thread_id.clone(),
        label_ids: msg.label_ids.clone(),
        snippet: msg.snippet.clone(),
        history_id: msg.history_id.to_string(),
        internal_date: msg.internal_date.to_string(),
        size_estimate: msg.size_estimate,
        payload,
    }
}

fn gmail_label_to_v1(label: &GmailLabel, service: &GmailTwinService) -> V1Label {
    let (msgs_total, msgs_unread) = service.label_message_count(&label.id);
    let (threads_total, threads_unread) = service.label_thread_count(&label.id);
    V1Label {
        id: label.id.clone(),
        name: label.name.clone(),
        label_type: match label.label_type {
            LabelType::System => "system".to_string(),
            LabelType::User => "user".to_string(),
        },
        message_list_visibility: match label.message_list_visibility {
            Visibility::Show => "show".to_string(),
            Visibility::Hide => "hide".to_string(),
        },
        label_list_visibility: match label.label_list_visibility {
            LabelVisibility::LabelShow => "labelShow".to_string(),
            LabelVisibility::LabelShowIfUnread => "labelShowIfUnread".to_string(),
            LabelVisibility::LabelHide => "labelHide".to_string(),
        },
        messages_total: msgs_total,
        messages_unread: msgs_unread,
        threads_total,
        threads_unread,
        color: label.color.as_ref().map(|c| V1LabelColor {
            text_color: c.text_color.clone(),
            background_color: c.background_color.clone(),
        }),
    }
}

fn extract_actor_id(
    ext: &Option<Extension<ResolvedActorId>>,
    headers: &axum::http::HeaderMap,
) -> String {
    if let Some(Extension(ResolvedActorId(id))) = ext {
        return id.clone();
    }
    headers
        .get("X-Twin-Actor-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("default")
        .to_string()
}

fn v1_error_response(status: StatusCode, message: impl Into<String>) -> axum::response::Response {
    let code = status.as_u16();
    (
        status,
        Json(serde_json::json!({ "error": { "code": code, "message": message.into() } })),
    )
        .into_response()
}

fn twin_error_to_v1_response(e: TwinError) -> axum::response::Response {
    let msg = e.to_string();
    if msg.contains("not found") {
        v1_error_response(StatusCode::NOT_FOUND, msg)
    } else if msg.contains("cannot modify system label") || msg.contains("cannot delete system label") {
        v1_error_response(StatusCode::FORBIDDEN, msg)
    } else if msg.contains("already exists") {
        v1_error_response(StatusCode::CONFLICT, msg)
    } else {
        v1_error_response(StatusCode::BAD_REQUEST, msg)
    }
}

// ---------------------------------------------------------------------------
// Seed types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct SeedMessage {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub thread_id: String,
    pub from: String,
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Option<Vec<String>>,
    #[serde(default)]
    pub bcc: Option<Vec<String>>,
    pub subject: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub body_html: Option<String>,
    #[serde(default)]
    pub label_ids: Vec<String>,
    #[serde(default, alias = "internal_date")]
    pub timestamp_ms: Option<u64>,
    #[serde(default)]
    pub attachments: Vec<SeedAttachment>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SeedAttachment {
    #[serde(default)]
    pub attachment_id: Option<String>,
    pub filename: String,
    pub mime_type: String,
    /// Base64-encoded content.
    pub content: String,
}

fn deserialize_seed_field<T: DeserializeOwned>(
    value: &serde_json::Value,
    root_path: &str,
) -> Result<T, TwinError> {
    let raw = serde_json::to_vec(value)
        .map_err(|e| TwinError::Operation(format!("invalid seed at {root_path}: {e}")))?;
    let mut de = serde_json::Deserializer::from_slice(&raw);
    serde_path_to_error::deserialize(&mut de).map_err(|e| {
        let path = e.path().to_string();
        let full_path = if path.is_empty() {
            root_path.to_string()
        } else {
            format!("{root_path}.{path}")
        };
        TwinError::Operation(format!("invalid seed at {full_path}: {}", e.into_inner()))
    })
}

#[derive(Debug, Clone, Deserialize)]
struct SeedLabel {
    id: String,
    name: String,
    #[serde(default = "default_label_type")]
    label_type: String,
}

fn default_label_type() -> String {
    "user".to_string()
}

// ---------------------------------------------------------------------------
// TwinService implementation
// ---------------------------------------------------------------------------

impl TwinService for GmailTwinService {
    fn routes(shared: SharedTwinState<Self>) -> Router {
        Router::new()
            // ----- Native routes -----
            .route("/gmail/messages/send", post(route_send_message))
            .route("/gmail/messages/{id}", get(route_get_message))
            .route(
                "/gmail/messages/{id}",
                delete(route_delete_message_native),
            )
            .route("/gmail/messages/{id}/labels", post(route_modify_labels))
            .route("/gmail/labels", get(route_list_labels_native))
            .route("/gmail/labels", post(route_create_label_native))
            .route("/gmail/labels/{id}", delete(route_delete_label_native))
            .route("/gmail/threads/{id}", get(route_get_thread_native))
            // ----- Gmail API v1 mimicry routes -----
            .route(
                "/gmail/v1/users/me/messages",
                get(route_v1_list_messages),
            )
            .route(
                "/gmail/v1/users/me/messages/send",
                post(route_v1_send_message),
            )
            .route(
                "/gmail/v1/users/me/messages",
                post(route_v1_insert_message),
            )
            .route(
                "/gmail/v1/users/me/messages/{id}",
                get(route_v1_get_message),
            )
            .route(
                "/gmail/v1/users/me/messages/{id}",
                delete(route_v1_delete_message),
            )
            .route(
                "/gmail/v1/users/me/messages/{id}/modify",
                post(route_v1_modify_message),
            )
            .route(
                "/gmail/v1/users/me/messages/{id}/trash",
                post(route_v1_trash_message),
            )
            .route(
                "/gmail/v1/users/me/messages/{id}/untrash",
                post(route_v1_untrash_message),
            )
            .route(
                "/gmail/v1/users/me/threads",
                get(route_v1_list_threads),
            )
            .route(
                "/gmail/v1/users/me/threads/{id}",
                get(route_v1_get_thread),
            )
            .route(
                "/gmail/v1/users/me/threads/{id}/modify",
                post(route_v1_modify_thread),
            )
            .route(
                "/gmail/v1/users/me/threads/{id}/trash",
                post(route_v1_trash_thread),
            )
            .route(
                "/gmail/v1/users/me/threads/{id}/untrash",
                post(route_v1_untrash_thread),
            )
            .route(
                "/gmail/v1/users/me/threads/{id}",
                delete(route_v1_delete_thread),
            )
            .route(
                "/gmail/v1/users/me/labels",
                get(route_v1_list_labels),
            )
            .route(
                "/gmail/v1/users/me/labels",
                post(route_v1_create_label),
            )
            .route(
                "/gmail/v1/users/me/labels/{id}",
                get(route_v1_get_label),
            )
            .route(
                "/gmail/v1/users/me/labels/{id}",
                put(route_v1_update_label),
            )
            .route(
                "/gmail/v1/users/me/labels/{id}",
                patch(route_v1_patch_label),
            )
            .route(
                "/gmail/v1/users/me/labels/{id}",
                delete(route_v1_delete_label),
            )
            .route(
                "/gmail/v1/users/me/messages/{message_id}/attachments/{id}",
                get(route_v1_get_attachment),
            )
            .route(
                "/gmail/v1/users/me/profile",
                get(route_v1_get_profile),
            )
            // State inspection routes (framework-provided)
            .merge(state_inspection_routes(shared.clone()))
            .with_state(shared)
    }

    fn discovery_meta() -> Option<DiscoveryMeta> {
        // --- messages resource ---
        let mut msg_methods = BTreeMap::new();

        msg_methods.insert("list".to_string(), DiscoveryMethod {
            id: "gmail.users.messages.list".to_string(),
            http_method: "GET".to_string(),
            path: "users/{userId}/messages".to_string(),
            description: "Lists the messages in the user's mailbox.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true,
                    "description": "The user's email address (use 'me')."
                })),
                ("q".to_string(), serde_json::json!({
                    "type": "string", "location": "query",
                    "description": "Query string for searching messages."
                })),
                ("maxResults".to_string(), serde_json::json!({
                    "type": "integer", "location": "query",
                    "description": "Maximum number of messages to return."
                })),
                ("pageToken".to_string(), serde_json::json!({
                    "type": "string", "location": "query",
                    "description": "Page token to retrieve a specific page."
                })),
                ("labelIds".to_string(), serde_json::json!({
                    "type": "string", "location": "query", "repeated": true,
                    "description": "Only return messages with labels matching all of the specified label IDs."
                })),
            ]),
            parameter_order: vec!["userId".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: Some(serde_json::json!({"$ref": "ListMessagesResponse"})),
        });

        msg_methods.insert("get".to_string(), DiscoveryMethod {
            id: "gmail.users.messages.get".to_string(),
            http_method: "GET".to_string(),
            path: "users/{userId}/messages/{id}".to_string(),
            description: "Gets the specified message.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true,
                    "description": "The ID of the message to retrieve."
                })),
                ("format".to_string(), serde_json::json!({
                    "type": "string", "location": "query",
                    "description": "The format to return the message in."
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: Some(serde_json::json!({"$ref": "Message"})),
        });

        msg_methods.insert("insert".to_string(), DiscoveryMethod {
            id: "gmail.users.messages.insert".to_string(),
            http_method: "POST".to_string(),
            path: "users/{userId}/messages".to_string(),
            description: "Directly inserts a message into the mailbox.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: Some(serde_json::json!({"$ref": "Message"})),
            response: Some(serde_json::json!({"$ref": "Message"})),
        });

        msg_methods.insert("send".to_string(), DiscoveryMethod {
            id: "gmail.users.messages.send".to_string(),
            http_method: "POST".to_string(),
            path: "users/{userId}/messages/send".to_string(),
            description: "Sends the specified message.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: Some(serde_json::json!({"$ref": "Message"})),
            response: Some(serde_json::json!({"$ref": "Message"})),
        });

        msg_methods.insert("delete".to_string(), DiscoveryMethod {
            id: "gmail.users.messages.delete".to_string(),
            http_method: "DELETE".to_string(),
            path: "users/{userId}/messages/{id}".to_string(),
            description: "Immediately and permanently deletes the specified message.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: None,
        });

        msg_methods.insert("modify".to_string(), DiscoveryMethod {
            id: "gmail.users.messages.modify".to_string(),
            http_method: "POST".to_string(),
            path: "users/{userId}/messages/{id}/modify".to_string(),
            description: "Modifies the labels on the specified message.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: Some(serde_json::json!({"$ref": "ModifyMessageRequest"})),
            response: Some(serde_json::json!({"$ref": "Message"})),
        });

        msg_methods.insert("trash".to_string(), DiscoveryMethod {
            id: "gmail.users.messages.trash".to_string(),
            http_method: "POST".to_string(),
            path: "users/{userId}/messages/{id}/trash".to_string(),
            description: "Moves the specified message to the trash.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: Some(serde_json::json!({"$ref": "Message"})),
        });

        msg_methods.insert("untrash".to_string(), DiscoveryMethod {
            id: "gmail.users.messages.untrash".to_string(),
            http_method: "POST".to_string(),
            path: "users/{userId}/messages/{id}/untrash".to_string(),
            description: "Removes the specified message from the trash.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: Some(serde_json::json!({"$ref": "Message"})),
        });

        // --- messages.attachments sub-resource ---
        let mut attach_methods = BTreeMap::new();
        attach_methods.insert("get".to_string(), DiscoveryMethod {
            id: "gmail.users.messages.attachments.get".to_string(),
            http_method: "GET".to_string(),
            path: "users/{userId}/messages/{messageId}/attachments/{id}".to_string(),
            description: "Gets the specified message attachment.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("messageId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "messageId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: Some(serde_json::json!({"$ref": "MessagePartBody"})),
        });

        let messages_resource = DiscoveryResource {
            methods: msg_methods,
            resources: BTreeMap::from([(
                "attachments".to_string(),
                DiscoveryResource {
                    methods: attach_methods,
                    resources: BTreeMap::new(),
                },
            )]),
        };

        // --- threads resource ---
        let mut thread_methods = BTreeMap::new();

        thread_methods.insert("list".to_string(), DiscoveryMethod {
            id: "gmail.users.threads.list".to_string(),
            http_method: "GET".to_string(),
            path: "users/{userId}/threads".to_string(),
            description: "Lists the threads in the user's mailbox.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("q".to_string(), serde_json::json!({
                    "type": "string", "location": "query"
                })),
                ("maxResults".to_string(), serde_json::json!({
                    "type": "integer", "location": "query"
                })),
                ("pageToken".to_string(), serde_json::json!({
                    "type": "string", "location": "query"
                })),
            ]),
            parameter_order: vec!["userId".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: Some(serde_json::json!({"$ref": "ListThreadsResponse"})),
        });

        thread_methods.insert("get".to_string(), DiscoveryMethod {
            id: "gmail.users.threads.get".to_string(),
            http_method: "GET".to_string(),
            path: "users/{userId}/threads/{id}".to_string(),
            description: "Gets the specified thread.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: Some(serde_json::json!({"$ref": "Thread"})),
        });

        thread_methods.insert("delete".to_string(), DiscoveryMethod {
            id: "gmail.users.threads.delete".to_string(),
            http_method: "DELETE".to_string(),
            path: "users/{userId}/threads/{id}".to_string(),
            description: "Immediately and permanently deletes the specified thread.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: None,
        });

        thread_methods.insert("modify".to_string(), DiscoveryMethod {
            id: "gmail.users.threads.modify".to_string(),
            http_method: "POST".to_string(),
            path: "users/{userId}/threads/{id}/modify".to_string(),
            description: "Modifies the labels applied to the thread.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: Some(serde_json::json!({"$ref": "ModifyThreadRequest"})),
            response: Some(serde_json::json!({"$ref": "Thread"})),
        });

        thread_methods.insert("trash".to_string(), DiscoveryMethod {
            id: "gmail.users.threads.trash".to_string(),
            http_method: "POST".to_string(),
            path: "users/{userId}/threads/{id}/trash".to_string(),
            description: "Moves the specified thread to the trash.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: Some(serde_json::json!({"$ref": "Thread"})),
        });

        thread_methods.insert("untrash".to_string(), DiscoveryMethod {
            id: "gmail.users.threads.untrash".to_string(),
            http_method: "POST".to_string(),
            path: "users/{userId}/threads/{id}/untrash".to_string(),
            description: "Removes the specified thread from the trash.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: Some(serde_json::json!({"$ref": "Thread"})),
        });

        let threads_resource = DiscoveryResource {
            methods: thread_methods,
            resources: BTreeMap::new(),
        };

        // --- labels resource ---
        let mut label_methods = BTreeMap::new();

        label_methods.insert("list".to_string(), DiscoveryMethod {
            id: "gmail.users.labels.list".to_string(),
            http_method: "GET".to_string(),
            path: "users/{userId}/labels".to_string(),
            description: "Lists all labels in the user's mailbox.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: Some(serde_json::json!({"$ref": "ListLabelsResponse"})),
        });

        label_methods.insert("get".to_string(), DiscoveryMethod {
            id: "gmail.users.labels.get".to_string(),
            http_method: "GET".to_string(),
            path: "users/{userId}/labels/{id}".to_string(),
            description: "Gets the specified label.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: Some(serde_json::json!({"$ref": "Label"})),
        });

        label_methods.insert("create".to_string(), DiscoveryMethod {
            id: "gmail.users.labels.create".to_string(),
            http_method: "POST".to_string(),
            path: "users/{userId}/labels".to_string(),
            description: "Creates a new label.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: Some(serde_json::json!({"$ref": "Label"})),
            response: Some(serde_json::json!({"$ref": "Label"})),
        });

        label_methods.insert("update".to_string(), DiscoveryMethod {
            id: "gmail.users.labels.update".to_string(),
            http_method: "PUT".to_string(),
            path: "users/{userId}/labels/{id}".to_string(),
            description: "Updates the specified label.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: Some(serde_json::json!({"$ref": "Label"})),
            response: Some(serde_json::json!({"$ref": "Label"})),
        });

        label_methods.insert("patch".to_string(), DiscoveryMethod {
            id: "gmail.users.labels.patch".to_string(),
            http_method: "PATCH".to_string(),
            path: "users/{userId}/labels/{id}".to_string(),
            description: "Patch the specified label.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: Some(serde_json::json!({"$ref": "Label"})),
            response: Some(serde_json::json!({"$ref": "Label"})),
        });

        label_methods.insert("delete".to_string(), DiscoveryMethod {
            id: "gmail.users.labels.delete".to_string(),
            http_method: "DELETE".to_string(),
            path: "users/{userId}/labels/{id}".to_string(),
            description: "Immediately and permanently deletes the specified label.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
                ("id".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string(), "id".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: None,
        });

        let labels_resource = DiscoveryResource {
            methods: label_methods,
            resources: BTreeMap::new(),
        };

        // --- profile (nested under users) ---
        let mut profile_methods = BTreeMap::new();
        profile_methods.insert("getProfile".to_string(), DiscoveryMethod {
            id: "gmail.users.getProfile".to_string(),
            http_method: "GET".to_string(),
            path: "users/{userId}/profile".to_string(),
            description: "Gets the current user's Gmail profile.".to_string(),
            parameters: BTreeMap::from([
                ("userId".to_string(), serde_json::json!({
                    "type": "string", "location": "path", "required": true
                })),
            ]),
            parameter_order: vec!["userId".to_string()],
            supports_media_upload: false,
            media_upload: None,
            request: None,
            response: Some(serde_json::json!({"$ref": "Profile"})),
        });

        // The Gmail discovery doc nests messages, threads, labels under a
        // top-level "users" resource.  The "users" resource itself has
        // getProfile as a direct method plus sub-resources.
        let users_resource = DiscoveryResource {
            methods: profile_methods,
            resources: BTreeMap::from([
                ("messages".to_string(), messages_resource),
                ("threads".to_string(), threads_resource),
                ("labels".to_string(), labels_resource),
            ]),
        };

        Some(DiscoveryMeta {
            name: "gmail".to_string(),
            version: "v1".to_string(),
            title: "Gmail API".to_string(),
            description: "Digital twin of the Gmail API v1.".to_string(),
            service_path: "gmail/v1/".to_string(),
            resources: BTreeMap::from([("users".to_string(), users_resource)]),
            schemas: serde_json::json!({
                "Message": {
                    "id": "Message",
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "threadId": {"type": "string"},
                        "labelIds": {"type": "array", "items": {"type": "string"}}
                    }
                },
                "ListMessagesResponse": {
                    "id": "ListMessagesResponse",
                    "type": "object",
                    "properties": {
                        "messages": {"type": "array", "items": {"$ref": "Message"}},
                        "nextPageToken": {"type": "string"},
                        "resultSizeEstimate": {"type": "integer"}
                    }
                },
                "Thread": {
                    "id": "Thread",
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "messages": {"type": "array", "items": {"$ref": "Message"}}
                    }
                },
                "ListThreadsResponse": {
                    "id": "ListThreadsResponse",
                    "type": "object",
                    "properties": {
                        "threads": {"type": "array", "items": {"$ref": "Thread"}},
                        "nextPageToken": {"type": "string"},
                        "resultSizeEstimate": {"type": "integer"}
                    }
                },
                "Label": {
                    "id": "Label",
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "name": {"type": "string"},
                        "type": {"type": "string"}
                    }
                },
                "ListLabelsResponse": {
                    "id": "ListLabelsResponse",
                    "type": "object",
                    "properties": {
                        "labels": {"type": "array", "items": {"$ref": "Label"}}
                    }
                },
                "Profile": {
                    "id": "Profile",
                    "type": "object",
                    "properties": {
                        "emailAddress": {"type": "string"},
                        "messagesTotal": {"type": "integer"},
                        "threadsTotal": {"type": "integer"},
                        "historyId": {"type": "string"}
                    }
                },
                "MessagePartBody": {
                    "id": "MessagePartBody",
                    "type": "object",
                    "properties": {
                        "attachmentId": {"type": "string"},
                        "size": {"type": "integer"},
                        "data": {"type": "string"}
                    }
                },
                "ModifyMessageRequest": {
                    "id": "ModifyMessageRequest",
                    "type": "object",
                    "properties": {
                        "addLabelIds": {"type": "array", "items": {"type": "string"}},
                        "removeLabelIds": {"type": "array", "items": {"type": "string"}}
                    }
                },
                "ModifyThreadRequest": {
                    "id": "ModifyThreadRequest",
                    "type": "object",
                    "properties": {
                        "addLabelIds": {"type": "array", "items": {"type": "string"}},
                        "removeLabelIds": {"type": "array", "items": {"type": "string"}}
                    }
                }
            }),
        })
    }

    fn service_snapshot(&self) -> serde_json::Value {
        self._twin_snapshot()
    }

    fn service_restore(&mut self, snapshot: &serde_json::Value) -> Result<(), TwinError> {
        self._twin_restore(snapshot)
    }

    fn seed_from_scenario(&mut self, initial_state: &serde_json::Value) -> Result<(), TwinError> {
        // Seed user labels first
        if let Some(labels_val) = initial_state.get("labels") {
            let seed_labels: Vec<SeedLabel> = deserialize_seed_field(labels_val, "$.labels")?;
            for sl in seed_labels {
                if !self.labels.contains_key(&sl.id) {
                    self.labels.insert(
                        sl.id.clone(),
                        GmailLabel {
                            id: sl.id,
                            name: sl.name,
                            label_type: if sl.label_type == "system" {
                                LabelType::System
                            } else {
                                LabelType::User
                            },
                            message_list_visibility: Visibility::Show,
                            label_list_visibility: LabelVisibility::LabelShow,
                            color: None,
                        },
                    );
                }
            }
        }

        // Seed messages
        if let Some(messages_val) = initial_state.get("messages") {
            let seed_msgs: Vec<SeedMessage> = deserialize_seed_field(messages_val, "$.messages")?;
            for sm in seed_msgs {
                self.handle(GmailRequest::InsertMessage {
                    actor_id: sm.from.clone(),
                    raw_message: sm,
                })?;
            }
        }

        self.bump_next_id();
        Ok(())
    }

    fn evaluate_assertion(
        &self,
        check: &serde_json::Value,
    ) -> Result<AssertionResult, TwinError> {
        let check_type = check
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| TwinError::Operation("assertion missing 'type' field".to_string()))?;
        let check_id = check
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or(check_type)
            .to_string();

        match check_type {
            "message_exists" => {
                let msg_id = json_str(check, "message_id")?;
                let exists = self.messages.contains_key(&msg_id);
                Ok(AssertionResult {
                    id: check_id,
                    passed: exists,
                    message: if exists {
                        format!("message {msg_id} exists")
                    } else {
                        format!("message {msg_id} not found")
                    },
                })
            }

            "message_has_label" => {
                let msg_id = json_str(check, "message_id")?;
                let label_id = json_str(check, "label_id")?;
                let msg = self.messages.get(&msg_id).ok_or_else(|| {
                    TwinError::Operation(format!("message not found: {msg_id}"))
                })?;
                let has = msg.label_ids.iter().any(|l| l == &label_id);
                Ok(AssertionResult {
                    id: check_id,
                    passed: has,
                    message: if has {
                        format!("message {msg_id} has label {label_id}")
                    } else {
                        format!("message {msg_id} does not have label {label_id}")
                    },
                })
            }

            "message_not_has_label" => {
                let msg_id = json_str(check, "message_id")?;
                let label_id = json_str(check, "label_id")?;
                let msg = self.messages.get(&msg_id).ok_or_else(|| {
                    TwinError::Operation(format!("message not found: {msg_id}"))
                })?;
                let absent = !msg.label_ids.iter().any(|l| l == &label_id);
                Ok(AssertionResult {
                    id: check_id,
                    passed: absent,
                    message: if absent {
                        format!("message {msg_id} does not have label {label_id}")
                    } else {
                        format!("message {msg_id} unexpectedly has label {label_id}")
                    },
                })
            }

            "label_exists" => {
                let label_id = json_str(check, "label_id")?;
                let exists = self.labels.contains_key(&label_id);
                Ok(AssertionResult {
                    id: check_id,
                    passed: exists,
                    message: if exists {
                        format!("label {label_id} exists")
                    } else {
                        format!("label {label_id} not found")
                    },
                })
            }

            "thread_message_count" => {
                let thread_id = json_str(check, "thread_id")?;
                let expected = check
                    .get("count")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| {
                        TwinError::Operation("assertion missing 'count' field".to_string())
                    })? as usize;
                let actual = self
                    .messages
                    .values()
                    .filter(|m| m.thread_id == thread_id)
                    .count();
                Ok(AssertionResult {
                    id: check_id,
                    passed: actual == expected,
                    message: format!(
                        "thread {thread_id} has {actual} messages (expected {expected})"
                    ),
                })
            }

            "message_in_trash" => {
                let msg_id = json_str(check, "message_id")?;
                let msg = self.messages.get(&msg_id).ok_or_else(|| {
                    TwinError::Operation(format!("message not found: {msg_id}"))
                })?;
                let in_trash = msg.label_ids.iter().any(|l| l == "TRASH");
                Ok(AssertionResult {
                    id: check_id,
                    passed: in_trash,
                    message: if in_trash {
                        format!("message {msg_id} is in trash")
                    } else {
                        format!("message {msg_id} is not in trash")
                    },
                })
            }

            other => Err(TwinError::Operation(format!(
                "unknown assertion type: {other}"
            ))),
        }
    }

    fn execute_timeline_action(
        &mut self,
        action: &serde_json::Value,
        actor_id: &str,
    ) -> Result<TimelineActionResult, TwinError> {
        let action_type = action
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| TwinError::Operation("action missing 'type' field".to_string()))?;

        match action_type {
            "send_message" => {
                let to: Vec<String> = action
                    .get("to")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                let subject = json_str_or(action, "subject", "");
                let body = json_str_or(action, "body", "");
                let thread_id = action.get("thread_id").and_then(|v| v.as_str()).map(String::from);

                let result = self.handle(GmailRequest::SendMessage {
                    actor_id: actor_id.to_string(),
                    to,
                    cc: Vec::new(),
                    bcc: Vec::new(),
                    subject,
                    body,
                    thread_id,
                    attachments: Vec::new(),
                })?;
                Ok(TimelineActionResult {
                    endpoint: "/gmail/messages/send".to_string(),
                    response: serde_json::to_value(&result).unwrap_or_default(),
                })
            }

            "get_message" => {
                let message_id = json_str(action, "message_id")?;
                let format = action
                    .get("format")
                    .and_then(|v| v.as_str())
                    .map(MessageFormat::from_str)
                    .unwrap_or(MessageFormat::Full);
                let result = self.handle(GmailRequest::GetMessage {
                    actor_id: actor_id.to_string(),
                    message_id: message_id.clone(),
                    format,
                })?;
                Ok(TimelineActionResult {
                    endpoint: format!("/gmail/messages/{message_id}"),
                    response: serde_json::to_value(&result).unwrap_or_default(),
                })
            }

            "modify_labels" => {
                let message_id = json_str(action, "message_id")?;
                let add: Vec<String> = action
                    .get("add_label_ids")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                let remove: Vec<String> = action
                    .get("remove_label_ids")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                let result = self.handle(GmailRequest::ModifyMessage {
                    actor_id: actor_id.to_string(),
                    message_id: message_id.clone(),
                    add_label_ids: add,
                    remove_label_ids: remove,
                })?;
                Ok(TimelineActionResult {
                    endpoint: format!("/gmail/messages/{message_id}/labels"),
                    response: serde_json::to_value(&result).unwrap_or_default(),
                })
            }

            "trash_message" => {
                let message_id = json_str(action, "message_id")?;
                let result = self.handle(GmailRequest::TrashMessage {
                    actor_id: actor_id.to_string(),
                    message_id: message_id.clone(),
                })?;
                Ok(TimelineActionResult {
                    endpoint: format!("/gmail/messages/{message_id}/trash"),
                    response: serde_json::to_value(&result).unwrap_or_default(),
                })
            }

            "delete_message" => {
                let message_id = json_str(action, "message_id")?;
                let result = self.handle(GmailRequest::DeleteMessage {
                    actor_id: actor_id.to_string(),
                    message_id: message_id.clone(),
                })?;
                Ok(TimelineActionResult {
                    endpoint: format!("/gmail/messages/{message_id}"),
                    response: serde_json::to_value(&result).unwrap_or_default(),
                })
            }

            "create_label" => {
                let name = json_str(action, "name")?;
                let result = self.handle(GmailRequest::CreateLabel {
                    actor_id: actor_id.to_string(),
                    name,
                    message_list_visibility: None,
                    label_list_visibility: None,
                })?;
                Ok(TimelineActionResult {
                    endpoint: "/gmail/labels".to_string(),
                    response: serde_json::to_value(&result).unwrap_or_default(),
                })
            }

            "delete_label" => {
                let label_id = json_str(action, "label_id")?;
                let result = self.handle(GmailRequest::DeleteLabel {
                    actor_id: actor_id.to_string(),
                    label_id: label_id.clone(),
                })?;
                Ok(TimelineActionResult {
                    endpoint: format!("/gmail/labels/{label_id}"),
                    response: serde_json::to_value(&result).unwrap_or_default(),
                })
            }

            "get_thread" => {
                let thread_id = json_str(action, "thread_id")?;
                let result = self.handle(GmailRequest::GetThread {
                    actor_id: actor_id.to_string(),
                    thread_id: thread_id.clone(),
                    format: MessageFormat::Full,
                })?;
                Ok(TimelineActionResult {
                    endpoint: format!("/gmail/threads/{thread_id}"),
                    response: serde_json::to_value(&result).unwrap_or_default(),
                })
            }

            other => Err(TwinError::Operation(format!(
                "unknown timeline action type: {other}"
            ))),
        }
    }

    fn validate_scenario(scenario: &serde_json::Value) -> (Vec<String>, Vec<String>) {
        let mut errors = Vec::new();
        let warnings = Vec::new();

        let initial_state = match scenario.get("initial_state") {
            Some(v) => v,
            None => return (errors, warnings),
        };

        // Validate messages
        if let Some(messages_val) = initial_state.get("messages") {
            if let Some(msgs) = messages_val.as_array() {
                let mut seen_ids = std::collections::BTreeSet::new();
                for msg in msgs {
                    if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                        if !seen_ids.insert(id.to_string()) {
                            errors.push(format!("duplicate message id: {id}"));
                        }
                    }
                    if msg.get("from").and_then(|v| v.as_str()).is_none() {
                        let id = msg
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("<unknown>");
                        errors.push(format!("message {id} missing 'from' field"));
                    }
                    if msg.get("subject").and_then(|v| v.as_str()).is_none() {
                        let id = msg
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("<unknown>");
                        errors.push(format!("message {id} missing 'subject' field"));
                    }
                }
            }
        }

        // Validate labels
        if let Some(labels_val) = initial_state.get("labels") {
            if let Some(labels) = labels_val.as_array() {
                let mut seen_ids = std::collections::BTreeSet::new();
                for lbl in labels {
                    if let Some(id) = lbl.get("id").and_then(|v| v.as_str()) {
                        if !seen_ids.insert(id.to_string()) {
                            errors.push(format!("duplicate label id: {id}"));
                        }
                    }
                }
            }
        }

        (errors, warnings)
    }
}

// ---------------------------------------------------------------------------
// JSON helper
// ---------------------------------------------------------------------------

fn json_str(val: &serde_json::Value, key: &str) -> Result<String, TwinError> {
    val.get(key)
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| TwinError::Operation(format!("missing field '{key}'")))
}

fn json_str_or(val: &serde_json::Value, key: &str, default: &str) -> String {
    val.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
        .to_string()
}

// ---------------------------------------------------------------------------
// Native route handlers
// ---------------------------------------------------------------------------

async fn route_send_message(
    State(state): State<GmailState>,
    Json(body): Json<NativeSendBody>,
) -> impl IntoResponse {
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::SendMessage {
        actor_id: "default".to_string(),
        to: body.to,
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: body.subject,
        body: body.body,
        thread_id: body.thread_id,
        attachments: Vec::new(),
    });
    gmail_result_to_response(result)
}

async fn route_get_message(
    State(state): State<GmailState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let rt = state.lock().await;
    let result = rt.service.handle_get_message(&id);
    gmail_result_to_response(result)
}

async fn route_delete_message_native(
    State(state): State<GmailState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::DeleteMessage {
        actor_id: "default".to_string(),
        message_id: id,
    });
    gmail_result_to_response(result)
}

async fn route_modify_labels(
    State(state): State<GmailState>,
    Path(id): Path<String>,
    Json(body): Json<NativeModifyLabelsBody>,
) -> impl IntoResponse {
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::ModifyMessage {
        actor_id: "default".to_string(),
        message_id: id,
        add_label_ids: body.add_label_ids,
        remove_label_ids: body.remove_label_ids,
    });
    gmail_result_to_response(result)
}

async fn route_list_labels_native(
    State(state): State<GmailState>,
) -> impl IntoResponse {
    let rt = state.lock().await;
    let result = rt.service.handle_list_labels();
    gmail_result_to_response(result)
}

async fn route_create_label_native(
    State(state): State<GmailState>,
    Json(body): Json<NativeCreateLabelBody>,
) -> impl IntoResponse {
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::CreateLabel {
        actor_id: "default".to_string(),
        name: body.name,
        message_list_visibility: None,
        label_list_visibility: None,
    });
    gmail_result_to_response(result)
}

async fn route_delete_label_native(
    State(state): State<GmailState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::DeleteLabel {
        actor_id: "default".to_string(),
        label_id: id,
    });
    gmail_result_to_response(result)
}

async fn route_get_thread_native(
    State(state): State<GmailState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let rt = state.lock().await;
    let result = rt.service.handle_get_thread(&id);
    gmail_result_to_response(result)
}

impl GmailTwinService {
    fn handle_get_message(&self, id: &str) -> Result<GmailResponse, TwinError> {
        let msg = self
            .messages
            .get(id)
            .cloned()
            .ok_or_else(|| TwinError::Operation(format!("message not found: {id}")))?;
        Ok(GmailResponse::Message(msg))
    }

    fn handle_list_labels(&self) -> Result<GmailResponse, TwinError> {
        Ok(GmailResponse::LabelList(self.labels.values().cloned().collect()))
    }

    fn handle_get_thread(&self, id: &str) -> Result<GmailResponse, TwinError> {
        let thread = self
            .threads
            .get(id)
            .cloned()
            .ok_or_else(|| TwinError::Operation(format!("thread not found: {id}")))?;
        let mut msgs: Vec<GmailMessage> = self
            .messages
            .values()
            .filter(|m| m.thread_id == *id)
            .cloned()
            .collect();
        msgs.sort_by_key(|m| m.internal_date);
        Ok(GmailResponse::Thread {
            thread,
            messages: msgs,
        })
    }
}

fn gmail_result_to_response(result: Result<GmailResponse, TwinError>) -> axum::response::Response {
    match result {
        Ok(response) => {
            let json = serde_json::to_value(&response).unwrap_or_else(|e| {
                serde_json::json!({ "error": format!("serialization failed: {e}") })
            });
            (StatusCode::OK, Json(json)).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Gmail API v1 mimicry route handlers
// ---------------------------------------------------------------------------

async fn route_v1_list_messages(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Query(query): Query<V1ListQuery>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let label_ids: Vec<String> = query
        .label_ids
        .as_deref()
        .map(|s| s.split(',').map(|l| l.trim().to_string()).collect())
        .unwrap_or_default();
    let result = rt.service.handle(GmailRequest::ListMessages {
        actor_id,
        label_ids,
        max_results: query.max_results,
        page_token: query.page_token,
        q: query.q,
    });
    match result {
        Ok(GmailResponse::MessageList {
            messages,
            next_page_token,
            result_size_estimate,
        }) => {
            let v1 = V1MessageList {
                messages: messages
                    .into_iter()
                    .map(|(id, tid)| V1MessageRef {
                        id,
                        thread_id: tid,
                    })
                    .collect(),
                next_page_token,
                result_size_estimate,
            };
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_get_message(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<V1GetMessageQuery>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let format = MessageFormat::from_str(&query.format);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::GetMessage {
        actor_id,
        message_id: id,
        format,
    });
    match result {
        Ok(GmailResponse::Message(msg)) => {
            let v1 = gmail_message_to_v1(&msg, format);
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_send_message(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<V1SendBody>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::SendMessage {
        actor_id,
        to: body.to,
        cc: body.cc,
        bcc: body.bcc,
        subject: body.subject.unwrap_or_default(),
        body: body.body.unwrap_or_default(),
        thread_id: body.thread_id,
        attachments: Vec::new(),
    });
    match result {
        Ok(GmailResponse::Message(msg)) => {
            let v1 = gmail_message_to_v1(&msg, MessageFormat::Full);
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_insert_message(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<V1InsertBody>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::InsertMessage {
        actor_id,
        raw_message: SeedMessage {
            id: String::new(),
            thread_id: body.thread_id.unwrap_or_default(),
            from: body.from,
            to: body.to,
            cc: body.cc,
            bcc: body.bcc,
            subject: body.subject,
            body: body.body,
            body_html: body.body_html,
            label_ids: body.label_ids,
            timestamp_ms: body.internal_date.and_then(|s| s.parse().ok()),
            attachments: Vec::new(),
        },
    });
    match result {
        Ok(GmailResponse::Message(msg)) => {
            let v1 = gmail_message_to_v1(&msg, MessageFormat::Full);
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_modify_message(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<V1ModifyBody>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::ModifyMessage {
        actor_id,
        message_id: id,
        add_label_ids: body.add_label_ids,
        remove_label_ids: body.remove_label_ids,
    });
    match result {
        Ok(GmailResponse::Message(msg)) => {
            let v1 = gmail_message_to_v1(&msg, MessageFormat::Full);
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_trash_message(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::TrashMessage {
        actor_id,
        message_id: id,
    });
    match result {
        Ok(GmailResponse::Message(msg)) => {
            let v1 = gmail_message_to_v1(&msg, MessageFormat::Full);
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_untrash_message(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::UntrashMessage {
        actor_id,
        message_id: id,
    });
    match result {
        Ok(GmailResponse::Message(msg)) => {
            let v1 = gmail_message_to_v1(&msg, MessageFormat::Full);
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_delete_message(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::DeleteMessage {
        actor_id,
        message_id: id,
    });
    match result {
        Ok(GmailResponse::Deleted) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_list_threads(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Query(query): Query<V1ListQuery>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let label_ids: Vec<String> = query
        .label_ids
        .as_deref()
        .map(|s| s.split(',').map(|l| l.trim().to_string()).collect())
        .unwrap_or_default();
    let result = rt.service.handle(GmailRequest::ListThreads {
        actor_id,
        label_ids,
        max_results: query.max_results,
        page_token: query.page_token,
    });
    match result {
        Ok(GmailResponse::ThreadList {
            threads,
            next_page_token,
            result_size_estimate,
        }) => {
            let v1 = V1ThreadList {
                threads: threads
                    .into_iter()
                    .map(|t| V1ThreadRef {
                        id: t.id,
                        snippet: t.snippet,
                        history_id: t.history_id.to_string(),
                    })
                    .collect(),
                next_page_token,
                result_size_estimate,
            };
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_get_thread(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<V1GetMessageQuery>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let format = MessageFormat::from_str(&query.format);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::GetThread {
        actor_id,
        thread_id: id,
        format,
    });
    match result {
        Ok(GmailResponse::Thread { thread, messages }) => {
            let v1 = V1Thread {
                id: thread.id,
                history_id: thread.history_id.to_string(),
                snippet: thread.snippet,
                messages: Some(
                    messages
                        .iter()
                        .map(|m| gmail_message_to_v1(m, format))
                        .collect(),
                ),
            };
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_modify_thread(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<V1ModifyBody>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::ModifyThread {
        actor_id,
        thread_id: id,
        add_label_ids: body.add_label_ids,
        remove_label_ids: body.remove_label_ids,
    });
    match result {
        Ok(GmailResponse::Thread { thread, messages }) => {
            let v1 = V1Thread {
                id: thread.id,
                history_id: thread.history_id.to_string(),
                snippet: thread.snippet,
                messages: Some(
                    messages
                        .iter()
                        .map(|m| gmail_message_to_v1(m, MessageFormat::Full))
                        .collect(),
                ),
            };
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_trash_thread(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::TrashThread {
        actor_id,
        thread_id: id,
    });
    match result {
        Ok(GmailResponse::Thread { thread, messages }) => {
            let v1 = V1Thread {
                id: thread.id,
                history_id: thread.history_id.to_string(),
                snippet: thread.snippet,
                messages: Some(
                    messages
                        .iter()
                        .map(|m| gmail_message_to_v1(m, MessageFormat::Full))
                        .collect(),
                ),
            };
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_untrash_thread(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::UntrashThread {
        actor_id,
        thread_id: id,
    });
    match result {
        Ok(GmailResponse::Thread { thread, messages }) => {
            let v1 = V1Thread {
                id: thread.id,
                history_id: thread.history_id.to_string(),
                snippet: thread.snippet,
                messages: Some(
                    messages
                        .iter()
                        .map(|m| gmail_message_to_v1(m, MessageFormat::Full))
                        .collect(),
                ),
            };
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_delete_thread(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::DeleteThread {
        actor_id,
        thread_id: id,
    });
    match result {
        Ok(GmailResponse::Deleted) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_list_labels(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::ListLabels { actor_id });
    match result {
        Ok(GmailResponse::LabelList(labels)) => {
            let v1_labels: Vec<V1Label> = labels
                .iter()
                .map(|l| gmail_label_to_v1(l, &rt.service))
                .collect();
            let v1 = V1LabelList {
                labels: v1_labels,
            };
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_get_label(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::GetLabel {
        actor_id,
        label_id: id,
    });
    match result {
        Ok(GmailResponse::Label(label)) => {
            let v1 = gmail_label_to_v1(&label, &rt.service);
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_create_label(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<V1CreateLabelBody>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::CreateLabel {
        actor_id,
        name: body.name,
        message_list_visibility: body.message_list_visibility,
        label_list_visibility: body.label_list_visibility,
    });
    match result {
        Ok(GmailResponse::Label(label)) => {
            let v1 = gmail_label_to_v1(&label, &rt.service);
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_update_label(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<V1UpdateLabelBody>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::UpdateLabel {
        actor_id,
        label_id: id,
        name: body.name,
        message_list_visibility: body.message_list_visibility,
        label_list_visibility: body.label_list_visibility,
    });
    match result {
        Ok(GmailResponse::Label(label)) => {
            let v1 = gmail_label_to_v1(&label, &rt.service);
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_patch_label(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<V1UpdateLabelBody>,
) -> impl IntoResponse {
    // PATCH and PUT have the same behavior in our implementation
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::UpdateLabel {
        actor_id,
        label_id: id,
        name: body.name,
        message_list_visibility: body.message_list_visibility,
        label_list_visibility: body.label_list_visibility,
    });
    match result {
        Ok(GmailResponse::Label(label)) => {
            let v1 = gmail_label_to_v1(&label, &rt.service);
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_delete_label(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::DeleteLabel {
        actor_id,
        label_id: id,
    });
    match result {
        Ok(GmailResponse::Deleted) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_get_attachment(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path((message_id, id)): Path<(String, String)>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::GetAttachment {
        actor_id,
        message_id,
        attachment_id: id.clone(),
    });
    match result {
        Ok(GmailResponse::Attachment { data, size }) => {
            let v1 = V1Attachment {
                attachment_id: id,
                size,
                data: base64url_encode(&data),
            };
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

async fn route_v1_get_profile(
    State(state): State<GmailState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle(GmailRequest::GetProfile { actor_id });
    match result {
        Ok(GmailResponse::Profile {
            email,
            messages_total,
            threads_total,
            history_id,
        }) => {
            let v1 = V1Profile {
                email_address: email,
                messages_total,
                threads_total,
                history_id: history_id.to_string(),
            };
            (StatusCode::OK, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> GmailTwinService {
        GmailTwinService::default()
    }

    // --- Default state ---

    #[test]
    fn default_has_system_labels() {
        let s = service();
        assert!(s.labels.contains_key("INBOX"));
        assert!(s.labels.contains_key("SENT"));
        assert!(s.labels.contains_key("DRAFT"));
        assert!(s.labels.contains_key("TRASH"));
        assert!(s.labels.contains_key("SPAM"));
        assert!(s.labels.contains_key("UNREAD"));
        assert!(s.labels.contains_key("STARRED"));
        assert!(s.labels.contains_key("IMPORTANT"));
        assert_eq!(s.labels.len(), 13);
        assert!(s.messages.is_empty());
        assert!(s.threads.is_empty());
    }

    // --- Label operations ---

    #[test]
    fn create_label() {
        let mut s = service();
        let result = s
            .handle(GmailRequest::CreateLabel {
                actor_id: "alice".into(),
                name: "Work".into(),
                message_list_visibility: None,
                label_list_visibility: None,
            })
            .unwrap();
        match result {
            GmailResponse::Label(l) => {
                assert_eq!(l.name, "Work");
                assert_eq!(l.label_type, LabelType::User);
            }
            _ => panic!("expected Label response"),
        }
    }

    #[test]
    fn create_duplicate_label_fails() {
        let mut s = service();
        s.handle(GmailRequest::CreateLabel {
            actor_id: "alice".into(),
            name: "Work".into(),
            message_list_visibility: None,
            label_list_visibility: None,
        })
        .unwrap();
        let err = s
            .handle(GmailRequest::CreateLabel {
                actor_id: "alice".into(),
                name: "Work".into(),
                message_list_visibility: None,
                label_list_visibility: None,
            })
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn delete_system_label_fails() {
        let mut s = service();
        let err = s
            .handle(GmailRequest::DeleteLabel {
                actor_id: "alice".into(),
                label_id: "INBOX".into(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("cannot delete system label"));
    }

    #[test]
    fn update_system_label_fails() {
        let mut s = service();
        let err = s
            .handle(GmailRequest::UpdateLabel {
                actor_id: "alice".into(),
                label_id: "INBOX".into(),
                name: Some("MyInbox".into()),
                message_list_visibility: None,
                label_list_visibility: None,
            })
            .unwrap_err();
        assert!(err.to_string().contains("cannot modify system label"));
    }

    #[test]
    fn delete_user_label_removes_from_messages() {
        let mut s = service();
        let label = match s
            .handle(GmailRequest::CreateLabel {
                actor_id: "alice".into(),
                name: "Work".into(),
                message_list_visibility: None,
                label_list_visibility: None,
            })
            .unwrap()
        {
            GmailResponse::Label(l) => l,
            _ => panic!("expected Label"),
        };

        // Send a message, then add the label
        let msg = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Test".into(),
                body: "Hello".into(),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };

        s.handle(GmailRequest::ModifyMessage {
            actor_id: "alice".into(),
            message_id: msg.id.clone(),
            add_label_ids: vec![label.id.clone()],
            remove_label_ids: Vec::new(),
        })
        .unwrap();

        // Verify label is on message
        let m = s.messages.get(&msg.id).unwrap();
        assert!(m.label_ids.contains(&label.id));

        // Delete the label
        s.handle(GmailRequest::DeleteLabel {
            actor_id: "alice".into(),
            label_id: label.id.clone(),
        })
        .unwrap();

        // Verify label removed from message
        let m = s.messages.get(&msg.id).unwrap();
        assert!(!m.label_ids.contains(&label.id));
    }

    #[test]
    fn list_labels() {
        let mut s = service();
        let result = s
            .handle(GmailRequest::ListLabels {
                actor_id: "alice".into(),
            })
            .unwrap();
        match result {
            GmailResponse::LabelList(labels) => {
                assert_eq!(labels.len(), 13); // system labels
            }
            _ => panic!("expected LabelList"),
        }
    }

    #[test]
    fn update_user_label() {
        let mut s = service();
        let label = match s
            .handle(GmailRequest::CreateLabel {
                actor_id: "alice".into(),
                name: "Work".into(),
                message_list_visibility: None,
                label_list_visibility: None,
            })
            .unwrap()
        {
            GmailResponse::Label(l) => l,
            _ => panic!("expected Label"),
        };

        let updated = match s
            .handle(GmailRequest::UpdateLabel {
                actor_id: "alice".into(),
                label_id: label.id.clone(),
                name: Some("Projects".into()),
                message_list_visibility: None,
                label_list_visibility: None,
            })
            .unwrap()
        {
            GmailResponse::Label(l) => l,
            _ => panic!("expected Label"),
        };
        assert_eq!(updated.name, "Projects");
        assert_eq!(updated.id, label.id);
    }

    // --- Message operations ---

    #[test]
    fn send_message_creates_thread() {
        let mut s = service();
        let msg = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob@example.com".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Hello".into(),
                body: "Hi Bob!".into(),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };
        assert!(msg.label_ids.contains(&"SENT".to_string()));
        assert!(s.threads.contains_key(&msg.thread_id));
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn reply_joins_existing_thread() {
        let mut s = service();
        let msg1 = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Hello".into(),
                body: "Hi!".into(),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };

        let msg2 = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "bob".into(),
                to: vec!["alice".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Re: Hello".into(),
                body: "Hey!".into(),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };
        assert_eq!(msg1.thread_id, msg2.thread_id);
        assert_eq!(s.threads.len(), 1);
    }

    #[test]
    fn get_message_not_found() {
        let mut s = service();
        let err = s
            .handle(GmailRequest::GetMessage {
                actor_id: "alice".into(),
                message_id: "nonexistent".into(),
                format: MessageFormat::Full,
            })
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn modify_message_labels() {
        let mut s = service();
        let msg = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Test".into(),
                body: "Hi".into(),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };

        let modified = match s
            .handle(GmailRequest::ModifyMessage {
                actor_id: "alice".into(),
                message_id: msg.id.clone(),
                add_label_ids: vec!["STARRED".into(), "IMPORTANT".into()],
                remove_label_ids: vec!["SENT".into()],
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };
        assert!(modified.label_ids.contains(&"STARRED".to_string()));
        assert!(modified.label_ids.contains(&"IMPORTANT".to_string()));
        assert!(!modified.label_ids.contains(&"SENT".to_string()));
    }

    #[test]
    fn trash_and_untrash_message() {
        let mut s = service();
        let msg = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Test".into(),
                body: "Hi".into(),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };

        // Add INBOX label first
        s.handle(GmailRequest::ModifyMessage {
            actor_id: "alice".into(),
            message_id: msg.id.clone(),
            add_label_ids: vec!["INBOX".into()],
            remove_label_ids: Vec::new(),
        })
        .unwrap();

        // Trash
        let trashed = match s
            .handle(GmailRequest::TrashMessage {
                actor_id: "alice".into(),
                message_id: msg.id.clone(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };
        assert!(trashed.label_ids.contains(&"TRASH".to_string()));
        assert!(!trashed.label_ids.contains(&"INBOX".to_string()));

        // Untrash
        let untrashed = match s
            .handle(GmailRequest::UntrashMessage {
                actor_id: "alice".into(),
                message_id: msg.id.clone(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };
        assert!(!untrashed.label_ids.contains(&"TRASH".to_string()));
        assert!(untrashed.label_ids.contains(&"INBOX".to_string()));
    }

    #[test]
    fn delete_message_removes_from_state() {
        let mut s = service();
        let msg = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Test".into(),
                body: "Hi".into(),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };
        let tid = msg.thread_id.clone();

        s.handle(GmailRequest::DeleteMessage {
            actor_id: "alice".into(),
            message_id: msg.id.clone(),
        })
        .unwrap();

        assert!(!s.messages.contains_key(&msg.id));
        // Thread should be cleaned up too (was only message)
        assert!(!s.threads.contains_key(&tid));
    }

    #[test]
    fn list_messages_with_label_filter() {
        let mut s = service();
        // Send two messages
        let msg1 = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "One".into(),
                body: "First".into(),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };

        s.handle(GmailRequest::SendMessage {
            actor_id: "alice".into(),
            to: vec!["bob".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Two".into(),
            body: "Second".into(),
            thread_id: None,
            attachments: Vec::new(),
        })
        .unwrap();

        // Star only the first
        s.handle(GmailRequest::ModifyMessage {
            actor_id: "alice".into(),
            message_id: msg1.id.clone(),
            add_label_ids: vec!["STARRED".into()],
            remove_label_ids: Vec::new(),
        })
        .unwrap();

        // List all
        let all = match s
            .handle(GmailRequest::ListMessages {
                actor_id: "alice".into(),
                label_ids: Vec::new(),
                max_results: 100,
                page_token: None,
                q: None,
            })
            .unwrap()
        {
            GmailResponse::MessageList { messages, .. } => messages,
            _ => panic!("expected MessageList"),
        };
        assert_eq!(all.len(), 2);

        // List starred only
        let starred = match s
            .handle(GmailRequest::ListMessages {
                actor_id: "alice".into(),
                label_ids: vec!["STARRED".into()],
                max_results: 100,
                page_token: None,
                q: None,
            })
            .unwrap()
        {
            GmailResponse::MessageList { messages, .. } => messages,
            _ => panic!("expected MessageList"),
        };
        assert_eq!(starred.len(), 1);
        assert_eq!(starred[0].0, msg1.id);
    }

    // --- Thread operations ---

    #[test]
    fn get_thread_returns_messages() {
        let mut s = service();
        let msg = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Thread test".into(),
                body: "Hello".into(),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };

        let result = match s
            .handle(GmailRequest::GetThread {
                actor_id: "alice".into(),
                thread_id: msg.thread_id.clone(),
                format: MessageFormat::Full,
            })
            .unwrap()
        {
            GmailResponse::Thread { thread, messages } => (thread, messages),
            _ => panic!("expected Thread"),
        };
        assert_eq!(result.0.id, msg.thread_id);
        assert_eq!(result.1.len(), 1);
    }

    #[test]
    fn trash_thread_trashes_all_messages() {
        let mut s = service();
        // Create thread with two messages
        let msg1 = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Thread".into(),
                body: "First".into(),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };

        s.handle(GmailRequest::SendMessage {
            actor_id: "bob".into(),
            to: vec!["alice".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Re: Thread".into(),
            body: "Second".into(),
            thread_id: None,
            attachments: Vec::new(),
        })
        .unwrap();

        s.handle(GmailRequest::TrashThread {
            actor_id: "alice".into(),
            thread_id: msg1.thread_id.clone(),
        })
        .unwrap();

        for msg in s.messages.values() {
            if msg.thread_id == msg1.thread_id {
                assert!(msg.label_ids.contains(&"TRASH".to_string()));
            }
        }
    }

    #[test]
    fn delete_thread_removes_all() {
        let mut s = service();
        let msg = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Bye".into(),
                body: "Gone".into(),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };

        s.handle(GmailRequest::DeleteThread {
            actor_id: "alice".into(),
            thread_id: msg.thread_id.clone(),
        })
        .unwrap();

        assert!(s.threads.is_empty());
        assert!(s.messages.is_empty());
    }

    // --- Attachments ---

    #[test]
    fn send_with_attachment_and_download() {
        let mut s = service();
        let data = b"hello world";
        let msg = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "With file".into(),
                body: "See attached".into(),
                thread_id: None,
                attachments: vec![("test.txt".into(), "text/plain".into(), data.to_vec())],
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };
        assert_eq!(msg.attachments.len(), 1);
        let att_id = &msg.attachments[0].attachment_id;

        let result = s
            .handle(GmailRequest::GetAttachment {
                actor_id: "alice".into(),
                message_id: msg.id.clone(),
                attachment_id: att_id.clone(),
            })
            .unwrap();
        match result {
            GmailResponse::Attachment { data: d, size } => {
                assert_eq!(d, data.to_vec());
                assert_eq!(size, data.len() as u64);
            }
            _ => panic!("expected Attachment"),
        }
    }

    // --- Profile ---

    #[test]
    fn get_profile() {
        let mut s = service();
        s.handle(GmailRequest::SendMessage {
            actor_id: "alice".into(),
            to: vec!["bob".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Test".into(),
            body: "Hi".into(),
            thread_id: None,
            attachments: Vec::new(),
        })
        .unwrap();

        let result = s
            .handle(GmailRequest::GetProfile {
                actor_id: "alice".into(),
            })
            .unwrap();
        match result {
            GmailResponse::Profile {
                email,
                messages_total,
                threads_total,
                ..
            } => {
                assert_eq!(email, "alice@twin.local");
                assert_eq!(messages_total, 1);
                assert_eq!(threads_total, 1);
            }
            _ => panic!("expected Profile"),
        }
    }

    // --- Snapshot/Restore ---

    #[test]
    fn snapshot_restore_round_trip() {
        let mut s = service();
        s.handle(GmailRequest::SendMessage {
            actor_id: "alice".into(),
            to: vec!["bob".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Snapshot test".into(),
            body: "Content".into(),
            thread_id: None,
            attachments: vec![("file.bin".into(), "application/octet-stream".into(), vec![1, 2, 3])],
        })
        .unwrap();

        let snapshot = s.service_snapshot();
        let mut s2 = GmailTwinService::default();
        s2.service_restore(&snapshot).unwrap();

        assert_eq!(s2.messages.len(), 1);
        assert_eq!(s2.threads.len(), 1);
        assert_eq!(s2.attachments.len(), 1);

        // Verify attachment data survived round-trip
        let att_id = s2.messages.values().next().unwrap().attachments[0]
            .attachment_id
            .clone();
        assert_eq!(s2.attachments.get(&att_id).unwrap(), &vec![1u8, 2, 3]);
    }

    // --- Scenario seeding ---

    #[test]
    fn seed_from_scenario_basic() {
        let mut s = service();
        let initial_state = serde_json::json!({
            "labels": [
                { "id": "label_1", "name": "Work" }
            ],
            "messages": [
                {
                    "id": "msg_1",
                    "thread_id": "thread_1",
                    "from": "alice@example.com",
                    "to": ["bob@example.com"],
                    "subject": "Hello",
                    "body": "Hi Bob!",
                    "label_ids": ["INBOX", "UNREAD"],
                    "timestamp_ms": 1704067200000u64
                }
            ]
        });
        s.seed_from_scenario(&initial_state).unwrap();

        assert!(s.labels.contains_key("label_1"));
        assert!(s.messages.contains_key("msg_1"));
        assert!(s.threads.contains_key("thread_1"));
        let msg = s.messages.get("msg_1").unwrap();
        assert_eq!(msg.from, "alice@example.com");
        assert_eq!(msg.subject, "Hello");
        assert!(msg.label_ids.contains(&"INBOX".to_string()));
    }

    #[test]
    fn seed_with_attachments() {
        let mut s = service();
        let content = BASE64.encode(b"attachment data");
        let initial_state = serde_json::json!({
            "messages": [
                {
                    "id": "msg_1",
                    "thread_id": "thread_1",
                    "from": "alice@example.com",
                    "to": ["bob@example.com"],
                    "subject": "With attachment",
                    "body": "See attached",
                    "label_ids": ["INBOX"],
                    "attachments": [
                        {
                            "filename": "data.bin",
                            "mime_type": "application/octet-stream",
                            "content": content
                        }
                    ]
                }
            ]
        });
        s.seed_from_scenario(&initial_state).unwrap();

        let msg = s.messages.get("msg_1").unwrap();
        assert_eq!(msg.attachments.len(), 1);
        let att_data = s
            .attachments
            .get(&msg.attachments[0].attachment_id)
            .unwrap();
        assert_eq!(att_data, b"attachment data");
    }

    // --- Assertions ---

    #[test]
    fn assertion_message_exists() {
        let mut s = service();
        s.handle(GmailRequest::SendMessage {
            actor_id: "alice".into(),
            to: vec!["bob".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Test".into(),
            body: "Hi".into(),
            thread_id: None,
            attachments: Vec::new(),
        })
        .unwrap();

        let mid = s.messages.keys().next().unwrap().clone();
        let result = s
            .evaluate_assertion(&serde_json::json!({
                "type": "message_exists",
                "message_id": mid,
            }))
            .unwrap();
        assert!(result.passed);

        let result = s
            .evaluate_assertion(&serde_json::json!({
                "type": "message_exists",
                "message_id": "nonexistent",
            }))
            .unwrap();
        assert!(!result.passed);
    }

    #[test]
    fn assertion_message_has_label() {
        let mut s = service();
        let msg = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Test".into(),
                body: "Hi".into(),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };

        let result = s
            .evaluate_assertion(&serde_json::json!({
                "type": "message_has_label",
                "message_id": msg.id,
                "label_id": "SENT",
            }))
            .unwrap();
        assert!(result.passed);

        let result = s
            .evaluate_assertion(&serde_json::json!({
                "type": "message_has_label",
                "message_id": msg.id,
                "label_id": "STARRED",
            }))
            .unwrap();
        assert!(!result.passed);
    }

    #[test]
    fn assertion_thread_message_count() {
        let mut s = service();
        let msg = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Count test".into(),
                body: "One".into(),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };

        s.handle(GmailRequest::SendMessage {
            actor_id: "bob".into(),
            to: vec!["alice".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Re: Count test".into(),
            body: "Two".into(),
            thread_id: None,
            attachments: Vec::new(),
        })
        .unwrap();

        let result = s
            .evaluate_assertion(&serde_json::json!({
                "type": "thread_message_count",
                "thread_id": msg.thread_id,
                "count": 2,
            }))
            .unwrap();
        assert!(result.passed);
    }

    #[test]
    fn assertion_unknown_type() {
        let s = service();
        let err = s
            .evaluate_assertion(&serde_json::json!({
                "type": "bogus",
            }))
            .unwrap_err();
        assert!(err.to_string().contains("unknown assertion type"));
    }

    // --- Timeline actions ---

    #[test]
    fn timeline_action_send_message() {
        let mut s = service();
        let result = s
            .execute_timeline_action(
                &serde_json::json!({
                    "type": "send_message",
                    "to": ["bob@example.com"],
                    "subject": "Timeline test",
                    "body": "Hello from timeline",
                }),
                "alice",
            )
            .unwrap();
        assert_eq!(result.endpoint, "/gmail/messages/send");
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn timeline_action_unknown_type() {
        let mut s = service();
        let err = s
            .execute_timeline_action(
                &serde_json::json!({ "type": "unknown_action" }),
                "alice",
            )
            .unwrap_err();
        assert!(err.to_string().contains("unknown timeline action type"));
    }

    // --- Validation ---

    #[test]
    fn validate_scenario_detects_duplicate_ids() {
        let scenario = serde_json::json!({
            "initial_state": {
                "messages": [
                    { "id": "msg_1", "from": "a", "to": ["b"], "subject": "S" },
                    { "id": "msg_1", "from": "c", "to": ["d"], "subject": "S2" },
                ]
            }
        });
        let (errors, _) = GmailTwinService::validate_scenario(&scenario);
        assert!(errors.iter().any(|e| e.contains("duplicate message id")));
    }

    #[test]
    fn validate_scenario_detects_missing_fields() {
        let scenario = serde_json::json!({
            "initial_state": {
                "messages": [
                    { "id": "msg_1", "to": ["b"] },
                ]
            }
        });
        let (errors, _) = GmailTwinService::validate_scenario(&scenario);
        assert!(errors.iter().any(|e| e.contains("missing 'from'")));
        assert!(errors.iter().any(|e| e.contains("missing 'subject'")));
    }

    // --- State inspection ---

    #[test]
    fn state_inspection_returns_threads_and_messages() {
        let mut s = service();
        s.handle(GmailRequest::SendMessage {
            actor_id: "alice".into(),
            to: vec!["bob".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Inspect".into(),
            body: "Test".into(),
            thread_id: None,
            attachments: Vec::new(),
        })
        .unwrap();

        let nodes = s.inspect_state();
        let thread_nodes: Vec<_> = nodes.iter().filter(|n| n.kind == "thread").collect();
        let msg_nodes: Vec<_> = nodes.iter().filter(|n| n.kind == "message").collect();
        let label_nodes: Vec<_> = nodes.iter().filter(|n| n.kind == "label").collect();

        assert_eq!(thread_nodes.len(), 1);
        assert_eq!(msg_nodes.len(), 1);
        assert_eq!(label_nodes.len(), 13); // system labels

        // Message should be child of thread
        assert_eq!(msg_nodes[0].parent_id.as_ref().unwrap(), &thread_nodes[0].id);
    }

    #[test]
    fn state_inspection_inspect_node_by_id() {
        let mut s = service();
        let msg = match s
            .handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Find me".into(),
                body: "Here".into(),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap()
        {
            GmailResponse::Message(m) => m,
            _ => panic!("expected Message"),
        };

        let node = s.inspect_node(&msg.id).unwrap();
        assert_eq!(node.kind, "message");
        assert_eq!(node.label, "Find me");

        assert!(s.inspect_node("nonexistent").is_none());
    }

    // --- Pagination ---

    #[test]
    fn list_messages_pagination() {
        let mut s = service();
        for i in 0..5 {
            s.handle(GmailRequest::SendMessage {
                actor_id: "alice".into(),
                to: vec!["bob".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: format!("Msg {i}"),
                body: format!("Body {i}"),
                thread_id: None,
                attachments: Vec::new(),
            })
            .unwrap();
        }

        // Page 1: 2 results
        let page1 = match s
            .handle(GmailRequest::ListMessages {
                actor_id: "alice".into(),
                label_ids: Vec::new(),
                max_results: 2,
                page_token: None,
                q: None,
            })
            .unwrap()
        {
            GmailResponse::MessageList {
                messages,
                next_page_token,
                result_size_estimate,
            } => {
                assert_eq!(messages.len(), 2);
                assert!(next_page_token.is_some());
                assert_eq!(result_size_estimate, 5);
                next_page_token
            }
            _ => panic!("expected MessageList"),
        };

        // Page 2
        let page2 = match s
            .handle(GmailRequest::ListMessages {
                actor_id: "alice".into(),
                label_ids: Vec::new(),
                max_results: 2,
                page_token: page1,
                q: None,
            })
            .unwrap()
        {
            GmailResponse::MessageList {
                messages,
                next_page_token,
                ..
            } => {
                assert_eq!(messages.len(), 2);
                assert!(next_page_token.is_some());
                next_page_token
            }
            _ => panic!("expected MessageList"),
        };

        // Page 3 (last)
        match s
            .handle(GmailRequest::ListMessages {
                actor_id: "alice".into(),
                label_ids: Vec::new(),
                max_results: 2,
                page_token: page2,
                q: None,
            })
            .unwrap()
        {
            GmailResponse::MessageList {
                messages,
                next_page_token,
                ..
            } => {
                assert_eq!(messages.len(), 1);
                assert!(next_page_token.is_none());
            }
            _ => panic!("expected MessageList"),
        };
    }

    // --- Reset ---

    #[test]
    fn reset_clears_state() {
        let mut s = service();
        s.handle(GmailRequest::SendMessage {
            actor_id: "alice".into(),
            to: vec!["bob".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Test".into(),
            body: "Hi".into(),
            thread_id: None,
            attachments: Vec::new(),
        })
        .unwrap();
        assert!(!s.messages.is_empty());

        s.reset();
        assert!(s.messages.is_empty());
        assert!(s.threads.is_empty());
        assert_eq!(s.labels.len(), 13); // system labels restored
    }

    // --- Gmail query parsing (R4) ---

    #[test]
    fn parse_gmail_query_empty() {
        let f = parse_gmail_query("");
        assert!(f.include_labels.is_empty());
        assert!(f.exclude_labels.is_empty());
        assert!(f.after_ms.is_none());
        assert!(f.before_ms.is_none());
        assert!(f.from.is_none());
        assert!(f.subject.is_none());
    }

    #[test]
    fn parse_gmail_query_in_labels() {
        let f = parse_gmail_query("in:sent in:inbox");
        assert_eq!(f.include_labels, vec!["SENT", "INBOX"]);
    }

    #[test]
    fn parse_gmail_query_exclude_label() {
        let f = parse_gmail_query("-label:TRASH -label:SPAM");
        assert_eq!(f.exclude_labels, vec!["TRASH", "SPAM"]);
    }

    #[test]
    fn parse_gmail_query_date_range() {
        let f = parse_gmail_query("after:2024/01/15 before:2024/06/01");
        assert!(f.after_ms.is_some());
        assert!(f.before_ms.is_some());
        // 2024/01/15 = day 19737 since epoch => 19737 * 86400000 = 1705276800000
        assert_eq!(f.after_ms.unwrap(), 1_705_276_800_000);
        // 2024/06/01 = 1717200000000
        assert_eq!(f.before_ms.unwrap(), 1_717_200_000_000);
    }

    #[test]
    fn parse_gmail_query_from_and_subject() {
        let f = parse_gmail_query("from:alice@example.com subject:meeting");
        assert_eq!(f.from.as_deref(), Some("alice@example.com"));
        assert_eq!(f.subject.as_deref(), Some("meeting"));
    }

    #[test]
    fn parse_gmail_query_compound() {
        let f = parse_gmail_query("in:sent after:2024/01/01 -label:TRASH from:bob subject:report");
        assert_eq!(f.include_labels, vec!["SENT"]);
        assert_eq!(f.exclude_labels, vec!["TRASH"]);
        assert!(f.after_ms.is_some());
        assert_eq!(f.from.as_deref(), Some("bob"));
        assert_eq!(f.subject.as_deref(), Some("report"));
    }

    #[test]
    fn parse_gmail_query_unknown_tokens_ignored() {
        let f = parse_gmail_query("has:attachment is:unread in:inbox");
        // has: and is: are unknown, only in: is parsed
        assert_eq!(f.include_labels, vec!["INBOX"]);
    }

    #[test]
    fn parse_date_to_ms_valid() {
        // 1970/01/01 = epoch 0
        assert_eq!(parse_date_to_ms("1970/01/01"), Some(0));
        // 2000/01/01
        assert_eq!(parse_date_to_ms("2000/01/01"), Some(946_684_800_000));
        // Hyphen separator also works
        assert_eq!(parse_date_to_ms("2000-01-01"), Some(946_684_800_000));
    }

    #[test]
    fn parse_date_to_ms_invalid() {
        assert_eq!(parse_date_to_ms("not-a-date"), None);
        assert_eq!(parse_date_to_ms("2024/13/01"), None); // invalid month
        assert_eq!(parse_date_to_ms("2024"), None);
    }

    #[test]
    fn message_matches_query_include_label() {
        let msg = make_test_message("m1", "t1", vec!["INBOX", "UNREAD"], "alice@test.com", "Hello", 1_700_000_000_000);
        let f = parse_gmail_query("in:inbox");
        assert!(message_matches_query(&msg, &f));
        let f2 = parse_gmail_query("in:sent");
        assert!(!message_matches_query(&msg, &f2));
    }

    #[test]
    fn message_matches_query_exclude_label() {
        let msg = make_test_message("m1", "t1", vec!["INBOX", "TRASH"], "alice@test.com", "Hello", 1_700_000_000_000);
        let f = parse_gmail_query("-label:TRASH");
        assert!(!message_matches_query(&msg, &f));
        let msg2 = make_test_message("m2", "t2", vec!["INBOX"], "alice@test.com", "Hello", 1_700_000_000_000);
        assert!(message_matches_query(&msg2, &f));
    }

    #[test]
    fn message_matches_query_date_range() {
        // Message at 2024/06/15 = 1718409600000
        let msg = make_test_message("m1", "t1", vec!["INBOX"], "alice@test.com", "Hello", 1_718_409_600_000);
        let f = parse_gmail_query("after:2024/01/01 before:2024/12/31");
        assert!(message_matches_query(&msg, &f));
        let f2 = parse_gmail_query("after:2025/01/01");
        assert!(!message_matches_query(&msg, &f2));
    }

    #[test]
    fn message_matches_query_from_substring() {
        let msg = make_test_message("m1", "t1", vec!["INBOX"], "Alice Smith <alice@example.com>", "Hello", 1_700_000_000_000);
        let f = parse_gmail_query("from:alice");
        assert!(message_matches_query(&msg, &f));
        let f2 = parse_gmail_query("from:bob");
        assert!(!message_matches_query(&msg, &f2));
    }

    #[test]
    fn message_matches_query_subject_substring() {
        let msg = make_test_message("m1", "t1", vec!["INBOX"], "alice@test.com", "Weekly Meeting Notes", 1_700_000_000_000);
        let f = parse_gmail_query("subject:meeting");
        assert!(message_matches_query(&msg, &f));
        let f2 = parse_gmail_query("subject:budget");
        assert!(!message_matches_query(&msg, &f2));
    }

    #[test]
    fn list_messages_with_q_filter() {
        let mut s = service();
        // Send two messages — one from alice, one from bob
        s.handle(GmailRequest::SendMessage {
            actor_id: "user".into(),
            to: vec!["dest@test.com".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "From Alice".into(),
            body: "Hello from Alice".into(),
            thread_id: None,
            attachments: Vec::new(),
        }).unwrap();
        s.handle(GmailRequest::SendMessage {
            actor_id: "user".into(),
            to: vec!["dest@test.com".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "From Bob about budget".into(),
            body: "Budget report".into(),
            thread_id: None,
            attachments: Vec::new(),
        }).unwrap();

        // List all (no q)
        let all = match s.handle(GmailRequest::ListMessages {
            actor_id: "user".into(),
            label_ids: Vec::new(),
            max_results: 100,
            page_token: None,
            q: None,
        }).unwrap() {
            GmailResponse::MessageList { messages, .. } => messages,
            _ => panic!("expected MessageList"),
        };
        assert_eq!(all.len(), 2);

        // Filter by subject containing "budget"
        let filtered = match s.handle(GmailRequest::ListMessages {
            actor_id: "user".into(),
            label_ids: Vec::new(),
            max_results: 100,
            page_token: None,
            q: Some("subject:budget".into()),
        }).unwrap() {
            GmailResponse::MessageList { messages, .. } => messages,
            _ => panic!("expected MessageList"),
        };
        assert_eq!(filtered.len(), 1);

        // Filter by subject that matches nothing
        let none = match s.handle(GmailRequest::ListMessages {
            actor_id: "user".into(),
            label_ids: Vec::new(),
            max_results: 100,
            page_token: None,
            q: Some("subject:nonexistent".into()),
        }).unwrap() {
            GmailResponse::MessageList { messages, .. } => messages,
            _ => panic!("expected MessageList"),
        };
        assert_eq!(none.len(), 0);
    }

    /// Helper to create a GmailMessage for query-matching tests.
    fn make_test_message(
        id: &str,
        thread_id: &str,
        labels: Vec<&str>,
        from: &str,
        subject: &str,
        internal_date: u64,
    ) -> GmailMessage {
        GmailMessage {
            id: id.to_string(),
            thread_id: thread_id.to_string(),
            label_ids: labels.into_iter().map(|s| s.to_string()).collect(),
            from: from.to_string(),
            to: vec!["dest@test.com".to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: subject.to_string(),
            body_text: Some("body".to_string()),
            body_html: None,
            snippet: "body".to_string(),
            internal_date,
            size_estimate: 100,
            attachments: Vec::new(),
            history_id: 1,
        }
    }

    #[test]
    fn seed_internal_date_alias() {
        // The Whizy integration uses "internal_date" as the field name,
        // while the Rust struct uses "timestamp_ms". The serde alias should
        // make both work.
        let mut s = service();
        let initial_state = serde_json::json!({
            "messages": [
                {
                    "id": "msg_alias",
                    "thread_id": "thread_alias",
                    "from": "alice@example.com",
                    "to": ["bob@example.com"],
                    "subject": "Alias test",
                    "body": "body",
                    "label_ids": ["INBOX"],
                    "internal_date": 1700000000000u64
                }
            ]
        });
        s.seed_from_scenario(&initial_state).unwrap();
        let msg = s.messages.get("msg_alias").unwrap();
        assert_eq!(
            msg.internal_date, 1700000000000,
            "internal_date alias should set the message's internal_date"
        );
    }

    #[test]
    fn seed_timestamp_ms_still_works() {
        // Verify the original field name still works alongside the alias.
        let mut s = service();
        let initial_state = serde_json::json!({
            "messages": [
                {
                    "id": "msg_ts",
                    "thread_id": "thread_ts",
                    "from": "alice@example.com",
                    "to": ["bob@example.com"],
                    "subject": "Timestamp test",
                    "body": "body",
                    "label_ids": ["INBOX"],
                    "timestamp_ms": 1700000099000u64
                }
            ]
        });
        s.seed_from_scenario(&initial_state).unwrap();
        let msg = s.messages.get("msg_ts").unwrap();
        assert_eq!(
            msg.internal_date, 1700000099000,
            "timestamp_ms field should still work"
        );
    }

    #[test]
    fn seed_attachment_with_custom_id() {
        // When a seed attachment provides an explicit attachment_id,
        // the twin should use it instead of generating a new one.
        let mut s = service();
        let content = BASE64.encode(b"custom id data");
        let initial_state = serde_json::json!({
            "messages": [
                {
                    "id": "msg_att",
                    "thread_id": "thread_att",
                    "from": "alice@example.com",
                    "to": ["bob@example.com"],
                    "subject": "Custom att id",
                    "body": "See attached",
                    "label_ids": ["INBOX"],
                    "attachments": [
                        {
                            "attachment_id": "custom_att_42",
                            "filename": "report.pdf",
                            "mime_type": "application/pdf",
                            "content": content
                        }
                    ]
                }
            ]
        });
        s.seed_from_scenario(&initial_state).unwrap();
        let msg = s.messages.get("msg_att").unwrap();
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(
            msg.attachments[0].attachment_id, "custom_att_42",
            "seed-provided attachment_id should be preserved"
        );
        assert!(
            s.attachments.contains_key("custom_att_42"),
            "attachment data should be stored under the custom ID"
        );
    }

    #[test]
    fn seed_attachment_without_id_auto_generates() {
        // When no attachment_id is provided, the twin should still auto-generate.
        let mut s = service();
        let content = BASE64.encode(b"auto id data");
        let initial_state = serde_json::json!({
            "messages": [
                {
                    "id": "msg_auto_att",
                    "thread_id": "thread_auto_att",
                    "from": "alice@example.com",
                    "to": ["bob@example.com"],
                    "subject": "Auto att id",
                    "body": "See attached",
                    "label_ids": ["INBOX"],
                    "attachments": [
                        {
                            "filename": "notes.txt",
                            "mime_type": "text/plain",
                            "content": content
                        }
                    ]
                }
            ]
        });
        s.seed_from_scenario(&initial_state).unwrap();
        let msg = s.messages.get("msg_auto_att").unwrap();
        assert_eq!(msg.attachments.len(), 1);
        assert!(
            !msg.attachments[0].attachment_id.is_empty(),
            "auto-generated attachment_id should not be empty"
        );
        assert!(
            s.attachments.contains_key(&msg.attachments[0].attachment_id),
            "attachment data should be stored under the auto-generated ID"
        );
    }

    #[test]
    fn seed_reports_field_path_for_type_errors() {
        let mut s = service();
        let initial_state = serde_json::json!({
            "messages": [
                {
                    "id": "msg_bad",
                    "thread_id": "thread_bad",
                    "from": "alice@example.com",
                    "to": ["bob@example.com"],
                    "subject": "Bad",
                    "body": 123,
                    "label_ids": ["INBOX"]
                }
            ]
        });

        let err = s.seed_from_scenario(&initial_state).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid seed at $.messages"));
        assert!(msg.contains("body"));
    }
}
