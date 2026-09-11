use anyhow::{Context, Result, anyhow};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::{Args, Parser, Subcommand};
use directories::BaseDirs;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::net::TcpListener;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(name = "tar", about = "A small OpenAI-compatible proxy for Termux")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Serve(ServeArgs),
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Run {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
    Chat(ChatArgs),
}

#[derive(Args, Debug)]
struct ServeArgs {
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    port: Option<u16>,
}

#[derive(Args, Debug)]
struct ChatArgs {
    #[arg(long, default_value = "default")]
    session: String,
    #[arg(long)]
    new: bool,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    tools: bool,
    #[arg(long, default_value_t = 8)]
    max_iterations: usize,
    #[arg(required = true, trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    Init,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Config {
    #[serde(default)]
    providers: Vec<Provider>,
    default_model: String,
    #[serde(default)]
    fallback_models: Vec<String>,
    #[serde(default = "default_host")]
    listen_host: String,
    #[serde(default = "default_port")]
    listen_port: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Provider {
    id: String,
    base_url: String,
    api_key_env: String,
    model: String,
}

fn default_host() -> String {
    "127.0.0.1".into()
}

fn default_port() -> u16 {
    8787
}

impl Config {
    fn candidates<'a>(&'a self, requested: Option<&'a str>) -> Vec<&'a str> {
        let mut models = Vec::new();
        let mut seen = HashSet::new();
        let first = requested
            .filter(|model| !model.is_empty())
            .or_else(|| (!self.default_model.is_empty()).then_some(self.default_model.as_str()));
        for model in first
            .into_iter()
            .chain(self.fallback_models.iter().map(String::as_str))
        {
            if seen.insert(model) {
                models.push(model);
            }
        }
        models
    }

    fn provider_for(&self, model: &str) -> Option<&Provider> {
        self.providers
            .iter()
            .find(|provider| provider.model == model)
    }
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    client: Client,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    match Cli::parse().command {
        Commands::Serve(args) => serve(args.host, args.port).await,
        Commands::Config {
            command: ConfigCommand::Init,
        } => init_config(),
        Commands::Run {
            host,
            port,
            command,
        } => run_client(host, port, command),
        Commands::Chat(args) => chat_client(args).await,
    }
}

fn config_path() -> Result<PathBuf> {
    let dirs = BaseDirs::new().ok_or_else(|| anyhow!("cannot determine home directory"))?;
    Ok(dirs
        .config_dir()
        .join("termux-agent-router")
        .join("config.toml"))
}

fn load_config() -> Result<Config> {
    let path = config_path()?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read config {}", path.display()))?;
    let config: Config = toml::from_str(&text).context("parse config TOML")?;
    validate_config(&config)?;
    Ok(config)
}

fn validate_config(config: &Config) -> Result<()> {
    if config.providers.is_empty() {
        return Err(anyhow!("config must contain at least one provider"));
    }
    if config.default_model.trim().is_empty() {
        return Err(anyhow!("default_model must not be empty"));
    }
    let mut ids = HashSet::new();
    let mut models = HashSet::new();
    for provider in &config.providers {
        if provider.id.trim().is_empty()
            || provider.model.trim().is_empty()
            || provider.api_key_env.trim().is_empty()
        {
            return Err(anyhow!(
                "provider id, model, and api_key_env must not be empty"
            ));
        }
        if !ids.insert(&provider.id) || !models.insert(&provider.model) {
            return Err(anyhow!("provider ids and models must be unique"));
        }
        let scheme = reqwest::Url::parse(&provider.base_url)
            .with_context(|| format!("invalid provider URL for {}", provider.id))?
            .scheme()
            .to_owned();
        if scheme != "http" && scheme != "https" {
            return Err(anyhow!("provider URL must use http or https"));
        }
    }
    for model in std::iter::once(&config.default_model).chain(config.fallback_models.iter()) {
        if !models.contains(model) {
            return Err(anyhow!("configured model has no provider: {model}"));
        }
    }
    Ok(())
}

fn init_config() -> Result<()> {
    let path = config_path()?;
    if path.exists() {
        println!("Config already exists: {}", path.display());
        return Ok(());
    }
    let example = Config {
        providers: vec![
            Provider {
                id: "primary".into(),
                base_url: "https://api.example.com".into(),
                api_key_env: "EXAMPLE_API_KEY".into(),
                model: "example-model".into(),
            },
            Provider {
                id: "fallback".into(),
                base_url: "https://backup.example.com".into(),
                api_key_env: "BACKUP_API_KEY".into(),
                model: "backup-model".into(),
            },
        ],
        default_model: "example-model".into(),
        fallback_models: vec!["backup-model".into()],
        listen_host: default_host(),
        listen_port: default_port(),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, toml::to_string_pretty(&example)?)?;
    println!("Wrote example config: {}", path.display());
    Ok(())
}

async fn serve(host: Option<String>, port: Option<u16>) -> Result<()> {
    let mut config = load_config()?;
    if let Some(host) = host {
        config.listen_host = host;
    }
    if let Some(port) = port {
        config.listen_port = port;
    }
    let address = format!("{}:{}", config.listen_host, config.listen_port);
    let state = AppState {
        config: Arc::new(config),
        client: Client::new(),
    };
    let app = router(state);
    let listener = TcpListener::bind(&address).await?;
    info!("proxy listening on http://{}", address);
    axum::serve(listener, app).await?;
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/tools", post(run_tool))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

#[derive(Debug, Deserialize)]
struct ToolRequest {
    name: String,
    path: Option<String>,
}

async fn run_tool(Json(request): Json<ToolRequest>) -> Response {
    match run_allowlisted_tool(&request.name, request.path.as_deref()) {
        Ok(output) => (StatusCode::OK, Json(json!({"result": output}))).into_response(),
        Err(error) => error_response(StatusCode::BAD_REQUEST, &error.to_string()),
    }
}

fn run_allowlisted_tool(name: &str, requested_path: Option<&str>) -> Result<String> {
    let root = env::current_dir()?;
    match name {
        "pwd" => Ok(root.display().to_string()),
        "list" => {
            let path = safe_path(&root, requested_path.unwrap_or("."))?;
            let mut entries = fs::read_dir(path)?
                .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
                .collect::<std::io::Result<Vec<_>>>()?;
            entries.sort();
            Ok(entries.join("\n"))
        }
        "read" => {
            let path = safe_path(
                &root,
                requested_path.ok_or_else(|| anyhow!("read requires path"))?,
            )?;
            Ok(fs::read_to_string(path)?)
        }
        "git_diff" => {
            let output = Command::new("git")
                .args(["diff", "--no-ext-diff", "--"])
                .current_dir(root)
                .output()
                .context("run git diff")?;
            if !output.status.success() {
                return Err(anyhow!("git diff failed"));
            }
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        _ => Err(anyhow!("tool not allowed: {name}")),
    }
}

fn safe_path(root: &Path, requested: &str) -> Result<PathBuf> {
    let path = root.join(requested);
    let canonical = path.canonicalize().context("path does not exist")?;
    if !canonical.starts_with(root.canonicalize()?) {
        return Err(anyhow!("path escapes router workspace"));
    }
    Ok(canonical)
}

async fn chat_completions(
    State(state): State<AppState>,
    Json(mut request): Json<Value>,
) -> Response {
    let requested = request
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let candidates = state.config.candidates(requested.as_deref());
    if candidates.is_empty() {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "no models configured");
    }

    let mut last_upstream: Option<(StatusCode, Vec<u8>, Option<HeaderValue>)> = None;
    for model in candidates {
        let Some(provider) = state.config.provider_for(model) else {
            warn!(model, "no provider configured for model");
            continue;
        };
        request["model"] = Value::String(model.to_owned());
        let url = format!(
            "{}/v1/chat/completions",
            provider.base_url.trim_end_matches('/')
        );
        let mut builder = state.client.post(url).json(&request);
        if let Ok(key) = env::var(&provider.api_key_env) {
            builder = builder.bearer_auth(key);
        } else {
            warn!(
                provider = provider.id,
                env = provider.api_key_env,
                "API key not set"
            );
        }

        match builder.send().await {
            Ok(response) => {
                let status = response.status();
                let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
                let body = match response.bytes().await {
                    Ok(body) => body.to_vec(),
                    Err(error) => {
                        warn!(%error, provider = provider.id, "failed reading upstream response");
                        continue;
                    }
                };
                if status.is_success() {
                    return upstream_response(status, body, content_type);
                }
                let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                last_upstream = Some((status, body, content_type));
                if !retryable {
                    break;
                }
            }
            Err(error) => warn!(%error, provider = provider.id, "upstream request failed"),
        }
    }
    if let Some((status, body, content_type)) = last_upstream {
        return upstream_response(status, body, content_type);
    }
    error_response(StatusCode::BAD_GATEWAY, "all providers failed")
}

fn upstream_response(
    status: StatusCode,
    body: Vec<u8>,
    content_type: Option<HeaderValue>,
) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    if let Some(content_type) = content_type {
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
    }
    response
}

fn error_response(status: StatusCode, message: &str) -> Response {
    let mut response = Response::new(Body::from(json!({ "error": message }).to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn run_client(host: Option<String>, port: Option<u16>, command: Vec<String>) -> Result<()> {
    let config = load_config()?;
    let host = host.unwrap_or(config.listen_host);
    let port = port.unwrap_or(config.listen_port);
    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("a command is required after `run --`"))?;
    let base_url = format!("http://{}:{}/v1", host, port);
    let status = Command::new(program)
        .args(args)
        .env("OPENAI_BASE_URL", &base_url)
        .env("OPENAI_API_BASE", &base_url)
        .env("TERMUX_AGENT_ROUTER_BASE_URL", &base_url)
        .env("OPENAI_API_KEY", "termux-agent-router")
        .status()
        .with_context(|| format!("launch {program}"))?;
    std::process::exit(status.code().unwrap_or(1));
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Session {
    messages: Vec<ChatMessage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

fn sessions_dir() -> Result<PathBuf> {
    let dirs = BaseDirs::new().ok_or_else(|| anyhow!("cannot determine data directory"))?;
    Ok(dirs.data_dir().join("termux-agent-router").join("sessions"))
}

fn session_path(id: &str) -> Result<PathBuf> {
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
    {
        return Err(anyhow!("invalid session name"));
    }
    Ok(sessions_dir()?.join(format!("{id}.json")))
}

async fn chat_client(args: ChatArgs) -> Result<()> {
    let config = load_config()?;
    let session_id = if args.new {
        format!(
            "session-{}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        )
    } else {
        args.session
    };
    let path = session_path(&session_id)?;
    let mut session: Session = if args.new || !path.exists() {
        Session::default()
    } else {
        serde_json::from_str(&fs::read_to_string(&path)?)?
    };
    let prompt = args.prompt.join(" ");
    if prompt.trim().is_empty() {
        return Err(anyhow!("prompt must not be empty"));
    }
    session.messages.push(ChatMessage {
        role: "user".into(),
        content: prompt,
    });
    let model = args.model.or_else(|| Some(config.default_model.clone()));
    let mut messages: Vec<Value> = session
        .messages
        .iter()
        .map(|message| json!({"role": message.role, "content": message.content}))
        .collect();
    let tools = args.tools.then(safe_tool_definitions);
    let mut answer = None;
    let max_iterations = args.max_iterations.clamp(1, 32);
    let client = Client::new();
    let url = format!(
        "http://{}:{}/v1/chat/completions",
        config.listen_host, config.listen_port
    );
    for _ in 0..max_iterations {
        let mut request = json!({"model": model, "messages": messages});
        if let Some(tool_definitions) = &tools {
            request["tools"] = tool_definitions.clone();
            request["tool_choice"] = json!("auto");
        }
        let response: Value = client
            .post(&url)
            .json(&request)
            .send()
            .await?
            .error_for_status()
            .context("proxy request failed")?
            .json()
            .await?;
        if let Some(error) = response.get("error") {
            return Err(anyhow!("proxy error: {error}"));
        }
        let message = response["choices"][0]["message"].clone();
        let tool_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned);
        messages.push(message);
        if tool_calls.is_empty() {
            answer = content;
            break;
        }
        for call in tool_calls {
            let id = call["id"].as_str().unwrap_or("tool-call");
            let name = call["function"]["name"].as_str().unwrap_or_default();
            let arguments = call["function"]["arguments"].as_str().unwrap_or("{}");
            let path = serde_json::from_str::<Value>(arguments)
                .ok()
                .and_then(|value| value["path"].as_str().map(str::to_owned));
            let result = run_allowlisted_tool(name, path.as_deref())
                .map_err(|error| error.to_string())
                .unwrap_or_else(|error| format!("tool error: {error}"));
            messages.push(json!({
                "role": "tool",
                "tool_call_id": id,
                "content": result,
            }));
        }
    }
    let answer = answer.ok_or_else(|| anyhow!("agent loop reached max iterations"))?;
    session.messages.push(ChatMessage {
        role: "assistant".into(),
        content: answer.clone(),
    });
    fs::create_dir_all(sessions_dir()?)?;
    fs::write(path, serde_json::to_string_pretty(&session)?)?;
    println!("{answer}");
    Ok(())
}

fn safe_tool_definitions() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "pwd",
                "description": "Return the current workspace path.",
                "parameters": {"type": "object", "properties": {}, "additionalProperties": false}
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list",
                "description": "List entries in a workspace-relative directory.",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read a workspace-relative text file.",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "git_diff",
                "description": "Return the current git diff without executing a shell.",
                "parameters": {"type": "object", "properties": {}, "additionalProperties": false}
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_config() -> Config {
        Config {
            providers: vec![
                Provider {
                    id: "one".into(),
                    base_url: "https://one".into(),
                    api_key_env: "ONE_KEY".into(),
                    model: "one-model".into(),
                },
                Provider {
                    id: "two".into(),
                    base_url: "https://two".into(),
                    api_key_env: "TWO_KEY".into(),
                    model: "two-model".into(),
                },
            ],
            default_model: "one-model".into(),
            fallback_models: vec!["two-model".into()],
            listen_host: default_host(),
            listen_port: default_port(),
        }
    }

    #[test]
    fn selects_requested_then_default_and_fallback() {
        let config = test_config();
        assert_eq!(config.candidates(None), vec!["one-model", "two-model"]);
        assert_eq!(config.candidates(Some("two-model")), vec!["two-model"]);
        assert_eq!(config.provider_for("two-model").unwrap().id, "two");
    }

    #[tokio::test]
    async fn health_route_is_available() {
        let app = router(AppState {
            config: Arc::new(test_config()),
            client: Client::new(),
        });
        let response = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), br#"{"status":"ok"}"#);
    }

    #[test]
    fn session_names_cannot_traverse_directories() {
        assert!(session_path("../escape").is_err());
        assert!(session_path("work/session").is_err());
        assert!(session_path("safe_session-1").is_ok());
    }

    #[test]
    fn tools_reject_paths_outside_workspace_and_unknown_commands() {
        assert!(run_allowlisted_tool("unknown", None).is_err());
        assert!(run_allowlisted_tool("read", Some("../Cargo.toml")).is_err());
    }

    #[test]
    fn tool_definitions_are_allowlisted() {
        let tools = safe_tool_definitions();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect();
        assert_eq!(names, vec!["pwd", "list", "read", "git_diff"]);
    }

    #[test]
    fn config_validation_rejects_missing_model_provider() {
        let mut config = test_config();
        config.fallback_models = vec!["missing".into()];
        assert!(validate_config(&config).is_err());
    }
}
