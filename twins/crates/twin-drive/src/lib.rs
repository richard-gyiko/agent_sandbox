use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};
use twin_service::{
    AssertionResult, DiscoveryMeta, DiscoveryMethod, DiscoveryResource,
    ResolvedActorId, SharedTwinState, StateInspectable, StateNode,
    TimelineActionResult, TwinError, TwinService, TwinSnapshot, state_inspection_routes,
};

mod generated;
use generated::*;

// ---------------------------------------------------------------------------
// State inspection response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct StateItemPermission {
    pub actor_id: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateItem {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub parent_id: Option<String>,
    pub owner_id: String,
    pub permissions: Vec<StateItemPermission>,
    pub revision: u64,
    pub mime_type: Option<String>,
    pub size: Option<u64>,
    pub has_content: bool,
    pub app_properties: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeNode {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub owner_id: String,
    pub revision: u64,
    pub full_path: String,
    pub app_properties: serde_json::Value,
    pub children: Vec<TreeNode>,
}

type ActorId = String;
type ItemId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DriveItemKind {
    File,
    Folder,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PermissionRole {
    Owner,
    Editor,
    Viewer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Permission {
    pub actor_id: ActorId,
    pub role: PermissionRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveItem {
    pub id: ItemId,
    pub name: String,
    pub kind: DriveItemKind,
    pub parent_id: Option<ItemId>,
    pub owner_id: ActorId,
    pub permissions: Vec<Permission>,
    pub revision: u64,
    pub mime_type: Option<String>,
    pub size: Option<u64>,
    pub app_properties: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DriveRequest {
    Health,
    CreateFolder {
        actor_id: ActorId,
        parent_id: ItemId,
        name: String,
    },
    CreateFile {
        actor_id: ActorId,
        parent_id: ItemId,
        name: String,
    },
    ListChildren {
        actor_id: ActorId,
        parent_id: ItemId,
    },
    SetPermission {
        actor_id: ActorId,
        item_id: ItemId,
        target_actor_id: ActorId,
        role: PermissionRole,
    },
    MoveItem {
        actor_id: ActorId,
        item_id: ItemId,
        new_parent_id: ItemId,
    },
    GetItem {
        actor_id: ActorId,
        item_id: ItemId,
    },
    DeleteItem {
        actor_id: ActorId,
        item_id: ItemId,
    },
    UpdateItem {
        actor_id: ActorId,
        item_id: ItemId,
        new_name: Option<String>,
        new_parent_id: Option<ItemId>,
    },
    UploadContent {
        actor_id: ActorId,
        parent_id: ItemId,
        name: String,
        mime_type: Option<String>,
        content: Vec<u8>,
        app_properties: BTreeMap<String, String>,
    },
    DownloadContent {
        actor_id: ActorId,
        item_id: ItemId,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DriveResponse {
    Ok { service: &'static str },
    Created { item: DriveItem },
    Listed { items: Vec<DriveItem> },
    Updated { item: DriveItem },
    Got { item: DriveItem },
    Deleted { item_id: String },
    ContentCreated { item: DriveItem, size: u64 },
    Content { item: DriveItem, data: Vec<u8> },
}

#[derive(Serialize, Deserialize, TwinSnapshot)]
pub struct DriveTwinService {
    items: BTreeMap<ItemId, DriveItem>,
    #[twin_snapshot(encode = "base64")]
    content: BTreeMap<ItemId, Vec<u8>>,
    next_id: u64,
}

impl Default for DriveTwinService {
    fn default() -> Self {
        let root = DriveItem {
            id: "root".to_string(),
            name: "My Drive".to_string(),
            kind: DriveItemKind::Folder,
            parent_id: None,
            owner_id: "system".to_string(),
            permissions: vec![Permission {
                actor_id: "system".to_string(),
                role: PermissionRole::Owner,
            }],
            revision: 0,
            mime_type: Some("application/vnd.google-apps.folder".to_string()),
            size: None,
            app_properties: BTreeMap::new(),
        };

        let mut items = BTreeMap::new();
        items.insert(root.id.clone(), root);
        Self {
            items,
            content: BTreeMap::new(),
            next_id: 1,
        }
    }
}

impl DriveTwinService {
    fn new_item_id(&mut self) -> ItemId {
        let id = format!("item_{}", self.next_id);
        self.next_id += 1;
        id
    }

    fn has_role(item: &DriveItem, actor_id: &str, min_role: PermissionRole) -> bool {
        let rank = |role: &PermissionRole| match role {
            PermissionRole::Viewer => 1,
            PermissionRole::Editor => 2,
            PermissionRole::Owner => 3,
        };

        item.permissions
            .iter()
            .find(|p| p.actor_id == actor_id)
            .map(|p| rank(&p.role) >= rank(&min_role))
            .unwrap_or(false)
    }

    pub fn item_exists(&self, item_id: &str) -> bool {
        self.items.contains_key(item_id)
    }

    pub fn actor_can_access(&self, actor_id: &str, item_id: &str) -> bool {
        self.items
            .get(item_id)
            .map(|item| Self::has_role(item, actor_id, PermissionRole::Viewer))
            .unwrap_or(false)
    }

    pub fn has_orphans(&self) -> bool {
        self.items
            .values()
            .filter_map(|item| item.parent_id.as_deref())
            .any(|parent_id| !self.items.contains_key(parent_id))
    }

    pub fn seed_root(&mut self, owner_id: &str, name: Option<String>) -> Result<(), TwinError> {
        let root = self
            .items
            .get_mut("root")
            .ok_or_else(|| TwinError::Operation("root missing".to_string()))?;
        if let Some(name) = name {
            root.name = name;
        }
        root.owner_id = owner_id.to_string();
        root.permissions.retain(|p| p.actor_id != owner_id);
        root.permissions.push(Permission {
            actor_id: owner_id.to_string(),
            role: PermissionRole::Owner,
        });
        root.revision += 1;
        Ok(())
    }

    pub fn seed_item(
        &mut self,
        id: &str,
        name: String,
        parent_id: Option<String>,
        owner_id: String,
        kind: DriveItemKind,
    ) -> Result<(), TwinError> {
        if id == "root" {
            return self.seed_root(&owner_id, Some(name));
        }
        if self.items.contains_key(id) {
            return Err(TwinError::Operation("duplicate seed id".to_string()));
        }
        if let Some(parent_id) = &parent_id {
            let parent = self
                .items
                .get(parent_id)
                .ok_or_else(|| TwinError::Operation("seed parent not found".to_string()))?;
            if parent.kind != DriveItemKind::Folder {
                return Err(TwinError::Operation(
                    "seed parent must be folder".to_string(),
                ));
            }
        }

        let mime_type = match kind {
            DriveItemKind::Folder => Some("application/vnd.google-apps.folder".to_string()),
            DriveItemKind::File => None,
        };
        let item = DriveItem {
            id: id.to_string(),
            name,
            kind,
            parent_id,
            owner_id: owner_id.clone(),
            permissions: vec![Permission {
                actor_id: owner_id,
                role: PermissionRole::Owner,
            }],
            revision: 1,
            mime_type,
            size: None,
            app_properties: BTreeMap::new(),
        };
        self.bump_next_id(id);
        self.items.insert(item.id.clone(), item);
        Ok(())
    }

    fn bump_next_id(&mut self, id: &str) {
        if let Some(raw) = id.strip_prefix("item_") {
            if let Ok(n) = raw.parse::<u64>() {
                if n >= self.next_id {
                    self.next_id = n + 1;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // State inspection helpers
    // -----------------------------------------------------------------------

    fn drive_item_to_state_item(&self, item: &DriveItem) -> StateItem {
        StateItem {
            id: item.id.clone(),
            name: item.name.clone(),
            kind: match item.kind {
                DriveItemKind::File => "file".to_string(),
                DriveItemKind::Folder => "folder".to_string(),
            },
            parent_id: item.parent_id.clone(),
            owner_id: item.owner_id.clone(),
            permissions: item
                .permissions
                .iter()
                .map(|p| StateItemPermission {
                    actor_id: p.actor_id.clone(),
                    role: match p.role {
                        PermissionRole::Owner => "owner".to_string(),
                        PermissionRole::Editor => "editor".to_string(),
                        PermissionRole::Viewer => "viewer".to_string(),
                    },
                })
                .collect(),
            revision: item.revision,
            mime_type: item.mime_type.clone(),
            size: item.size,
            has_content: self.content.contains_key(&item.id),
            app_properties: serde_json::to_value(&item.app_properties)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
        }
    }

    /// Returns all items as a flat list of `StateItem`.
    pub fn state_items(&self) -> Vec<StateItem> {
        self.items
            .values()
            .map(|item| self.drive_item_to_state_item(item))
            .collect()
    }

    /// Returns a single item by ID as a `StateItem`, or `None` if not found.
    pub fn state_item(&self, id: &str) -> Option<StateItem> {
        self.items.get(id).map(|item| self.drive_item_to_state_item(item))
    }

    /// Builds a hierarchical tree rooted at "root". Orphans are omitted.
    ///
    /// Returns `None` if the root item is missing (should not happen under
    /// normal operation, but possible after a corrupt `service_restore`).
    pub fn state_tree(&self) -> Option<TreeNode> {
        // Build children lookup: parent_id -> Vec<&DriveItem>
        let mut children_map: BTreeMap<&str, Vec<&DriveItem>> = BTreeMap::new();
        for item in self.items.values() {
            if let Some(ref parent_id) = item.parent_id {
                children_map.entry(parent_id).or_default().push(item);
            }
        }

        let root = self.items.get("root")?;

        Some(Self::build_tree_node(root, &root.name, &children_map))
    }

    fn build_tree_node(
        item: &DriveItem,
        full_path: &str,
        children_map: &BTreeMap<&str, Vec<&DriveItem>>,
    ) -> TreeNode {
        let children = children_map
            .get(item.id.as_str())
            .map(|kids| {
                kids.iter()
                    .map(|child| {
                        let child_path = format!("{}/{}", full_path, child.name);
                        Self::build_tree_node(child, &child_path, children_map)
                    })
                    .collect()
            })
            .unwrap_or_default();

        TreeNode {
            id: item.id.clone(),
            name: item.name.clone(),
            kind: match item.kind {
                DriveItemKind::File => "file".to_string(),
                DriveItemKind::Folder => "folder".to_string(),
            },
            owner_id: item.owner_id.clone(),
            revision: item.revision,
            full_path: full_path.to_string(),
            app_properties: serde_json::to_value(&item.app_properties)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
            children,
        }
    }
}

// ---------------------------------------------------------------------------
// StateInspectable implementation (generic state inspection)
// ---------------------------------------------------------------------------

impl DriveTwinService {
    fn drive_item_to_state_node(&self, item: &DriveItem) -> StateNode {
        let mut properties = BTreeMap::new();
        properties.insert("owner_id".to_string(), serde_json::json!(item.owner_id));
        let perms: Vec<serde_json::Value> = item
            .permissions
            .iter()
            .map(|p| {
                serde_json::json!({
                    "actor_id": p.actor_id,
                    "role": match p.role {
                        PermissionRole::Owner => "owner",
                        PermissionRole::Editor => "editor",
                        PermissionRole::Viewer => "viewer",
                    }
                })
            })
            .collect();
        properties.insert("permissions".to_string(), serde_json::json!(perms));
        properties.insert("revision".to_string(), serde_json::json!(item.revision));
        if let Some(ref mime) = item.mime_type {
            properties.insert("mime_type".to_string(), serde_json::json!(mime));
        }
        if let Some(size) = item.size {
            properties.insert("size".to_string(), serde_json::json!(size));
        }
        properties.insert(
            "has_content".to_string(),
            serde_json::json!(self.content.contains_key(&item.id)),
        );
        properties.insert("app_properties".to_string(), serde_json::to_value(&item.app_properties).unwrap_or(serde_json::json!({})));

        StateNode {
            id: item.id.clone(),
            label: item.name.clone(),
            kind: match item.kind {
                DriveItemKind::File => "file".to_string(),
                DriveItemKind::Folder => "folder".to_string(),
            },
            parent_id: item.parent_id.clone(),
            properties,
        }
    }
}

impl StateInspectable for DriveTwinService {
    fn inspect_state(&self) -> Vec<StateNode> {
        self.items
            .values()
            .map(|item| self.drive_item_to_state_node(item))
            .collect()
    }

    fn inspect_node(&self, id: &str) -> Option<StateNode> {
        self.items
            .get(id)
            .map(|item| self.drive_item_to_state_node(item))
    }
}

// ---------------------------------------------------------------------------
// Domain logic — request dispatch
// ---------------------------------------------------------------------------

impl DriveTwinService {
    pub fn handle(&mut self, request: DriveRequest) -> Result<DriveResponse, TwinError> {
        match request {
            DriveRequest::Health => Ok(DriveResponse::Ok {
                service: "google-drive-twin",
            }),
            DriveRequest::CreateFolder {
                actor_id,
                parent_id,
                name,
            } => {
                let parent = self
                    .items
                    .get(&parent_id)
                    .ok_or_else(|| TwinError::Operation("parent not found".to_string()))?;
                if parent.kind != DriveItemKind::Folder {
                    return Err(TwinError::Operation(
                        "parent must be a folder".to_string(),
                    ));
                }
                if !Self::has_role(parent, &actor_id, PermissionRole::Editor) {
                    return Err(TwinError::Operation("permission denied".to_string()));
                }
                let id = self.new_item_id();
                let item = DriveItem {
                    id: id.clone(),
                    name,
                    kind: DriveItemKind::Folder,
                    parent_id: Some(parent_id),
                    owner_id: actor_id.clone(),
                    permissions: vec![Permission {
                        actor_id,
                        role: PermissionRole::Owner,
                    }],
                    revision: 1,
                    mime_type: Some("application/vnd.google-apps.folder".to_string()),
                    size: None,
                    app_properties: BTreeMap::new(),
                };
                self.items.insert(id, item.clone());
                Ok(DriveResponse::Created { item })
            }
            DriveRequest::CreateFile {
                actor_id,
                parent_id,
                name,
            } => {
                let parent = self
                    .items
                    .get(&parent_id)
                    .ok_or_else(|| TwinError::Operation("parent not found".to_string()))?;
                if parent.kind != DriveItemKind::Folder {
                    return Err(TwinError::Operation(
                        "parent must be a folder".to_string(),
                    ));
                }
                if !Self::has_role(parent, &actor_id, PermissionRole::Editor) {
                    return Err(TwinError::Operation("permission denied".to_string()));
                }
                let id = self.new_item_id();
                let item = DriveItem {
                    id: id.clone(),
                    name,
                    kind: DriveItemKind::File,
                    parent_id: Some(parent_id),
                    owner_id: actor_id.clone(),
                    permissions: vec![Permission {
                        actor_id,
                        role: PermissionRole::Owner,
                    }],
                    revision: 1,
                    mime_type: None,
                    size: None,
                    app_properties: BTreeMap::new(),
                };
                self.items.insert(id, item.clone());
                Ok(DriveResponse::Created { item })
            }
            DriveRequest::ListChildren {
                actor_id,
                parent_id,
            } => {
                let parent = self
                    .items
                    .get(&parent_id)
                    .ok_or_else(|| TwinError::Operation("parent not found".to_string()))?;
                if !Self::has_role(parent, &actor_id, PermissionRole::Viewer) {
                    return Err(TwinError::Operation("permission denied".to_string()));
                }
                let items = self
                    .items
                    .values()
                    .filter(|item| item.parent_id.as_deref() == Some(parent_id.as_str()))
                    .cloned()
                    .collect();
                Ok(DriveResponse::Listed { items })
            }
            DriveRequest::SetPermission {
                actor_id,
                item_id,
                target_actor_id,
                role,
            } => {
                let item = self
                    .items
                    .get_mut(&item_id)
                    .ok_or_else(|| TwinError::Operation("item not found".to_string()))?;
                if !Self::has_role(item, &actor_id, PermissionRole::Owner) {
                    return Err(TwinError::Operation(
                        "only owners can set permissions".to_string(),
                    ));
                }
                if let Some(existing) = item
                    .permissions
                    .iter_mut()
                    .find(|p| p.actor_id == target_actor_id)
                {
                    existing.role = role;
                } else {
                    item.permissions.push(Permission {
                        actor_id: target_actor_id,
                        role,
                    });
                }
                item.revision += 1;
                Ok(DriveResponse::Updated { item: item.clone() })
            }
            DriveRequest::MoveItem {
                actor_id,
                item_id,
                new_parent_id,
            } => {
                let dest_parent = self.items.get(&new_parent_id).ok_or_else(|| {
                    TwinError::Operation("destination parent not found".to_string())
                })?;
                if dest_parent.kind != DriveItemKind::Folder {
                    return Err(TwinError::Operation(
                        "destination parent must be a folder".to_string(),
                    ));
                }
                if !Self::has_role(dest_parent, &actor_id, PermissionRole::Editor) {
                    return Err(TwinError::Operation(
                        "permission denied on destination parent".to_string(),
                    ));
                }

                let item = self
                    .items
                    .get_mut(&item_id)
                    .ok_or_else(|| TwinError::Operation("item not found".to_string()))?;
                if item.id == "root" {
                    return Err(TwinError::Operation("cannot move root".to_string()));
                }
                if !Self::has_role(item, &actor_id, PermissionRole::Editor) {
                    return Err(TwinError::Operation("permission denied".to_string()));
                }

                item.parent_id = Some(new_parent_id);
                item.revision += 1;
                Ok(DriveResponse::Updated { item: item.clone() })
            }
            DriveRequest::GetItem { actor_id, item_id } => {
                let item = self
                    .items
                    .get(&item_id)
                    .ok_or_else(|| TwinError::Operation("item not found".to_string()))?;
                if !Self::has_role(item, &actor_id, PermissionRole::Viewer) {
                    return Err(TwinError::Operation("permission denied".to_string()));
                }
                Ok(DriveResponse::Got {
                    item: item.clone(),
                })
            }
            DriveRequest::DeleteItem { actor_id, item_id } => {
                let item = self
                    .items
                    .get(&item_id)
                    .ok_or_else(|| TwinError::Operation("item not found".to_string()))?;
                if item.id == "root" {
                    return Err(TwinError::Operation("cannot delete root".to_string()));
                }
                if !Self::has_role(item, &actor_id, PermissionRole::Editor) {
                    return Err(TwinError::Operation("permission denied".to_string()));
                }
                // Build parent->children index once (O(n)), then BFS is O(k).
                // Permission is checked only on the target item; descendants
                // are removed regardless of their own permissions (matches
                // Google Drive cascade-delete semantics).
                let mut children_map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
                for item in self.items.values() {
                    if let Some(ref pid) = item.parent_id {
                        children_map.entry(pid.as_str()).or_default().push(&item.id);
                    }
                }
                let mut to_delete = vec![item_id.clone()];
                let mut i = 0;
                while i < to_delete.len() {
                    if let Some(kids) = children_map.get(to_delete[i].as_str()) {
                        to_delete.extend(kids.iter().map(|s| s.to_string()));
                    }
                    i += 1;
                }
                for id in &to_delete {
                    self.items.remove(id);
                    self.content.remove(id);
                }
                Ok(DriveResponse::Deleted { item_id })
            }
            DriveRequest::UpdateItem {
                actor_id,
                item_id,
                new_name,
                new_parent_id,
            } => {
                // Move first (if requested)
                if let Some(ref new_pid) = new_parent_id {
                    let dest_parent = self.items.get(new_pid).ok_or_else(|| {
                        TwinError::Operation("destination parent not found".to_string())
                    })?;
                    if dest_parent.kind != DriveItemKind::Folder {
                        return Err(TwinError::Operation(
                            "destination parent must be a folder".to_string(),
                        ));
                    }
                    if !Self::has_role(dest_parent, &actor_id, PermissionRole::Editor) {
                        return Err(TwinError::Operation(
                            "permission denied on destination parent".to_string(),
                        ));
                    }
                }

                let item = self
                    .items
                    .get_mut(&item_id)
                    .ok_or_else(|| TwinError::Operation("item not found".to_string()))?;
                if item.id == "root" && new_parent_id.is_some() {
                    return Err(TwinError::Operation("cannot move root".to_string()));
                }
                if !Self::has_role(item, &actor_id, PermissionRole::Editor) {
                    return Err(TwinError::Operation("permission denied".to_string()));
                }

                let mut changed = false;
                if let Some(name) = new_name {
                    item.name = name;
                    changed = true;
                }
                if let Some(new_pid) = new_parent_id {
                    item.parent_id = Some(new_pid);
                    changed = true;
                }
                if changed {
                    item.revision += 1;
                }
                Ok(DriveResponse::Updated { item: item.clone() })
            }
            DriveRequest::UploadContent {
                actor_id,
                parent_id,
                name,
                mime_type,
                content,
                app_properties,
            } => {
                let parent = self
                    .items
                    .get(&parent_id)
                    .ok_or_else(|| TwinError::Operation("parent not found".to_string()))?;
                if parent.kind != DriveItemKind::Folder {
                    return Err(TwinError::Operation(
                        "parent must be a folder".to_string(),
                    ));
                }
                if !Self::has_role(parent, &actor_id, PermissionRole::Editor) {
                    return Err(TwinError::Operation("permission denied".to_string()));
                }
                let size = content.len() as u64;
                let id = self.new_item_id();
                let item = DriveItem {
                    id: id.clone(),
                    name,
                    kind: DriveItemKind::File,
                    parent_id: Some(parent_id),
                    owner_id: actor_id.clone(),
                    permissions: vec![Permission {
                        actor_id,
                        role: PermissionRole::Owner,
                    }],
                    revision: 1,
                    mime_type: Some(
                        mime_type.unwrap_or_else(|| "application/octet-stream".to_string()),
                    ),
                    size: Some(size),
                    app_properties,
                };
                self.items.insert(id.clone(), item.clone());
                self.content.insert(id, content);
                Ok(DriveResponse::ContentCreated { item, size })
            }
            DriveRequest::DownloadContent {
                actor_id,
                item_id,
            } => {
                let item = self
                    .items
                    .get(&item_id)
                    .ok_or_else(|| TwinError::Operation("item not found".to_string()))?;
                if !Self::has_role(item, &actor_id, PermissionRole::Viewer) {
                    return Err(TwinError::Operation("permission denied".to_string()));
                }
                let data = self
                    .content
                    .get(&item_id)
                    .ok_or_else(|| TwinError::Operation("file has no content".to_string()))?
                    .clone();
                Ok(DriveResponse::Content {
                    item: item.clone(),
                    data,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// parse_role — moved from main.rs (Drive-specific)
// ---------------------------------------------------------------------------

/// Parse a role string ("owner", "editor", "viewer") into a [`PermissionRole`].
pub fn parse_role(raw: &str) -> Option<PermissionRole> {
    match raw.to_ascii_lowercase().as_str() {
        "owner" => Some(PermissionRole::Owner),
        "editor" => Some(PermissionRole::Editor),
        "viewer" => Some(PermissionRole::Viewer),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// V3 helpers
// ---------------------------------------------------------------------------

/// Convert a `DriveItem` to a `V3File` response shape.
fn drive_item_to_v3_file(item: &DriveItem, include_permissions: bool) -> V3File {
    V3File {
        kind: "drive#file".to_string(),
        id: item.id.clone(),
        name: item.name.clone(),
        mime_type: item
            .mime_type
            .clone()
            .unwrap_or_else(|| match item.kind {
                DriveItemKind::Folder => "application/vnd.google-apps.folder".to_string(),
                DriveItemKind::File => "application/octet-stream".to_string(),
            }),
        parents: item.parent_id.iter().cloned().collect(),
        size: item.size.map(|s| s.to_string()),
        web_view_link: Some(format!("https://drive.google.com/file/d/{}/view", item.id)),
        app_properties: if item.app_properties.is_empty() {
            None
        } else {
            Some(item.app_properties.clone())
        },
        permissions: if include_permissions {
            Some(
                item.permissions
                    .iter()
                    .map(|p| V3Permission {
                        id: p.actor_id.clone(),
                        permission_type: "user".to_string(),
                        role: match p.role {
                            PermissionRole::Owner => "owner".to_string(),
                            PermissionRole::Editor => "writer".to_string(),
                            PermissionRole::Viewer => "reader".to_string(),
                        },
                        email_address: format!("{}@twin.local", p.actor_id),
                    })
                    .collect(),
            )
        } else {
            None
        },
    }
}

/// Parsed filter from a Drive V3 `q` query string.
#[derive(Debug, Default)]
struct V3QueryFilter {
    parent_id: Option<String>,
    name: Option<String>,
    mime_type: Option<String>,
    app_properties: Vec<(String, String)>,
}

/// Parse a Drive V3 `q` parameter into a structured filter.
///
/// Supports the following clause types joined by ` and `:
/// - `'<id>' in parents`
/// - `name='<value>'` or `name = '<value>'`
/// - `mimeType='<value>'` or `mimeType = '<value>'`
/// - `appProperties has {key='<k>' and value='<v>'}`
fn parse_v3_query(q: &str) -> V3QueryFilter {
    let mut filter = V3QueryFilter::default();
    // Split on ` and ` but NOT inside `{...}` blocks.
    let clauses = split_v3_clauses(q);

    for clause in &clauses {
        let clause = clause.trim();
        // Pattern: '<id>' in parents
        if clause.ends_with("in parents") {
            let prefix = clause.trim_end_matches("in parents").trim();
            if let Some(val) = strip_single_quotes(prefix) {
                filter.parent_id = Some(val);
                continue;
            }
        }
        // Pattern: name='<value>' or name = '<value>'
        if let Some(val) = extract_eq_value(clause, "name") {
            filter.name = Some(val);
            continue;
        }
        // Pattern: mimeType='<value>' or mimeType = '<value>'
        if let Some(val) = extract_eq_value(clause, "mimeType") {
            filter.mime_type = Some(val);
            continue;
        }
        // Pattern: appProperties has {key='<k>' and value='<v>'}
        if let Some(stripped) = clause.strip_prefix("appProperties has") {
            let stripped = stripped.trim();
            if let (Some(start), Some(end)) = (stripped.find('{'), stripped.rfind('}')) {
                let inner = &stripped[start + 1..end];
                let mut key = None;
                let mut value = None;
                for part in inner.split(" and ") {
                    let part = part.trim();
                    if let Some(v) = extract_eq_value(part, "key") {
                        key = Some(v);
                    } else if let Some(v) = extract_eq_value(part, "value") {
                        value = Some(v);
                    }
                }
                if let (Some(k), Some(v)) = (key, value) {
                    filter.app_properties.push((k, v));
                }
            }
            continue;
        }
    }
    filter
}

/// Split a V3 query on ` and ` delimiters, but don't split inside `{...}`.
fn split_v3_clauses(q: &str) -> Vec<String> {
    let mut clauses = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    let chars: Vec<char> = q.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        let ch = chars[i];
        if ch == '{' {
            depth += 1;
            current.push(ch);
            i += 1;
        } else if ch == '}' {
            depth = depth.saturating_sub(1);
            current.push(ch);
            i += 1;
        } else if depth == 0 && i + 5 <= len {
            // Check for " and " delimiter (5 chars)
            let window: String = chars[i..i + 5].iter().collect();
            if window == " and " {
                clauses.push(current.trim().to_string());
                current.clear();
                i += 5;
                continue;
            }
            current.push(ch);
            i += 1;
        } else {
            current.push(ch);
            i += 1;
        }
    }
    let remaining = current.trim().to_string();
    if !remaining.is_empty() {
        clauses.push(remaining);
    }
    clauses
}

/// Extract a single-quoted value from `key='value'` or `key = 'value'`.
fn extract_eq_value(clause: &str, key: &str) -> Option<String> {
    let clause = clause.trim();
    if !clause.starts_with(key) {
        return None;
    }
    let rest = clause[key.len()..].trim();
    let rest = rest.strip_prefix('=')?;
    strip_single_quotes(rest.trim())
}

/// Strip surrounding single quotes from a string, returning the inner value.
fn strip_single_quotes(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        Some(s[1..s.len() - 1].to_string())
    } else {
        None
    }
}

/// Check whether a `DriveItem` matches all the criteria in a [`V3QueryFilter`].
fn item_matches_filter(item: &DriveItem, filter: &V3QueryFilter) -> bool {
    if let Some(ref parent_id) = filter.parent_id {
        if item.parent_id.as_deref() != Some(parent_id.as_str()) {
            return false;
        }
    }
    if let Some(ref name) = filter.name {
        if item.name != *name {
            return false;
        }
    }
    if let Some(ref mime) = filter.mime_type {
        let item_mime = item.mime_type.as_deref().unwrap_or_else(|| match item.kind {
            DriveItemKind::Folder => "application/vnd.google-apps.folder",
            DriveItemKind::File => "application/octet-stream",
        });
        if item_mime != mime.as_str() {
            return false;
        }
    }
    for (key, value) in &filter.app_properties {
        match item.app_properties.get(key) {
            Some(v) if v == value => {}
            _ => return false,
        }
    }
    true
}

/// Extract actor ID from the auth middleware extension, or fall back to
/// reading `X-Twin-Actor-Id` header directly (for unit tests and contexts
/// without the auth middleware).
fn extract_actor_id_from_ext(
    ext: &Option<Extension<ResolvedActorId>>,
    headers: &axum::http::HeaderMap,
) -> String {
    if let Some(Extension(ResolvedActorId(id))) = ext {
        return id.clone();
    }
    // Fallback for contexts without the middleware (e.g. unit tests)
    headers
        .get("X-Twin-Actor-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("default")
        .to_string()
}

/// Parse a Google Drive v3 role string into a [`PermissionRole`].
fn parse_v3_role(raw: &str) -> Option<PermissionRole> {
    match raw.to_ascii_lowercase().as_str() {
        "owner" => Some(PermissionRole::Owner),
        "writer" => Some(PermissionRole::Editor),
        "reader" => Some(PermissionRole::Viewer),
        _ => None,
    }
}

/// Build a Google Drive v3 API error response JSON body.
fn v3_error_response(status: StatusCode, message: impl Into<String>) -> axum::response::Response {
    let code = status.as_u16();
    (
        status,
        Json(serde_json::json!({ "error": { "code": code, "message": message.into() } })),
    )
        .into_response()
}

/// Map a [`TwinError`] to the appropriate v3 error response, using string
/// matching on the message to select the HTTP status code.
fn twin_error_to_v3_response(e: TwinError) -> axum::response::Response {
    let msg = e.to_string();
    if msg.contains("not found") {
        v3_error_response(StatusCode::NOT_FOUND, msg)
    } else if msg.contains("permission denied") {
        v3_error_response(StatusCode::FORBIDDEN, msg)
    } else {
        v3_error_response(StatusCode::BAD_REQUEST, msg)
    }
}

// ---------------------------------------------------------------------------
// TwinService implementation
// ---------------------------------------------------------------------------

/// Intermediate structs for seeding from scenario JSON.
#[derive(Debug, Deserialize)]
struct SeedFile {
    id: String,
    name: String,
    parent_id: Option<String>,
    owner_id: String,
    kind: SeedItemKind,
    mime_type: Option<String>,
    /// Base64-encoded file content.
    content: Option<String>,
    app_properties: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
enum SeedItemKind {
    File,
    Folder,
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

impl TwinService for DriveTwinService {
    fn routes(shared: SharedTwinState<Self>) -> Router {
        Router::new()
            // Twin-native routes
            .route("/drive/folders", post(route_create_folder))
            .route("/drive/files", post(route_create_file))
            .route(
                "/drive/items/{parent_id}/children",
                get(route_list_children),
            )
            .route(
                "/drive/items/{item_id}/permissions",
                post(route_set_permission),
            )
            .route("/drive/items/{item_id}/move", post(route_move_item))
            .route("/drive/items/{item_id}", get(route_get_item))
            .route("/drive/items/{item_id}", delete(route_delete_item))
            // Google Drive v3 mimicry routes
            .route(
                "/drive/v3/files",
                get(route_v3_list_files).post(route_v3_create_file),
            )
            .route(
                "/drive/v3/files/{file_id}",
                get(route_v3_get_file)
                    .patch(route_v3_update_file)
                    .delete(route_v3_delete_file),
            )
            .route(
                "/drive/v3/files/{file_id}/permissions",
                post(route_v3_create_permission),
            )
            // Google Drive v3 upload route
            .route(
                "/upload/drive/v3/files",
                post(route_v3_upload_file).put(route_v3_resumable_chunk),
            )
            // State inspection routes (framework-provided)
            .merge(state_inspection_routes(shared.clone()))
            .with_state(shared)
    }

    fn discovery_meta() -> Option<DiscoveryMeta> {
        let mut files_methods = BTreeMap::new();

        // files.list
        files_methods.insert(
            "list".to_string(),
            DiscoveryMethod {
                id: "drive.files.list".to_string(),
                http_method: "GET".to_string(),
                path: "files".to_string(),
                description: "Lists the user's files.".to_string(),
                parameters: BTreeMap::from([
                    ("q".to_string(), serde_json::json!({
                        "type": "string",
                        "location": "query",
                        "description": "Query string for searching files."
                    })),
                    ("pageSize".to_string(), serde_json::json!({
                        "type": "integer",
                        "location": "query",
                        "description": "Maximum number of files to return."
                    })),
                    ("pageToken".to_string(), serde_json::json!({
                        "type": "string",
                        "location": "query",
                        "description": "Token for continuing a previous list request."
                    })),
                    ("fields".to_string(), serde_json::json!({
                        "type": "string",
                        "location": "query",
                        "description": "Selector specifying which fields to include."
                    })),
                    ("orderBy".to_string(), serde_json::json!({
                        "type": "string",
                        "location": "query",
                        "description": "Sort order for results."
                    })),
                    ("spaces".to_string(), serde_json::json!({
                        "type": "string",
                        "location": "query",
                        "description": "A comma-separated list of spaces to query (for example: drive, appDataFolder)."
                    })),
                ]),
                parameter_order: vec![],
                supports_media_upload: false,
                media_upload: None,
                request: None,
                response: Some(serde_json::json!({"$ref": "FileList"})),
            },
        );

        // files.get
        files_methods.insert(
            "get".to_string(),
            DiscoveryMethod {
                id: "drive.files.get".to_string(),
                http_method: "GET".to_string(),
                path: "files/{fileId}".to_string(),
                description: "Gets a file's metadata by ID.".to_string(),
                parameters: BTreeMap::from([
                    ("fileId".to_string(), serde_json::json!({
                        "type": "string",
                        "location": "path",
                        "required": true,
                        "description": "The ID of the file."
                    })),
                    ("fields".to_string(), serde_json::json!({
                        "type": "string",
                        "location": "query",
                        "description": "Selector specifying which fields to include."
                    })),
                ]),
                parameter_order: vec!["fileId".to_string()],
                supports_media_upload: false,
                media_upload: None,
                request: None,
                response: Some(serde_json::json!({"$ref": "File"})),
            },
        );

        // files.create
        files_methods.insert(
            "create".to_string(),
            DiscoveryMethod {
                id: "drive.files.create".to_string(),
                http_method: "POST".to_string(),
                path: "files".to_string(),
                description: "Creates a new file.".to_string(),
                parameters: BTreeMap::from([
                    ("fields".to_string(), serde_json::json!({
                        "type": "string",
                        "location": "query",
                        "description": "Selector specifying which fields to include."
                    })),
                ]),
                parameter_order: vec![],
                supports_media_upload: true,
                media_upload: Some(serde_json::json!({
                    "accept": ["*/*"],
                    "protocols": {
                        "simple": {
                            "multipart": true,
                            "path": "/upload/drive/v3/files"
                        },
                        "resumable": {
                            "multipart": true,
                            "path": "/upload/drive/v3/files"
                        }
                    }
                })),
                request: Some(serde_json::json!({"$ref": "File"})),
                response: Some(serde_json::json!({"$ref": "File"})),
            },
        );

        // files.update
        files_methods.insert(
            "update".to_string(),
            DiscoveryMethod {
                id: "drive.files.update".to_string(),
                http_method: "PATCH".to_string(),
                path: "files/{fileId}".to_string(),
                description: "Updates a file's metadata and/or content.".to_string(),
                parameters: BTreeMap::from([
                    ("fileId".to_string(), serde_json::json!({
                        "type": "string",
                        "location": "path",
                        "required": true,
                        "description": "The ID of the file to update."
                    })),
                    ("fields".to_string(), serde_json::json!({
                        "type": "string",
                        "location": "query",
                        "description": "Selector specifying which fields to include."
                    })),
                ]),
                parameter_order: vec!["fileId".to_string()],
                supports_media_upload: false,
                media_upload: None,
                request: Some(serde_json::json!({"$ref": "File"})),
                response: Some(serde_json::json!({"$ref": "File"})),
            },
        );

        // files.delete
        files_methods.insert(
            "delete".to_string(),
            DiscoveryMethod {
                id: "drive.files.delete".to_string(),
                http_method: "DELETE".to_string(),
                path: "files/{fileId}".to_string(),
                description: "Permanently deletes a file.".to_string(),
                parameters: BTreeMap::from([
                    ("fileId".to_string(), serde_json::json!({
                        "type": "string",
                        "location": "path",
                        "required": true,
                        "description": "The ID of the file."
                    })),
                ]),
                parameter_order: vec!["fileId".to_string()],
                supports_media_upload: false,
                media_upload: None,
                request: None,
                response: None,
            },
        );

        // Nested permissions resource under files
        let mut perms_methods = BTreeMap::new();
        perms_methods.insert(
            "create".to_string(),
            DiscoveryMethod {
                id: "drive.permissions.create".to_string(),
                http_method: "POST".to_string(),
                path: "files/{fileId}/permissions".to_string(),
                description: "Creates a permission for a file.".to_string(),
                parameters: BTreeMap::from([
                    ("fileId".to_string(), serde_json::json!({
                        "type": "string",
                        "location": "path",
                        "required": true,
                        "description": "The ID of the file."
                    })),
                ]),
                parameter_order: vec!["fileId".to_string()],
                supports_media_upload: false,
                media_upload: None,
                request: Some(serde_json::json!({"$ref": "Permission"})),
                response: Some(serde_json::json!({"$ref": "Permission"})),
            },
        );

        let files_resource = DiscoveryResource {
            methods: files_methods,
            resources: BTreeMap::from([(
                "permissions".to_string(),
                DiscoveryResource {
                    methods: perms_methods,
                    resources: BTreeMap::new(),
                },
            )]),
        };

        Some(DiscoveryMeta {
            name: "drive".to_string(),
            version: "v3".to_string(),
            title: "Google Drive API".to_string(),
            description: "Digital twin of the Google Drive API v3.".to_string(),
            service_path: "drive/v3/".to_string(),
            resources: BTreeMap::from([("files".to_string(), files_resource)]),
            schemas: serde_json::json!({
                "File": {
                    "id": "File",
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string"},
                        "id": {"type": "string"},
                        "name": {"type": "string"},
                        "mimeType": {"type": "string"}
                    }
                },
                "FileList": {
                    "id": "FileList",
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string"},
                        "files": {
                            "type": "array",
                            "items": {"$ref": "File"}
                        },
                        "nextPageToken": {"type": "string"}
                    }
                },
                "Permission": {
                    "id": "Permission",
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "type": {"type": "string"},
                        "role": {"type": "string"}
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
        let files_value = initial_state
            .get("files")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![]));
        let files: Vec<SeedFile> = deserialize_seed_field(&files_value, "$.files")?;

        // Process root first (if present).
        if let Some(root_seed) = files.iter().find(|f| f.id == "root") {
            self.seed_root(&root_seed.owner_id, Some(root_seed.name.clone()))?;
        }

        // Process remaining files with dependency resolution.
        let mut pending: Vec<&SeedFile> = files.iter().filter(|f| f.id != "root").collect();
        let mut stalled_rounds = 0usize;

        while !pending.is_empty() {
            let before = pending.len();
            let mut i = 0usize;
            while i < pending.len() {
                let seed = pending[i];
                let kind = match seed.kind {
                    SeedItemKind::File => DriveItemKind::File,
                    SeedItemKind::Folder => DriveItemKind::Folder,
                };
                let result = self.seed_item(
                    &seed.id,
                    seed.name.clone(),
                    seed.parent_id.clone(),
                    seed.owner_id.clone(),
                    kind,
                );
                match result {
                    Ok(()) => {
                        // Apply mime_type if provided.
                        if let Some(ref mt) = seed.mime_type {
                            if let Some(item) = self.items.get_mut(&seed.id) {
                                item.mime_type = Some(mt.clone());
                            }
                        }
                        // Apply app_properties if provided.
                        if let Some(ref ap) = seed.app_properties {
                            if let Some(item) = self.items.get_mut(&seed.id) {
                                item.app_properties = ap.clone();
                            }
                        }
                        // Decode and store content if provided.
                        if let Some(ref b64) = seed.content {
                            match BASE64.decode(b64) {
                                Ok(bytes) => {
                                    let size = bytes.len() as u64;
                                    if let Some(item) = self.items.get_mut(&seed.id) {
                                        item.size = Some(size);
                                    }
                                    self.content.insert(seed.id.clone(), bytes);
                                }
                                Err(e) => {
                                    return Err(TwinError::Operation(format!(
                                        "failed to decode base64 content for {}: {e}",
                                        seed.id
                                    )));
                                }
                            }
                        }
                        pending.remove(i);
                    }
                    Err(_) => i += 1,
                }
            }

            if pending.len() == before {
                stalled_rounds += 1;
                if stalled_rounds > 1 {
                    return Err(TwinError::Operation(
                        "invalid initial_state dependency graph".to_string(),
                    ));
                }
            } else {
                stalled_rounds = 0;
            }
        }

        Ok(())
    }

    fn evaluate_assertion(&self, check: &serde_json::Value) -> Result<AssertionResult, TwinError> {
        let check_type = check
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| TwinError::Operation("assertion check missing 'type'".to_string()))?;

        match check_type {
            "no_orphans" => {
                let has_orphans = self.has_orphans();
                Ok(AssertionResult {
                    id: "no_orphans".to_string(),
                    passed: !has_orphans,
                    message: if has_orphans {
                        "orphan items detected".to_string()
                    } else {
                        "no orphan items".to_string()
                    },
                })
            }
            "actor_can_access" => {
                let actor_id = check
                    .get("actor_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        TwinError::Operation("actor_can_access missing 'actor_id'".to_string())
                    })?;
                let item_id = check
                    .get("item_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        TwinError::Operation("actor_can_access missing 'item_id'".to_string())
                    })?;
                let can_access = self.actor_can_access(actor_id, item_id);
                Ok(AssertionResult {
                    id: "actor_can_access".to_string(),
                    passed: can_access,
                    message: if can_access {
                        format!("actor '{actor_id}' can access item '{item_id}'")
                    } else {
                        format!("actor '{actor_id}' cannot access item '{item_id}'")
                    },
                })
            }
            "item_exists" => {
                let item_id = check
                    .get("item_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        TwinError::Operation("item_exists missing 'item_id'".to_string())
                    })?;
                let exists = self.item_exists(item_id);
                Ok(AssertionResult {
                    id: "item_exists".to_string(),
                    passed: exists,
                    message: if exists {
                        format!("item '{item_id}' exists")
                    } else {
                        format!("item '{item_id}' does not exist")
                    },
                })
            }
            other => Err(TwinError::Operation(format!(
                "unknown assertion type: {other}"
            ))),
        }
    }

    fn validate_scenario(
        scenario: &serde_json::Value,
    ) -> (Vec<String>, Vec<String>) {
        let mut errors = Vec::new();
        let warnings = Vec::new();

        // Collect actor IDs for cross-referencing.
        let actor_ids: std::collections::BTreeSet<String> = scenario
            .get("actors")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        // --- initial_state validation ---
        if let Some(initial_state) = scenario.get("initial_state") {
            if let Some(files) = initial_state.get("files").and_then(|f| f.as_array()) {
                let mut file_ids = std::collections::BTreeSet::new();

                for file in files {
                    let id = file.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    if id.is_empty() {
                        errors.push("initial_state file id must not be empty".to_string());
                        continue;
                    }
                    if !file_ids.insert(id.to_string()) {
                        errors.push(format!("duplicate initial_state file id `{id}`"));
                    }
                }

                if !file_ids.contains("root") {
                    errors.push("initial_state must include `root`".to_string());
                }

                for file in files {
                    let id = file.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let parent_id = file.get("parent_id").and_then(|v| v.as_str());

                    if id == "root" && parent_id.is_some() {
                        errors.push("root must not have a parent_id".to_string());
                    }
                    if let Some(pid) = parent_id {
                        if !file_ids.contains(pid) {
                            errors.push(format!(
                                "file `{id}` references missing parent `{pid}` in initial_state"
                            ));
                        }
                    }
                }
            }
        }

        // --- assertion validation ---
        if let Some(assertions) = scenario.get("assertions").and_then(|v| v.as_array()) {
            for assertion in assertions {
                let assertion_id = assertion
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(check) = assertion.get("check") {
                    let check_type = check.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if check_type == "actor_can_access" {
                        if let Some(actor_id) = check.get("actor_id").and_then(|v| v.as_str()) {
                            if !actor_ids.contains(actor_id) {
                                errors.push(format!(
                                    "assertion `{assertion_id}` references unknown actor `{actor_id}`"
                                ));
                            }
                        }
                    }
                }
            }
        }

        // --- timeline action validation ---
        if let Some(timeline) = scenario.get("timeline").and_then(|v| v.as_array()) {
            for (idx, event) in timeline.iter().enumerate() {
                if let Some(action) = event.get("action") {
                    let action_type = action.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if action_type == "set_permission" {
                        if let Some(role) = action.get("role").and_then(|v| v.as_str()) {
                            if parse_role(role).is_none() {
                                errors.push(format!(
                                    "timeline event {idx} has invalid role `{role}` for set_permission"
                                ));
                            }
                        }
                    }
                }
            }
        }

        (errors, warnings)
    }

    fn execute_timeline_action(
        &mut self,
        action: &serde_json::Value,
        actor_id: &str,
    ) -> Result<TimelineActionResult, TwinError> {
        let action_type = action
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| TwinError::Operation("action missing 'type'".to_string()))?;

        let (endpoint, request) = match action_type {
            "create_file" => {
                let parent_id = json_str(action, "parent_id")?;
                let name = json_str(action, "name")?;
                (
                    "/drive/files".to_string(),
                    DriveRequest::CreateFile {
                        actor_id: actor_id.to_string(),
                        parent_id,
                        name,
                    },
                )
            }
            "create_folder" => {
                let parent_id = json_str(action, "parent_id")?;
                let name = json_str(action, "name")?;
                (
                    "/drive/folders".to_string(),
                    DriveRequest::CreateFolder {
                        actor_id: actor_id.to_string(),
                        parent_id,
                        name,
                    },
                )
            }
            "set_permission" => {
                let item_id = json_str(action, "item_id")?;
                let target_actor_id = json_str(action, "target_actor_id")?;
                let role_str = json_str(action, "role")?;
                let role = parse_role(&role_str).ok_or_else(|| {
                    TwinError::Operation(format!("invalid role: {role_str}"))
                })?;
                (
                    "/drive/items/{item_id}/permissions".to_string(),
                    DriveRequest::SetPermission {
                        actor_id: actor_id.to_string(),
                        item_id,
                        target_actor_id,
                        role,
                    },
                )
            }
            "get_item" => {
                let item_id = json_str(action, "item_id")?;
                (
                    "/drive/items/{item_id}".to_string(),
                    DriveRequest::GetItem {
                        actor_id: actor_id.to_string(),
                        item_id,
                    },
                )
            }
            "delete_item" => {
                let item_id = json_str(action, "item_id")?;
                (
                    "/drive/items/{item_id}".to_string(),
                    DriveRequest::DeleteItem {
                        actor_id: actor_id.to_string(),
                        item_id,
                    },
                )
            }
            other => {
                return Err(TwinError::Operation(format!(
                    "unknown timeline action type: {other}"
                )));
            }
        };

        let response = self.handle(request)?;
        let response_json = serde_json::to_value(&response)
            .map_err(|e| TwinError::Operation(format!("failed to serialize response: {e}")))?;

        Ok(TimelineActionResult {
            endpoint,
            response: response_json,
        })
    }
}

/// Helper to extract a string field from a JSON object.
fn json_str(value: &serde_json::Value, field: &str) -> Result<String, TwinError> {
    value
        .get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| TwinError::Operation(format!("missing field '{field}'")))
}

// ---------------------------------------------------------------------------
// HTTP route handlers
// ---------------------------------------------------------------------------

type DriveState = SharedTwinState<DriveTwinService>;

async fn route_create_folder(
    State(state): State<DriveState>,
    Json(body): Json<CreateItemBody>,
) -> impl IntoResponse {
    let mut rt = state.lock().await;
    let result = rt.service.handle(DriveRequest::CreateFolder {
        actor_id: body.actor_id,
        parent_id: body.parent_id,
        name: body.name,
    });
    drive_result_to_response(result)
}

async fn route_create_file(
    State(state): State<DriveState>,
    Json(body): Json<CreateItemBody>,
) -> impl IntoResponse {
    let mut rt = state.lock().await;
    let result = rt.service.handle(DriveRequest::CreateFile {
        actor_id: body.actor_id,
        parent_id: body.parent_id,
        name: body.name,
    });
    drive_result_to_response(result)
}

async fn route_list_children(
    State(state): State<DriveState>,
    Path(parent_id): Path<String>,
    Query(query): Query<ListChildrenQuery>,
) -> impl IntoResponse {
    let mut rt = state.lock().await;
    let result = rt.service.handle(DriveRequest::ListChildren {
        actor_id: query.actor_id,
        parent_id,
    });
    drive_result_to_response(result)
}

async fn route_set_permission(
    State(state): State<DriveState>,
    Path(item_id): Path<String>,
    Json(body): Json<SetPermissionBody>,
) -> impl IntoResponse {
    let role = match parse_role(&body.role) {
        Some(r) => r,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid role" })),
            )
                .into_response();
        }
    };

    let mut rt = state.lock().await;
    let result = rt.service.handle(DriveRequest::SetPermission {
        actor_id: body.actor_id,
        item_id,
        target_actor_id: body.target_actor_id,
        role,
    });
    drive_result_to_response(result)
}

async fn route_move_item(
    State(state): State<DriveState>,
    Path(item_id): Path<String>,
    Json(body): Json<MoveItemBody>,
) -> impl IntoResponse {
    let mut rt = state.lock().await;
    let result = rt.service.handle(DriveRequest::MoveItem {
        actor_id: body.actor_id,
        item_id,
        new_parent_id: body.new_parent_id,
    });
    drive_result_to_response(result)
}

async fn route_get_item(
    State(state): State<DriveState>,
    Path(item_id): Path<String>,
    Query(query): Query<ActorQuery>,
) -> impl IntoResponse {
    let mut rt = state.lock().await;
    let result = rt.service.handle(DriveRequest::GetItem {
        actor_id: query.actor_id,
        item_id,
    });
    drive_result_to_response(result)
}

async fn route_delete_item(
    State(state): State<DriveState>,
    Path(item_id): Path<String>,
    Query(query): Query<ActorQuery>,
) -> impl IntoResponse {
    let mut rt = state.lock().await;
    let result = rt.service.handle(DriveRequest::DeleteItem {
        actor_id: query.actor_id,
        item_id,
    });
    drive_result_to_response(result)
}

fn drive_result_to_response(result: Result<DriveResponse, TwinError>) -> axum::response::Response {
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
// Google Drive v3 mimicry route handlers
// ---------------------------------------------------------------------------

/// List files.  Accesses `rt.service.items` directly rather than going through
/// `handle_compat` because there is no single `DriveRequest` variant for "list
/// all visible items with optional parent filter + pagination".  Adding one
/// would bloat the domain model for little gain — this is a read-only query.
async fn route_v3_list_files(
    State(state): State<DriveState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Query(query): Query<V3ListFilesQuery>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id_from_ext(&resolved, &headers);
    let rt = state.lock().await;

    let items: Vec<&DriveItem> = if let Some(ref q) = query.q {
        let filter = parse_v3_query(q);

        // If query specifies a parent, check actor has access to that parent.
        if let Some(ref parent_id) = filter.parent_id {
            match rt.service.items.get(parent_id.as_str()) {
                Some(parent) if DriveTwinService::has_role(parent, &actor_id, PermissionRole::Viewer) => {}
                Some(_) => {
                    return v3_error_response(StatusCode::FORBIDDEN, "permission denied");
                }
                None => {
                    return v3_error_response(StatusCode::NOT_FOUND, "parent not found");
                }
            }
        }

        rt.service
            .items
            .values()
            .filter(|item| item.id != "root")
            .filter(|item| item_matches_filter(item, &filter))
            .filter(|item| DriveTwinService::has_role(item, &actor_id, PermissionRole::Viewer))
            .collect()
    } else {
        // No query — return all non-root items the actor can see
        rt.service
            .items
            .values()
            .filter(|item| item.id != "root")
            .filter(|item| DriveTwinService::has_role(item, &actor_id, PermissionRole::Viewer))
            .collect()
    };

    let mut files: Vec<V3File> = items
        .iter()
        .map(|item| drive_item_to_v3_file(item, false))
        .collect();

    if let Some(page_size) = query.page_size {
        files.truncate(page_size);
    }

    let list = V3FileList {
        kind: "drive#fileList".to_string(),
        files,
    };

    (StatusCode::OK, Json(list)).into_response()
}

async fn route_v3_get_file(
    State(state): State<DriveState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(file_id): Path<String>,
    Query(query): Query<V3GetFileQuery>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id_from_ext(&resolved, &headers);

    // alt=media means "download raw content"
    if query.alt.as_deref() == Some("media") {
        let mut rt = state.lock().await;
        let result = rt.service.handle(DriveRequest::DownloadContent {
            actor_id,
            item_id: file_id,
        });
        return match result {
            Ok(DriveResponse::Content { item, data }) => {
                let mime = item
                    .mime_type
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                let len = data.len().to_string();
                (
                    StatusCode::OK,
                    [
                        (axum::http::header::CONTENT_TYPE, mime),
                        (axum::http::header::CONTENT_LENGTH, len),
                    ],
                    data,
                )
                    .into_response()
            }
            Ok(_) => v3_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
            Err(e) => twin_error_to_v3_response(e),
        };
    }

    // Default: return JSON metadata
    let mut rt = state.lock().await;

    let result = rt.service.handle(DriveRequest::GetItem {
        actor_id,
        item_id: file_id,
    });

    match result {
        Ok(DriveResponse::Got { item }) => {
            let v3 = drive_item_to_v3_file(&item, true);
            (StatusCode::OK, Json(v3)).into_response()
        }
        Ok(_) => v3_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
        Err(e) => twin_error_to_v3_response(e),
    }
}

async fn route_v3_create_file(
    State(state): State<DriveState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<V3CreateFileBody>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id_from_ext(&resolved, &headers);
    let parent_id = body
        .parents
        .as_ref()
        .and_then(|p| p.first().cloned())
        .unwrap_or_else(|| "root".to_string());

    let is_folder = body
        .mime_type
        .as_deref()
        .map(|m| m == "application/vnd.google-apps.folder")
        .unwrap_or(false);

    let mut rt = state.lock().await;

    let request = if is_folder {
        DriveRequest::CreateFolder {
            actor_id,
            parent_id,
            name: body.name,
        }
    } else {
        DriveRequest::CreateFile {
            actor_id,
            parent_id,
            name: body.name,
        }
    };

    let result = rt.service.handle(request);

    match result {
        Ok(DriveResponse::Created { item }) => {
            let v3 = drive_item_to_v3_file(&item, true);
            (StatusCode::CREATED, Json(v3)).into_response()
        }
        Ok(_) => v3_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
        Err(e) => twin_error_to_v3_response(e),
    }
}

async fn route_v3_upload_file(
    State(state): State<DriveState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: HeaderMap,
    Query(query): Query<V3UploadQuery>,
    body: Bytes,
) -> impl IntoResponse {
    let actor_id = extract_actor_id_from_ext(&resolved, &headers);

    if query.upload_type.as_deref() == Some("resumable") {
        let metadata: MultipartMetadata = if body.is_empty() {
            MultipartMetadata::default()
        } else {
            match serde_json::from_slice(&body) {
                Ok(m) => m,
                Err(e) => {
                    return v3_error_response(
                        StatusCode::BAD_REQUEST,
                        format!("invalid JSON metadata for resumable upload: {e}"),
                    );
                }
            }
        };

        let upload_id = new_resumable_upload_id();
        let expected_len = headers
            .get("X-Upload-Content-Length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());

        let session = ResumableUploadSession {
            actor_id,
            name: metadata.name.unwrap_or_else(|| {
                query
                    .name
                    .clone()
                    .unwrap_or_else(|| "Untitled".to_string())
            }),
            parent_id: metadata
                .parents
                .and_then(|p| p.into_iter().next())
                .or_else(|| query.parents.clone())
                .unwrap_or_else(|| "root".to_string()),
            mime_type: metadata
                .mime_type
                .or_else(|| query.mime_type.clone())
                .or_else(|| {
                    headers
                        .get("X-Upload-Content-Type")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string())
                }),
            app_properties: metadata.app_properties.unwrap_or_default(),
            expected_len,
            data: Vec::new(),
        };

        {
            let mut uploads = resumable_upload_sessions().lock().await;
            uploads.insert(upload_id.clone(), session);
        }

        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("127.0.0.1:8080");
        let location = format!(
            "http://{host}/upload/drive/v3/files?uploadType=resumable&upload_id={upload_id}"
        );

        return (
            StatusCode::OK,
            [(axum::http::header::LOCATION, location)],
            Json(serde_json::json!({})),
        )
            .into_response();
    }

    // Detect multipart/related uploads (used by the Google Python SDK).
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if content_type.starts_with("multipart/related") {
        return handle_multipart_upload(state, actor_id, content_type, &body).await;
    }

    // Simple media upload (uploadType=media): the body IS the file content.
    let mime_type = query.mime_type.or_else(|| Some(content_type.to_string()));

    let name = query
        .name
        .unwrap_or_else(|| "Untitled".to_string());

    let parent_id = query
        .parents
        .unwrap_or_else(|| "root".to_string());

    // Parse app_properties from X-Twin-App-Properties header (JSON object).
    let app_properties: BTreeMap<String, String> = headers
        .get("X-Twin-App-Properties")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    let mut rt = state.lock().await;

    let result = rt.service.handle(DriveRequest::UploadContent {
        actor_id,
        parent_id,
        name,
        mime_type,
        content: body.to_vec(),
        app_properties,
    });

    match result {
        Ok(DriveResponse::ContentCreated { item, .. }) => {
            let v3 = drive_item_to_v3_file(&item, true);
            (StatusCode::CREATED, Json(v3)).into_response()
        }
        Ok(_) => v3_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
        Err(e) => twin_error_to_v3_response(e),
    }
}

#[derive(Debug, Deserialize)]
struct ResumableChunkQuery {
    upload_id: String,
}

#[derive(Debug, Clone)]
struct ResumableUploadSession {
    actor_id: String,
    name: String,
    parent_id: String,
    mime_type: Option<String>,
    app_properties: BTreeMap<String, String>,
    expected_len: Option<u64>,
    data: Vec<u8>,
}

static NEXT_RESUMABLE_UPLOAD_ID: AtomicU64 = AtomicU64::new(1);
static RESUMABLE_UPLOAD_SESSIONS: OnceLock<tokio::sync::Mutex<BTreeMap<String, ResumableUploadSession>>> =
    OnceLock::new();

fn resumable_upload_sessions(
) -> &'static tokio::sync::Mutex<BTreeMap<String, ResumableUploadSession>> {
    RESUMABLE_UPLOAD_SESSIONS.get_or_init(|| tokio::sync::Mutex::new(BTreeMap::new()))
}

fn new_resumable_upload_id() -> String {
    let id = NEXT_RESUMABLE_UPLOAD_ID.fetch_add(1, Ordering::Relaxed);
    format!("upload_{id}")
}

fn parse_content_range(header: &str) -> Option<(u64, u64, Option<u64>)> {
    // Expected forms:
    // - bytes 0-99/200
    // - bytes 0-99/*
    let rest = header.strip_prefix("bytes ")?;
    let mut parts = rest.split('/');
    let range_part = parts.next()?;
    let total_part = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let mut bounds = range_part.split('-');
    let start = bounds.next()?.parse::<u64>().ok()?;
    let end = bounds.next()?.parse::<u64>().ok()?;
    if bounds.next().is_some() || end < start {
        return None;
    }

    let total = if total_part == "*" {
        None
    } else {
        Some(total_part.parse::<u64>().ok()?)
    };

    Some((start, end, total))
}

async fn route_v3_resumable_chunk(
    State(state): State<DriveState>,
    Query(query): Query<ResumableChunkQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let content_range = headers
        .get("Content-Range")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_range);

    let Some((start, end, total_from_header)) = content_range else {
        return v3_error_response(
            StatusCode::BAD_REQUEST,
            "missing or invalid Content-Range for resumable upload chunk",
        );
    };

    if end.saturating_sub(start) + 1 != body.len() as u64 {
        return v3_error_response(
            StatusCode::BAD_REQUEST,
            "Content-Range does not match uploaded body length",
        );
    }

    let mut uploads = resumable_upload_sessions().lock().await;
    let Some(session) = uploads.get_mut(&query.upload_id) else {
        return v3_error_response(StatusCode::NOT_FOUND, "unknown resumable upload session");
    };

    if start != session.data.len() as u64 {
        let range_header = if session.data.is_empty() {
            "bytes */0".to_string()
        } else {
            format!("bytes=0-{}", session.data.len() - 1)
        };
        return (
            StatusCode::PERMANENT_REDIRECT,
            [("Range", range_header)],
            Json(serde_json::json!({})),
        )
            .into_response();
    }

    session.data.extend_from_slice(&body);

    let expected_total = total_from_header.or(session.expected_len);
    if let Some(total) = expected_total {
        if (session.data.len() as u64) < total {
            let range_header = format!("bytes=0-{}", session.data.len() - 1);
            return (
                StatusCode::PERMANENT_REDIRECT,
                [("Range", range_header)],
                Json(serde_json::json!({})),
            )
                .into_response();
        }
        if session.data.len() as u64 > total {
            return v3_error_response(
                StatusCode::BAD_REQUEST,
                "received more bytes than declared upload length",
            );
        }
    }

    let session = uploads.remove(&query.upload_id).expect("session exists");
    drop(uploads);

    let mut rt = state.lock().await;
    let result = rt.service.handle(DriveRequest::UploadContent {
        actor_id: session.actor_id,
        parent_id: session.parent_id,
        name: session.name,
        mime_type: session.mime_type,
        content: session.data,
        app_properties: session.app_properties,
    });

    match result {
        Ok(DriveResponse::ContentCreated { item, .. }) => {
            let v3 = drive_item_to_v3_file(&item, true);
            (StatusCode::CREATED, Json(v3)).into_response()
        }
        Ok(_) => v3_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
        Err(e) => twin_error_to_v3_response(e),
    }
}

/// Handle a `multipart/related` upload as sent by the Google Drive SDK.
///
/// The request body contains exactly two MIME parts separated by a boundary:
///   Part 1 — `application/json` metadata (name, parents, mimeType, appProperties)
///   Part 2 — file content
async fn handle_multipart_upload(
    state: DriveState,
    actor_id: String,
    content_type: &str,
    body: &[u8],
) -> axum::response::Response {
    // Extract boundary from Content-Type header.
    let boundary = match extract_boundary(content_type) {
        Some(b) => b,
        None => {
            return v3_error_response(
                StatusCode::BAD_REQUEST,
                "missing boundary in multipart/related Content-Type",
            );
        }
    };

    // Parse the multipart body into parts.
    let parts = match parse_multipart_related(body, &boundary) {
        Ok(p) if p.len() >= 2 => p,
        Ok(_) => {
            return v3_error_response(
                StatusCode::BAD_REQUEST,
                "multipart/related upload requires exactly 2 parts (metadata + content)",
            );
        }
        Err(msg) => {
            return v3_error_response(StatusCode::BAD_REQUEST, &msg);
        }
    };

    // Part 1: JSON metadata
    let metadata: MultipartMetadata = match serde_json::from_slice(&parts[0].body) {
        Ok(m) => m,
        Err(e) => {
            return v3_error_response(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON in metadata part: {e}"),
            );
        }
    };

    // Part 2: file content
    let content = parts[1].body.clone();

    let name = metadata.name.unwrap_or_else(|| "Untitled".to_string());
    let parent_id = metadata
        .parents
        .and_then(|p| p.into_iter().next())
        .unwrap_or_else(|| "root".to_string());
    let mime_type = metadata.mime_type;
    let app_properties = metadata.app_properties.unwrap_or_default();

    let mut rt = state.lock().await;

    let result = rt.service.handle(DriveRequest::UploadContent {
        actor_id,
        parent_id,
        name,
        mime_type,
        content,
        app_properties,
    });

    match result {
        Ok(DriveResponse::ContentCreated { item, .. }) => {
            let v3 = drive_item_to_v3_file(&item, true);
            (StatusCode::CREATED, Json(v3)).into_response()
        }
        Ok(_) => v3_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
        Err(e) => twin_error_to_v3_response(e),
    }
}

/// JSON metadata from the first part of a multipart/related upload.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MultipartMetadata {
    name: Option<String>,
    parents: Option<Vec<String>>,
    mime_type: Option<String>,
    app_properties: Option<BTreeMap<String, String>>,
}

/// A single part extracted from a multipart/related body.
struct MultipartPart {
    #[allow(dead_code)]
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Extract the `boundary` parameter from a `multipart/related` Content-Type.
fn extract_boundary(content_type: &str) -> Option<String> {
    for param in content_type.split(';').skip(1) {
        let param = param.trim();
        if let Some(rest) = param.strip_prefix("boundary=") {
            // Remove surrounding quotes if present.
            let b = rest.trim_matches('"');
            if !b.is_empty() {
                return Some(b.to_string());
            }
        }
    }
    None
}

/// Parse a multipart/related body into its constituent parts.
///
/// The format is:
/// ```text
/// --boundary\r\n
/// Content-Type: application/json\r\n
/// \r\n
/// {json}\r\n
/// --boundary\r\n
/// Content-Type: text/plain\r\n
/// \r\n
/// <file bytes>\r\n
/// --boundary--
/// ```
fn parse_multipart_related(body: &[u8], boundary: &str) -> Result<Vec<MultipartPart>, String> {
    let delimiter = format!("--{boundary}");
    let close = format!("--{boundary}--");

    // Split the body by the delimiter.
    // We work with bytes to support binary content in part 2.
    let delim_bytes = delimiter.as_bytes();
    let close_bytes = close.as_bytes();

    let mut parts = Vec::new();
    let mut pos = 0;

    // Skip preamble — find first delimiter.
    if let Some(start) = find_bytes(body, delim_bytes, pos) {
        pos = start + delim_bytes.len();
    } else {
        return Err("no multipart boundary found in body".to_string());
    }

    loop {
        // Skip \r\n after delimiter
        if body.get(pos..pos + 2) == Some(b"\r\n") {
            pos += 2;
        } else if body.get(pos..pos + 1) == Some(b"\n") {
            pos += 1;
        }

        // Check for closing delimiter
        if pos >= body.len() {
            break;
        }

        // Find the next delimiter
        let next_delim = find_bytes(body, delim_bytes, pos);
        let part_end = next_delim.unwrap_or(body.len());

        // The part includes headers + blank line + body
        let part_data = &body[pos..part_end];

        // Split headers from body at \r\n\r\n or \n\n
        let (headers, part_body) = split_headers_body(part_data);

        parts.push(MultipartPart {
            headers,
            body: part_body.to_vec(),
        });

        if let Some(nd) = next_delim {
            // Check if this is the closing delimiter
            if body.get(nd..nd + close_bytes.len()) == Some(close_bytes) {
                break;
            }
            pos = nd + delim_bytes.len();
        } else {
            break;
        }
    }

    Ok(parts)
}

/// Find the position of `needle` in `haystack` starting from `start`.
fn find_bytes(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    if needle.is_empty() || start + needle.len() > haystack.len() {
        return None;
    }
    haystack[start..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + start)
}

/// Split a MIME part into headers and body at the first blank line.
fn split_headers_body(data: &[u8]) -> (Vec<(String, String)>, &[u8]) {
    // Look for \r\n\r\n or \n\n
    let mut headers = Vec::new();

    let (header_end, body_start) = if let Some(pos) = find_bytes(data, b"\r\n\r\n", 0) {
        (pos, pos + 4)
    } else if let Some(pos) = find_bytes(data, b"\n\n", 0) {
        (pos, pos + 2)
    } else {
        // No blank line — entire part is body (or headers only, edge case)
        return (Vec::new(), data);
    };

    let header_str = String::from_utf8_lossy(&data[..header_end]);
    for line in header_str.lines() {
        if let Some((key, value)) = line.split_once(':') {
            headers.push((key.trim().to_string(), value.trim().to_string()));
        }
    }

    // Trim trailing \r\n before the next boundary from body.
    let mut body = &data[body_start..];
    if body.ends_with(b"\r\n") {
        body = &body[..body.len() - 2];
    } else if body.ends_with(b"\n") {
        body = &body[..body.len() - 1];
    }

    (headers, body)
}

async fn route_v3_update_file(
    State(state): State<DriveState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(file_id): Path<String>,
    Query(query): Query<V3UpdateFileQuery>,
    Json(body): Json<V3UpdateFileBody>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id_from_ext(&resolved, &headers);

    // Determine new parent from addParents (take first comma-separated value).
    // Filter out empty strings so that `?addParents=` is treated as "no move".
    let new_parent_id = query.add_parents.as_ref().and_then(|p| {
        let first = p.split(',').next().unwrap_or("").trim();
        if first.is_empty() { None } else { Some(first.to_string()) }
    });

    let mut rt = state.lock().await;

    let result = rt.service.handle(DriveRequest::UpdateItem {
        actor_id,
        item_id: file_id,
        new_name: body.name,
        new_parent_id,
    });

    match result {
        Ok(DriveResponse::Updated { item }) => {
            let v3 = drive_item_to_v3_file(&item, true);
            (StatusCode::OK, Json(v3)).into_response()
        }
        Ok(_) => v3_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
        Err(e) => twin_error_to_v3_response(e),
    }
}

async fn route_v3_create_permission(
    State(state): State<DriveState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(file_id): Path<String>,
    Json(body): Json<V3CreatePermissionBody>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id_from_ext(&resolved, &headers);

    let role = match parse_v3_role(&body.role) {
        Some(r) => r,
        None => {
            return v3_error_response(StatusCode::BAD_REQUEST, "invalid role");
        }
    };

    // Determine target actor: explicit actor_id field, or parse from emailAddress
    let target_actor_id = body
        .actor_id
        .or_else(|| {
            body.email_address
                .as_ref()
                .map(|e| e.split('@').next().unwrap_or(e).to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let mut rt = state.lock().await;

    let result = rt.service.handle(DriveRequest::SetPermission {
        actor_id,
        item_id: file_id,
        target_actor_id: target_actor_id.clone(),
        role: role.clone(),
    });

    match result {
        Ok(DriveResponse::Updated { .. }) => {
            let perm = V3Permission {
                id: target_actor_id.clone(),
                permission_type: "user".to_string(),
                role: body.role.to_lowercase(),
                email_address: format!("{target_actor_id}@twin.local"),
            };
            (StatusCode::OK, Json(perm)).into_response()
        }
        Ok(_) => v3_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
        Err(e) => twin_error_to_v3_response(e),
    }
}

async fn route_v3_delete_file(
    State(state): State<DriveState>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    Path(file_id): Path<String>,
) -> impl IntoResponse {
    let actor_id = extract_actor_id_from_ext(&resolved, &headers);
    let mut rt = state.lock().await;

    let result = rt.service.handle(DriveRequest::DeleteItem {
        actor_id,
        item_id: file_id,
    });

    match result {
        Ok(DriveResponse::Deleted { .. }) => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => v3_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
        Err(e) => twin_error_to_v3_response(e),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_item_ids() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        let first = svc
            .handle(DriveRequest::CreateFolder {
                actor_id: "alice".to_string(),
                parent_id: "root".to_string(),
                name: "A".to_string(),
            })
            .unwrap();
        let second = svc
            .handle(DriveRequest::CreateFile {
                actor_id: "alice".to_string(),
                parent_id: "root".to_string(),
                name: "B".to_string(),
            })
            .unwrap();

        let first_id = match first {
            DriveResponse::Created { item } => item.id,
            _ => panic!("unexpected response"),
        };
        let second_id = match second {
            DriveResponse::Created { item } => item.id,
            _ => panic!("unexpected response"),
        };

        assert_eq!(first_id, "item_1");
        assert_eq!(second_id, "item_2");
    }

    // -- snapshot / restore round-trip --

    #[test]
    fn snapshot_restore_round_trip() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();
        svc.handle(DriveRequest::CreateFolder {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "Docs".to_string(),
        })
        .unwrap();

        let snap = svc.service_snapshot();

        // Mutate state after snapshot.
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "extra.txt".to_string(),
        })
        .unwrap();
        assert_eq!(svc.next_id, 3);

        // Restore to the snapshot.
        svc.service_restore(&snap).unwrap();
        assert_eq!(svc.next_id, 2);
        assert!(svc.item_exists("item_1")); // Docs folder
        assert!(!svc.item_exists("item_2")); // extra.txt gone
    }

    #[test]
    fn snapshot_contains_expected_keys() {
        let svc = DriveTwinService::default();
        let snap = svc.service_snapshot();
        assert!(snap.get("items").is_some());
        assert!(snap.get("next_id").is_some());
        assert_eq!(snap["next_id"], 1);
    }

    // -- seed_from_scenario --

    #[test]
    fn seed_from_scenario_basic() {
        let mut svc = DriveTwinService::default();
        let initial_state = serde_json::json!({
            "files": [
                {
                    "id": "root",
                    "name": "Team Drive",
                    "parent_id": null,
                    "owner_id": "alice",
                    "kind": "Folder"
                },
                {
                    "id": "item_1",
                    "name": "Reports",
                    "parent_id": "root",
                    "owner_id": "alice",
                    "kind": "Folder"
                },
                {
                    "id": "item_2",
                    "name": "Q1.pdf",
                    "parent_id": "item_1",
                    "owner_id": "bob",
                    "kind": "File"
                }
            ]
        });

        svc.seed_from_scenario(&initial_state).unwrap();

        // Root was updated.
        let root = svc.items.get("root").unwrap();
        assert_eq!(root.name, "Team Drive");
        assert_eq!(root.owner_id, "alice");

        // Children seeded.
        assert!(svc.item_exists("item_1"));
        assert!(svc.item_exists("item_2"));
        let reports = svc.items.get("item_1").unwrap();
        assert_eq!(reports.kind, DriveItemKind::Folder);
        let q1 = svc.items.get("item_2").unwrap();
        assert_eq!(q1.kind, DriveItemKind::File);
        assert_eq!(q1.parent_id.as_deref(), Some("item_1"));

        // next_id should be bumped past item_2.
        assert!(svc.next_id >= 3);
    }

    #[test]
    fn seed_from_scenario_resolves_out_of_order_deps() {
        let mut svc = DriveTwinService::default();
        // Child listed before parent — should still resolve.
        let initial_state = serde_json::json!({
            "files": [
                {
                    "id": "root",
                    "name": "My Drive",
                    "parent_id": null,
                    "owner_id": "alice",
                    "kind": "Folder"
                },
                {
                    "id": "item_2",
                    "name": "deep.txt",
                    "parent_id": "item_1",
                    "owner_id": "alice",
                    "kind": "File"
                },
                {
                    "id": "item_1",
                    "name": "parent_folder",
                    "parent_id": "root",
                    "owner_id": "alice",
                    "kind": "Folder"
                }
            ]
        });

        svc.seed_from_scenario(&initial_state).unwrap();
        assert!(svc.item_exists("item_1"));
        assert!(svc.item_exists("item_2"));
    }

    #[test]
    fn seed_from_scenario_empty_files() {
        let mut svc = DriveTwinService::default();
        let initial_state = serde_json::json!({ "files": [] });
        svc.seed_from_scenario(&initial_state).unwrap();
        // Only root should exist.
        assert_eq!(svc.items.len(), 1);
    }

    #[test]
    fn seed_from_scenario_no_files_key() {
        let mut svc = DriveTwinService::default();
        let initial_state = serde_json::json!({});
        svc.seed_from_scenario(&initial_state).unwrap();
        assert_eq!(svc.items.len(), 1);
    }

    #[test]
    fn seed_from_scenario_reports_field_path_for_type_errors() {
        let mut svc = DriveTwinService::default();
        let initial_state = serde_json::json!({
            "files": [
                {
                    "id": 123,
                    "name": "bad",
                    "owner_id": "alice",
                    "kind": "File"
                }
            ]
        });

        let err = svc.seed_from_scenario(&initial_state).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid seed at $.files"));
        assert!(msg.contains("id"));
    }

    // -- evaluate_assertion --

    #[test]
    fn assertion_no_orphans_passes() {
        let svc = DriveTwinService::default();
        let check = serde_json::json!({ "type": "no_orphans" });
        let result = svc.evaluate_assertion(&check).unwrap();
        assert!(result.passed);
    }

    #[test]
    fn assertion_no_orphans_fails_when_orphans_exist() {
        let mut svc = DriveTwinService::default();
        // Insert item with non-existent parent to create orphan.
         svc.items.insert(
            "orphan".to_string(),
            DriveItem {
                id: "orphan".to_string(),
                name: "orphan.txt".to_string(),
                kind: DriveItemKind::File,
                parent_id: Some("nonexistent".to_string()),
                owner_id: "alice".to_string(),
                permissions: vec![],
                revision: 1,
                mime_type: None,
                size: None,
                app_properties: BTreeMap::new(),
            },
        );
        let check = serde_json::json!({ "type": "no_orphans" });
        let result = svc.evaluate_assertion(&check).unwrap();
        assert!(!result.passed);
    }

    #[test]
    fn assertion_actor_can_access() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();
        let check = serde_json::json!({
            "type": "actor_can_access",
            "actor_id": "alice",
            "item_id": "root"
        });
        let result = svc.evaluate_assertion(&check).unwrap();
        assert!(result.passed);
    }

    #[test]
    fn assertion_actor_can_access_denied() {
        let svc = DriveTwinService::default();
        let check = serde_json::json!({
            "type": "actor_can_access",
            "actor_id": "stranger",
            "item_id": "root"
        });
        let result = svc.evaluate_assertion(&check).unwrap();
        assert!(!result.passed);
    }

    #[test]
    fn assertion_item_exists_present() {
        let svc = DriveTwinService::default();
        let check = serde_json::json!({
            "type": "item_exists",
            "item_id": "root"
        });
        let result = svc.evaluate_assertion(&check).unwrap();
        assert!(result.passed);
    }

    #[test]
    fn assertion_item_exists_missing() {
        let svc = DriveTwinService::default();
        let check = serde_json::json!({
            "type": "item_exists",
            "item_id": "no_such_item"
        });
        let result = svc.evaluate_assertion(&check).unwrap();
        assert!(!result.passed);
    }

    #[test]
    fn assertion_unknown_type_returns_error() {
        let svc = DriveTwinService::default();
        let check = serde_json::json!({ "type": "bogus" });
        let result = svc.evaluate_assertion(&check);
        assert!(result.is_err());
    }

    #[test]
    fn assertion_missing_type_returns_error() {
        let svc = DriveTwinService::default();
        let check = serde_json::json!({ "actor_id": "a" });
        let result = svc.evaluate_assertion(&check);
        assert!(result.is_err());
    }

    // -- reset --

    #[test]
    fn reset_clears_state() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();
        svc.handle(DriveRequest::CreateFolder {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "stuff".to_string(),
        })
        .unwrap();
        assert!(svc.items.len() > 1);

        svc.reset();

        assert_eq!(svc.items.len(), 1);
        assert_eq!(svc.next_id, 1);
        let root = svc.items.get("root").unwrap();
        assert_eq!(root.owner_id, "system");
    }

    // -- execute_timeline_action --

    #[test]
    fn timeline_action_create_file() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        let action = serde_json::json!({
            "type": "create_file",
            "parent_id": "root",
            "name": "report.pdf"
        });
        let result = svc.execute_timeline_action(&action, "alice").unwrap();
        assert_eq!(result.endpoint, "/drive/files");
        assert!(result.response.get("Created").is_some());
    }

    #[test]
    fn timeline_action_create_folder() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        let action = serde_json::json!({
            "type": "create_folder",
            "parent_id": "root",
            "name": "Archives"
        });
        let result = svc.execute_timeline_action(&action, "alice").unwrap();
        assert_eq!(result.endpoint, "/drive/folders");
        assert!(result.response.get("Created").is_some());
    }

    #[test]
    fn timeline_action_set_permission() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        let action = serde_json::json!({
            "type": "set_permission",
            "item_id": "root",
            "target_actor_id": "bob",
            "role": "editor"
        });
        let result = svc.execute_timeline_action(&action, "alice").unwrap();
        assert_eq!(result.endpoint, "/drive/items/{item_id}/permissions");
        assert!(result.response.get("Updated").is_some());
    }

    #[test]
    fn timeline_action_unknown_type() {
        let mut svc = DriveTwinService::default();
        let action = serde_json::json!({ "type": "delete_everything" });
        let result = svc.execute_timeline_action(&action, "alice");
        assert!(result.is_err());
    }

    #[test]
    fn timeline_action_invalid_role() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        let action = serde_json::json!({
            "type": "set_permission",
            "item_id": "root",
            "target_actor_id": "bob",
            "role": "superadmin"
        });
        let result = svc.execute_timeline_action(&action, "alice");
        assert!(result.is_err());
    }

    // -- parse_role --

    #[test]
    fn parse_role_valid() {
        assert_eq!(parse_role("owner"), Some(PermissionRole::Owner));
        assert_eq!(parse_role("Editor"), Some(PermissionRole::Editor));
        assert_eq!(parse_role("VIEWER"), Some(PermissionRole::Viewer));
    }

    #[test]
    fn parse_role_invalid() {
        assert_eq!(parse_role("admin"), None);
        assert_eq!(parse_role(""), None);
    }

    // -- state inspection: state_items --

    #[test]
    fn state_items_returns_all_items() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();
        svc.handle(DriveRequest::CreateFolder {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "Docs".to_string(),
        })
        .unwrap();
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "readme.md".to_string(),
        })
        .unwrap();

        let items = svc.state_items();
        assert_eq!(items.len(), 3); // root + folder + file

        // Verify root
        let root = items.iter().find(|i| i.id == "root").unwrap();
        assert_eq!(root.name, "My Drive");
        assert_eq!(root.kind, "folder");
        assert!(root.parent_id.is_none());
        assert_eq!(root.owner_id, "alice");
        assert_eq!(root.app_properties, serde_json::json!({}));

        // Verify folder
        let folder = items.iter().find(|i| i.id == "item_1").unwrap();
        assert_eq!(folder.name, "Docs");
        assert_eq!(folder.kind, "folder");
        assert_eq!(folder.parent_id.as_deref(), Some("root"));

        // Verify file
        let file = items.iter().find(|i| i.id == "item_2").unwrap();
        assert_eq!(file.name, "readme.md");
        assert_eq!(file.kind, "file");
        assert_eq!(file.parent_id.as_deref(), Some("root"));
    }

    #[test]
    fn state_items_default_has_only_root() {
        let svc = DriveTwinService::default();
        let items = svc.state_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "root");
        assert_eq!(items[0].kind, "folder");
    }

    // -- state inspection: state_item --

    #[test]
    fn state_item_returns_correct_item() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "report.pdf".to_string(),
        })
        .unwrap();

        let item = svc.state_item("item_1").unwrap();
        assert_eq!(item.id, "item_1");
        assert_eq!(item.name, "report.pdf");
        assert_eq!(item.kind, "file");
        assert_eq!(item.parent_id.as_deref(), Some("root"));
        assert_eq!(item.owner_id, "alice");
        assert_eq!(item.revision, 1);
        assert_eq!(item.permissions.len(), 1);
        assert_eq!(item.permissions[0].actor_id, "alice");
        assert_eq!(item.permissions[0].role, "owner");
    }

    #[test]
    fn state_item_returns_none_for_missing() {
        let svc = DriveTwinService::default();
        assert!(svc.state_item("nonexistent").is_none());
    }

    // -- state inspection: state_tree --

    #[test]
    fn state_tree_default_has_root_only() {
        let svc = DriveTwinService::default();
        let tree = svc.state_tree().expect("root must exist");
        assert_eq!(tree.id, "root");
        assert_eq!(tree.name, "My Drive");
        assert_eq!(tree.kind, "folder");
        assert_eq!(tree.full_path, "My Drive");
        assert!(tree.children.is_empty());
    }

    #[test]
    fn state_tree_with_nested_structure() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        // Create: root -> Docs (folder) -> report.pdf (file)
        svc.handle(DriveRequest::CreateFolder {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "Docs".to_string(),
        })
        .unwrap();
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "item_1".to_string(),
            name: "report.pdf".to_string(),
        })
        .unwrap();

        let tree = svc.state_tree().expect("root must exist");
        assert_eq!(tree.full_path, "My Drive");
        assert_eq!(tree.children.len(), 1);

        let docs = &tree.children[0];
        assert_eq!(docs.id, "item_1");
        assert_eq!(docs.name, "Docs");
        assert_eq!(docs.kind, "folder");
        assert_eq!(docs.full_path, "My Drive/Docs");
        assert_eq!(docs.children.len(), 1);

        let report = &docs.children[0];
        assert_eq!(report.id, "item_2");
        assert_eq!(report.name, "report.pdf");
        assert_eq!(report.kind, "file");
        assert_eq!(report.full_path, "My Drive/Docs/report.pdf");
        assert!(report.children.is_empty());
    }

    #[test]
    fn state_tree_deeply_nested_paths() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        // root -> Projects (folder) -> 2025 (folder) -> plan.txt (file)
        svc.handle(DriveRequest::CreateFolder {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "Projects".to_string(),
        })
        .unwrap();
        svc.handle(DriveRequest::CreateFolder {
            actor_id: "alice".to_string(),
            parent_id: "item_1".to_string(),
            name: "2025".to_string(),
        })
        .unwrap();
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "item_2".to_string(),
            name: "plan.txt".to_string(),
        })
        .unwrap();

        let tree = svc.state_tree().expect("root must exist");

        let projects = &tree.children[0];
        assert_eq!(projects.full_path, "My Drive/Projects");

        let year = &projects.children[0];
        assert_eq!(year.full_path, "My Drive/Projects/2025");

        let plan = &year.children[0];
        assert_eq!(plan.full_path, "My Drive/Projects/2025/plan.txt");
    }

    #[test]
    fn state_tree_omits_orphans() {
        let mut svc = DriveTwinService::default();
        // Manually insert an orphan (parent doesn't exist)
        svc.items.insert(
            "orphan".to_string(),
            DriveItem {
                id: "orphan".to_string(),
                name: "orphan.txt".to_string(),
                kind: DriveItemKind::File,
                parent_id: Some("nonexistent".to_string()),
                owner_id: "alice".to_string(),
                permissions: vec![],
                revision: 1,
                mime_type: None,
                size: None,
                app_properties: BTreeMap::new(),
            },
        );

        let tree = svc.state_tree().expect("root must exist");
        // Orphan should not appear in the tree
        assert!(tree.children.is_empty());
    }

    #[test]
    fn state_tree_app_properties_is_empty_object() {
        let svc = DriveTwinService::default();
        let tree = svc.state_tree().expect("root must exist");
        assert_eq!(tree.app_properties, serde_json::json!({}));
    }

    #[test]
    fn state_item_permissions_serialized_as_lowercase() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();
        svc.handle(DriveRequest::SetPermission {
            actor_id: "alice".to_string(),
            item_id: "root".to_string(),
            target_actor_id: "bob".to_string(),
            role: PermissionRole::Editor,
        })
        .unwrap();
        svc.handle(DriveRequest::SetPermission {
            actor_id: "alice".to_string(),
            item_id: "root".to_string(),
            target_actor_id: "charlie".to_string(),
            role: PermissionRole::Viewer,
        })
        .unwrap();

        let item = svc.state_item("root").unwrap();
        let roles: Vec<&str> = item.permissions.iter().map(|p| p.role.as_str()).collect();
        assert!(roles.contains(&"owner"));
        assert!(roles.contains(&"editor"));
        assert!(roles.contains(&"viewer"));
    }

    // -- validate_scenario --

    #[test]
    fn validate_scenario_valid() {
        let scenario = serde_json::json!({
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" },
                    { "id": "f1", "name": "Doc", "parent_id": "root", "owner_id": "alice", "kind": "File" }
                ]
            },
            "timeline": [],
            "assertions": []
        });
        let (errors, warnings) = DriveTwinService::validate_scenario(&scenario);
        assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
        assert!(warnings.is_empty());
    }

    #[test]
    fn validate_scenario_missing_root() {
        let scenario = serde_json::json!({
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "f1", "name": "Doc", "parent_id": "root", "owner_id": "alice", "kind": "File" }
                ]
            },
            "timeline": [],
            "assertions": []
        });
        let (errors, _) = DriveTwinService::validate_scenario(&scenario);
        assert!(errors.iter().any(|e| e.contains("root")), "expected root error, got: {errors:?}");
    }

    #[test]
    fn validate_scenario_duplicate_file_ids() {
        let scenario = serde_json::json!({
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" },
                    { "id": "root", "name": "Dup", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                ]
            },
            "timeline": [],
            "assertions": []
        });
        let (errors, _) = DriveTwinService::validate_scenario(&scenario);
        assert!(errors.iter().any(|e| e.contains("duplicate")), "expected duplicate error, got: {errors:?}");
    }

    #[test]
    fn validate_scenario_invalid_timeline_role() {
        let scenario = serde_json::json!({
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                ]
            },
            "timeline": [
                { "at_ms": 1000, "actor_id": "alice", "action": { "type": "set_permission", "item_id": "root", "target_actor_id": "bob", "role": "superadmin" } }
            ],
            "assertions": []
        });
        let (errors, _) = DriveTwinService::validate_scenario(&scenario);
        assert!(errors.iter().any(|e| e.contains("role")), "expected role error, got: {errors:?}");
    }

    #[test]
    fn validate_scenario_assertion_unknown_actor() {
        let scenario = serde_json::json!({
            "actors": [{ "id": "alice", "label": "Alice" }],
            "initial_state": {
                "files": [
                    { "id": "root", "name": "My Drive", "parent_id": null, "owner_id": "alice", "kind": "Folder" }
                ]
            },
            "timeline": [],
            "assertions": [
                { "id": "a1", "check": { "type": "actor_can_access", "actor_id": "ghost", "item_id": "root" } }
            ]
        });
        let (errors, _) = DriveTwinService::validate_scenario(&scenario);
        assert!(errors.iter().any(|e| e.contains("ghost")), "expected unknown actor error, got: {errors:?}");
    }

    // -- GetItem --

    #[test]
    fn get_item_success() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "notes.txt".to_string(),
        })
        .unwrap();

        let result = svc
            .handle(DriveRequest::GetItem {
                actor_id: "alice".to_string(),
                item_id: "item_1".to_string(),
            })
            .unwrap();

        match result {
            DriveResponse::Got { item } => {
                assert_eq!(item.id, "item_1");
                assert_eq!(item.name, "notes.txt");
                assert_eq!(item.kind, DriveItemKind::File);
                assert_eq!(item.parent_id.as_deref(), Some("root"));
                assert_eq!(item.owner_id, "alice");
            }
            other => panic!("expected Got, got: {other:?}"),
        }
    }

    #[test]
    fn get_item_not_found() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        let result = svc.handle(DriveRequest::GetItem {
            actor_id: "alice".to_string(),
            item_id: "nonexistent".to_string(),
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn get_item_permission_denied() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "secret.txt".to_string(),
        })
        .unwrap();

        let result = svc.handle(DriveRequest::GetItem {
            actor_id: "stranger".to_string(),
            item_id: "item_1".to_string(),
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("permission denied"));
    }

    // -- DeleteItem --

    #[test]
    fn delete_item_removes_item() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "temp.txt".to_string(),
        })
        .unwrap();
        assert!(svc.item_exists("item_1"));

        let result = svc
            .handle(DriveRequest::DeleteItem {
                actor_id: "alice".to_string(),
                item_id: "item_1".to_string(),
            })
            .unwrap();

        match result {
            DriveResponse::Deleted { item_id } => assert_eq!(item_id, "item_1"),
            other => panic!("expected Deleted, got: {other:?}"),
        }
        assert!(!svc.item_exists("item_1"));
    }

    #[test]
    fn delete_item_recursive() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        // root -> folder (item_1) -> subfolder (item_2) -> file (item_3)
        //                          -> file2 (item_4)
        svc.handle(DriveRequest::CreateFolder {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "folder".to_string(),
        })
        .unwrap();
        svc.handle(DriveRequest::CreateFolder {
            actor_id: "alice".to_string(),
            parent_id: "item_1".to_string(),
            name: "subfolder".to_string(),
        })
        .unwrap();
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "item_2".to_string(),
            name: "deep.txt".to_string(),
        })
        .unwrap();
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "item_1".to_string(),
            name: "shallow.txt".to_string(),
        })
        .unwrap();

        assert!(svc.item_exists("item_1"));
        assert!(svc.item_exists("item_2"));
        assert!(svc.item_exists("item_3"));
        assert!(svc.item_exists("item_4"));

        // Delete the top-level folder — should remove all descendants
        svc.handle(DriveRequest::DeleteItem {
            actor_id: "alice".to_string(),
            item_id: "item_1".to_string(),
        })
        .unwrap();

        assert!(!svc.item_exists("item_1"));
        assert!(!svc.item_exists("item_2"));
        assert!(!svc.item_exists("item_3"));
        assert!(!svc.item_exists("item_4"));
        // Root should still exist
        assert!(svc.item_exists("root"));
    }

    #[test]
    fn delete_item_cannot_delete_root() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        let result = svc.handle(DriveRequest::DeleteItem {
            actor_id: "alice".to_string(),
            item_id: "root".to_string(),
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot delete root"));
    }

    #[test]
    fn delete_item_permission_denied() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "protected.txt".to_string(),
        })
        .unwrap();

        // Give stranger only Viewer access
        svc.handle(DriveRequest::SetPermission {
            actor_id: "alice".to_string(),
            item_id: "item_1".to_string(),
            target_actor_id: "stranger".to_string(),
            role: PermissionRole::Viewer,
        })
        .unwrap();

        let result = svc.handle(DriveRequest::DeleteItem {
            actor_id: "stranger".to_string(),
            item_id: "item_1".to_string(),
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("permission denied"));
    }

    #[test]
    fn delete_item_not_found() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        let result = svc.handle(DriveRequest::DeleteItem {
            actor_id: "alice".to_string(),
            item_id: "nonexistent".to_string(),
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    // -- Timeline actions for GetItem / DeleteItem --

    #[test]
    fn timeline_action_get_item() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "doc.txt".to_string(),
        })
        .unwrap();

        let action = serde_json::json!({
            "type": "get_item",
            "item_id": "item_1"
        });
        let result = svc.execute_timeline_action(&action, "alice").unwrap();
        assert_eq!(result.endpoint, "/drive/items/{item_id}");
        assert!(result.response.get("Got").is_some());
    }

    #[test]
    fn timeline_action_delete_item() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "trash.txt".to_string(),
        })
        .unwrap();
        assert!(svc.item_exists("item_1"));

        let action = serde_json::json!({
            "type": "delete_item",
            "item_id": "item_1"
        });
        let result = svc.execute_timeline_action(&action, "alice").unwrap();
        assert_eq!(result.endpoint, "/drive/items/{item_id}");
        assert!(result.response.get("Deleted").is_some());
        assert!(!svc.item_exists("item_1"));
    }

    // -- UpdateItem --

    #[test]
    fn update_item_rename() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "old.txt".to_string(),
        })
        .unwrap();

        let result = svc
            .handle(DriveRequest::UpdateItem {
                actor_id: "alice".to_string(),
                item_id: "item_1".to_string(),
                new_name: Some("new.txt".to_string()),
                new_parent_id: None,
            })
            .unwrap();

        match result {
            DriveResponse::Updated { item } => {
                assert_eq!(item.name, "new.txt");
                assert_eq!(item.revision, 2);
            }
            other => panic!("expected Updated, got: {other:?}"),
        }
    }

    #[test]
    fn update_item_move_and_rename() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();
        svc.handle(DriveRequest::CreateFolder {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "dest".to_string(),
        })
        .unwrap();
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "old.txt".to_string(),
        })
        .unwrap();

        let result = svc
            .handle(DriveRequest::UpdateItem {
                actor_id: "alice".to_string(),
                item_id: "item_2".to_string(),
                new_name: Some("renamed.txt".to_string()),
                new_parent_id: Some("item_1".to_string()),
            })
            .unwrap();

        match result {
            DriveResponse::Updated { item } => {
                assert_eq!(item.name, "renamed.txt");
                assert_eq!(item.parent_id.as_deref(), Some("item_1"));
                assert_eq!(item.revision, 2);
            }
            other => panic!("expected Updated, got: {other:?}"),
        }
    }

    // -- V3 query parser --

    #[test]
    fn parse_v3_query_parent_clause() {
        let filter = parse_v3_query("'root' in parents");
        assert_eq!(filter.parent_id, Some("root".to_string()));
        assert!(filter.name.is_none());
        assert!(filter.mime_type.is_none());
        assert!(filter.app_properties.is_empty());

        let filter = parse_v3_query("'item_1' in parents");
        assert_eq!(filter.parent_id, Some("item_1".to_string()));
    }

    #[test]
    fn parse_v3_query_name_and_mime_type() {
        let filter = parse_v3_query("name='test.txt'");
        assert_eq!(filter.name, Some("test.txt".to_string()));
        assert!(filter.parent_id.is_none());

        let filter = parse_v3_query("mimeType='application/vnd.google-apps.folder'");
        assert_eq!(filter.mime_type, Some("application/vnd.google-apps.folder".to_string()));
    }

    #[test]
    fn parse_v3_query_compound() {
        let q = "'root' in parents and name='doc.txt' and mimeType='text/plain'";
        let filter = parse_v3_query(q);
        assert_eq!(filter.parent_id, Some("root".to_string()));
        assert_eq!(filter.name, Some("doc.txt".to_string()));
        assert_eq!(filter.mime_type, Some("text/plain".to_string()));
    }

    #[test]
    fn parse_v3_query_app_properties() {
        let q = "'root' in parents and appProperties has {key='sha256' and value='abc123'}";
        let filter = parse_v3_query(q);
        assert_eq!(filter.parent_id, Some("root".to_string()));
        assert_eq!(filter.app_properties, vec![("sha256".to_string(), "abc123".to_string())]);
    }

    #[test]
    fn parse_v3_query_empty_string() {
        let filter = parse_v3_query("");
        assert!(filter.parent_id.is_none());
        assert!(filter.name.is_none());
        assert!(filter.mime_type.is_none());
        assert!(filter.app_properties.is_empty());
    }

    // -- V3 integration tests --

    use std::sync::Arc;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    fn test_drive_state() -> DriveState {
        use twin_kernel::{TwinConfig, TwinKernel};
        use twin_service::TwinRuntime;
        let kernel = TwinKernel::new(TwinConfig {
            seed: 42,
            start_time_unix_ms: 1000,
        });
        let mut service = DriveTwinService::default();
        service.seed_root("alice", None).unwrap();
        Arc::new(tokio::sync::Mutex::new(TwinRuntime::new(kernel, service)))
    }

    async fn read_json(response: axum::http::Response<Body>) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn v3_create_folder() {
        let state = test_drive_state();
        let app = DriveTwinService::routes(state.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/drive/v3/files")
                    .header("Content-Type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Projects",
                            "mimeType": "application/vnd.google-apps.folder",
                            "parents": ["root"]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        let json = read_json(response).await;
        assert_eq!(json["kind"], "drive#file");
        assert_eq!(json["name"], "Projects");
        assert_eq!(json["mimeType"], "application/vnd.google-apps.folder");
        assert_eq!(json["parents"], serde_json::json!(["root"]));
    }

    #[tokio::test]
    async fn v3_create_file() {
        let state = test_drive_state();
        let app = DriveTwinService::routes(state.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/drive/v3/files")
                    .header("Content-Type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "report.pdf",
                            "parents": ["root"]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        let json = read_json(response).await;
        assert_eq!(json["kind"], "drive#file");
        assert_eq!(json["name"], "report.pdf");
        assert_eq!(json["mimeType"], "application/octet-stream");
    }

    #[tokio::test]
    async fn v3_create_file_default_parent() {
        let state = test_drive_state();
        let app = DriveTwinService::routes(state.clone());

        // No parents field — should default to root
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/drive/v3/files")
                    .header("Content-Type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::from(
                        serde_json::json!({ "name": "orphan.txt" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        let json = read_json(response).await;
        assert_eq!(json["parents"], serde_json::json!(["root"]));
    }

    #[tokio::test]
    async fn v3_get_file() {
        let state = test_drive_state();

        // Create a file first
        {
            let mut rt = state.lock().await;
            rt.service
                .handle(DriveRequest::CreateFile {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "doc.txt".to_string(),
                })
                .unwrap();
        }

        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files/item_1")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        assert_eq!(json["kind"], "drive#file");
        assert_eq!(json["id"], "item_1");
        assert_eq!(json["name"], "doc.txt");
        assert_eq!(json["mimeType"], "application/octet-stream");
        assert_eq!(json["parents"], serde_json::json!(["root"]));
        // Permissions should be included in get
        assert!(json["permissions"].is_array());
        assert_eq!(json["permissions"][0]["role"], "owner");
    }

    #[tokio::test]
    async fn v3_get_file_not_found() {
        let state = test_drive_state();
        let app = DriveTwinService::routes(state.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files/nonexistent")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn v3_list_files() {
        let state = test_drive_state();

        // Create some items
        {
            let mut rt = state.lock().await;
            rt.service
                .handle(DriveRequest::CreateFile {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "a.txt".to_string(),
                })
                .unwrap();
            rt.service
                .handle(DriveRequest::CreateFolder {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "Docs".to_string(),
                })
                .unwrap();
        }

        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        assert_eq!(json["kind"], "drive#fileList");
        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 2); // root excluded
    }

    #[tokio::test]
    async fn v3_list_files_with_parent_query() {
        let state = test_drive_state();

        {
            let mut rt = state.lock().await;
            // Create folder and file inside it
            rt.service
                .handle(DriveRequest::CreateFolder {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "Projects".to_string(),
                })
                .unwrap();
            rt.service
                .handle(DriveRequest::CreateFile {
                    actor_id: "alice".to_string(),
                    parent_id: "item_1".to_string(),
                    name: "plan.txt".to_string(),
                })
                .unwrap();
            // Also a file at root level
            rt.service
                .handle(DriveRequest::CreateFile {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "root_file.txt".to_string(),
                })
                .unwrap();
        }

        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files?q=%27root%27+in+parents")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        let files = json["files"].as_array().unwrap();
        // Should only have items whose parent is root (Projects folder + root_file.txt)
        assert_eq!(files.len(), 2);
        let names: Vec<&str> = files.iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"Projects"));
        assert!(names.contains(&"root_file.txt"));
    }

    #[tokio::test]
    async fn v3_list_files_page_size() {
        let state = test_drive_state();

        {
            let mut rt = state.lock().await;
            for i in 0..5 {
                rt.service
                    .handle(DriveRequest::CreateFile {
                        actor_id: "alice".to_string(),
                        parent_id: "root".to_string(),
                        name: format!("file_{i}.txt"),
                    })
                    .unwrap();
            }
        }

        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files?pageSize=2")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
    }

    #[tokio::test]
    async fn v3_delete_file() {
        let state = test_drive_state();

        {
            let mut rt = state.lock().await;
            rt.service
                .handle(DriveRequest::CreateFile {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "temp.txt".to_string(),
                })
                .unwrap();
        }

        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/drive/v3/files/item_1")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // Verify item is gone
        let rt = state.lock().await;
        assert!(!rt.service.item_exists("item_1"));
    }

    #[tokio::test]
    async fn v3_update_file_rename() {
        let state = test_drive_state();

        {
            let mut rt = state.lock().await;
            rt.service
                .handle(DriveRequest::CreateFile {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "old.txt".to_string(),
                })
                .unwrap();
        }

        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri("/drive/v3/files/item_1")
                    .header("Content-Type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::from(
                        serde_json::json!({ "name": "new.txt" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        assert_eq!(json["name"], "new.txt");
        assert_eq!(json["kind"], "drive#file");
    }

    #[tokio::test]
    async fn v3_update_file_move() {
        let state = test_drive_state();

        {
            let mut rt = state.lock().await;
            rt.service
                .handle(DriveRequest::CreateFolder {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "dest".to_string(),
                })
                .unwrap();
            rt.service
                .handle(DriveRequest::CreateFile {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "moveme.txt".to_string(),
                })
                .unwrap();
        }

        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri("/drive/v3/files/item_2?addParents=item_1&removeParents=root")
                    .header("Content-Type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::from(serde_json::json!({}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        assert_eq!(json["parents"], serde_json::json!(["item_1"]));
    }

    #[tokio::test]
    async fn v3_create_permission() {
        let state = test_drive_state();

        {
            let mut rt = state.lock().await;
            rt.service
                .handle(DriveRequest::CreateFile {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "shared.txt".to_string(),
                })
                .unwrap();
        }

        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/drive/v3/files/item_1/permissions")
                    .header("Content-Type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::from(
                        serde_json::json!({
                            "role": "writer",
                            "emailAddress": "bob@twin.local"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        assert_eq!(json["role"], "writer");
        assert_eq!(json["type"], "user");
        assert_eq!(json["id"], "bob");
        assert_eq!(json["emailAddress"], "bob@twin.local");
    }

    #[tokio::test]
    async fn v3_create_permission_with_actor_id() {
        let state = test_drive_state();

        {
            let mut rt = state.lock().await;
            rt.service
                .handle(DriveRequest::CreateFile {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "shared.txt".to_string(),
                })
                .unwrap();
        }

        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/drive/v3/files/item_1/permissions")
                    .header("Content-Type", "application/json")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::from(
                        serde_json::json!({
                            "role": "reader",
                            "actorId": "charlie"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        assert_eq!(json["role"], "reader");
        assert_eq!(json["id"], "charlie");
    }

    #[tokio::test]
    async fn v3_response_shapes() {
        let state = test_drive_state();

        {
            let mut rt = state.lock().await;
            rt.service
                .handle(DriveRequest::CreateFolder {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "Folder".to_string(),
                })
                .unwrap();
            rt.service
                .handle(DriveRequest::CreateFile {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "file.txt".to_string(),
                })
                .unwrap();
        }

        // Test file list shape
        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let json = read_json(response).await;
        assert_eq!(json["kind"], "drive#fileList");
        assert!(json["files"].is_array());

        let files = json["files"].as_array().unwrap();
        for file in files {
            assert_eq!(file["kind"], "drive#file");
            assert!(file["id"].is_string());
            assert!(file["name"].is_string());
            assert!(file["mimeType"].is_string());
            assert!(file["parents"].is_array());
        }

        // Verify folder mimeType
        let folder = files.iter().find(|f| f["name"] == "Folder").unwrap();
        assert_eq!(folder["mimeType"], "application/vnd.google-apps.folder");

        // Verify file mimeType
        let file = files.iter().find(|f| f["name"] == "file.txt").unwrap();
        assert_eq!(file["mimeType"], "application/octet-stream");
    }

    #[tokio::test]
    async fn v3_get_root() {
        let state = test_drive_state();
        let app = DriveTwinService::routes(state.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files/root")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        assert_eq!(json["kind"], "drive#file");
        assert_eq!(json["id"], "root");
        assert_eq!(json["mimeType"], "application/vnd.google-apps.folder");
        // Root has no parent, so parents should be empty
        assert_eq!(json["parents"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn v3_default_actor_header() {
        // When no X-Twin-Actor-Id header is provided, should use "default" actor
        let state = test_drive_state();

        // Give "default" actor editor access on root
        {
            let mut rt = state.lock().await;
            rt.service
                .handle(DriveRequest::SetPermission {
                    actor_id: "alice".to_string(),
                    item_id: "root".to_string(),
                    target_actor_id: "default".to_string(),
                    role: PermissionRole::Editor,
                })
                .unwrap();
        }

        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/drive/v3/files")
                    .header("Content-Type", "application/json")
                    // No X-Twin-Actor-Id header
                    .body(Body::from(
                        serde_json::json!({
                            "name": "default_file.txt",
                            "parents": ["root"]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
    }

    // -- File content unit tests --

    #[test]
    fn upload_and_download_round_trip() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        let content = b"Hello, world!".to_vec();
        let result = svc
            .handle(DriveRequest::UploadContent {
                actor_id: "alice".to_string(),
                parent_id: "root".to_string(),
                name: "hello.txt".to_string(),
                mime_type: Some("text/plain".to_string()),
                content: content.clone(),
                app_properties: BTreeMap::new(),
            })
            .unwrap();

        let file_id = match &result {
            DriveResponse::ContentCreated { item, size } => {
                assert_eq!(*size, 13);
                item.id.clone()
            }
            _ => panic!("expected ContentCreated"),
        };

        let download = svc
            .handle(DriveRequest::DownloadContent {
                actor_id: "alice".to_string(),
                item_id: file_id,
            })
            .unwrap();

        match download {
            DriveResponse::Content { item, data } => {
                assert_eq!(data, content);
                assert_eq!(item.mime_type.as_deref(), Some("text/plain"));
                assert_eq!(item.size, Some(13));
            }
            _ => panic!("expected Content"),
        }
    }

    #[test]
    fn upload_creates_file_with_correct_metadata() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        let result = svc
            .handle(DriveRequest::UploadContent {
                actor_id: "alice".to_string(),
                parent_id: "root".to_string(),
                name: "report.pdf".to_string(),
                mime_type: Some("application/pdf".to_string()),
                content: vec![0x25, 0x50, 0x44, 0x46], // %PDF
                app_properties: BTreeMap::new(),
            })
            .unwrap();

        match result {
            DriveResponse::ContentCreated { item, size } => {
                assert_eq!(item.name, "report.pdf");
                assert_eq!(item.kind, DriveItemKind::File);
                assert_eq!(item.mime_type.as_deref(), Some("application/pdf"));
                assert_eq!(item.size, Some(4));
                assert_eq!(size, 4);
            }
            _ => panic!("expected ContentCreated"),
        }
    }

    #[test]
    fn upload_defaults_mime_type_to_octet_stream() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        let result = svc
            .handle(DriveRequest::UploadContent {
                actor_id: "alice".to_string(),
                parent_id: "root".to_string(),
                name: "data.bin".to_string(),
                mime_type: None,
                content: vec![1, 2, 3],
                app_properties: BTreeMap::new(),
            })
            .unwrap();

        match result {
            DriveResponse::ContentCreated { item, .. } => {
                assert_eq!(
                    item.mime_type.as_deref(),
                    Some("application/octet-stream")
                );
            }
            _ => panic!("expected ContentCreated"),
        }
    }

    #[test]
    fn download_nonexistent_content_returns_error() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        // Create a file without content
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "empty.txt".to_string(),
        })
        .unwrap();

        let result = svc.handle(DriveRequest::DownloadContent {
            actor_id: "alice".to_string(),
            item_id: "item_1".to_string(),
        });

        assert!(result.is_err());
        let err = result.unwrap_err();
        let TwinError::Operation(msg) = err;
        assert!(msg.contains("no content"), "msg: {msg}");
    }

    #[test]
    fn download_permission_denied() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        svc.handle(DriveRequest::UploadContent {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "secret.txt".to_string(),
            mime_type: None,
            content: b"secret".to_vec(),
            app_properties: BTreeMap::new(),
        })
        .unwrap();

        let result = svc.handle(DriveRequest::DownloadContent {
            actor_id: "bob".to_string(),
            item_id: "item_1".to_string(),
        });

        assert!(result.is_err());
    }

    #[test]
    fn delete_cascades_to_content_store() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        // Create folder
        svc.handle(DriveRequest::CreateFolder {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "folder".to_string(),
        })
        .unwrap();

        // Upload file in folder
        svc.handle(DriveRequest::UploadContent {
            actor_id: "alice".to_string(),
            parent_id: "item_1".to_string(),
            name: "file.txt".to_string(),
            mime_type: None,
            content: b"data".to_vec(),
            app_properties: BTreeMap::new(),
        })
        .unwrap();

        assert!(svc.content.contains_key("item_2"));

        // Delete the folder — should cascade-delete the file and its content
        svc.handle(DriveRequest::DeleteItem {
            actor_id: "alice".to_string(),
            item_id: "item_1".to_string(),
        })
        .unwrap();

        assert!(!svc.items.contains_key("item_2"));
        assert!(!svc.content.contains_key("item_2"));
    }

    #[test]
    fn snapshot_includes_content_round_trip() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        let content = b"snapshot test content".to_vec();
        svc.handle(DriveRequest::UploadContent {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "snap.txt".to_string(),
            mime_type: Some("text/plain".to_string()),
            content: content.clone(),
            app_properties: BTreeMap::new(),
        })
        .unwrap();

        let snap = svc.service_snapshot();

        // Verify snapshot contains content key
        assert!(snap.get("content").is_some());

        // Restore into fresh instance
        let mut svc2 = DriveTwinService::default();
        svc2.service_restore(&snap).unwrap();

        // Download from restored instance
        let result = svc2
            .handle(DriveRequest::DownloadContent {
                actor_id: "alice".to_string(),
                item_id: "item_1".to_string(),
            })
            .unwrap();

        match result {
            DriveResponse::Content { data, .. } => {
                assert_eq!(data, content);
            }
            _ => panic!("expected Content"),
        }
    }

    #[test]
    fn snapshot_restore_backward_compat_no_content_key() {
        // Old snapshots without "content" key should restore with empty content store
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        let old_snapshot = serde_json::json!({
            "items": svc.items,
            "next_id": svc.next_id,
        });

        let mut svc2 = DriveTwinService::default();
        svc2.service_restore(&old_snapshot).unwrap();
        assert!(svc2.content.is_empty());
    }

    #[test]
    fn seed_from_scenario_with_content() {
        let mut svc = DriveTwinService::default();
        let content_b64 = BASE64.encode(b"seeded file content");

        let initial_state = serde_json::json!({
            "files": [
                {
                    "id": "root",
                    "name": "My Drive",
                    "parent_id": null,
                    "owner_id": "alice",
                    "kind": "Folder"
                },
                {
                    "id": "item_1",
                    "name": "seeded.txt",
                    "parent_id": "root",
                    "owner_id": "alice",
                    "kind": "File",
                    "mime_type": "text/plain",
                    "content": content_b64
                }
            ]
        });

        svc.seed_from_scenario(&initial_state).unwrap();

        // Verify content was stored
        let item = svc.items.get("item_1").unwrap();
        assert_eq!(item.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(item.size, Some(19)); // "seeded file content".len()
        assert!(svc.content.contains_key("item_1"));

        // Verify download works
        let result = svc
            .handle(DriveRequest::DownloadContent {
                actor_id: "alice".to_string(),
                item_id: "item_1".to_string(),
            })
            .unwrap();

        match result {
            DriveResponse::Content { data, .. } => {
                assert_eq!(data, b"seeded file content");
            }
            _ => panic!("expected Content"),
        }
    }

    #[test]
    fn state_item_includes_content_metadata() {
        let mut svc = DriveTwinService::default();
        svc.seed_root("alice", None).unwrap();

        // Upload a file with content
        svc.handle(DriveRequest::UploadContent {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "with_content.txt".to_string(),
            mime_type: Some("text/plain".to_string()),
            content: b"some content".to_vec(),
            app_properties: BTreeMap::new(),
        })
        .unwrap();

        // Create a file without content
        svc.handle(DriveRequest::CreateFile {
            actor_id: "alice".to_string(),
            parent_id: "root".to_string(),
            name: "no_content.txt".to_string(),
        })
        .unwrap();

        let with_content = svc.state_item("item_1").unwrap();
        assert_eq!(with_content.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(with_content.size, Some(12));
        assert!(with_content.has_content);

        let no_content = svc.state_item("item_2").unwrap();
        assert!(no_content.mime_type.is_none());
        assert!(no_content.size.is_none());
        assert!(!no_content.has_content);
    }

    // -- V3 file content integration tests --

    async fn read_body(response: axum::http::Response<Body>) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    fn location_to_path_and_query(location: &str) -> String {
        if let Some(idx) = location.find("/upload/") {
            location[idx..].to_string()
        } else {
            location.to_string()
        }
    }

    #[tokio::test]
    async fn v3_upload_and_download() {
        let state = test_drive_state();
        let content = b"integration test content";

        // Upload via POST /upload/drive/v3/files?uploadType=media&name=test.txt
        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/upload/drive/v3/files?uploadType=media&name=test.txt&mimeType=text/plain")
                    .header("X-Twin-Actor-Id", "alice")
                    .header("Content-Type", "text/plain")
                    .body(Body::from(content.to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        let json = read_json(response).await;
        assert_eq!(json["name"], "test.txt");
        assert_eq!(json["mimeType"], "text/plain");
        assert_eq!(json["size"], "24"); // "integration test content".len()
        let file_id = json["id"].as_str().unwrap().to_string();

        // Download via GET /drive/v3/files/{id}?alt=media
        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/drive/v3/files/{file_id}?alt=media"))
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/plain"
        );
        assert_eq!(
            response.headers().get("content-length").unwrap(),
            "24"
        );
        let body = read_body(response).await;
        assert_eq!(body, content);
    }

    #[tokio::test]
    async fn v3_resumable_upload_and_download() {
        let state = test_drive_state();
        let content = b"resumable content";

        let app = DriveTwinService::routes(state.clone());
        let init_response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/upload/drive/v3/files?uploadType=resumable")
                    .header("X-Twin-Actor-Id", "alice")
                    .header("Content-Type", "application/json")
                    .header("X-Upload-Content-Type", "text/plain")
                    .header("X-Upload-Content-Length", content.len().to_string())
                    .body(Body::from(
                        serde_json::json!({
                            "name": "resumable.txt",
                            "parents": ["root"],
                            "mimeType": "text/plain"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(init_response.status(), StatusCode::OK);
        let location = init_response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap()
            .to_string();
        let upload_uri = location_to_path_and_query(&location);

        let app = DriveTwinService::routes(state.clone());
        let upload_response = app
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(&upload_uri)
                    .header("X-Twin-Actor-Id", "alice")
                    .header("Content-Type", "text/plain")
                    .header(
                        "Content-Range",
                        format!("bytes 0-{}/{}", content.len() - 1, content.len()),
                    )
                    .body(Body::from(content.to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(upload_response.status(), StatusCode::CREATED);
        let json = read_json(upload_response).await;
        assert_eq!(json["name"], "resumable.txt");
        assert_eq!(json["mimeType"], "text/plain");
        let file_id = json["id"].as_str().unwrap().to_string();

        let app = DriveTwinService::routes(state.clone());
        let download_response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/drive/v3/files/{file_id}?alt=media"))
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(download_response.status(), StatusCode::OK);
        assert_eq!(read_body(download_response).await, content);
    }

    #[tokio::test]
    async fn v3_resumable_upload_returns_308_for_partial_chunk() {
        let state = test_drive_state();
        let full = b"abcdefgh";

        let app = DriveTwinService::routes(state.clone());
        let init_response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/upload/drive/v3/files?uploadType=resumable")
                    .header("X-Twin-Actor-Id", "alice")
                    .header("Content-Type", "application/json")
                    .header("X-Upload-Content-Type", "text/plain")
                    .header("X-Upload-Content-Length", "8")
                    .body(Body::from(
                        serde_json::json!({ "name": "chunked.txt" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(init_response.status(), StatusCode::OK);
        let upload_uri = location_to_path_and_query(
            init_response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .unwrap(),
        );

        let app = DriveTwinService::routes(state.clone());
        let part1 = app
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(&upload_uri)
                    .header("X-Twin-Actor-Id", "alice")
                    .header("Content-Type", "text/plain")
                    .header("Content-Range", "bytes 0-3/8")
                    .body(Body::from(full[0..4].to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(part1.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(part1.headers().get("Range").unwrap(), "bytes=0-3");

        let app = DriveTwinService::routes(state.clone());
        let part2 = app
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(&upload_uri)
                    .header("X-Twin-Actor-Id", "alice")
                    .header("Content-Type", "text/plain")
                    .header("Content-Range", "bytes 4-7/8")
                    .body(Body::from(full[4..8].to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(part2.status(), StatusCode::CREATED);
        let json = read_json(part2).await;
        assert_eq!(json["name"], "chunked.txt");
    }

    #[tokio::test]
    async fn v3_get_file_without_alt_returns_json() {
        let state = test_drive_state();

        // Upload a file first
        {
            let mut rt = state.lock().await;
            rt.service
                .handle(DriveRequest::UploadContent {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "meta.txt".to_string(),
                    mime_type: Some("text/plain".to_string()),
                    content: b"metadata test".to_vec(),
                    app_properties: BTreeMap::new(),
                })
                .unwrap();
        }

        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files/item_1")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        assert_eq!(json["kind"], "drive#file");
        assert_eq!(json["name"], "meta.txt");
        assert_eq!(json["mimeType"], "text/plain");
        assert_eq!(json["size"], "13");
    }

    #[tokio::test]
    async fn v3_upload_then_list_shows_file() {
        let state = test_drive_state();

        // Upload via v3
        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/upload/drive/v3/files?uploadType=media&name=listed.txt&parents=root")
                    .header("X-Twin-Actor-Id", "alice")
                    .header("Content-Type", "application/octet-stream")
                    .body(Body::from(b"list test".to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);

        // List files
        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files?q='root'+in+parents")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["name"], "listed.txt");
    }

    #[tokio::test]
    async fn v3_download_no_content_returns_error() {
        let state = test_drive_state();

        // Create file without content
        {
            let mut rt = state.lock().await;
            rt.service
                .handle(DriveRequest::CreateFile {
                    actor_id: "alice".to_string(),
                    parent_id: "root".to_string(),
                    name: "empty.txt".to_string(),
                })
                .unwrap();
        }

        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files/item_1?alt=media")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should return an error (404 or similar)
        assert_ne!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn v3_scenario_seed_content_download() {
        use twin_kernel::{TwinConfig, TwinKernel};
        use twin_service::TwinRuntime;

        // Seed a service with content via seed_from_scenario, then download via HTTP.
        // This is the end-to-end test for the fix where scenario seeding now preserves
        // twin-specific fields (mime_type, content) through serde_json::Value.
        let mut svc = DriveTwinService::default();
        let original_content = b"# Seeded Document\n\nThis was seeded via scenario.";
        let content_b64 = BASE64.encode(original_content);

        let initial_state = serde_json::json!({
            "files": [
                {
                    "id": "root",
                    "name": "My Drive",
                    "parent_id": null,
                    "owner_id": "alice",
                    "kind": "Folder"
                },
                {
                    "id": "seeded_doc",
                    "name": "seeded.md",
                    "parent_id": "root",
                    "owner_id": "alice",
                    "kind": "File",
                    "mime_type": "text/markdown",
                    "content": content_b64
                }
            ]
        });
        svc.seed_from_scenario(&initial_state).unwrap();

        let state: DriveState = Arc::new(tokio::sync::Mutex::new(TwinRuntime::new(
            TwinKernel::new(TwinConfig {
                seed: 42,
                start_time_unix_ms: 1000,
            }),
            svc,
        )));

        // Download seeded content via alt=media
        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files/seeded_doc?alt=media")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/markdown"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), original_content);

        // Verify metadata via GET JSON
        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/drive/v3/files/seeded_doc")
                    .header("X-Twin-Actor-Id", "alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        assert_eq!(json["mimeType"], "text/markdown");
        assert_eq!(json["size"], original_content.len().to_string());

        // Verify state inspection shows has_content
        let app = DriveTwinService::routes(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/state/items/seeded_doc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        assert_eq!(json["item"]["properties"]["has_content"], true);
        assert_eq!(json["item"]["properties"]["mime_type"], "text/markdown");
        assert_eq!(json["item"]["properties"]["size"], original_content.len());
    }

    #[test]
    fn discovery_files_list_supports_spaces_and_resumable_upload() {
        let discovery = DriveTwinService::discovery_meta().expect("drive discovery meta");
        let files = discovery
            .resources
            .get("files")
            .expect("files resource in discovery");

        let list = files.methods.get("list").expect("files.list method");
        assert!(
            list.parameters.contains_key("spaces"),
            "files.list should declare 'spaces' query parameter"
        );

        let create = files.methods.get("create").expect("files.create method");
        let media_upload = create
            .media_upload
            .as_ref()
            .expect("files.create media_upload config");
        assert_eq!(
            media_upload["protocols"]["resumable"]["path"],
            "/upload/drive/v3/files"
        );
    }

    // --- Multipart parsing unit tests ---

    #[test]
    fn extract_boundary_standard() {
        let ct = "multipart/related; boundary=====boundary123===";
        assert_eq!(
            extract_boundary(ct),
            Some("====boundary123===".to_string())
        );
    }

    #[test]
    fn extract_boundary_quoted() {
        let ct = r#"multipart/related; boundary="my_boundary""#;
        assert_eq!(extract_boundary(ct), Some("my_boundary".to_string()));
    }

    #[test]
    fn extract_boundary_missing() {
        assert_eq!(extract_boundary("multipart/related"), None);
        assert_eq!(extract_boundary("application/json"), None);
    }

    #[test]
    fn parse_multipart_two_parts() {
        let boundary = "boundary123";
        let body = format!(
            "--{boundary}\r\n\
             Content-Type: application/json\r\n\
             \r\n\
             {{\"name\":\"test.txt\"}}\r\n\
             --{boundary}\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             hello world\r\n\
             --{boundary}--"
        );
        let parts = parse_multipart_related(body.as_bytes(), boundary).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            String::from_utf8_lossy(&parts[0].body),
            "{\"name\":\"test.txt\"}"
        );
        assert_eq!(String::from_utf8_lossy(&parts[1].body), "hello world");
    }

    #[test]
    fn parse_multipart_with_lf_only() {
        // Some HTTP clients use \n instead of \r\n.
        let boundary = "b";
        let body = "--b\nContent-Type: application/json\n\n{\"name\":\"f\"}\n--b\nContent-Type: text/plain\n\ndata\n--b--";
        let parts = parse_multipart_related(body.as_bytes(), boundary).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(String::from_utf8_lossy(&parts[0].body), "{\"name\":\"f\"}");
        assert_eq!(String::from_utf8_lossy(&parts[1].body), "data");
    }

    #[test]
    fn multipart_metadata_deserialization() {
        let json = r#"{"name":"doc.txt","parents":["folder1"],"mimeType":"text/plain","appProperties":{"key":"val"}}"#;
        let meta: MultipartMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.name.as_deref(), Some("doc.txt"));
        assert_eq!(meta.parents.as_ref().unwrap(), &["folder1"]);
        assert_eq!(meta.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(
            meta.app_properties.as_ref().unwrap().get("key").unwrap(),
            "val"
        );
    }

    #[test]
    fn multipart_metadata_minimal() {
        let json = "{}";
        let meta: MultipartMetadata = serde_json::from_str(json).unwrap();
        assert!(meta.name.is_none());
        assert!(meta.parents.is_none());
        assert!(meta.mime_type.is_none());
        assert!(meta.app_properties.is_none());
    }
}
