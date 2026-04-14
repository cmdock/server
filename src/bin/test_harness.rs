//! Test harness CLI for exercising the cmdock-server REST API.
//!
//! Subcommands:
//!   smoke    — Hit all endpoints and verify response shapes
//!   seed     — Create a known set of test tasks via the API
//!   compare  — Run a filter against both TW CLI and the server, diff results
//!   filter   — Test filter against TW CLI locally (no server needed)
//!   unseed   — Delete all tasks created by seed

use std::collections::{BTreeSet, HashSet};
use std::io::Write;
use std::sync::LazyLock;
use std::time::Duration;

use clap::{Parser, Subcommand};
use colored::Colorize;
use regex::Regex;
use serde::Deserialize;
use tokio::process::Command;

/// Pre-compiled UUID regex (case-insensitive).
static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
        .expect("valid regex")
});

/// Unique tag applied to all seeded tasks for safe identification and cleanup.
const SEED_TAG: &str = "tc_harness_seed";

/// Tag applied to ephemeral smoke test tasks for cleanup on failure.
const SMOKE_TAG: &str = "tc_harness_smoke";

#[derive(Parser)]
#[command(name = "test-harness", about = "Test harness for cmdock-server")]
struct Cli {
    /// Server base URL (trailing slash is stripped automatically)
    #[arg(long, default_value = "http://localhost:8080", global = true)]
    url: String,

    /// Bearer token for authentication (not required for 'filter' subcommand)
    #[arg(long, env = "TC_TOKEN", global = true, default_value = "")]
    token: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Hit all endpoints and verify response shapes
    Smoke,
    /// Create a known set of test tasks via the API
    Seed,
    /// Run a filter against both TW CLI and the server, diff UUIDs
    Compare {
        /// View ID to use (looks up filter from server and applies it)
        view: String,
    },
    /// Test a filter against TW CLI locally (no server needed)
    Filter {
        /// Filter expression to test
        filter: String,
    },
    /// Delete tasks created by seed (identified by +tc_harness_seed tag)
    Unseed,
}

// --- API response types ---
// Use #[serde(default)] for robustness, but validate invariants after deserialization.

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct HealthResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default, alias = "pending_tasks")]
    pending_tasks: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TaskItem {
    #[serde(default)]
    uuid: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    due: Option<String>,
    #[serde(default)]
    urgency: Option<f64>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ViewConfig {
    #[serde(default)]
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    icon: String,
    #[serde(default)]
    filter: String,
    #[serde(default)]
    group: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TaskActionResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    output: String,
}

// --- HTTP client ---

fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("taskchampion-test-harness/0.1")
        .build()
        .expect("Failed to build HTTP client")
}

/// Normalise base URL by stripping trailing slashes.
fn normalise_url(url: &str) -> &str {
    url.trim_end_matches('/')
}

async fn get<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    path: &str,
) -> anyhow::Result<T> {
    let base = normalise_url(url);
    let resp = client
        .get(format!("{base}{path}"))
        .bearer_auth(token)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("GET {path} returned {status}: {body}");
    }
    let body = resp.text().await?;
    serde_json::from_str(&body).map_err(|e| {
        let snippet = &body[..body.len().min(200)];
        anyhow::anyhow!("GET {path}: failed to parse response: {e}\n  body: {snippet}")
    })
}

async fn post_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> anyhow::Result<T> {
    let base = normalise_url(url);
    let resp = client
        .post(format!("{base}{path}"))
        .bearer_auth(token)
        .json(body)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("POST {path} returned {status}: {body_text}");
    }
    let body_text = resp.text().await?;
    serde_json::from_str(&body_text).map_err(|e| {
        let snippet = &body_text[..body_text.len().min(200)];
        anyhow::anyhow!("POST {path}: failed to parse response: {e}\n  body: {snippet}")
    })
}

fn require_token(token: &str) -> anyhow::Result<()> {
    if token.is_empty() {
        anyhow::bail!("--token or TC_TOKEN env var is required for this command");
    }
    Ok(())
}

/// Safely truncate a UUID for display (no panic on short strings).
fn short_uuid(uuid: &str) -> &str {
    &uuid[..uuid.len().min(8)]
}

/// Extract a UUID from free-text output using regex fallback (case-insensitive).
fn extract_uuid(text: &str) -> Option<String> {
    // Try structured format first: "Created task <uuid>."
    if let Some(uuid) = text
        .strip_prefix("Created task ")
        .and_then(|s| s.strip_suffix('.'))
    {
        return Some(uuid.to_string());
    }

    // Regex fallback: find any UUID-shaped string (case-insensitive)
    UUID_RE.find(text).map(|m| m.as_str().to_string())
}

// --- Smoke test helpers ---

struct SmokeResults {
    pass: usize,
    fail: usize,
}

impl SmokeResults {
    fn new() -> Self {
        Self { pass: 0, fail: 0 }
    }

    fn record_ok(&mut self, msg: &str) {
        println!("{} {msg}", "OK".green());
        self.pass += 1;
    }

    fn record_fail(&mut self, msg: &str) {
        println!("{} {msg}", "FAIL".red());
        self.fail += 1;
    }

    fn print_check(&self, label: &str) {
        print!("  {label} ... ");
        let _ = std::io::stdout().flush();
    }

    fn summary(&self) -> anyhow::Result<()> {
        println!();
        println!(
            "Results: {} passed, {} failed",
            self.pass.to_string().green(),
            if self.fail > 0 {
                self.fail.to_string().red()
            } else {
                self.fail.to_string().green()
            }
        );
        if self.fail > 0 {
            anyhow::bail!("{} smoke tests failed", self.fail);
        }
        Ok(())
    }
}

// --- Subcommand: smoke ---

async fn cmd_smoke(url: &str, token: &str) -> anyhow::Result<()> {
    require_token(token)?;
    let client = build_client();
    println!("{}", "=== Smoke Tests ===".bold());
    let mut r = SmokeResults::new();

    // GET /healthz (no auth)
    r.print_check("GET /healthz");
    match client
        .get(format!("{}/healthz", normalise_url(url)))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<HealthResponse>().await {
                Ok(health) => {
                    // Validate invariants
                    if health.status.is_some() {
                        r.record_ok(&format!(
                            "status={} pending={}",
                            health.status.as_deref().unwrap_or("?"),
                            health
                                .pending_tasks
                                .as_ref()
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "?".to_string())
                        ));
                    } else {
                        r.record_fail("response missing 'status' field");
                    }
                }
                Err(e) => r.record_fail(&format!("response parse error: {e}")),
            }
        }
        Ok(resp) => r.record_fail(&format!("status={}", resp.status())),
        Err(e) => r.record_fail(&format!("{e}")),
    }

    // GET /api/views
    r.print_check("GET /api/views");
    match get::<Vec<ViewConfig>>(&client, url, token, "/api/views").await {
        Ok(views) => {
            // Validate: views should have non-empty ids
            let valid = views.iter().all(|v| !v.id.is_empty());
            if valid {
                r.record_ok(&format!("{} views", views.len()));
            } else {
                r.record_fail("some views have empty id");
            }
        }
        Err(e) => r.record_fail(&format!("{e}")),
    }

    // GET /api/tasks (no view filter)
    r.print_check("GET /api/tasks");
    match get::<Vec<TaskItem>>(&client, url, token, "/api/tasks").await {
        Ok(tasks) => {
            // Validate: tasks should have non-empty UUIDs
            let valid = tasks.iter().all(|t| !t.uuid.is_empty());
            if valid {
                r.record_ok(&format!("{} tasks", tasks.len()));
            } else {
                r.record_fail("some tasks have empty uuid");
            }
        }
        Err(e) => r.record_fail(&format!("{e}")),
    }

    // POST /api/tasks (add, complete, delete — full lifecycle)
    r.print_check("POST /api/tasks");
    let add_body = serde_json::json!({"raw": format!("+{SMOKE_TAG} Test smoke task")});
    match post_json::<TaskActionResponse>(&client, url, token, "/api/tasks", &add_body).await {
        Ok(resp) if resp.success => {
            match extract_uuid(&resp.output) {
                Some(uuid) => {
                    r.record_ok(&format!("uuid={}", short_uuid(&uuid)));

                    // Complete
                    r.print_check(&format!("POST /api/tasks/{uuid}/done"));
                    match post_json::<TaskActionResponse>(
                        &client,
                        url,
                        token,
                        &format!("/api/tasks/{uuid}/done"),
                        &serde_json::json!({}),
                    )
                    .await
                    {
                        Ok(resp) if resp.success => r.record_ok("completed"),
                        Ok(resp) => r.record_fail(&resp.output),
                        Err(e) => r.record_fail(&format!("{e}")),
                    }

                    // Delete (cleanup)
                    r.print_check(&format!("POST /api/tasks/{uuid}/delete"));
                    match post_json::<TaskActionResponse>(
                        &client,
                        url,
                        token,
                        &format!("/api/tasks/{uuid}/delete"),
                        &serde_json::json!({}),
                    )
                    .await
                    {
                        Ok(resp) if resp.success => r.record_ok("deleted"),
                        Ok(resp) => r.record_fail(&resp.output),
                        Err(e) => r.record_fail(&format!("{e}")),
                    }
                }
                None => {
                    r.record_fail(&format!("could not extract UUID from: {}", resp.output));
                    // Best-effort cleanup by tag
                    cleanup_by_tag(&client, url, token, SMOKE_TAG).await;
                }
            }
        }
        Ok(resp) => r.record_fail(&resp.output),
        Err(e) => r.record_fail(&format!("{e}")),
    }

    // POST /api/sync
    r.print_check("POST /api/sync");
    match post_json::<TaskActionResponse>(&client, url, token, "/api/sync", &serde_json::json!({}))
        .await
    {
        Ok(resp) if resp.success => r.record_ok(&resp.output),
        Ok(resp) => r.record_fail(&resp.output),
        Err(e) => r.record_fail(&format!("{e}")),
    }

    // GET /api/app-config
    r.print_check("GET /api/app-config");
    match get::<serde_json::Value>(&client, url, token, "/api/app-config").await {
        Ok(config) => match config.as_object() {
            Some(obj) => {
                let keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
                r.record_ok(&format!("keys: {}", keys.join(", ")));
            }
            None => r.record_fail("response is not a JSON object"),
        },
        Err(e) => r.record_fail(&format!("{e}")),
    }

    // GET /api/contexts
    r.print_check("GET /api/contexts");
    match get::<Vec<serde_json::Value>>(&client, url, token, "/api/contexts").await {
        Ok(ctx) => r.record_ok(&format!("{} contexts", ctx.len())),
        Err(e) => r.record_fail(&format!("{e}")),
    }

    // GET /api/stores
    r.print_check("GET /api/stores");
    match get::<Vec<serde_json::Value>>(&client, url, token, "/api/stores").await {
        Ok(stores) => r.record_ok(&format!("{} stores", stores.len())),
        Err(e) => r.record_fail(&format!("{e}")),
    }

    // Always cleanup any leftover smoke tasks (idempotent safety net)
    cleanup_by_tag(&client, url, token, SMOKE_TAG).await;

    r.summary()
}

/// Best-effort cleanup: delete all tasks with the given tag.
async fn cleanup_by_tag(client: &reqwest::Client, url: &str, token: &str, tag: &str) {
    if let Ok(tasks) = get::<Vec<TaskItem>>(client, url, token, "/api/tasks").await {
        for task in tasks.iter().filter(|t| t.tags.iter().any(|t| t == tag)) {
            let _ = post_json::<TaskActionResponse>(
                client,
                url,
                token,
                &format!("/api/tasks/{}/delete", task.uuid),
                &serde_json::json!({}),
            )
            .await;
        }
    }
}

// --- Subcommand: seed ---

/// Seed task definitions. Each gets +tc_harness_seed tag for safe cleanup.
const SEED_TASKS: &[&str] = &[
    "project:PERSONAL.Home +shopping +coles priority:H due:today Buy milk",
    "project:PERSONAL.Home +shopping +woolworths Buy bread",
    "project:PERSONAL.Home +shopping +bunnings Buy screws",
    "project:PERSONAL.Health +gym priority:M due:tomorrow Morning workout",
    "project:PERSONAL.Health due:eow Book dentist appointment",
    "project:PERSONAL +reading Read chapter 5",
    "project:10FIFTEEN priority:H due:today Review PR for auth module",
    "project:10FIFTEEN.Backend +deploy due:eow Deploy staging fixes",
    "project:SSRP +meeting due:tomorrow Sprint planning",
    "project:SSRP priority:L Organise shared drive",
    "+urgent priority:H due:today Call plumber",
    "due:later Someday organise garage",
    "project:PERSONAL +errands priority:M due:yesterday Return library books",
];

async fn cmd_seed(url: &str, token: &str) -> anyhow::Result<()> {
    require_token(token)?;
    let client = build_client();
    println!("{}", "=== Seeding Test Tasks ===".bold());

    let mut created = 0;
    let mut failures = 0;
    for raw in SEED_TASKS {
        let tagged_raw = format!("+{SEED_TAG} {raw}");
        let body = serde_json::json!({"raw": tagged_raw});
        match post_json::<TaskActionResponse>(&client, url, token, "/api/tasks", &body).await {
            Ok(resp) if resp.success => {
                println!("  {} {}", "created".green(), raw);
                created += 1;
            }
            Ok(resp) => {
                println!("  {} {} — {}", "FAIL".red(), raw, resp.output);
                failures += 1;
            }
            Err(e) => {
                println!("  {} {} — {e}", "FAIL".red(), raw);
                failures += 1;
            }
        }
    }

    println!();
    println!(
        "Created: {created}  Failed: {failures}  Total: {}",
        SEED_TASKS.len()
    );
    if failures > 0 {
        anyhow::bail!(
            "{failures} of {} seed tasks failed to create",
            SEED_TASKS.len()
        );
    }
    Ok(())
}

// --- Subcommand: unseed ---

async fn cmd_unseed(url: &str, token: &str) -> anyhow::Result<()> {
    require_token(token)?;
    let client = build_client();
    println!("{}", "=== Removing Seeded Tasks ===".bold());

    let tasks: Vec<TaskItem> = get(&client, url, token, "/api/tasks").await?;
    let tagged: Vec<&TaskItem> = tasks
        .iter()
        .filter(|t| t.tags.iter().any(|tag| tag == SEED_TAG))
        .collect();

    println!("Found {} tasks tagged +{SEED_TAG}.", tagged.len());

    let mut deleted = 0;
    let mut failed = 0;
    for task in &tagged {
        match post_json::<TaskActionResponse>(
            &client,
            url,
            token,
            &format!("/api/tasks/{}/delete", task.uuid),
            &serde_json::json!({}),
        )
        .await
        {
            Ok(resp) if resp.success => {
                println!(
                    "  {} {} — {}",
                    "deleted".green(),
                    short_uuid(&task.uuid),
                    task.description
                );
                deleted += 1;
            }
            Ok(resp) => {
                println!(
                    "  {} {} — {}",
                    "FAIL".red(),
                    short_uuid(&task.uuid),
                    resp.output
                );
                failed += 1;
            }
            Err(e) => {
                println!("  {} {} — {e}", "FAIL".red(), short_uuid(&task.uuid));
                failed += 1;
            }
        }
    }

    println!();
    println!(
        "Deleted: {deleted}  Failed: {failed}  Found: {}",
        tagged.len()
    );
    if failed > 0 {
        anyhow::bail!("{failed} tasks failed to delete");
    }
    Ok(())
}

// --- Subcommand: compare ---

async fn cmd_compare(url: &str, token: &str, view_id: &str) -> anyhow::Result<()> {
    require_token(token)?;
    let client = build_client();
    println!("{}", "=== Filter Comparison ===".bold());

    let views: Vec<ViewConfig> = get(&client, url, token, "/api/views").await?;
    let view = views.iter().find(|v| v.id == view_id).ok_or_else(|| {
        anyhow::anyhow!(
            "View '{view_id}' not found. Available: {}",
            views
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    let filter = &view.filter;
    println!("View:   {view_id}");
    println!("Filter: {}", filter.cyan());
    println!();

    let server_tasks: Vec<TaskItem> = get(
        &client,
        url,
        token,
        &format!("/api/tasks?view={}", urlencoding::encode(view_id)),
    )
    .await?;
    let server_uuids: HashSet<String> = server_tasks.iter().map(|t| t.uuid.clone()).collect();

    let tw_uuids = run_tw_filter(filter).await?;

    print_diff(&server_tasks, &server_uuids, &tw_uuids);
    Ok(())
}

// --- Subcommand: filter ---

async fn cmd_filter(filter: &str) -> anyhow::Result<()> {
    println!("{}", "=== Local Filter Test ===".bold());
    println!("Filter: {}", filter.cyan());
    println!();

    let args = shell_words::split(filter)?;
    let output = Command::new("task")
        .args(&args)
        .arg("export")
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("task CLI failed: {}", stderr.trim());
    }

    let json_str = String::from_utf8_lossy(&output.stdout);
    let tw_tasks: Vec<serde_json::Value> = serde_json::from_str(&json_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse task export JSON: {e}"))?;

    println!("TW CLI returned {} tasks.", tw_tasks.len());
    println!();

    println!("{}", "Tasks matching filter:".bold());
    for task in &tw_tasks {
        let uuid = task.get("uuid").and_then(|u| u.as_str()).unwrap_or("?");
        let desc = task
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("?");
        let project = task.get("project").and_then(|p| p.as_str()).unwrap_or("-");
        let status = task.get("status").and_then(|s| s.as_str()).unwrap_or("?");
        let tags: Vec<&str> = task
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        println!(
            "  {} {} {} [{}] {}",
            short_uuid(uuid),
            status.dimmed(),
            desc,
            project.dimmed(),
            if tags.is_empty() {
                String::new()
            } else {
                format!("+{}", tags.join(" +")).dimmed().to_string()
            }
        );
    }

    Ok(())
}

// --- Helpers ---

/// Run `task <filter> export` and return the set of matching task UUIDs.
async fn run_tw_filter(filter: &str) -> anyhow::Result<HashSet<String>> {
    let args = shell_words::split(filter)
        .map_err(|e| anyhow::anyhow!("Invalid filter expression: {e}"))?;

    let output = Command::new("task")
        .args(&args)
        .arg("export")
        .output()
        .await;

    match output {
        Ok(output) if output.status.success() => {
            let json_str = String::from_utf8_lossy(&output.stdout);
            let tw_tasks: Vec<serde_json::Value> = serde_json::from_str(&json_str)
                .map_err(|e| anyhow::anyhow!("Failed to parse task export JSON: {e}"))?;
            Ok(tw_tasks
                .iter()
                .filter_map(|t| {
                    t.get("uuid")
                        .and_then(|u| u.as_str())
                        .map(|s| s.to_string())
                })
                .collect())
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("task CLI failed: {}", stderr.trim());
        }
        Err(e) => {
            anyhow::bail!("task CLI not available: {e}");
        }
    }
}

/// Print a diff between server and TW CLI UUID sets, with deterministic sort order.
fn print_diff(
    server_tasks: &[TaskItem],
    server_uuids: &HashSet<String>,
    tw_uuids: &HashSet<String>,
) {
    let only_server: BTreeSet<&String> = server_uuids.difference(tw_uuids).collect();
    let only_tw: BTreeSet<&String> = tw_uuids.difference(server_uuids).collect();
    let matching = server_uuids.intersection(tw_uuids).count();

    println!("{}", "Results:".bold());
    println!(
        "  Server: {} tasks    TW CLI: {} tasks    Matching: {}",
        server_uuids.len().to_string().cyan(),
        tw_uuids.len().to_string().cyan(),
        matching.to_string().green()
    );

    if only_server.is_empty() && only_tw.is_empty() {
        println!();
        println!("  {} All UUIDs match!", "PASS".green().bold());
    } else {
        if !only_server.is_empty() {
            println!();
            println!(
                "  {} {} tasks only in server:",
                "DIFF".yellow().bold(),
                only_server.len()
            );
            for uuid in &only_server {
                let desc = server_tasks
                    .iter()
                    .find(|t| &t.uuid == *uuid)
                    .map(|t| t.description.as_str())
                    .unwrap_or("?");
                println!("    + {} {}", short_uuid(uuid), desc.dimmed());
            }
        }

        if !only_tw.is_empty() {
            println!();
            println!(
                "  {} {} tasks only in TW CLI:",
                "DIFF".yellow().bold(),
                only_tw.len()
            );
            for uuid in &only_tw {
                println!("    - {}", short_uuid(uuid));
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Smoke => cmd_smoke(&cli.url, &cli.token).await,
        Commands::Seed => cmd_seed(&cli.url, &cli.token).await,
        Commands::Compare { view } => cmd_compare(&cli.url, &cli.token, view).await,
        Commands::Filter { filter } => cmd_filter(filter).await,
        Commands::Unseed => cmd_unseed(&cli.url, &cli.token).await,
    }
}
