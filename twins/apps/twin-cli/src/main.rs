//! `twin-cli` — scaffolding tool for the digital-twins workspace.
//!
//! Usage:
//!     twin-cli new <name>

mod generate;

use anyhow::{bail, ensure, Context};
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "twin-cli", about = "Scaffolding tool for digital-twins")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new twin crate + server + Dockerfile from templates.
    New {
        /// Lowercase name for the new twin (e.g. "calendar", "my-service").
        name: String,
    },
    /// Generate Rust code from a TOML twin spec file.
    Generate {
        /// Path to the TOML spec file.
        #[arg(long)]
        spec: PathBuf,
        /// Output file path (default: stdout).
        #[arg(long)]
        output: Option<PathBuf>,
        /// Check mode: verify generated code is up-to-date (exit 1 if not).
        #[arg(long)]
        check: bool,
    },
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validate that `name` is lowercase alphanumeric + hyphens with no
/// leading/trailing hyphens and no consecutive hyphens.
fn validate_name(name: &str) -> anyhow::Result<()> {
    ensure!(!name.is_empty(), "name must not be empty");
    ensure!(
        !name.starts_with('-') && !name.ends_with('-'),
        "name must not start or end with a hyphen"
    );
    ensure!(
        name.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "name must contain only lowercase letters, digits, and hyphens"
    );
    ensure!(
        !name.contains("--"),
        "name must not contain consecutive hyphens"
    );
    Ok(())
}

/// Convert a hyphen-separated name to PascalCase.
///   "calendar"    -> "Calendar"
///   "my-service"  -> "MyService"
fn to_pascal_case(name: &str) -> String {
    name.split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    upper + chars.as_str()
                }
                None => String::new(),
            }
        })
        .collect()
}

/// Convert a hyphen-separated name to snake_case.
///   "calendar"    -> "calendar"
///   "my-service"  -> "my_service"
fn to_snake_case(name: &str) -> String {
    name.replace('-', "_")
}

/// Walk upward from `start` looking for a `Cargo.toml` that contains
/// `[workspace]`. Returns the directory containing that file.
fn find_workspace_root(start: &Path) -> anyhow::Result<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            let content = fs::read_to_string(&candidate).context("failed to read Cargo.toml")?;
            if content.contains("[workspace]") {
                return Ok(dir);
            }
        }
        if !dir.pop() {
            bail!(
                "could not find a workspace Cargo.toml (with [workspace]) \
                 in any parent of {}",
                start.display()
            );
        }
    }
}

/// Read a template file and apply `{{name}}` / `{{Name}}` / `{{name_snake}}` replacements.
fn render_template(path: &Path, name: &str, pascal: &str) -> anyhow::Result<String> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read template {}", path.display()))?;
    let snake = to_snake_case(name);
    Ok(content
        .replace("{{name_snake}}", &snake)
        .replace("{{name}}", name)
        .replace("{{Name}}", pascal))
}

/// Write `content` to `path`, creating parent directories as needed.
fn write_file(path: &Path, content: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Insert two new member paths into the workspace `Cargo.toml`'s
/// `members = [...]` array. We do simple string manipulation: find the
/// closing `]` of the members list and insert the new entries before it.
fn update_workspace_members(
    cargo_toml: &Path,
    crate_path: &str,
    server_path: &str,
) -> anyhow::Result<()> {
    let content = fs::read_to_string(cargo_toml).context("failed to read workspace Cargo.toml")?;

    // Find the members array closing bracket.  We look for the first `]`
    // that appears after `members = [`.
    let members_start = content
        .find("members = [")
        .context("could not find `members = [` in workspace Cargo.toml")?;
    let after_open = members_start + "members = [".len();
    let close_bracket = content[after_open..]
        .find(']')
        .context("could not find closing `]` for members array")?
        + after_open;

    // Build the two new entries, matching the existing indentation style.
    let insertion = format!("  \"{crate_path}\",\n  \"{server_path}\",\n");

    let mut new_content = String::with_capacity(content.len() + insertion.len());
    new_content.push_str(&content[..close_bracket]);
    new_content.push_str(&insertion);
    new_content.push_str(&content[close_bracket..]);

    fs::write(cargo_toml, new_content).context("failed to write updated workspace Cargo.toml")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// `new` subcommand
// ---------------------------------------------------------------------------

/// Core scaffold logic that operates on a given workspace root.
/// Extracted from `cmd_new` to enable testing without relying on CWD.
fn cmd_new_in(name: &str, root: &Path) -> anyhow::Result<()> {
    // 1. Validate name
    validate_name(name)?;

    let pascal = to_pascal_case(name);

    println!("Workspace root: {}", root.display());

    // 2. Check that the crate doesn't already exist
    let crate_dir = root.join(format!("crates/twin-{name}"));
    ensure!(
        !crate_dir.exists(),
        "crates/twin-{name} already exists — aborting"
    );

    // 3. Render templates
    let templates = root.join("templates");
    ensure!(
        templates.is_dir(),
        "templates/ directory not found at workspace root"
    );

    let files: Vec<(PathBuf, PathBuf)> = vec![
        (
            templates.join("twin-crate/Cargo.toml.tmpl"),
            root.join(format!("crates/twin-{name}/Cargo.toml")),
        ),
        (
            templates.join("twin-crate/lib.rs.tmpl"),
            root.join(format!("crates/twin-{name}/src/lib.rs")),
        ),
        (
            templates.join("twin-server/Cargo.toml.tmpl"),
            root.join(format!("apps/twin-{name}-server/Cargo.toml")),
        ),
        (
            templates.join("twin-server/main.rs.tmpl"),
            root.join(format!("apps/twin-{name}-server/src/main.rs")),
        ),
        (
            templates.join("docker/Dockerfile.tmpl"),
            root.join(format!("docker/Dockerfile.{name}")),
        ),
    ];

    // 4. Write generated files
    println!();
    for (tmpl, dest) in &files {
        let rendered = render_template(tmpl, name, &pascal)?;
        write_file(dest, &rendered)?;
        // Print path relative to workspace root for readability.
        let rel = dest.strip_prefix(root).unwrap_or(dest);
        println!("  created  {}", rel.display());
    }

    // 5. Create empty scenarios directory
    let scenarios_dir = root.join(format!("scenarios/{name}"));
    fs::create_dir_all(&scenarios_dir)
        .with_context(|| format!("failed to create {}", scenarios_dir.display()))?;
    {
        let rel = scenarios_dir.strip_prefix(root).unwrap_or(&scenarios_dir);
        println!("  created  {}/", rel.display());
    }

    // 6. Update workspace Cargo.toml
    let crate_member = format!("crates/twin-{name}");
    let server_member = format!("apps/twin-{name}-server");
    update_workspace_members(&root.join("Cargo.toml"), &crate_member, &server_member)?;
    println!("  updated  Cargo.toml (workspace members)");

    // Summary
    println!();
    println!("Twin \"{name}\" scaffolded successfully.");
    println!();
    println!("Next steps:");
    println!("  1. Fill in domain types in crates/twin-{name}/src/lib.rs");
    println!("  2. Add scenario files to scenarios/{name}/");
    println!("  3. Run `cargo check --package twin-{name}` to verify");

    Ok(())
}

fn cmd_new(name: &str) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not determine current directory")?;
    let root = find_workspace_root(&cwd)?;
    cmd_new_in(name, &root)
}

fn cmd_generate(spec_path: &Path, output: Option<&Path>, check: bool) -> anyhow::Result<()> {
    let spec = generate::parse_spec(spec_path)?;

    if check {
        let out_path = output.context("--check requires --output")?;
        let up_to_date = generate::check_generated(&spec, out_path)?;
        if up_to_date {
            println!("Generated code is up-to-date.");
            Ok(())
        } else {
            bail!(
                "Generated code is out of date. Re-run: twin-cli generate --spec {} --output {}",
                spec_path.display(),
                out_path.display()
            );
        }
    } else {
        let code = generate::generate_code(&spec);
        if let Some(out_path) = output {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(out_path, &code)
                .with_context(|| format!("failed to write {}", out_path.display()))?;
            println!("Generated {} bytes to {}", code.len(), out_path.display());
        } else {
            print!("{code}");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Commands::New { name } => cmd_new(name),
        Commands::Generate {
            spec,
            output,
            check,
        } => cmd_generate(spec, output.as_deref(), *check),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // -----------------------------------------------------------------------
    // Existing unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn pascal_case_single_word() {
        assert_eq!(to_pascal_case("calendar"), "Calendar");
    }

    #[test]
    fn pascal_case_multi_word() {
        assert_eq!(to_pascal_case("my-service"), "MyService");
    }

    #[test]
    fn pascal_case_three_words() {
        assert_eq!(to_pascal_case("a-b-c"), "ABC");
    }

    #[test]
    fn snake_case_single_word() {
        assert_eq!(to_snake_case("calendar"), "calendar");
    }

    #[test]
    fn snake_case_multi_word() {
        assert_eq!(to_snake_case("my-service"), "my_service");
    }

    #[test]
    fn snake_case_three_words() {
        assert_eq!(to_snake_case("a-b-c"), "a_b_c");
    }

    #[test]
    fn validate_name_ok() {
        assert!(validate_name("calendar").is_ok());
        assert!(validate_name("my-service").is_ok());
        assert!(validate_name("foo123").is_ok());
    }

    #[test]
    fn validate_name_rejects_uppercase() {
        assert!(validate_name("Calendar").is_err());
    }

    #[test]
    fn validate_name_rejects_leading_hyphen() {
        assert!(validate_name("-foo").is_err());
    }

    #[test]
    fn validate_name_rejects_trailing_hyphen() {
        assert!(validate_name("foo-").is_err());
    }

    #[test]
    fn validate_name_rejects_consecutive_hyphens() {
        assert!(validate_name("foo--bar").is_err());
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_special_chars() {
        assert!(validate_name("foo_bar").is_err());
        assert!(validate_name("foo.bar").is_err());
    }

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Create a temporary directory with a fake workspace structure that
    /// mirrors the real repo layout enough for `cmd_new_in` to work.
    fn setup_fake_workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let root = tmp.path();

        // Minimal workspace Cargo.toml
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\n]\nresolver = \"2\"\n",
        )
        .unwrap();

        // Copy all five templates from the real repo
        let real_templates = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("templates");

        let template_files = [
            "twin-crate/Cargo.toml.tmpl",
            "twin-crate/lib.rs.tmpl",
            "twin-server/Cargo.toml.tmpl",
            "twin-server/main.rs.tmpl",
            "docker/Dockerfile.tmpl",
        ];

        for rel in &template_files {
            let src = real_templates.join(rel);
            let dest = root.join("templates").join(rel);
            fs::create_dir_all(dest.parent().unwrap()).unwrap();
            fs::copy(&src, &dest)
                .unwrap_or_else(|e| panic!("copy {} -> {}: {e}", src.display(), dest.display()));
        }

        tmp
    }

    // -----------------------------------------------------------------------
    // Integration tests: render_template
    // -----------------------------------------------------------------------

    #[test]
    fn render_template_replaces_placeholders() {
        let tmp = tempfile::tempdir().unwrap();
        let tmpl_path = tmp.path().join("test.tmpl");
        fs::write(&tmpl_path, "name={{name}} pascal={{Name}}").unwrap();

        let result = render_template(&tmpl_path, "my-svc", "MySvc").unwrap();
        assert_eq!(result, "name=my-svc pascal=MySvc");
    }

    #[test]
    fn render_template_replaces_name_snake() {
        let tmp = tempfile::tempdir().unwrap();
        let tmpl_path = tmp.path().join("snake.tmpl");
        fs::write(&tmpl_path, "use twin_{{name_snake}}::{{Name}}TwinService;").unwrap();

        let result = render_template(&tmpl_path, "my-svc", "MySvc").unwrap();
        assert_eq!(result, "use twin_my_svc::MySvcTwinService;");
    }

    #[test]
    fn render_template_no_placeholders_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let tmpl_path = tmp.path().join("plain.tmpl");
        let content = "no placeholders here";
        fs::write(&tmpl_path, content).unwrap();

        let result = render_template(&tmpl_path, "foo", "Foo").unwrap();
        assert_eq!(result, content);
    }

    #[test]
    fn render_template_missing_file_errors() {
        let result = render_template(Path::new("/nonexistent/file.tmpl"), "x", "X");
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Integration tests: update_workspace_members
    // -----------------------------------------------------------------------

    #[test]
    fn update_workspace_members_inserts_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo = tmp.path().join("Cargo.toml");
        fs::write(&cargo, "[workspace]\nmembers = [\n]\n").unwrap();

        update_workspace_members(&cargo, "crates/twin-foo", "apps/twin-foo-server").unwrap();

        let content = fs::read_to_string(&cargo).unwrap();
        assert!(
            content.contains("\"crates/twin-foo\""),
            "missing crate member in:\n{content}"
        );
        assert!(
            content.contains("\"apps/twin-foo-server\""),
            "missing server member in:\n{content}"
        );
    }

    #[test]
    fn update_workspace_members_preserves_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo = tmp.path().join("Cargo.toml");
        fs::write(
            &cargo,
            "[workspace]\nmembers = [\n  \"crates/existing\",\n]\n",
        )
        .unwrap();

        update_workspace_members(&cargo, "crates/twin-bar", "apps/twin-bar-server").unwrap();

        let content = fs::read_to_string(&cargo).unwrap();
        assert!(
            content.contains("\"crates/existing\""),
            "existing member was removed:\n{content}"
        );
        assert!(
            content.contains("\"crates/twin-bar\""),
            "new crate member missing:\n{content}"
        );
        assert!(
            content.contains("\"apps/twin-bar-server\""),
            "new server member missing:\n{content}"
        );
    }

    // -----------------------------------------------------------------------
    // Integration tests: write_file
    // -----------------------------------------------------------------------

    #[test]
    fn write_file_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let deep_path = tmp.path().join("a/b/c/file.txt");

        write_file(&deep_path, "hello").unwrap();

        assert_eq!(fs::read_to_string(&deep_path).unwrap(), "hello");
    }

    // -----------------------------------------------------------------------
    // Integration tests: find_workspace_root
    // -----------------------------------------------------------------------

    #[test]
    fn find_workspace_root_from_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();

        let sub = root.join("a/b/c");
        fs::create_dir_all(&sub).unwrap();

        let found = find_workspace_root(&sub).unwrap();
        assert_eq!(found, root);
    }

    #[test]
    fn find_workspace_root_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        // No Cargo.toml at all
        let result = find_workspace_root(tmp.path());
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Integration tests: full scaffold via cmd_new_in
    // -----------------------------------------------------------------------

    #[test]
    fn scaffold_generates_all_files() {
        let tmp = setup_fake_workspace();
        let root = tmp.path();

        cmd_new_in("test-svc", root).unwrap();

        // Crate files
        assert!(root.join("crates/twin-test-svc/Cargo.toml").exists());
        assert!(root.join("crates/twin-test-svc/src/lib.rs").exists());
        // Server files
        assert!(root.join("apps/twin-test-svc-server/Cargo.toml").exists());
        assert!(root.join("apps/twin-test-svc-server/src/main.rs").exists());
        // Dockerfile
        assert!(root.join("docker/Dockerfile.test-svc").exists());
        // Scenarios directory
        assert!(root.join("scenarios/test-svc").is_dir());
    }

    #[test]
    fn scaffold_updates_workspace_members() {
        let tmp = setup_fake_workspace();
        let root = tmp.path();

        cmd_new_in("test-svc", root).unwrap();

        let content = fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(
            content.contains("\"crates/twin-test-svc\""),
            "workspace missing crate member:\n{content}"
        );
        assert!(
            content.contains("\"apps/twin-test-svc-server\""),
            "workspace missing server member:\n{content}"
        );
    }

    #[test]
    fn scaffold_rejects_duplicate() {
        let tmp = setup_fake_workspace();
        let root = tmp.path();

        cmd_new_in("dup-svc", root).unwrap();

        let result = cmd_new_in("dup-svc", root);
        assert!(
            result.is_err(),
            "second scaffold with same name should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("already exists"),
            "error should mention 'already exists', got: {err}"
        );
    }

    #[test]
    fn scaffold_renders_templates_correctly() {
        let tmp = setup_fake_workspace();
        let root = tmp.path();

        cmd_new_in("my-thing", root).unwrap();

        // Check that no raw placeholders remain in any generated file
        let generated_files = [
            "crates/twin-my-thing/Cargo.toml",
            "crates/twin-my-thing/src/lib.rs",
            "apps/twin-my-thing-server/Cargo.toml",
            "apps/twin-my-thing-server/src/main.rs",
            "docker/Dockerfile.my-thing",
        ];

        for rel in &generated_files {
            let content =
                fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"));
            assert!(
                !content.contains("{{name_snake}}"),
                "{rel} still contains {{{{name_snake}}}} placeholder"
            );
            assert!(
                !content.contains("{{name}}"),
                "{rel} still contains {{{{name}}}} placeholder"
            );
            assert!(
                !content.contains("{{Name}}"),
                "{rel} still contains {{{{Name}}}} placeholder"
            );
        }

        // Verify correct PascalCase substitution in lib.rs
        let lib_rs = fs::read_to_string(root.join("crates/twin-my-thing/src/lib.rs")).unwrap();
        assert!(
            lib_rs.contains("MyThingEntity"),
            "lib.rs should contain MyThingEntity"
        );
        assert!(
            lib_rs.contains("MyThingTwinService"),
            "lib.rs should contain MyThingTwinService"
        );

        // Verify correct name substitution in crate Cargo.toml
        let crate_cargo = fs::read_to_string(root.join("crates/twin-my-thing/Cargo.toml")).unwrap();
        assert!(
            crate_cargo.contains("name = \"twin-my-thing\""),
            "crate Cargo.toml should have correct package name"
        );

        // Verify server main.rs references the correct types
        let server_main =
            fs::read_to_string(root.join("apps/twin-my-thing-server/src/main.rs")).unwrap();
        assert!(
            server_main.contains("MyThingTwinService"),
            "server main.rs should reference MyThingTwinService"
        );
        // Template uses `twin_{{name_snake}}` so hyphens become underscores
        assert!(
            server_main.contains("twin_my_thing"),
            "server main.rs should use twin_my_thing as crate import"
        );
    }
}
