//! Code generation from twin spec files.
//!
//! Reads a TOML spec describing a twin's API surface and generates Rust source
//! code for response types, request body types, query parameter structs, route
//! handler functions, routes wiring, and error helpers.

use anyhow::{ensure, Context};
use serde::Deserialize;
use std::fmt::Write;
use std::path::Path;

// ---------------------------------------------------------------------------
// Spec schema — Rust structs that mirror the TOML spec format
// ---------------------------------------------------------------------------

/// Top-level spec file.
#[derive(Debug, Deserialize)]
pub struct TwinSpec {
    pub twin: TwinMeta,
    #[serde(default)]
    pub resources: Vec<Resource>,
    #[serde(default)]
    pub response_types: Vec<StructDef>,
    #[serde(default)]
    pub body_types: Vec<StructDef>,
}

/// `[twin]` section — metadata about the twin.
#[derive(Debug, Deserialize)]
pub struct TwinMeta {
    pub name: String,
    pub api_version: String,
    pub base_path: String,
    pub state_type: String,
    pub request_type: String,
    pub response_type: String,
}

/// A resource (e.g. Message, Thread, Label).
#[derive(Debug, Deserialize)]
pub struct Resource {
    pub name: String,
    #[serde(default)]
    pub operations: Vec<Operation>,
}

/// A single API operation on a resource.
#[derive(Debug, Deserialize)]
pub struct Operation {
    pub name: String,
    pub method: String,
    pub path: String,
    pub handler: String,
    pub request_variant: String,
    pub response_variant: String,
    pub success_status: u16,
    #[serde(default)]
    pub body_type: Option<String>,
    #[serde(default)]
    pub response_conversion: Option<String>,
    #[serde(default)]
    pub path_params: Vec<ParamDef>,
    #[serde(default)]
    pub query_params: Option<String>,
    /// Extra field mappings from HTTP inputs to request variant fields.
    /// Each entry is a line of Rust code like `message_id: id,`
    #[serde(default)]
    pub field_mappings: Vec<String>,
    /// If true, the handler uses `let rt = state.lock()` (immutable) instead
    /// of `let mut rt = state.lock()`.
    #[serde(default)]
    pub read_only: bool,
    /// Response destructuring pattern, e.g. `(thread, messages)`
    #[serde(default)]
    pub response_destructure: Option<String>,
}

/// A path parameter definition.
#[derive(Debug, Deserialize)]
pub struct ParamDef {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    /// Rename in the domain request (e.g., path param `id` → field `message_id`)
    #[serde(default)]
    pub rename: Option<String>,
}

/// A struct definition (used for both response_types and body_types).
#[derive(Debug, Deserialize)]
pub struct StructDef {
    pub name: String,
    pub fields: Vec<FieldDef>,
    /// Extra derive traits beyond the defaults.
    #[serde(default)]
    pub extra_derives: Vec<String>,
    /// Whether to apply `#[serde(rename_all = "camelCase")]`.
    #[serde(default = "default_true")]
    pub camel_case: bool,
    /// Whether to add `#[allow(dead_code)]` to the struct.
    #[serde(default)]
    pub allow_dead_code: bool,
    /// Doc comment for the struct.
    #[serde(default)]
    pub doc: Option<String>,
}

fn default_true() -> bool {
    true
}

/// A field within a struct definition.
#[derive(Debug, Deserialize)]
pub struct FieldDef {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    /// Add `#[serde(skip_serializing_if = "Option::is_none")]`
    #[serde(default)]
    pub skip_none: bool,
    /// Add `#[serde(skip_serializing_if = "Vec::is_empty")]`
    #[serde(default)]
    pub skip_empty: bool,
    /// Default value expression for `#[serde(default = "...")]` or `#[serde(default)]`
    #[serde(default)]
    pub default: Option<String>,
    /// Custom serde rename, e.g. `type` → `#[serde(rename = "type")]`
    #[serde(default)]
    pub serde_rename: Option<String>,
    /// Add `#[allow(dead_code)]` to the field.
    #[serde(default)]
    pub allow_dead_code: bool,
    /// Doc comment for the field.
    #[serde(default)]
    pub doc: Option<String>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a TOML spec file into a `TwinSpec`.
pub fn parse_spec(path: &Path) -> anyhow::Result<TwinSpec> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read spec file {}", path.display()))?;
    let spec: TwinSpec =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;
    validate_spec(&spec)?;
    Ok(spec)
}

fn validate_spec(spec: &TwinSpec) -> anyhow::Result<()> {
    ensure!(!spec.twin.name.is_empty(), "twin.name must not be empty");
    ensure!(
        !spec.twin.state_type.is_empty(),
        "twin.state_type must not be empty"
    );
    for rt in &spec.response_types {
        ensure!(!rt.name.is_empty(), "response_type name must not be empty");
    }
    for bt in &spec.body_types {
        ensure!(!bt.name.is_empty(), "body_type name must not be empty");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Code generation
// ---------------------------------------------------------------------------

/// Generate the full `generated.rs` content from a spec.
pub fn generate_code(spec: &TwinSpec) -> String {
    let mut out = String::with_capacity(8192);

    // Header
    writeln!(
        out,
        "// This file is generated by `twin-cli generate`. Do not edit manually."
    )
    .unwrap();
    writeln!(
        out,
        "// Re-generate with: twin-cli generate --spec specs/{}.toml",
        spec.twin.name
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "#![allow(unused_imports)]").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "use serde::{{Deserialize, Serialize}};").unwrap();
    writeln!(out).unwrap();

    // Response types
    if !spec.response_types.is_empty() {
        writeln!(
            out,
            "// -----------------------------------------------------------------------"
        )
        .unwrap();
        writeln!(out, "// API response types (Serialize)").unwrap();
        writeln!(
            out,
            "// -----------------------------------------------------------------------"
        )
        .unwrap();
        writeln!(out).unwrap();
        for rt in &spec.response_types {
            generate_struct(&mut out, rt, StructKind::Response);
            writeln!(out).unwrap();
        }
    }

    // Body types
    if !spec.body_types.is_empty() {
        writeln!(
            out,
            "// -----------------------------------------------------------------------"
        )
        .unwrap();
        writeln!(out, "// HTTP body / query types (Deserialize)").unwrap();
        writeln!(
            out,
            "// -----------------------------------------------------------------------"
        )
        .unwrap();
        writeln!(out).unwrap();
        for bt in &spec.body_types {
            generate_struct(&mut out, bt, StructKind::Body);
            writeln!(out).unwrap();
        }
    }

    // Default value functions
    let default_fns = collect_default_functions(spec);
    if !default_fns.is_empty() {
        writeln!(
            out,
            "// -----------------------------------------------------------------------"
        )
        .unwrap();
        writeln!(out, "// Default value functions for serde").unwrap();
        writeln!(
            out,
            "// -----------------------------------------------------------------------"
        )
        .unwrap();
        writeln!(out).unwrap();
        for (fn_name, fn_body) in &default_fns {
            writeln!(out, "fn {fn_name}() -> {fn_body}").unwrap();
            writeln!(out).unwrap();
        }
    }

    // Route handlers
    let all_ops: Vec<(&Resource, &Operation)> = spec
        .resources
        .iter()
        .flat_map(|r| r.operations.iter().map(move |op| (r, op)))
        .collect();

    if !all_ops.is_empty() {
        writeln!(
            out,
            "// -----------------------------------------------------------------------"
        )
        .unwrap();
        writeln!(out, "// V{} route handlers", spec.twin.api_version).unwrap();
        writeln!(
            out,
            "// -----------------------------------------------------------------------"
        )
        .unwrap();
        writeln!(out).unwrap();
        for (_resource, op) in &all_ops {
            generate_route_handler(&mut out, spec, op);
            writeln!(out).unwrap();
        }
    }

    // Routes wiring function
    if !all_ops.is_empty() {
        generate_routes_fn(&mut out, spec, &all_ops);
    }

    // Error helpers
    if !spec.resources.is_empty() {
        generate_error_helpers(&mut out, spec);
    }

    out
}

#[derive(PartialEq)]
enum StructKind {
    Response,
    Body,
}

fn generate_struct(out: &mut String, def: &StructDef, kind: StructKind) {
    // Doc comment
    if let Some(doc) = &def.doc {
        writeln!(out, "/// {doc}").unwrap();
    }

    // Derives
    let base_derive = match kind {
        StructKind::Response => "Debug, Serialize",
        StructKind::Body => "Debug, Deserialize",
    };
    if def.extra_derives.is_empty() {
        writeln!(out, "#[derive({base_derive})]").unwrap();
    } else {
        let extras = def.extra_derives.join(", ");
        writeln!(out, "#[derive({base_derive}, {extras})]").unwrap();
    }

    // Serde rename_all
    if def.camel_case {
        writeln!(out, "#[serde(rename_all = \"camelCase\")]").unwrap();
    }

    // allow(dead_code)
    if def.allow_dead_code {
        writeln!(out, "#[allow(dead_code)]").unwrap();
    }

    writeln!(out, "pub(crate) struct {} {{", def.name).unwrap();

    for field in &def.fields {
        // Field doc comment
        if let Some(doc) = &field.doc {
            writeln!(out, "    /// {doc}").unwrap();
        }

        // Field-level serde attributes
        if let Some(rename) = &field.serde_rename {
            writeln!(out, "    #[serde(rename = \"{rename}\")]").unwrap();
        }
        if field.skip_none {
            writeln!(
                out,
                "    #[serde(skip_serializing_if = \"Option::is_none\")]"
            )
            .unwrap();
        }
        if field.skip_empty {
            writeln!(out, "    #[serde(skip_serializing_if = \"Vec::is_empty\")]").unwrap();
        }
        if let Some(default) = &field.default {
            if default == "default" || default.is_empty() {
                writeln!(out, "    #[serde(default)]").unwrap();
            } else {
                // Named default function
                let fn_name = format!("default_{}", default_fn_key(&def.name, &field.name));
                writeln!(out, "    #[serde(default = \"{fn_name}\")]").unwrap();
            }
        }
        if field.allow_dead_code {
            writeln!(out, "    #[allow(dead_code)]").unwrap();
        }

        writeln!(out, "    pub(crate) {}: {},", field.name, field.ty).unwrap();
    }

    writeln!(out, "}}").unwrap();
}

/// Create a unique key for a default function based on struct+field name.
fn default_fn_key(struct_name: &str, field_name: &str) -> String {
    format!("{}_{}", to_snake(struct_name), field_name)
}

/// Rough PascalCase → snake_case conversion (good enough for identifiers).
fn to_snake(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            result.push('_');
        }
        result.push(c.to_lowercase().next().unwrap_or(c));
    }
    result
}

/// Collect named default functions that need to be emitted.
/// Returns pairs of (function_name, return_type_and_expression).
fn collect_default_functions(spec: &TwinSpec) -> Vec<(String, String)> {
    let mut fns = Vec::new();
    let all_structs = spec.response_types.iter().chain(spec.body_types.iter());

    for s in all_structs {
        for field in &s.fields {
            if let Some(default) = &field.default {
                if default != "default" && !default.is_empty() {
                    let fn_name = format!("default_{}", default_fn_key(&s.name, &field.name));
                    // Parse the default value to determine return type.
                    // The default value in the spec is a Rust expression like `100` or `"full".to_string()`.
                    let fn_body = format!("{} {{ {default} }}", field.ty);
                    // Avoid duplicates
                    if !fns.iter().any(|(name, _)| name == &fn_name) {
                        fns.push((fn_name, fn_body));
                    }
                }
            }
        }
    }

    fns
}

fn generate_route_handler(out: &mut String, spec: &TwinSpec, op: &Operation) {
    let state_type = format!("SharedTwinState<{}>", spec.twin.state_type);
    let req_type = &spec.twin.request_type;
    let resp_type = &spec.twin.response_type;

    // Function signature
    writeln!(out, "pub(crate) async fn {}(", op.handler).unwrap();
    writeln!(out, "    State(state): State<{state_type}>,").unwrap();
    writeln!(out, "    resolved: Option<Extension<ResolvedActorId>>,").unwrap();
    writeln!(out, "    headers: axum::http::HeaderMap,").unwrap();

    // Path params extractor
    if op.path_params.len() == 1 {
        let p = &op.path_params[0];
        writeln!(out, "    Path({}): Path<{}>,", p.name, p.ty).unwrap();
    } else if op.path_params.len() > 1 {
        let types: Vec<&str> = op.path_params.iter().map(|p| p.ty.as_str()).collect();
        let names: Vec<&str> = op.path_params.iter().map(|p| p.name.as_str()).collect();
        writeln!(
            out,
            "    Path(({}),): Path<({},)>,",
            names.join(", "),
            types.join(", "),
        )
        .unwrap();
    }

    // Query params extractor
    if let Some(query_type) = &op.query_params {
        writeln!(out, "    Query(query): Query<{query_type}>,").unwrap();
    }

    // Body extractor
    if let Some(body_type) = &op.body_type {
        writeln!(out, "    Json(body): Json<{body_type}>,").unwrap();
    }

    writeln!(out, ") -> impl IntoResponse {{").unwrap();

    // Actor ID extraction
    writeln!(
        out,
        "    let actor_id = extract_actor_id(&resolved, &headers);"
    )
    .unwrap();

    // Lock state
    if op.read_only {
        writeln!(out, "    let rt = state.lock().await;").unwrap();
    } else {
        writeln!(out, "    let mut rt = state.lock().await;").unwrap();
    }

    // Build and dispatch request
    writeln!(
        out,
        "    let result = rt.service.handle({req_type}::{} {{",
        op.request_variant
    )
    .unwrap();
    writeln!(out, "        actor_id,").unwrap();

    // Field mappings from path params
    for p in &op.path_params {
        let field = p.rename.as_deref().unwrap_or(&p.name);
        writeln!(out, "        {field}: {},", p.name).unwrap();
    }

    // Custom field mappings
    for mapping in &op.field_mappings {
        writeln!(out, "        {mapping}").unwrap();
    }

    writeln!(out, "    }});").unwrap();

    // Response handling
    let success_status = status_code_name(op.success_status);

    writeln!(out, "    match result {{").unwrap();

    let destructure = match &op.response_destructure {
        Some(d) => format!("({d})"),
        None => {
            // For simple variants like Ok, Deleted — no destructure
            if op.response_variant == "Ok" || op.response_variant == "Deleted" {
                String::new()
            } else {
                "(data)".to_string()
            }
        }
    };

    if op.success_status == 204 {
        // No content response
        writeln!(
            out,
            "        Ok({resp_type}::{}{}) => StatusCode::{success_status}.into_response(),",
            op.response_variant,
            if destructure.is_empty() { "" } else { "(..)" }
        )
        .unwrap();
    } else if let Some(conversion) = &op.response_conversion {
        // Custom conversion expression
        writeln!(
            out,
            "        Ok({resp_type}::{}{destructure}) => {{",
            op.response_variant
        )
        .unwrap();
        writeln!(out, "            let v1 = {conversion};").unwrap();
        writeln!(
            out,
            "            (StatusCode::{success_status}, Json(serde_json::to_value(&v1).unwrap())).into_response()"
        )
        .unwrap();
        writeln!(out, "        }}").unwrap();
    } else {
        // Direct serialization of data
        writeln!(
            out,
            "        Ok({resp_type}::{}{destructure}) => {{",
            op.response_variant
        )
        .unwrap();
        writeln!(
            out,
            "            (StatusCode::{success_status}, Json(serde_json::to_value(&data).unwrap())).into_response()"
        )
        .unwrap();
        writeln!(out, "        }}").unwrap();
    }

    writeln!(
        out,
        "        Err(e) => twin_error_to_v{}_response(e),",
        spec.twin.api_version
    )
    .unwrap();
    writeln!(
        out,
        "        _ => v{}_error_response(StatusCode::INTERNAL_SERVER_ERROR, \"unexpected response\"),",
        spec.twin.api_version
    )
    .unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();
}

fn generate_routes_fn(out: &mut String, spec: &TwinSpec, ops: &[(&Resource, &Operation)]) {
    let state_type = &spec.twin.state_type;

    writeln!(
        out,
        "// -----------------------------------------------------------------------"
    )
    .unwrap();
    writeln!(out, "// Routes wiring").unwrap();
    writeln!(
        out,
        "// -----------------------------------------------------------------------"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "/// Generated V{} mimicry routes. Merge into your `routes()` implementation.",
        spec.twin.api_version
    )
    .unwrap();
    writeln!(
        out,
        "pub(crate) fn v{}_routes(shared: SharedTwinState<{state_type}>) -> Router<SharedTwinState<{state_type}>> {{",
        spec.twin.api_version
    )
    .unwrap();
    writeln!(out, "    Router::new()").unwrap();

    for (_resource, op) in ops {
        let full_path = format!("{}{}", spec.twin.base_path, op.path);
        let method = op.method.to_lowercase();
        writeln!(
            out,
            "        .route(\"{full_path}\", {method}({}))",
            op.handler
        )
        .unwrap();
    }

    writeln!(out, "}}").unwrap();
}

fn generate_error_helpers(out: &mut String, spec: &TwinSpec) {
    let v = &spec.twin.api_version;

    writeln!(out).unwrap();
    writeln!(
        out,
        "// -----------------------------------------------------------------------"
    )
    .unwrap();
    writeln!(out, "// Error helpers").unwrap();
    writeln!(
        out,
        "// -----------------------------------------------------------------------"
    )
    .unwrap();
    writeln!(out).unwrap();

    // v{N}_error_response
    writeln!(out, "pub(crate) fn v{v}_error_response(status: StatusCode, message: &str) -> axum::response::Response {{").unwrap();
    writeln!(out, "    let body = serde_json::json!({{").unwrap();
    writeln!(out, "        \"error\": {{").unwrap();
    writeln!(out, "            \"code\": status.as_u16(),").unwrap();
    writeln!(out, "            \"message\": message,").unwrap();
    writeln!(out, "        }}").unwrap();
    writeln!(out, "    }});").unwrap();
    writeln!(out, "    (status, Json(body)).into_response()").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    // twin_error_to_v{N}_response
    writeln!(
        out,
        "pub(crate) fn twin_error_to_v{v}_response(err: TwinError) -> axum::response::Response {{"
    )
    .unwrap();
    writeln!(out, "    match err {{").unwrap();
    writeln!(
        out,
        "        TwinError::NotFound(msg) => v{v}_error_response(StatusCode::NOT_FOUND, &msg),"
    )
    .unwrap();
    writeln!(out, "        TwinError::PermissionDenied(msg) => v{v}_error_response(StatusCode::FORBIDDEN, &msg),").unwrap();
    writeln!(
        out,
        "        TwinError::Operation(msg) => v{v}_error_response(StatusCode::BAD_REQUEST, &msg),"
    )
    .unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();
}

fn status_code_name(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "CREATED",
        204 => "NO_CONTENT",
        400 => "BAD_REQUEST",
        401 => "UNAUTHORIZED",
        403 => "FORBIDDEN",
        404 => "NOT_FOUND",
        409 => "CONFLICT",
        500 => "INTERNAL_SERVER_ERROR",
        _ => "OK",
    }
}

// ---------------------------------------------------------------------------
// Check mode — verify generated code matches spec
// ---------------------------------------------------------------------------

/// Compare generated output with existing file content.
/// Returns Ok(true) if up-to-date, Ok(false) if outdated.
pub fn check_generated(spec: &TwinSpec, output_path: &Path) -> anyhow::Result<bool> {
    let generated = generate_code(spec);
    if !output_path.exists() {
        return Ok(false);
    }
    let existing = std::fs::read_to_string(output_path)
        .with_context(|| format!("failed to read {}", output_path.display()))?;
    Ok(normalize_whitespace(&generated) == normalize_whitespace(&existing))
}

fn normalize_whitespace(s: &str) -> String {
    s.lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_spec() -> TwinSpec {
        toml::from_str(
            r#"
[twin]
name = "test"
api_version = "1"
base_path = "/test/v1"
state_type = "TestTwinService"
request_type = "TestRequest"
response_type = "TestResponse"

[[response_types]]
name = "V1Item"
fields = [
    { name = "id", type = "String" },
    { name = "label", type = "String" },
]

[[body_types]]
name = "CreateItemBody"
fields = [
    { name = "name", type = "String" },
    { name = "parent_id", type = "Option<String>", default = "default" },
]
"#,
        )
        .unwrap()
    }

    #[test]
    fn parse_minimal_spec() {
        let spec = minimal_spec();
        assert_eq!(spec.twin.name, "test");
        assert_eq!(spec.twin.api_version, "1");
        assert_eq!(spec.response_types.len(), 1);
        assert_eq!(spec.response_types[0].name, "V1Item");
        assert_eq!(spec.response_types[0].fields.len(), 2);
        assert_eq!(spec.body_types.len(), 1);
    }

    #[test]
    fn generate_response_struct() {
        let spec = minimal_spec();
        let code = generate_code(&spec);
        assert!(
            code.contains("pub(crate) struct V1Item {"),
            "missing V1Item struct in:\n{code}"
        );
        assert!(
            code.contains("#[derive(Debug, Serialize)]"),
            "missing Serialize derive"
        );
        assert!(
            code.contains("#[serde(rename_all = \"camelCase\")]"),
            "missing camelCase rename"
        );
        assert!(code.contains("pub(crate) id: String,"), "missing id field");
        assert!(
            code.contains("pub(crate) label: String,"),
            "missing label field"
        );
    }

    #[test]
    fn generate_body_struct() {
        let spec = minimal_spec();
        let code = generate_code(&spec);
        assert!(
            code.contains("pub(crate) struct CreateItemBody {"),
            "missing CreateItemBody struct in:\n{code}"
        );
        assert!(
            code.contains("#[derive(Debug, Deserialize)]"),
            "missing Deserialize derive"
        );
        assert!(code.contains("#[serde(default)]"), "missing serde(default)");
    }

    #[test]
    fn generate_skip_none_attribute() {
        let spec: TwinSpec = toml::from_str(
            r#"
[twin]
name = "test"
api_version = "1"
base_path = "/test/v1"
state_type = "TestTwinService"
request_type = "TestRequest"
response_type = "TestResponse"

[[response_types]]
name = "V1Opt"
fields = [
    { name = "id", type = "String" },
    { name = "extra", type = "Option<String>", skip_none = true },
]
"#,
        )
        .unwrap();
        let code = generate_code(&spec);
        assert!(
            code.contains("skip_serializing_if = \"Option::is_none\""),
            "missing skip_serializing_if in:\n{code}"
        );
    }

    #[test]
    fn generate_skip_empty_attribute() {
        let spec: TwinSpec = toml::from_str(
            r#"
[twin]
name = "test"
api_version = "1"
base_path = "/test/v1"
state_type = "TestTwinService"
request_type = "TestRequest"
response_type = "TestResponse"

[[response_types]]
name = "V1List"
fields = [
    { name = "items", type = "Vec<String>", skip_empty = true },
]
"#,
        )
        .unwrap();
        let code = generate_code(&spec);
        assert!(
            code.contains("skip_serializing_if = \"Vec::is_empty\""),
            "missing skip_serializing_if for empty vec in:\n{code}"
        );
    }

    #[test]
    fn generate_serde_rename() {
        let spec: TwinSpec = toml::from_str(
            r#"
[twin]
name = "test"
api_version = "1"
base_path = "/test/v1"
state_type = "TestTwinService"
request_type = "TestRequest"
response_type = "TestResponse"

[[response_types]]
name = "V1Label"
fields = [
    { name = "label_type", type = "String", serde_rename = "type" },
]
"#,
        )
        .unwrap();
        let code = generate_code(&spec);
        assert!(
            code.contains("#[serde(rename = \"type\")]"),
            "missing serde rename in:\n{code}"
        );
    }

    #[test]
    fn generate_named_default_function() {
        let spec: TwinSpec = toml::from_str(
            r#"
[twin]
name = "test"
api_version = "1"
base_path = "/test/v1"
state_type = "TestTwinService"
request_type = "TestRequest"
response_type = "TestResponse"

[[body_types]]
name = "V1Query"
fields = [
    { name = "max_results", type = "u32", default = "100" },
]
"#,
        )
        .unwrap();
        let code = generate_code(&spec);
        assert!(
            code.contains("fn default_v1_query_max_results() -> u32 { 100 }"),
            "missing default function in:\n{code}"
        );
        assert!(
            code.contains("default = \"default_v1_query_max_results\""),
            "missing serde default reference in:\n{code}"
        );
    }

    #[test]
    fn generate_route_handler_simple() {
        let spec: TwinSpec = toml::from_str(
            r#"
[twin]
name = "test"
api_version = "1"
base_path = "/test/v1"
state_type = "TestTwinService"
request_type = "TestRequest"
response_type = "TestResponse"

[[resources]]
name = "Item"

[[resources.operations]]
name = "list"
method = "GET"
path = "/items"
handler = "route_v1_list_items"
request_variant = "ListItems"
response_variant = "ItemList"
success_status = 200
response_conversion = "items_to_v1(&data)"
"#,
        )
        .unwrap();
        let code = generate_code(&spec);
        assert!(
            code.contains("pub(crate) async fn route_v1_list_items("),
            "missing handler function in:\n{code}"
        );
        assert!(
            code.contains("TestRequest::ListItems"),
            "missing request variant in:\n{code}"
        );
        assert!(
            code.contains("items_to_v1(&data)"),
            "missing response conversion in:\n{code}"
        );
    }

    #[test]
    fn generate_route_handler_with_path_param() {
        let spec: TwinSpec = toml::from_str(
            r#"
[twin]
name = "test"
api_version = "1"
base_path = "/test/v1"
state_type = "TestTwinService"
request_type = "TestRequest"
response_type = "TestResponse"

[[resources]]
name = "Item"

[[resources.operations]]
name = "get"
method = "GET"
path = "/items/{id}"
handler = "route_v1_get_item"
request_variant = "GetItem"
response_variant = "Item"
success_status = 200
response_conversion = "item_to_v1(&data)"
path_params = [{ name = "id", type = "String", rename = "item_id" }]
"#,
        )
        .unwrap();
        let code = generate_code(&spec);
        assert!(
            code.contains("Path(id): Path<String>"),
            "missing path param extractor in:\n{code}"
        );
        assert!(
            code.contains("item_id: id,"),
            "missing renamed field mapping in:\n{code}"
        );
    }

    #[test]
    fn generate_204_no_content() {
        let spec: TwinSpec = toml::from_str(
            r#"
[twin]
name = "test"
api_version = "1"
base_path = "/test/v1"
state_type = "TestTwinService"
request_type = "TestRequest"
response_type = "TestResponse"

[[resources]]
name = "Item"

[[resources.operations]]
name = "delete"
method = "DELETE"
path = "/items/{id}"
handler = "route_v1_delete_item"
request_variant = "DeleteItem"
response_variant = "Deleted"
success_status = 204
path_params = [{ name = "id", type = "String" }]
"#,
        )
        .unwrap();
        let code = generate_code(&spec);
        assert!(
            code.contains("StatusCode::NO_CONTENT.into_response()"),
            "missing 204 response in:\n{code}"
        );
    }

    #[test]
    fn generate_routes_wiring() {
        let spec: TwinSpec = toml::from_str(
            r#"
[twin]
name = "test"
api_version = "1"
base_path = "/test/v1"
state_type = "TestTwinService"
request_type = "TestRequest"
response_type = "TestResponse"

[[resources]]
name = "Item"

[[resources.operations]]
name = "list"
method = "GET"
path = "/items"
handler = "route_v1_list_items"
request_variant = "ListItems"
response_variant = "ItemList"
success_status = 200

[[resources.operations]]
name = "delete"
method = "DELETE"
path = "/items/{id}"
handler = "route_v1_delete_item"
request_variant = "DeleteItem"
response_variant = "Deleted"
success_status = 204
path_params = [{ name = "id", type = "String" }]
"#,
        )
        .unwrap();
        let code = generate_code(&spec);
        assert!(
            code.contains("pub(crate) fn v1_routes("),
            "missing routes function in:\n{code}"
        );
        assert!(
            code.contains(".route(\"/test/v1/items\", get(route_v1_list_items))"),
            "missing list route in:\n{code}"
        );
        assert!(
            code.contains(".route(\"/test/v1/items/{id}\", delete(route_v1_delete_item))"),
            "missing delete route in:\n{code}"
        );
    }

    #[test]
    fn generate_error_helpers_v1() {
        let spec: TwinSpec = toml::from_str(
            r#"
[twin]
name = "test"
api_version = "1"
base_path = "/test/v1"
state_type = "TestTwinService"
request_type = "TestRequest"
response_type = "TestResponse"

[[resources]]
name = "Item"

[[resources.operations]]
name = "list"
method = "GET"
path = "/items"
handler = "route_v1_list"
request_variant = "List"
response_variant = "ItemList"
success_status = 200
"#,
        )
        .unwrap();
        let code = generate_code(&spec);
        assert!(
            code.contains("fn v1_error_response("),
            "missing error response helper in:\n{code}"
        );
        assert!(
            code.contains("fn twin_error_to_v1_response("),
            "missing twin_error helper in:\n{code}"
        );
        assert!(
            code.contains("TwinError::NotFound"),
            "missing NotFound handling in:\n{code}"
        );
    }

    #[test]
    fn generate_allow_dead_code() {
        let spec: TwinSpec = toml::from_str(
            r#"
[twin]
name = "test"
api_version = "1"
base_path = "/test/v1"
state_type = "TestTwinService"
request_type = "TestRequest"
response_type = "TestResponse"

[[body_types]]
name = "V1Body"
allow_dead_code = true
fields = [
    { name = "used_field", type = "String" },
    { name = "unused_field", type = "Option<String>", allow_dead_code = true },
]
"#,
        )
        .unwrap();
        let code = generate_code(&spec);
        // Struct-level allow(dead_code) should appear before the struct
        let struct_pos = code.find("pub(crate) struct V1Body").unwrap();
        let allow_before = code[..struct_pos].rfind("#[allow(dead_code)]").unwrap();
        assert!(
            allow_before < struct_pos,
            "struct-level allow(dead_code) should be before struct"
        );
        // Field-level allow(dead_code)
        assert!(
            code.contains("    #[allow(dead_code)]\n    pub(crate) unused_field"),
            "missing field-level allow(dead_code)"
        );
    }

    #[test]
    fn generate_no_camel_case() {
        let spec: TwinSpec = toml::from_str(
            r#"
[twin]
name = "test"
api_version = "1"
base_path = "/test/v1"
state_type = "TestTwinService"
request_type = "TestRequest"
response_type = "TestResponse"

[[body_types]]
name = "NativeBody"
camel_case = false
fields = [
    { name = "name", type = "String" },
]
"#,
        )
        .unwrap();
        let code = generate_code(&spec);
        // Should NOT have rename_all for this struct
        let struct_pos = code.find("pub(crate) struct NativeBody").unwrap();
        let preceding = &code[..struct_pos];
        let last_derive = preceding.rfind("#[derive").unwrap();
        let between = &code[last_derive..struct_pos];
        assert!(
            !between.contains("rename_all"),
            "native body should not have camelCase rename"
        );
    }

    #[test]
    fn check_mode_detects_difference() {
        let spec = minimal_spec();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("generated.rs");

        // File doesn't exist → not up-to-date
        assert!(!check_generated(&spec, &path).unwrap());

        // Write correct content
        let code = generate_code(&spec);
        std::fs::write(&path, &code).unwrap();
        assert!(check_generated(&spec, &path).unwrap());

        // Modify content
        std::fs::write(&path, "// different content").unwrap();
        assert!(!check_generated(&spec, &path).unwrap());
    }
}
