use anyhow::{Context, Result, anyhow};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::Response,
    routing::{get, post},
};
use clap::{Args, Parser, Subcommand};
use directories::BaseDirs;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{env, path::PathBuf, process::Command, sync::Arc};
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
}

#[derive(Args, Debug)]
struct ServeArgs {
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    port: Option<u16>,
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
        if let Some(model) = requested.filter(|model| !model.is_empty()) {
            models.push(model);
        } else if !self.default_model.is_empty() {
            models.push(self.default_model.as_str());
        }
        models.extend(self.fallback_models.iter().map(String::as_str));
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
    if config.providers.is_empty() {
        return Err(anyhow!("config must contain at least one provider"));
    }
    Ok(config)
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
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
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
        assert_eq!(
            config.candidates(Some("two-model")),
            vec!["two-model", "two-model"]
        );
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
}
