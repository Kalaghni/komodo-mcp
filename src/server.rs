//! The MCP tool surface.
//!
//! Tools come in three routers that are combined according to `KOMODO_MCP_MODE`:
//! - read:    always on. Only ever calls Komodo's `/read` API.
//! - execute: deploy / restart / run builds, procedures, actions, syncs.
//! - write:   create / update / delete resource configuration.
//!
//! Tools that aren't enabled are never listed, so the model can't call them.
//! Destructive requests (destroy, delete, remove, prune, rotate, clear) are
//! refused unless `KOMODO_MCP_ALLOW_DESTRUCTIVE` is set, whatever the mode.

use std::{sync::Arc, time::Duration};

use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::{
    config::{Config, Mode},
    format,
    komodo::{Api, Komodo, extract_id},
};

type ToolResult = Result<CallToolResult, ErrorData>;

// ---------------------------------------------------------------------------
// Shared parameter types
// ---------------------------------------------------------------------------

/// The Komodo resource types.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Server,
    Swarm,
    Stack,
    Deployment,
    Build,
    Builder,
    Repo,
    Procedure,
    Action,
    ResourceSync,
    Alerter,
}

impl Kind {
    const ALL: [Kind; 11] = [
        Kind::Server,
        Kind::Swarm,
        Kind::Stack,
        Kind::Deployment,
        Kind::Build,
        Kind::Builder,
        Kind::Repo,
        Kind::Procedure,
        Kind::Action,
        Kind::ResourceSync,
        Kind::Alerter,
    ];

    /// The PascalCase name Komodo uses in request names and update targets.
    fn pascal(self) -> &'static str {
        match self {
            Kind::Server => "Server",
            Kind::Swarm => "Swarm",
            Kind::Stack => "Stack",
            Kind::Deployment => "Deployment",
            Kind::Build => "Build",
            Kind::Builder => "Builder",
            Kind::Repo => "Repo",
            Kind::Procedure => "Procedure",
            Kind::Action => "Action",
            Kind::ResourceSync => "ResourceSync",
            Kind::Alerter => "Alerter",
        }
    }

    /// The parameter name Get* requests use for the resource.
    fn param(self) -> &'static str {
        match self {
            Kind::ResourceSync => "sync",
            Kind::Server => "server",
            Kind::Swarm => "swarm",
            Kind::Stack => "stack",
            Kind::Deployment => "deployment",
            Kind::Build => "build",
            Kind::Builder => "builder",
            Kind::Repo => "repo",
            Kind::Procedure => "procedure",
            Kind::Action => "action",
            Kind::Alerter => "alerter",
        }
    }

    fn plural_label(self) -> String {
        match self {
            Kind::ResourceSync => "resource_syncs".into(),
            other => format!("{}s", other.param()),
        }
    }
}

/// Options shared by every log tool.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct LogOptions {
    /// Number of most recent lines to return (default 100, max 5000). Ignored when `search` is set.
    pub tail: Option<u64>,
    /// Search the full log for these terms instead of tailing. Matching lines are returned.
    pub search: Option<Vec<String>>,
    /// With `search`: require every term on a line (AND) instead of any term (OR). Default false.
    pub match_all: Option<bool>,
    /// With `search`: return lines that do NOT match. Default false.
    pub invert: Option<bool>,
    /// Prefix each line with its Docker timestamp. Default false.
    pub timestamps: Option<bool>,
}

impl LogOptions {
    /// Returns the request name to use and fills in the shared params.
    fn apply(&self, get_req: &'static str, search_req: &'static str, params: &mut Map<String, Value>) -> &'static str {
        params.insert("timestamps".into(), json!(self.timestamps.unwrap_or(false)));
        match self.search.as_ref().filter(|t| !t.is_empty()) {
            Some(terms) => {
                params.insert("terms".into(), json!(terms));
                params.insert(
                    "combinator".into(),
                    json!(if self.match_all.unwrap_or(false) { "And" } else { "Or" }),
                );
                params.insert("invert".into(), json!(self.invert.unwrap_or(false)));
                search_req
            }
            None => {
                params.insert("tail".into(), json!(self.tail.unwrap_or(100).clamp(1, 5000)));
                get_req
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Read tool parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListParams {
    /// Which resource type to list.
    pub kind: Kind,
    /// Only return resources with these exact names.
    pub names: Option<Vec<String>>,
    /// Only return resources that have these tags (tag names or ids).
    pub tags: Option<Vec<String>>,
    /// Free-text search terms matched against resource names.
    pub search: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetParams {
    /// The resource type.
    pub kind: Kind,
    /// Resource name or id.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ServerStatsParams {
    /// Server name or id.
    pub server: String,
    /// Also return static system information (OS, kernel, CPU model, core count, total memory). Default false.
    pub include_system_info: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ServerParam {
    /// Server name or id.
    pub server: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StackParam {
    /// Stack name or id.
    pub stack: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StackLogParams {
    /// Stack name or id.
    pub stack: String,
    /// Limit to these compose service names. Empty or omitted means all services.
    pub services: Option<Vec<String>>,
    #[serde(flatten)]
    pub log: LogOptions,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeploymentLogParams {
    /// Deployment name or id.
    pub deployment: String,
    #[serde(flatten)]
    pub log: LogOptions,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContainerLogParams {
    /// Server name or id the container runs on.
    pub server: String,
    /// Container name.
    pub container: String,
    #[serde(flatten)]
    pub log: LogOptions,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SwarmView {
    /// `docker swarm inspect`: cluster-wide settings and join info.
    Info,
    Nodes,
    Services,
    Tasks,
    Stacks,
    Configs,
    Secrets,
    Networks,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SwarmParams {
    /// Swarm resource name or id (as configured in Komodo).
    pub swarm: String,
    /// What to show.
    pub view: SwarmView,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SwarmServiceLogParams {
    /// Swarm resource name or id.
    pub swarm: String,
    /// Swarm service name or id (e.g. `mystack_web`).
    pub service: String,
    #[serde(flatten)]
    pub log: LogOptions,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdatesParams {
    /// Only show updates for this resource type. Requires `name`.
    pub kind: Option<Kind>,
    /// Only show updates for this resource (name or id). Requires `kind`.
    pub name: Option<String>,
    /// Page number from a previous call's "more available" line. Default 0.
    pub page: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateIdParam {
    /// Update id, as returned by komodo_updates or an execute tool.
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AlertsParams {
    /// Include alerts that have already resolved. Default false (open alerts only).
    pub include_resolved: Option<bool>,
    /// Page number from a previous call. Default 0.
    pub page: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RawParams {
    /// Komodo request type in PascalCase, e.g. `ListSwarmNodes`, `GetStack`, `InspectSwarmService`.
    /// The full list is at https://docs.rs/komodo_client/latest/komodo_client/api/
    pub request: String,
    /// The request's parameters as a JSON object, e.g. `{"swarm": "my-swarm"}`.
    #[schemars(with = "Option<Map<String, Value>>")]
    pub params: Option<Value>,
}

// ---------------------------------------------------------------------------
// Execute tool parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StackAction {
    Deploy,
    /// Deploy only if the compose files or config changed since the last deploy.
    DeployIfChanged,
    Pull,
    Start,
    Restart,
    Stop,
    Pause,
    Unpause,
    /// `docker compose down` / `docker stack rm`. Needs KOMODO_MCP_ALLOW_DESTRUCTIVE.
    Destroy,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StackActionParams {
    /// Stack name or id.
    pub stack: String,
    pub action: StackAction,
    /// Limit to these compose services (not used by deploy_if_changed). Omit for the whole stack.
    pub services: Option<Vec<String>>,
    /// Wait for the operation to finish and return its logs. Default true.
    pub wait: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentAction {
    Deploy,
    Pull,
    Start,
    Restart,
    Stop,
    Pause,
    Unpause,
    /// Remove the container / swarm service. Needs KOMODO_MCP_ALLOW_DESTRUCTIVE.
    Destroy,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeploymentActionParams {
    /// Deployment name or id.
    pub deployment: String,
    pub action: DeploymentAction,
    /// Wait for the operation to finish and return its logs. Default true.
    pub wait: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    /// Run a Build (docker build + push).
    Build,
    Procedure,
    /// Run an Action script. Pass arguments in `args`.
    Action,
    /// Execute a Resource Sync (applies pending changes).
    ResourceSync,
    RepoClone,
    RepoPull,
    /// Build a Repo on its attached Builder.
    RepoBuild,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunParams {
    pub kind: RunKind,
    /// Resource name or id.
    pub name: String,
    /// Only for `action`: arguments object passed to the Action script.
    #[schemars(with = "Option<Map<String, Value>>")]
    pub args: Option<Value>,
    /// Wait for the run to finish and return its logs. Default true.
    pub wait: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RawExecuteParams {
    /// Komodo execute request in PascalCase, e.g. `UpdateSwarmNode`, `BatchDeployStack`, `RestartContainer`.
    pub request: String,
    /// The request's parameters as a JSON object.
    #[schemars(with = "Option<Map<String, Value>>")]
    pub params: Option<Value>,
    /// Wait for the operation to finish and return its logs. Default true.
    pub wait: Option<bool>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct KomodoMcp {
    komodo: Komodo,
    cfg: Arc<Config>,
    tool_router: ToolRouter<Self>,
}

/// Words that mark a request as destructive.
const DESTRUCTIVE: [&str; 6] = ["Destroy", "Delete", "Remove", "Prune", "Rotate", "Clear"];

/// Requests this server never sends: credentials, users and permissions are
/// managed in the Komodo UI by a person, not by a model.
const NEVER: [&str; 5] = ["ApiKey", "OnboardingKey", "User", "Permission", "Login"];

fn ok(text: String) -> ToolResult {
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

fn fail(msg: impl std::fmt::Display) -> ToolResult {
    Ok(CallToolResult::error(vec![ContentBlock::text(msg.to_string())]))
}

impl KomodoMcp {
    pub fn new(komodo: Komodo, cfg: Config) -> Self {
        let mut router = Self::read_tools();
        if cfg.mode >= Mode::Execute {
            router += Self::execute_tools();
        }
        if cfg.mode >= Mode::Write {
            router += Self::write_tools();
        }
        KomodoMcp { komodo, cfg: Arc::new(cfg), tool_router: router }
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tool_router.list_all().into_iter().map(|t| t.name.to_string()).collect()
    }

    fn max(&self) -> usize {
        self.cfg.max_output_chars
    }

    /// Send a JSON result, truncated from the end.
    fn json_result(&self, res: anyhow::Result<Value>) -> ToolResult {
        match res {
            Ok(v) => ok(format::truncate_head(format::json(&v), self.max())),
            Err(e) => fail(format!("{e:#}")),
        }
    }

    /// Send a log result, keeping the newest lines.
    fn log_result(&self, res: anyhow::Result<Value>) -> ToolResult {
        match res {
            Ok(v) => ok(format::truncate_tail(format::log(&v), self.max())),
            Err(e) => fail(format!("{e:#}")),
        }
    }

    /// Refuse requests this configuration doesn't allow. Returns the refusal text.
    fn guard(&self, request: &str) -> Option<String> {
        if let Some(word) = NEVER.iter().find(|w| request.contains(*w)) {
            return Some(format!(
                "Refused: '{request}' touches {word} management, which komodo-mcp never does. Do this in the Komodo UI."
            ));
        }
        if !self.cfg.allow_destructive
            && let Some(word) = DESTRUCTIVE.iter().find(|w| request.contains(*w)) {
                return Some(format!(
                    "Refused: '{request}' is destructive ({word}) and KOMODO_MCP_ALLOW_DESTRUCTIVE is off. \
                     Ask the user to run it from the Komodo UI, or to enable destructive operations for this server."
                ));
            }
        None
    }

    /// Run an execute request, then optionally wait for its Update to finish.
    async fn execute(&self, request: &str, params: Value, wait: bool) -> ToolResult {
        if let Some(refusal) = self.guard(request) {
            return fail(refusal);
        }
        let res = match self.komodo.call(Api::Execute, request, params).await {
            Ok(v) => v,
            Err(e) => return fail(format!("{e:#}")),
        };

        // Batch requests return [{status: "Ok", data: Update} | {status: "Err", data: {name, error}}].
        if let Some(items) = res.as_array() {
            let mut out = format!("{request}: {} item(s)\n", items.len());
            for item in items {
                let data = item.get("data").cloned().unwrap_or(Value::Null);
                if item.get("status").and_then(Value::as_str) == Some("Ok") {
                    let update = if wait { self.wait_for(data).await } else { data };
                    out.push_str(&format!("\n{}", format::update(&update)));
                } else {
                    let name = data.get("name").and_then(Value::as_str).unwrap_or("?");
                    let err = data.get("error").map(format::json).unwrap_or_default();
                    out.push_str(&format!("\n{name}: FAILED to start — {err}\n"));
                }
            }
            return ok(format::truncate_tail(out, self.max()));
        }

        let update = if wait { self.wait_for(res).await } else { res };
        let mut text = format::update(&update);
        if update.get("status").and_then(Value::as_str) != Some("Complete") {
            text.push_str(&format!(
                "\nNot finished yet. Check progress with komodo_get_update id = {}",
                extract_id(&update).unwrap_or_default()
            ));
        }
        ok(format::truncate_tail(text, self.max()))
    }

    /// Poll GetUpdate until the update completes or the wait timeout passes.
    async fn wait_for(&self, update: Value) -> Value {
        let Some(id) = extract_id(&update) else { return update };
        let deadline = Instant::now() + self.cfg.wait_timeout;
        let mut current = update;
        let mut delay = Duration::from_millis(750);
        while current.get("status").and_then(Value::as_str) != Some("Complete") && Instant::now() < deadline {
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(5));
            match self.komodo.read("GetUpdate", json!({ "id": id })).await {
                Ok(v) => current = v,
                Err(e) => {
                    tracing::warn!("polling update {id} failed: {e:#}");
                }
            }
        }
        current
    }

    /// Resolve a resource name to its Komodo id.
    async fn resolve_id(&self, kind: Kind, name: &str) -> anyhow::Result<String> {
        let v = self
            .komodo
            .read(&format!("Get{}", kind.pascal()), json!({ kind.param(): name }))
            .await?;
        extract_id(&v).ok_or_else(|| anyhow::anyhow!("Komodo returned no id for {} '{name}'", kind.pascal()))
    }
}

// ---------------------------------------------------------------------------
// Read tools
// ---------------------------------------------------------------------------

#[tool_router(router = read_tools)]
impl KomodoMcp {
    #[tool(
        description = "Start here. Returns the Komodo Core version and a health summary for every resource type \
            (how many servers are ok/down, stacks running/unhealthy, swarms healthy, builds failed, etc.).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_overview(&self) -> ToolResult {
        let version = self.komodo.read("GetVersion", json!({}));
        let summaries = futures::future::join_all(Kind::ALL.iter().map(|k| async move {
            let req = match k {
                Kind::ResourceSync => "GetResourceSyncsSummary".to_string(),
                other => format!("Get{}sSummary", other.pascal()),
            };
            (k.plural_label(), self.komodo.read(&req, json!({})).await)
        }));
        let (version, summaries) = futures::join!(version, summaries);

        let mut out = format!("Komodo at {}\n", self.komodo.address());
        match version {
            Ok(v) => out.push_str(&format!(
                "core version: {}\n",
                v.get("version").and_then(Value::as_str).unwrap_or("unknown")
            )),
            Err(e) => return fail(format!("{e:#}")),
        }
        out.push_str(&format!(
            "komodo-mcp mode: {}{}\n\n",
            self.cfg.mode,
            if self.cfg.allow_destructive { " (destructive allowed)" } else { "" }
        ));
        for (label, res) in summaries {
            match res {
                Ok(v) => out.push_str(&format!("{label}: {}\n", format::json(&v))),
                Err(e) => out.push_str(&format!("{label}: unavailable ({})\n", e.to_string().lines().next().unwrap_or(""))),
            }
        }
        ok(out)
    }

    #[tool(
        description = "List Komodo resources of one type with their current state (e.g. stack status, server health, \
            swarm health, build version, last run). Optional filters by exact names, tags, or search terms.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_list(&self, Parameters(p): Parameters<ListParams>) -> ToolResult {
        let mut query = Map::new();
        if let Some(n) = p.names {
            query.insert("names".into(), json!(n));
        }
        if let Some(t) = p.tags {
            query.insert("tags".into(), json!(t));
        }
        if let Some(s) = p.search {
            query.insert("terms".into(), json!(s));
        }
        let req = format!("List{}s", p.kind.pascal());
        self.json_result(self.komodo.read(&req, json!({ "query": query })).await)
    }

    #[tool(
        description = "Get one Komodo resource's full configuration and info by name or id \
            (e.g. a stack's compose source, server address, swarm managers, build repo and version).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_get(&self, Parameters(p): Parameters<GetParams>) -> ToolResult {
        let req = format!("Get{}", p.kind.pascal());
        self.json_result(self.komodo.read(&req, json!({ p.kind.param(): p.name })).await)
    }

    #[tool(
        description = "Live CPU, memory, disk and network stats for a server, as reported by its Periphery agent.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_server_stats(&self, Parameters(p): Parameters<ServerStatsParams>) -> ToolResult {
        let stats = self.komodo.read("GetSystemStats", json!({ "server": p.server }));
        if p.include_system_info.unwrap_or(false) {
            let info = self.komodo.read("GetSystemInformation", json!({ "server": p.server }));
            let (stats, info) = futures::join!(stats, info);
            self.json_result(stats.and_then(|s| info.map(|i| json!({ "stats": s, "system_info": i }))))
        } else {
            self.json_result(stats.await)
        }
    }

    #[tool(
        description = "List every Docker container on a server (name, image, state, status, ports, networks).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_containers(&self, Parameters(p): Parameters<ServerParam>) -> ToolResult {
        self.json_result(self.komodo.read("ListContainers", json!({ "server": p.server })).await)
    }

    #[tool(
        description = "List the services of a Komodo Stack and the state of each service's container(s).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_stack_services(&self, Parameters(p): Parameters<StackParam>) -> ToolResult {
        self.json_result(self.komodo.read("ListStackServices", json!({ "stack": p.stack })).await)
    }

    #[tool(
        description = "Logs for a Komodo Stack (docker compose logs, or the swarm services of a swarm stack). \
            Tails recent lines by default; pass `search` to grep the full log.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_stack_logs(&self, Parameters(p): Parameters<StackLogParams>) -> ToolResult {
        let mut params = Map::new();
        params.insert("stack".into(), json!(p.stack));
        params.insert("services".into(), json!(p.services.unwrap_or_default()));
        let req = p.log.apply("GetStackLog", "SearchStackLog", &mut params);
        self.log_result(self.komodo.read(req, Value::Object(params)).await)
    }

    #[tool(
        description = "Logs for a Komodo Deployment's container (or swarm service). \
            Tails recent lines by default; pass `search` to grep the full log.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_deployment_logs(&self, Parameters(p): Parameters<DeploymentLogParams>) -> ToolResult {
        let mut params = Map::new();
        params.insert("deployment".into(), json!(p.deployment));
        let req = p.log.apply("GetDeploymentLog", "SearchDeploymentLog", &mut params);
        self.log_result(self.komodo.read(req, Value::Object(params)).await)
    }

    #[tool(
        description = "Logs for any container on a server, by container name, even ones Komodo doesn't manage. \
            Tails recent lines by default; pass `search` to grep the full log.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_container_logs(&self, Parameters(p): Parameters<ContainerLogParams>) -> ToolResult {
        let mut params = Map::new();
        params.insert("server".into(), json!(p.server));
        params.insert("container".into(), json!(p.container));
        let req = p.log.apply("GetContainerLog", "SearchContainerLog", &mut params);
        self.log_result(self.komodo.read(req, Value::Object(params)).await)
    }

    #[tool(
        description = "Look inside a Docker Swarm through Komodo: cluster info, nodes (role, availability, status), \
            services (replicas running vs desired), tasks, stacks, configs, secrets (metadata only) or networks.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_swarm(&self, Parameters(p): Parameters<SwarmParams>) -> ToolResult {
        let req = match p.view {
            SwarmView::Info => "InspectSwarm",
            SwarmView::Nodes => "ListSwarmNodes",
            SwarmView::Services => "ListSwarmServices",
            SwarmView::Tasks => "ListSwarmTasks",
            SwarmView::Stacks => "ListSwarmStacks",
            SwarmView::Configs => "ListSwarmConfigs",
            SwarmView::Secrets => "ListSwarmSecrets",
            SwarmView::Networks => "ListSwarmNetworks",
        };
        self.json_result(self.komodo.read(req, json!({ "swarm": p.swarm })).await)
    }

    #[tool(
        description = "Logs for a Docker Swarm service, aggregated across its tasks. \
            Tails recent lines by default; pass `search` to grep the full log.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_swarm_service_logs(&self, Parameters(p): Parameters<SwarmServiceLogParams>) -> ToolResult {
        let mut params = Map::new();
        params.insert("swarm".into(), json!(p.swarm));
        params.insert("service".into(), json!(p.service));
        let req = p.log.apply("GetSwarmServiceLog", "SearchSwarmServiceLog", &mut params);
        self.log_result(self.komodo.read(req, Value::Object(params)).await)
    }

    #[tool(
        description = "Recent Komodo activity (deploys, builds, config changes, who did them, success or failure), \
            newest first. Filter to one resource with kind + name. Use komodo_get_update for full logs.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_updates(&self, Parameters(p): Parameters<UpdatesParams>) -> ToolResult {
        let query = match (p.kind, p.name) {
            (Some(kind), Some(name)) => match self.resolve_id(kind, &name).await {
                Ok(id) => json!({ "target.type": kind.pascal(), "target.id": id }),
                Err(e) => return fail(format!("{e:#}")),
            },
            (None, None) => Value::Null,
            _ => return fail("Pass both `kind` and `name` to filter, or neither."),
        };
        match self
            .komodo
            .read("ListUpdates", json!({ "query": query, "page": p.page.unwrap_or(0) }))
            .await
        {
            Ok(v) => ok(format::truncate_head(format::update_list(&v), self.max())),
            Err(e) => fail(format!("{e:#}")),
        }
    }

    #[tool(
        description = "Full detail of one Komodo update: status, every stage's command, stdout and stderr. \
            Use it to see why a deploy or build failed.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_get_update(&self, Parameters(p): Parameters<UpdateIdParam>) -> ToolResult {
        match self.komodo.read("GetUpdate", json!({ "id": p.id })).await {
            Ok(v) => ok(format::truncate_tail(format::update(&v), self.max())),
            Err(e) => fail(format!("{e:#}")),
        }
    }

    #[tool(
        description = "Komodo alerts (server down, high CPU/disk, unhealthy containers or swarm, failed builds). \
            Open alerts only unless include_resolved is true.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_alerts(&self, Parameters(p): Parameters<AlertsParams>) -> ToolResult {
        let query = if p.include_resolved.unwrap_or(false) { Value::Null } else { json!({ "resolved": false }) };
        self.json_result(
            self.komodo
                .read("ListAlerts", json!({ "query": query, "page": p.page.unwrap_or(0) }))
                .await,
        )
    }

    #[tool(
        description = "Call any Komodo read request directly, for things the other tools don't cover \
            (e.g. InspectSwarmNode, InspectSwarmService, ListImages, ListNetworks, GetHistoricalServerStats, \
            ExportResourcesToToml, ListBuildVersions). Read requests never change anything.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn komodo_read(&self, Parameters(p): Parameters<RawParams>) -> ToolResult {
        self.json_result(self.komodo.read(&p.request, p.params.unwrap_or(Value::Null)).await)
    }
}

// ---------------------------------------------------------------------------
// Execute tools (KOMODO_MCP_MODE=execute or write)
// ---------------------------------------------------------------------------

#[tool_router(router = execute_tools)]
impl KomodoMcp {
    #[tool(
        description = "Run an action on a Komodo Stack: deploy, deploy_if_changed, pull, start, restart, stop, pause, \
            unpause or destroy. Waits for completion and returns the logs unless wait is false. \
            Confirm with the user before stopping or destroying anything in production.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn komodo_stack_action(&self, Parameters(p): Parameters<StackActionParams>) -> ToolResult {
        let services = p.services.unwrap_or_default();
        let (req, params) = match p.action {
            StackAction::Deploy => ("DeployStack", json!({ "stack": p.stack, "services": services })),
            StackAction::DeployIfChanged => ("DeployStackIfChanged", json!({ "stack": p.stack })),
            StackAction::Pull => ("PullStack", json!({ "stack": p.stack, "services": services })),
            StackAction::Start => ("StartStack", json!({ "stack": p.stack, "services": services })),
            StackAction::Restart => ("RestartStack", json!({ "stack": p.stack, "services": services })),
            StackAction::Stop => ("StopStack", json!({ "stack": p.stack, "services": services })),
            StackAction::Pause => ("PauseStack", json!({ "stack": p.stack, "services": services })),
            StackAction::Unpause => ("UnpauseStack", json!({ "stack": p.stack, "services": services })),
            StackAction::Destroy => (
                "DestroyStack",
                json!({ "stack": p.stack, "services": services, "remove_orphans": false }),
            ),
        };
        self.execute(req, params, p.wait.unwrap_or(true)).await
    }

    #[tool(
        description = "Run an action on a Komodo Deployment: deploy, pull, start, restart, stop, pause, unpause or destroy. \
            Waits for completion and returns the logs unless wait is false. \
            Confirm with the user before stopping or destroying anything in production.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn komodo_deployment_action(&self, Parameters(p): Parameters<DeploymentActionParams>) -> ToolResult {
        let req = match p.action {
            DeploymentAction::Deploy => "Deploy",
            DeploymentAction::Pull => "PullDeployment",
            DeploymentAction::Start => "StartDeployment",
            DeploymentAction::Restart => "RestartDeployment",
            DeploymentAction::Stop => "StopDeployment",
            DeploymentAction::Pause => "PauseDeployment",
            DeploymentAction::Unpause => "UnpauseDeployment",
            DeploymentAction::Destroy => "DestroyDeployment",
        };
        self.execute(req, json!({ "deployment": p.deployment }), p.wait.unwrap_or(true)).await
    }

    #[tool(
        description = "Run a Build, Procedure, Action, Resource Sync, or a Repo clone / pull / build. \
            Waits for completion and returns the logs unless wait is false.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn komodo_run(&self, Parameters(p): Parameters<RunParams>) -> ToolResult {
        let (req, params) = match p.kind {
            RunKind::Build => ("RunBuild", json!({ "build": p.name })),
            RunKind::Procedure => ("RunProcedure", json!({ "procedure": p.name })),
            RunKind::Action => ("RunAction", json!({ "action": p.name, "args": p.args })),
            RunKind::ResourceSync => ("RunSync", json!({ "sync": p.name })),
            RunKind::RepoClone => ("CloneRepo", json!({ "repo": p.name })),
            RunKind::RepoPull => ("PullRepo", json!({ "repo": p.name })),
            RunKind::RepoBuild => ("BuildRepo", json!({ "repo": p.name })),
        };
        self.execute(req, params, p.wait.unwrap_or(true)).await
    }

    #[tool(
        description = "Call any Komodo execute request directly, e.g. UpdateSwarmNode (drain / activate a node), \
            RestartContainer, BatchDeployStack, CancelBuild. Destructive requests are refused unless enabled. \
            Confirm with the user before running anything that affects production.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn komodo_execute(&self, Parameters(p): Parameters<RawExecuteParams>) -> ToolResult {
        self.execute(&p.request, p.params.unwrap_or(Value::Null), p.wait.unwrap_or(true)).await
    }
}

// ---------------------------------------------------------------------------
// Write tools (KOMODO_MCP_MODE=write)
// ---------------------------------------------------------------------------

#[tool_router(router = write_tools)]
impl KomodoMcp {
    #[tool(
        description = "Call any Komodo write request: create, update, copy or rename resources and change their \
            configuration (e.g. UpdateStack with a partial config, CreateDeployment, UpdateVariableValue). \
            Update requests take {\"id\": \"<name or id>\", \"config\": {...only the fields to change...}}. \
            Read the resource first with komodo_get, and confirm changes with the user before writing.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn komodo_write(&self, Parameters(p): Parameters<RawParams>) -> ToolResult {
        if let Some(refusal) = self.guard(&p.request) {
            return fail(refusal);
        }
        self.json_result(self.komodo.call(Api::Write, &p.request, p.params.unwrap_or(Value::Null)).await)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KomodoMcp {
    fn get_info(&self) -> ServerConfig {
        let tiers = match self.cfg.mode {
            Mode::Read => "read-only: you can inspect everything but cannot change anything",
            Mode::Execute => "read + execute: you can inspect and run deploys, restarts, builds and procedures, but not change configuration",
            Mode::Write => "read + execute + write: you can inspect, run operations and change resource configuration",
        };
        let destructive = if self.cfg.allow_destructive {
            "Destructive operations (destroy, delete, remove, prune) are allowed — always confirm them with the user first."
        } else {
            "Destructive operations (destroy, delete, remove, prune) are disabled."
        };
        let instructions = format!(
            "Tools for the Komodo deployment platform at {}. This server is {tiers}. {destructive} \
             Start with komodo_overview, then komodo_list to find resources by name. Resource names \
             are accepted anywhere an id is. For Docker Swarm questions use komodo_swarm. When something \
             failed, komodo_updates and komodo_get_update show what ran and its output.",
            self.komodo.address()
        );
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("komodo-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(instructions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(mode: Mode, destructive: bool) -> KomodoMcp {
        let cfg = Config {
            address: "http://127.0.0.1:9".into(),
            api_key: "k".into(),
            api_secret: "s".into(),
            mode,
            allow_destructive: destructive,
            request_timeout: Duration::from_secs(1),
            wait_timeout: Duration::from_secs(1),
            max_output_chars: 1000,
            ca_cert_pem: None,
        };
        let komodo = Komodo::new(&cfg).unwrap();
        KomodoMcp::new(komodo, cfg)
    }

    #[test]
    fn tiers_control_listed_tools() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let read = server(Mode::Read, false).tool_names();
        assert!(read.contains(&"komodo_overview".to_string()));
        assert!(!read.iter().any(|t| t == "komodo_stack_action" || t == "komodo_write"));

        let exec = server(Mode::Execute, false).tool_names();
        assert!(exec.contains(&"komodo_stack_action".to_string()));
        assert!(!exec.contains(&"komodo_write".to_string()));

        let write = server(Mode::Write, false).tool_names();
        assert!(write.contains(&"komodo_write".to_string()));
    }

    #[test]
    fn guard_blocks_destructive_and_admin() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let s = server(Mode::Write, false);
        assert!(s.guard("DestroyStack").is_some());
        assert!(s.guard("PruneImages").is_some());
        assert!(s.guard("DeleteStack").is_some());
        assert!(s.guard("CreateApiKey").is_some());
        assert!(s.guard("UpdateUserAdmin").is_some());
        assert!(s.guard("DeployStack").is_none());
        assert!(s.guard("UpdateStack").is_none());

        let s = server(Mode::Write, true);
        assert!(s.guard("DestroyStack").is_none());
        assert!(s.guard("CreateApiKeyForServiceUser").is_some(), "admin requests stay blocked");
    }
}
