#![allow(clippy::too_many_arguments)]

use std::{
    fmt, fs,
    io::{self, BufRead, Write},
    path::PathBuf,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand};
use keyz::client::KeyzClient;
use keyz::config::{
    self, BackendType, Config, ConfigContext, ConfigSource, ProtocolConfig, ServerConfig,
};
use keyz::protocol::{self, Request, ResponseBody};
use keyz::server::backend::{encryption, format};
use rustyline::{error::ReadlineError, DefaultEditor};
use serde_json::json;

const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 3;
const DEFAULT_RESPONSE_TIMEOUT_SECS: u64 = 5;
const DEFAULT_STATUS_INTERVAL_SECS: u64 = 2;
const DEFAULT_CONFIG_TEMPLATE: &str = r#"[server]
host = "127.0.0.1"
port = 7667
max_connections = 1024
work_queue_capacity = 1024
write_timeout_secs = 30
shutdown_timeout_secs = 30

[protocol]
max_message_bytes = 4_194_304
idle_timeout_secs = 30
close_command = "CLOSE"
timeout_response = "error:timeout"
invalid_command_response = "error:invalid command"

[store]
# backend = "memory"
compression_threshold = 512
cleanup_interval_ms = 250
max_bytes = 0 # Unlimited; counts stored keys and compressed payloads.
# default_ttl_secs = 60

# [store.disk]
# data_dir = "./keyz_data"
# partition_count = 16
# encryption_enabled = false
# encryption_key = "64-hex-char-key-here"
# cache_max_entries = 10000
# flush_interval_ms = 5000
# aof_enabled = true
# aof_fsync = "every_second"

# Prefer KEYZ_AUTH_TOKEN and KEYZ_REPLICATION_TOKEN environment variables.
# [security]
# auth_token = "replace-with-a-secret"
# replication_token = "replace-with-a-separate-secret"
# trusted_cluster_addresses = ["127.0.0.1:7669"] # Explicit IP:port trust for discovery probes
# [security.tls]
# enabled = true
# cert_path = "server.crt"
# key_path = "server.key"
# ca_path = "ca.crt"
# server_name = "localhost"
"#;

#[derive(Parser)]
#[command(
    name = "keyz-cli",
    version,
    about = "Command-line client for the Keyz TCP store"
)]
struct Cli {
    #[arg(
        long = "config",
        value_name = "PATH",
        help = "Path to a configuration file (overrides KEYZ_CONFIG/env/default lookup)"
    )]
    config_path: Option<PathBuf>,
    #[arg(
        long = "host",
        value_name = "HOST",
        global = true,
        help = "Override the host declared in the configuration"
    )]
    host_override: Option<String>,
    #[arg(
        long = "port",
        value_name = "PORT",
        global = true,
        help = "Override the port declared in the configuration"
    )]
    port_override: Option<u16>,
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = DEFAULT_CONNECT_TIMEOUT_SECS,
        help = "Connection timeout in seconds"
    )]
    connect_timeout: u64,
    #[arg(
        long = "response-timeout",
        value_name = "SECS",
        default_value_t = DEFAULT_RESPONSE_TIMEOUT_SECS,
        help = "Total request I/O deadline in seconds (includes authentication and retry)"
    )]
    response_timeout: u64,
    #[arg(
        global = true,
        long,
        help = "Emit JSON where available for easier scripting"
    )]
    json: bool,
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    Exec(ExecArgs),
    Commands(CommandsArgs),
    #[command(subcommand)]
    Config(ConfigCommand),
    Status(StatusArgs),
    Interactive(InteractiveArgs),
    Batch(BatchArgs),
    Metrics(MetricsArgs),
    /// Analyze a .keyz data file (header, partitions, key distribution)
    Analyze(AnalyzeArgs),
    /// Monitor cluster instances (leader + followers)
    #[command(subcommand)]
    Cluster(ClusterCommand),
}

#[derive(Args)]
struct ExecArgs {
    #[arg(
        long,
        requires = "raw",
        help = "Interpret --raw as a version 1 JSON request envelope and emit a typed JSON response"
    )]
    typed: bool,
    #[arg(
        long,
        value_name = "STRING",
        conflicts_with = "parts",
        help = "Send the command exactly as provided without additional parsing"
    )]
    raw: Option<String>,
    #[arg(
        trailing_var_arg = true,
        value_name = "PART",
        help = "Command broken into whitespace-separated parts; joined internally"
    )]
    parts: Vec<String>,
}

#[derive(Args)]
struct CommandsArgs {
    #[arg(long, help = "Filter by command prefix (e.g. GET)")]
    filter: Option<String>,
    #[arg(long, help = "Show detailed notes for each command")]
    verbose: bool,
}

#[derive(Subcommand)]
enum ConfigCommand {
    Show,
    Init(ConfigInitArgs),
}

#[derive(Args)]
struct ConfigInitArgs {
    #[arg(long, value_name = "PATH", default_value = "keyz.toml")]
    output: PathBuf,
    #[arg(long, help = "Overwrite existing file if present")]
    force: bool,
}

#[derive(Args)]
struct StatusArgs {
    #[arg(long, help = "Continuously watch server health")]
    watch: bool,
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = DEFAULT_STATUS_INTERVAL_SECS,
        help = "Polling interval when --watch is enabled"
    )]
    interval: u64,
}

#[derive(Args)]
struct InteractiveArgs {
    #[arg(
        long,
        value_name = "PATH",
        help = "Persist REPL history to this file (default: in-memory only)"
    )]
    history: Option<PathBuf>,
}

#[derive(Args)]
struct BatchArgs {
    #[arg(
        long,
        value_name = "PATH",
        help = "Read commands from file instead of STDIN"
    )]
    file: Option<PathBuf>,
    #[arg(long, help = "Abort at the first command that returns an error")]
    stop_on_error: bool,
}

#[derive(Args)]
struct MetricsArgs {
    #[arg(long, help = "Display raw response without formatting")]
    raw: bool,
}

#[derive(Args)]
struct AnalyzeArgs {
    /// Path to the .keyz data file to analyze
    #[arg(value_name = "PATH")]
    path: PathBuf,

    /// Hex-encoded 256-bit encryption key (64 hex chars) to decrypt encrypted files
    #[arg(long = "encryption-key", value_name = "HEX")]
    encryption_key: Option<String>,

    /// List all keys in each partition
    #[arg(long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum ClusterCommand {
    /// Show cluster instances (one-shot)
    Status,
    /// Live monitoring dashboard
    Monitor(ClusterMonitorArgs),
}

#[derive(Args)]
struct ClusterMonitorArgs {
    /// Refresh interval in seconds
    #[arg(long, default_value_t = 2)]
    interval: u64,
}

fn command_response_failed(command: &str, response: &str, protocol: &ProtocolConfig) -> bool {
    if protocol::is_typed(command) {
        return protocol::decode_response(response).map_or(true, |response| {
            matches!(response.result, ResponseBody::Error { .. })
        });
    }
    if command == protocol.close_command {
        return response != "Closing connection";
    }

    let mut parts = command.trim().splitn(2, ' ');
    let name = parts.next().unwrap_or_default();
    let remainder = parts.next().map(str::trim);
    let key = remainder.filter(|key| !key.is_empty() && key.split_whitespace().nth(1).is_none());

    // Frames carry no status bit: an arbitrary GET value can be identical to
    // an error message, and DEL returns the key itself. Check each command's
    // response contract instead of treating every "error:" prefix as a failure.
    match name {
        "GET" if key.is_some() => false,
        "DEL" if key.is_some() => response != "null" && Some(response) != key,
        "EXIN" if key.is_some() => response != "null" && response.parse::<u64>().is_err(),
        "SET" => response != "ok",
        "INFO" | "HEALTH" if remainder.is_none_or(str::is_empty) => {
            serde_json::from_str::<serde_json::Value>(response).is_err()
        }
        _ => true,
    }
}

#[derive(Clone, Copy)]
struct CommandDoc {
    name: &'static str,
    syntax: &'static str,
    description: &'static str,
    notes: &'static str,
}

const COMMANDS: &[CommandDoc] = &[
    CommandDoc {
        name: "SET",
        syntax: "SET <key> <value> [EX <seconds>]",
        description: "Insert or update a value with optional TTL.",
        notes: "Values may contain spaces. TTL applies as seconds; omit EX for default TTL if configured.",
    },
    CommandDoc {
        name: "GET",
        syntax: "GET <key>",
        description: "Fetch the latest value stored for the key.",
        notes: "Returns the value or the literal string `null` when absent or expired.",
    },
    CommandDoc {
        name: "DEL",
        syntax: "DEL <key>",
        description: "Delete a key if present.",
        notes: "Responds with the deleted key or `null` if nothing was removed.",
    },
    CommandDoc {
        name: "EXIN",
        syntax: "EXIN <key>",
        description: "Inspect remaining TTL for a key.",
        notes: "Returns seconds remaining or `null` when the key has no expiry or is missing.",
    },
    CommandDoc {
        name: "CLOSE",
        syntax: "CLOSE",
        description: "Gracefully terminate the connection.",
        notes: "Response is configurable via protocol.close_command / timeout responses.",
    },
    CommandDoc {
        name: "INFO",
        syntax: "INFO",
        description: "Return server metrics and configuration summary as JSON.",
        notes: "Useful for health dashboards and scripting; fields evolve but remain backward compatible.",
    },
    CommandDoc {
        name: "HEALTH",
        syntax: "HEALTH",
        description: "Return process liveness and readiness as JSON.",
        notes: "Readiness reflects storage failures, shutdown and follower synchronization.",
    },
];

#[derive(Clone)]
struct ResolvedAddress {
    host: String,
    port: u16,
}

impl fmt::Display for ResolvedAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') && !self.host.starts_with('[') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let ConfigContext { config, source } = Config::load_with_source(cli.config_path.as_ref())?;

    let server_cfg = resolve_server_config(&config.server, &cli);
    let endpoint = ResolvedAddress {
        host: server_cfg.host.clone(),
        port: server_cfg.port,
    };

    let protocol_cfg = config.protocol.clone();
    let client = KeyzClient::new(
        endpoint.to_string(),
        Duration::from_secs(cli.connect_timeout),
        Duration::from_secs(cli.response_timeout),
        &protocol_cfg,
    )
    .with_security(config.security.clone());

    match cli.command {
        CliCommand::Exec(args) => handle_exec(&client, &protocol_cfg, args),
        CliCommand::Commands(args) => handle_commands(args, cli.json, &protocol_cfg),
        CliCommand::Config(ConfigCommand::Show) => {
            handle_config_show(&config, &source, cli.json, &endpoint)
        }
        CliCommand::Config(ConfigCommand::Init(args)) => handle_config_init(args),
        CliCommand::Status(args) => handle_status(&client, args, cli.json),
        CliCommand::Interactive(args) => {
            handle_interactive(&client, &protocol_cfg, args, &endpoint, &config, &source)
        }
        CliCommand::Batch(args) => handle_batch(&client, &protocol_cfg, args, cli.json),
        CliCommand::Metrics(args) => handle_metrics(&client, args, cli.json),
        CliCommand::Analyze(args) => handle_analyze(args, cli.json),
        CliCommand::Cluster(ClusterCommand::Status) => {
            handle_cluster_status(&client, &endpoint, cli.json)
        }
        CliCommand::Cluster(ClusterCommand::Monitor(args)) => {
            handle_cluster_monitor(&client, &endpoint, args, cli.json)
        }
    }
}

fn resolve_server_config(server: &ServerConfig, cli: &Cli) -> ServerConfig {
    let mut resolved = server.clone();
    if let Some(host) = &cli.host_override {
        resolved.host = host.clone();
    }
    if let Some(port) = cli.port_override {
        resolved.port = port;
    }
    resolved
}

fn handle_exec(client: &KeyzClient, protocol: &ProtocolConfig, args: ExecArgs) -> Result<()> {
    let typed = args.typed;
    let command = args
        .raw
        .or_else(|| {
            if args.parts.is_empty() {
                None
            } else {
                Some(args.parts.join(" "))
            }
        })
        .ok_or_else(|| anyhow!("provide either --raw or command parts"))?;

    let start = Instant::now();
    let response = if typed {
        let request: Request = if protocol::is_typed(&command) {
            protocol::decode_request(&command)?
        } else {
            serde_json::from_str(&command)
                .context("--typed --raw expects JSON with id and command fields")?
        };
        let response = client.send_request(&request)?;
        println!("{}", serde_json::to_string_pretty(&response)?);
        if let ResponseBody::Error { code, message } = response.result {
            return Err(anyhow!("{code}: {message}"));
        }
        None
    } else {
        let response = client.send(&command)?;
        if command_response_failed(&command, &response, protocol) {
            return Err(anyhow!("server rejected command: {response}"));
        }
        Some(response)
    };
    let elapsed = start.elapsed();

    if let Some(response) = response {
        println!("{}", response);
    }
    eprintln!(
        "Executed in {:.2?}; max response size {} bytes",
        elapsed, protocol.max_message_bytes
    );
    Ok(())
}

fn handle_commands(args: CommandsArgs, json: bool, protocol: &ProtocolConfig) -> Result<()> {
    let entries: Vec<_> = COMMANDS
        .iter()
        .filter(|cmd| {
            if let Some(ref filter) = args.filter {
                cmd.name.starts_with(&filter.to_uppercase())
            } else {
                true
            }
        })
        .collect();

    if json {
        let payload = json!({
            "commands": entries.iter().map(|cmd| {
                json!({
                    "name": cmd.name,
                    "syntax": cmd.syntax,
                    "description": cmd.description,
                    "notes": if args.verbose { cmd.notes } else { "" },
                })
            }).collect::<Vec<_>>(),
            "close_command": protocol.close_command,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!(
        "Supported commands (close command: {})",
        protocol.close_command
    );
    for cmd in entries {
        println!("  {:<6} {}", cmd.name, cmd.description);
        println!("     syntax: {}", cmd.syntax);
        if args.verbose {
            println!("     notes : {}", cmd.notes);
        }
    }
    Ok(())
}

fn handle_config_show(
    config: &Config,
    source: &ConfigSource,
    json: bool,
    endpoint: &ResolvedAddress,
) -> Result<()> {
    let source_desc = match source {
        ConfigSource::ExplicitPath(path) => format!("explicit file ({})", path.to_string_lossy()),
        ConfigSource::Env(path) => {
            format!("environment via KEYZ_CONFIG ({})", path.to_string_lossy())
        }
        ConfigSource::DefaultFile(path) => format!("default file ({})", path.to_string_lossy()),
        ConfigSource::Defaults => "built-in defaults".to_string(),
    };

    let backend_str = match config.store.backend {
        BackendType::Memory => "memory",
        BackendType::Disk => "disk",
    };

    if json {
        let mut store_json = json!({
            "backend": backend_str,
            "compression_threshold": config.store.compression_threshold,
            "cleanup_interval_ms": config.store.cleanup_interval_ms,
            "default_ttl_secs": config.store.default_ttl_secs,
            "max_bytes": config.store.max_bytes,
        });
        if config.store.backend == BackendType::Disk {
            store_json["disk"] = json!({
                "data_dir": config.store.disk.data_dir,
                "partition_count": config.store.disk.partition_count,
                "encryption_enabled": config.store.disk.encryption_enabled,
                "cache_max_entries": config.store.disk.cache_max_entries,
                "flush_interval_ms": config.store.disk.flush_interval_ms,
            });
        }
        let payload = json!({
            "source": source_desc,
            "endpoint": endpoint.to_string(),
            "using_file": !matches!(source, ConfigSource::Defaults),
            "server": {
                "host": config.server.host,
                "port": config.server.port,
                "max_connections": config.server.max_connections,
                "work_queue_capacity": config.server.work_queue_capacity,
                "write_timeout_secs": config.server.write_timeout_secs,
                "shutdown_timeout_secs": config.server.shutdown_timeout_secs,
            },
            "security": {
                "authentication_configured": config.security.resolve_auth_token().is_some(),
                "replication_authentication_configured": config.security.resolve_replication_token().is_some(),
                "tls_enabled": config.security.tls.enabled,
                "trusted_cluster_addresses": config.security.trusted_cluster_addresses,
            },
            "store": store_json,
            "protocol": {
                "max_message_bytes": config.protocol.max_message_bytes,
                "idle_timeout_secs": config.protocol.idle_timeout_secs,
                "close_command": config.protocol.close_command,
                "timeout_response": config.protocol.timeout_response,
                "invalid_command_response": config.protocol.invalid_command_response,
            },
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("  Configuration");
    println!("  {}", "-".repeat(50));
    println!("  Source             : {}", source_desc);
    println!("  Server endpoint    : {}", endpoint);
    println!(
        "  Using config file  : {}",
        if matches!(source, ConfigSource::Defaults) {
            "no"
        } else {
            "yes"
        }
    );
    println!();
    println!("  Server");
    println!("  {}", "-".repeat(50));
    println!("  host                    = {}", config.server.host);
    println!("  port                    = {}", config.server.port);
    println!();
    println!("  Protocol");
    println!("  {}", "-".repeat(50));
    println!(
        "  max_message_bytes       = {}",
        config.protocol.max_message_bytes
    );
    println!(
        "  idle_timeout_secs       = {}",
        config.protocol.idle_timeout_secs
    );
    println!(
        "  close_command           = {}",
        config.protocol.close_command
    );
    println!(
        "  timeout_response        = {}",
        config.protocol.timeout_response
    );
    println!(
        "  invalid_command_response = {}",
        config.protocol.invalid_command_response
    );
    println!();
    println!("  Store");
    println!("  {}", "-".repeat(50));
    println!("  backend                 = {}", backend_str);
    println!(
        "  compression_threshold   = {}",
        config.store.compression_threshold
    );
    println!(
        "  cleanup_interval_ms     = {}",
        config.store.cleanup_interval_ms
    );
    match config.store.default_ttl_secs {
        Some(ttl) => println!("  default_ttl_secs        = {}", ttl),
        None => println!("  default_ttl_secs        = (disabled)"),
    }
    if config.store.backend == BackendType::Disk {
        println!();
        println!("  Disk Backend");
        println!("  {}", "-".repeat(50));
        println!("  data_dir                = {}", config.store.disk.data_dir);
        println!(
            "  partition_count         = {}",
            config.store.disk.partition_count
        );
        println!(
            "  encryption_enabled      = {}",
            config.store.disk.encryption_enabled
        );
        println!(
            "  cache_max_entries       = {}",
            config.store.disk.cache_max_entries
        );
        println!(
            "  flush_interval_ms       = {}",
            config.store.disk.flush_interval_ms
        );
    }
    Ok(())
}

fn handle_config_init(args: ConfigInitArgs) -> Result<()> {
    if args.output.exists() && !args.force {
        return Err(anyhow!(
            "{} already exists; pass --force to overwrite",
            args.output.display()
        ));
    }

    fs::write(&args.output, DEFAULT_CONFIG_TEMPLATE)?;
    println!("Wrote template configuration to {}", args.output.display());
    Ok(())
}

fn handle_status(client: &KeyzClient, args: StatusArgs, json: bool) -> Result<()> {
    if args.watch {
        loop {
            let snapshot = probe_status(client);
            output_status(&snapshot, json);
            thread::sleep(Duration::from_secs(args.interval));
        }
    } else {
        let snapshot = probe_status(client);
        output_status(&snapshot, json);
        if !snapshot.ready {
            return Err(anyhow!("server is not ready"));
        }
    }
    Ok(())
}

#[derive(Debug)]
struct StatusSnapshot {
    reachable: bool,
    ready: bool,
    latency: Option<Duration>,
    response: Option<String>,
    error: Option<String>,
}

fn probe_status(client: &KeyzClient) -> StatusSnapshot {
    let start = Instant::now();
    match client.send("HEALTH") {
        Ok(response) => {
            let health = serde_json::from_str::<serde_json::Value>(&response).ok();
            let ready = health
                .as_ref()
                .and_then(|v| v["ready"].as_bool())
                .unwrap_or(false);
            StatusSnapshot {
                reachable: true,
                ready,
                latency: Some(start.elapsed()),
                response: Some(response),
                error: if health.is_none() {
                    Some("server returned an invalid HEALTH response".into())
                } else {
                    None
                },
            }
        }
        Err(err) => StatusSnapshot {
            reachable: false,
            ready: false,
            latency: None,
            response: None,
            error: Some(err.to_string()),
        },
    }
}

fn output_status(snapshot: &StatusSnapshot, json: bool) {
    if json {
        let payload = json!({
            "reachable": snapshot.reachable,
            "ready": snapshot.ready,
            "latency_ms": snapshot.latency.map(|d| d.as_secs_f64() * 1000.0),
            "response": snapshot.response,
            "error": snapshot.error,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return;
    }

    if snapshot.reachable {
        println!(
            "Server reachable in {:.2} ms; response: {}",
            snapshot.latency.unwrap_or_default().as_secs_f64() * 1000.0,
            snapshot.response.as_deref().unwrap_or("n/a")
        );
    } else {
        println!(
            "Server unreachable: {}",
            snapshot.error.as_deref().unwrap_or("unknown error")
        );
    }
}

fn handle_interactive(
    client: &KeyzClient,
    protocol: &ProtocolConfig,
    args: InteractiveArgs,
    endpoint: &ResolvedAddress,
    config: &Config,
    source: &ConfigSource,
) -> Result<()> {
    let mut editor = DefaultEditor::new()?;
    if let Some(path) = &args.history {
        if path.exists() {
            let _ = editor.load_history(path);
        }
    }

    println!();
    println!("  keyz interactive shell");
    println!("  {}", "-".repeat(40));
    println!("  Endpoint  : {}", endpoint);
    println!("  Max frame : {} bytes", protocol.max_message_bytes);
    println!("  Type :help for available commands");
    println!();

    loop {
        match editor.readline("keyz> ") {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(trimmed);

                match trimmed {
                    ":quit" | ":exit" => break,
                    ":help" => {
                        repl_help();
                        continue;
                    }
                    ":commands" => {
                        handle_commands(
                            CommandsArgs {
                                filter: None,
                                verbose: true,
                            },
                            false,
                            protocol,
                        )?;
                        continue;
                    }
                    ":config" => {
                        if let Err(e) = handle_config_show(config, source, false, endpoint) {
                            println!("error: {e}");
                        }
                        continue;
                    }
                    ":config init" => {
                        if let Err(e) = handle_config_init(ConfigInitArgs {
                            output: PathBuf::from("keyz.toml"),
                            force: false,
                        }) {
                            println!("error: {e}");
                        }
                        continue;
                    }
                    ":config init --force" => {
                        if let Err(e) = handle_config_init(ConfigInitArgs {
                            output: PathBuf::from("keyz.toml"),
                            force: true,
                        }) {
                            println!("error: {e}");
                        }
                        continue;
                    }
                    ":metrics" | ":info" => {
                        if let Err(e) = handle_metrics(client, MetricsArgs { raw: false }, false) {
                            println!("error: {e}");
                        }
                        continue;
                    }
                    ":status" => {
                        let snapshot = probe_status(client);
                        output_status(&snapshot, false);
                        continue;
                    }
                    ":cluster" => {
                        if let Err(e) = handle_cluster_status(client, endpoint, false) {
                            println!("error: {e}");
                        }
                        continue;
                    }
                    ":clear" => {
                        print!("\x1b[2J\x1b[H");
                        let _ = io::stdout().flush();
                        continue;
                    }
                    _ => {}
                }

                match client.send(trimmed) {
                    Ok(response) => println!("{response}"),
                    Err(err) => println!("error: {err}"),
                }
            }
            Err(ReadlineError::Interrupted) => {
                println!("^C");
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(err) => return Err(err.into()),
        }
    }

    if let Some(path) = &args.history {
        let _ = editor.save_history(path);
    }
    println!("bye");
    Ok(())
}

fn repl_help() {
    println!();
    println!("  REPL Commands");
    println!("  {}", "-".repeat(50));
    println!("  {:24} Show this help message", ":help");
    println!("  {:24} List server commands", ":commands");
    println!("  {:24} Show current configuration", ":config");
    println!("  {:24} Create keyz.toml template", ":config init");
    println!(
        "  {:24} Overwrite existing keyz.toml",
        ":config init --force"
    );
    println!("  {:24} Fetch server metrics (INFO)", ":metrics / :info");
    println!("  {:24} Quick server health check", ":status");
    println!("  {:24} Show cluster instances", ":cluster");
    println!("  {:24} Clear the screen", ":clear");
    println!("  {:24} Exit the REPL", ":quit / :exit");
    println!();
    println!("  Any other input is sent as a command to the server.");
    println!();
}

fn handle_batch(
    client: &KeyzClient,
    protocol: &ProtocolConfig,
    args: BatchArgs,
    json: bool,
) -> Result<()> {
    let mut reader: Box<dyn BufRead> = if let Some(path) = args.file {
        Box::new(io::BufReader::new(fs::File::open(&path).with_context(
            || format!("unable to open batch file {}", path.display()),
        )?))
    } else {
        Box::new(io::BufReader::new(io::stdin()))
    };

    run_batch(client, protocol, reader.as_mut(), args.stop_on_error, json)
}

fn run_batch(
    client: &KeyzClient,
    protocol: &ProtocolConfig,
    reader: &mut dyn BufRead,
    stop_on_error: bool,
    json: bool,
) -> Result<()> {
    let mut line = String::new();
    let mut index = 0usize;
    while reader.read_line(&mut line)? > 0 {
        index += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            line.clear();
            continue;
        }

        let result = client.send(trimmed).and_then(|response| {
            if command_response_failed(trimmed, &response, protocol) {
                Err(anyhow!("server rejected command: {response}"))
            } else {
                Ok(response)
            }
        });
        match result {
            Ok(response) => {
                if json {
                    let payload = json!({
                        "line": index,
                        "command": trimmed,
                        "response": response,
                    });
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                } else {
                    println!("[line {index}] {response}");
                }
            }
            Err(err) => {
                if json {
                    let payload = json!({
                        "line": index,
                        "command": trimmed,
                        "error": err.to_string(),
                    });
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                } else {
                    println!("[line {index}] error: {err}");
                }

                if stop_on_error {
                    return Err(anyhow!("aborting due to --stop-on-error"));
                }
            }
        }

        line.clear();
    }

    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn handle_analyze(args: AnalyzeArgs, json_output: bool) -> Result<()> {
    let data =
        fs::read(&args.path).with_context(|| format!("failed to read {}", args.path.display()))?;

    let file_size = data.len() as u64;

    if data.len() < format::HEADER_SIZE {
        return Err(anyhow!(
            "file too small ({} bytes, minimum {} for header)",
            data.len(),
            format::HEADER_SIZE
        ));
    }

    let header = format::FileHeader::deserialize(&data[..format::HEADER_SIZE])
        .map_err(|e| anyhow!("invalid .keyz header: {e}"))?;

    let compression_name = match header.compression_algo {
        format::COMPRESSION_NONE => "none",
        format::COMPRESSION_GZIP => "gzip",
        _ => "unknown",
    };

    let table_entries = format::validate_partition_table(&data, &header)
        .map_err(|e| anyhow!("invalid snapshot partition table: {e}"))?;

    // Determine if we can decrypt
    let encryption_key: Option<[u8; 32]> = if header.encryption_enabled {
        if let Some(ref hex) = args.encryption_key {
            Some(config::parse_hex_key(hex).map_err(|e| anyhow!("{e}"))?)
        } else {
            None
        }
    } else {
        None
    };

    let can_read_entries = !header.encryption_enabled || encryption_key.is_some();

    // Analyze entries if possible
    struct PartitionStats {
        keys: usize,
        compressed: usize,
        expired: usize,
        active: usize,
        total_value_bytes: u64,
        key_names: Vec<(String, usize, bool, String)>, // (key, value_size, compressed, expiry_status)
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut partition_stats: Vec<Option<PartitionStats>> =
        Vec::with_capacity(header.partition_count as usize);

    for (i, te) in table_entries.iter().enumerate() {
        if !can_read_entries || te.length == 0 {
            partition_stats.push(if te.length == 0 && can_read_entries {
                Some(PartitionStats {
                    keys: 0,
                    compressed: 0,
                    expired: 0,
                    active: 0,
                    total_value_bytes: 0,
                    key_names: vec![],
                })
            } else {
                None
            });
            continue;
        }

        let start = te.offset as usize;
        let end = start + te.length as usize;
        if end > data.len() {
            return Err(anyhow!("partition {i} extends past end of file"));
        }

        let partition_data = &data[start..end];
        let plaintext = if let Some(ref key) = encryption_key {
            encryption::decrypt_partition(partition_data, key)
                .map_err(|e| anyhow!("failed to decrypt partition {i}: {e}"))?
        } else {
            partition_data.to_vec()
        };

        let entries = format::deserialize_partition_entries_counted(&plaintext, te.entry_count)
            .map_err(|e| anyhow!("failed to parse partition {i} entries: {e}"))?;

        let mut stats = PartitionStats {
            keys: entries.len(),
            compressed: 0,
            expired: 0,
            active: 0,
            total_value_bytes: 0,
            key_names: Vec::with_capacity(entries.len()),
        };

        for entry in &entries {
            stats.total_value_bytes += entry.value.len() as u64;
            if entry.compressed {
                stats.compressed += 1;
            }
            let expiry_status = if entry.expires_at == 0 {
                "no expiry".to_string()
            } else if now >= entry.expires_at {
                stats.expired += 1;
                "expired".to_string()
            } else {
                stats.active += 1;
                format!("{}s remaining", entry.expires_at - now)
            };
            if entry.expires_at == 0 {
                stats.active += 1;
            }
            stats.key_names.push((
                entry.key.clone(),
                entry.value.len(),
                entry.compressed,
                expiry_status,
            ));
        }

        partition_stats.push(Some(stats));
    }

    // JSON output
    if json_output {
        let partitions_json: Vec<_> = table_entries
            .iter()
            .enumerate()
            .map(|(i, te)| {
                let mut p = json!({
                    "index": i,
                    "offset": te.offset,
                    "size": te.length,
                    "entry_count": te.entry_count,
                });
                if let Some(Some(ref stats)) = partition_stats.get(i) {
                    p["active_keys"] = json!(stats.active);
                    p["expired_keys"] = json!(stats.expired);
                    p["compressed_keys"] = json!(stats.compressed);
                    p["total_value_bytes"] = json!(stats.total_value_bytes);
                    if args.verbose {
                        p["keys"] = json!(stats.key_names.iter().map(|(k, sz, comp, exp)| {
                            json!({"key": k, "value_size": sz, "compressed": comp, "expiry": exp})
                        }).collect::<Vec<_>>());
                    }
                }
                p
            })
            .collect();

        let mut summary = json!({
            "file_size": file_size,
            "partition_count": header.partition_count,
            "encryption_enabled": header.encryption_enabled,
            "compression": compression_name,
        });

        if can_read_entries {
            let total_keys: usize = partition_stats
                .iter()
                .filter_map(|s| s.as_ref())
                .map(|s| s.keys)
                .sum();
            let total_active: usize = partition_stats
                .iter()
                .filter_map(|s| s.as_ref())
                .map(|s| s.active)
                .sum();
            let total_expired: usize = partition_stats
                .iter()
                .filter_map(|s| s.as_ref())
                .map(|s| s.expired)
                .sum();
            let total_compressed: usize = partition_stats
                .iter()
                .filter_map(|s| s.as_ref())
                .map(|s| s.compressed)
                .sum();
            summary["total_keys"] = json!(total_keys);
            summary["active_keys"] = json!(total_active);
            summary["expired_keys"] = json!(total_expired);
            summary["compressed_keys"] = json!(total_compressed);
        }

        let payload = json!({
            "header": {
                "version": format!("{}.{}", format::VERSION_MAJOR, format::VERSION_MINOR),
                "partition_count": header.partition_count,
                "encryption_enabled": header.encryption_enabled,
                "compression": compression_name,
            },
            "partitions": partitions_json,
            "summary": summary,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    // Text output
    println!();
    println!(
        "  File: {} ({})",
        args.path.display(),
        format_bytes(file_size)
    );
    println!();
    println!("  Header");
    println!("  {}", "-".repeat(50));
    println!("  Magic          : KEZ\\0 (valid)");
    println!(
        "  Version        : {}.{}",
        format::VERSION_MAJOR,
        format::VERSION_MINOR
    );
    println!("  Partitions     : {}", header.partition_count);
    println!(
        "  Encryption     : {}",
        if header.encryption_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("  Compression    : {}", compression_name);

    println!();
    println!("  Partitions");
    println!("  {}", "-".repeat(50));
    println!(
        "  {:>4}  {:>10}  {:>10}  {:>8}",
        "#", "Offset", "Size", "Entries"
    );

    for (i, te) in table_entries.iter().enumerate() {
        println!(
            "  {:>4}  {:>10}  {:>10}  {:>8}",
            i,
            te.offset,
            format_bytes(te.length as u64),
            te.entry_count
        );

        if args.verbose {
            if let Some(Some(ref stats)) = partition_stats.get(i) {
                for (key, val_size, compressed, expiry) in &stats.key_names {
                    println!(
                        "         {:30} {:>8}  {}  {}",
                        key,
                        format_bytes(*val_size as u64),
                        if *compressed { "gz" } else { "  " },
                        expiry
                    );
                }
            }
        }
    }

    if !can_read_entries {
        println!();
        println!("  Note: file is encrypted. Provide --encryption-key to see entry details.");
        println!();
        return Ok(());
    }

    // Summary
    let total_keys: usize = partition_stats
        .iter()
        .filter_map(|s| s.as_ref())
        .map(|s| s.keys)
        .sum();
    let total_active: usize = partition_stats
        .iter()
        .filter_map(|s| s.as_ref())
        .map(|s| s.active)
        .sum();
    let total_expired: usize = partition_stats
        .iter()
        .filter_map(|s| s.as_ref())
        .map(|s| s.expired)
        .sum();
    let total_compressed: usize = partition_stats
        .iter()
        .filter_map(|s| s.as_ref())
        .map(|s| s.compressed)
        .sum();

    let entry_counts: Vec<usize> = partition_stats
        .iter()
        .filter_map(|s| s.as_ref())
        .map(|s| s.keys)
        .collect();
    let n = entry_counts.len() as f64;
    let avg = if n > 0.0 { total_keys as f64 / n } else { 0.0 };
    let (min_entries, min_idx) = entry_counts
        .iter()
        .enumerate()
        .min_by_key(|(_, &v)| v)
        .map(|(i, &v)| (v, i))
        .unwrap_or((0, 0));
    let (max_entries, max_idx) = entry_counts
        .iter()
        .enumerate()
        .max_by_key(|(_, &v)| v)
        .map(|(i, &v)| (v, i))
        .unwrap_or((0, 0));
    let variance = if n > 0.0 {
        entry_counts
            .iter()
            .map(|&x| (x as f64 - avg).powi(2))
            .sum::<f64>()
            / n
    } else {
        0.0
    };
    let stddev = variance.sqrt();

    let compressed_pct = if total_keys > 0 {
        format!(
            "{:.1}%",
            total_compressed as f64 / total_keys as f64 * 100.0
        )
    } else {
        "0%".to_string()
    };

    println!();
    println!("  Summary");
    println!("  {}", "-".repeat(50));
    println!("  Total keys           : {}", total_keys);
    println!("  Active keys          : {}", total_active);
    println!("  Expired keys         : {}", total_expired);
    println!(
        "  Compressed entries   : {} ({})",
        total_compressed, compressed_pct
    );
    println!("  File size            : {}", format_bytes(file_size));
    println!("  Avg entries/part     : {:.1}", avg);
    println!(
        "  Min entries          : {} (partition {})",
        min_entries, min_idx
    );
    println!(
        "  Max entries          : {} (partition {})",
        max_entries, max_idx
    );
    println!("  Std deviation        : {:.1}", stddev);
    println!();

    Ok(())
}

struct NodeStatus {
    name: String,
    address: String,
    role: String,
    online: bool,
    probe_skipped: bool,
    latency_ms: Option<f64>,
    keys: Option<u64>,
}

fn discover_cluster(client: &KeyzClient, endpoint: &ResolvedAddress) -> Result<Vec<NodeStatus>> {
    let start = Instant::now();
    let response = client.send("INFO")?;
    let latency = start.elapsed().as_secs_f64() * 1000.0;

    let info: serde_json::Value =
        serde_json::from_str(&response).context("failed to parse leader INFO as JSON")?;

    let role = info["replication"]["role"]
        .as_str()
        .unwrap_or("disabled")
        .to_string();
    let keys = info["store"]["keys"].as_u64();

    let mut nodes = vec![NodeStatus {
        name: "leader".to_string(),
        address: endpoint.to_string(),
        role,
        online: true,
        probe_skipped: false,
        latency_ms: Some(latency),
        keys,
    }];

    if let Some(followers) = info["replication"]["followers"].as_array() {
        for (i, f) in followers.iter().enumerate() {
            if let Some(addr) = f["address"].as_str() {
                let status = probe_node(client, addr);
                nodes.push(NodeStatus {
                    name: format!("follower-{}", i + 1),
                    address: addr.to_string(),
                    role: "follower".to_string(),
                    online: status.0,
                    probe_skipped: status.3,
                    latency_ms: status.1,
                    keys: status.2,
                });
            }
        }
    }

    Ok(nodes)
}

fn probe_node(client: &KeyzClient, address: &str) -> (bool, Option<f64>, Option<u64>, bool) {
    let Ok(probe_client) = client.for_discovered_address(address) else {
        return (false, None, None, true);
    };

    let start = Instant::now();
    match probe_client.send("INFO") {
        Ok(response) => {
            let latency = start.elapsed().as_secs_f64() * 1000.0;
            let keys = serde_json::from_str::<serde_json::Value>(&response)
                .ok()
                .and_then(|v| v["store"]["keys"].as_u64());
            (true, Some(latency), keys, false)
        }
        Err(_) => (false, None, None, false),
    }
}

fn render_cluster_table(nodes: &[NodeStatus]) {
    println!(
        "  {:<14} {:<22} {:<10} {:<9} {:<9} Keys",
        "Name", "Address", "Role", "Status", "Latency"
    );
    for node in nodes {
        let status = if node.probe_skipped {
            "untrusted"
        } else if node.online {
            "online"
        } else {
            "offline"
        };
        let latency = node
            .latency_ms
            .map(|l| format!("{:.1}ms", l))
            .unwrap_or_else(|| "-".to_string());
        let keys = node
            .keys
            .map(|k| k.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {:<14} {:<22} {:<10} {:<9} {:<9} {}",
            node.name, node.address, node.role, status, latency, keys
        );
    }
    if nodes.iter().any(|node| node.probe_skipped) {
        println!(
            "  Untrusted: probe skipped; configure security.trusted_cluster_addresses to allow it."
        );
    }
}

fn handle_cluster_status(
    client: &KeyzClient,
    endpoint: &ResolvedAddress,
    json: bool,
) -> Result<()> {
    let nodes = discover_cluster(client, endpoint)?;

    if json {
        let payload: Vec<serde_json::Value> = nodes
            .iter()
            .map(|n| {
                serde_json::json!({
                    "name": n.name,
                    "address": n.address,
                    "role": n.role,
                    "online": n.online,
                    "probe_skipped": n.probe_skipped,
                    "latency_ms": n.latency_ms,
                    "keys": n.keys,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!();
    println!("  keyz cluster status");
    println!("  {}", "\u{2500}".repeat(75));
    render_cluster_table(&nodes);
    println!("  {}", "\u{2500}".repeat(75));
    println!();

    Ok(())
}

fn handle_cluster_monitor(
    client: &KeyzClient,
    endpoint: &ResolvedAddress,
    args: ClusterMonitorArgs,
    json: bool,
) -> Result<()> {
    loop {
        // Clear screen
        print!("\x1b[2J\x1b[H");
        let _ = io::stdout().flush();

        match discover_cluster(client, endpoint) {
            Ok(nodes) => {
                if json {
                    let payload: Vec<serde_json::Value> = nodes
                        .iter()
                        .map(|n| {
                            serde_json::json!({
                                "name": n.name,
                                "address": n.address,
                                "role": n.role,
                                "online": n.online,
                                "probe_skipped": n.probe_skipped,
                                "latency_ms": n.latency_ms,
                                "keys": n.keys,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                } else {
                    println!();
                    println!("  keyz cluster monitor");
                    println!("  {}", "\u{2500}".repeat(75));
                    render_cluster_table(&nodes);
                    println!("  {}", "\u{2500}".repeat(75));
                    println!("  Refreshing every {}s... (Ctrl+C to stop)", args.interval);
                }
            }
            Err(e) => {
                println!("  error: leader unreachable: {e}");
            }
        }

        thread::sleep(Duration::from_secs(args.interval));
    }
}

fn handle_metrics(client: &KeyzClient, args: MetricsArgs, json: bool) -> Result<()> {
    let response = client.send("INFO");

    match response {
        Ok(payload) => {
            if args.raw {
                println!("{payload}");
                return Ok(());
            }

            if let Ok(json_payload) = serde_json::from_str::<serde_json::Value>(&payload) {
                println!("{}", serde_json::to_string_pretty(&json_payload)?);
                return Ok(());
            }

            if json {
                let payload = json!({
                    "raw": payload,
                    "parsed": null,
                    "note": "Server returned non-JSON metrics payload."
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!("Server metrics:\n{payload}");
                println!("(Unable to parse as JSON; displaying raw payload.)");
            }
        }
        Err(err) => {
            let message = format!("metrics unavailable: {err}");
            if json {
                let payload = json!({
                    "error": message,
                    "hint": "The server may not yet implement an INFO command."
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!("{message}");
                println!("Hint: upgrade the server once it supports an INFO command.");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod client_tests {
    use super::*;
    use std::{
        io::Read,
        net::{TcpListener, TcpStream},
        sync::mpsc,
    };

    fn listener_and_client(protocol: &ProtocolConfig) -> (TcpListener, KeyzClient) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = KeyzClient::new(
            listener.local_addr().unwrap().to_string(),
            Duration::from_secs(2),
            Duration::from_millis(100),
            protocol,
        );
        (listener, client)
    }

    #[test]
    fn cluster_probes_skip_untrusted_addresses_before_connection() {
        let (listener, client) = listener_and_client(&ProtocolConfig::default());
        let client = client.with_security(keyz::config::SecurityConfig {
            auth_token: Some("local-test-token".into()),
            ..Default::default()
        });
        assert_eq!(
            probe_node(&client, &listener.local_addr().unwrap().to_string()),
            (false, None, None, true)
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    fn accept(listener: &TcpListener) -> TcpStream {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream.set_nodelay(true).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    return stream;
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out accepting CLI connection"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("accept failed: {err}"),
            }
        }
    }

    fn request(stream: &mut TcpStream) -> String {
        let mut len = [0; 4];
        stream.read_exact(&mut len).unwrap();
        let mut bytes = vec![0; u32::from_be_bytes(len) as usize];
        stream.read_exact(&mut bytes).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    fn response(stream: &mut TcpStream, payload: &str) -> io::Result<()> {
        stream.write_all(&(payload.len() as u32).to_be_bytes())?;
        stream.write_all(payload.as_bytes())
    }

    #[test]
    fn typed_client_preserves_binary_values_and_reuses_connection() {
        let (listener, client) = listener_and_client(&ProtocolConfig::default());
        let worker = thread::spawn(move || {
            let mut stream = accept(&listener);
            let received = protocol::decode_request(&request(&mut stream)).unwrap();
            assert_eq!(received.id, 9);
            assert_eq!(
                received.command,
                protocol::Command::Set {
                    key: "a".into(),
                    value: vec![0, 255],
                    ttl_secs: Some(0)
                }
            );
            response(
                &mut stream,
                &protocol::encode_response(&protocol::Response {
                    id: Some(9),
                    result: ResponseBody::Ok,
                }),
            )
            .unwrap();
            let received = protocol::decode_request(&request(&mut stream)).unwrap();
            assert_eq!(received.id, 10);
            response(
                &mut stream,
                &protocol::encode_response(&protocol::Response {
                    id: Some(10),
                    result: ResponseBody::Value {
                        value: vec![0, 255],
                    },
                }),
            )
            .unwrap();
        });
        let first = client
            .send_request(&Request {
                id: 9,
                command: protocol::Command::Set {
                    key: "a".into(),
                    value: vec![0, 255],
                    ttl_secs: Some(0),
                },
            })
            .unwrap();
        assert_eq!(first.result, ResponseBody::Ok);
        assert!(client.is_connected());
        let next = client
            .send_request(&Request {
                id: 10,
                command: protocol::Command::Get { key: "a".into() },
            })
            .unwrap();
        assert_eq!(
            next.result,
            ResponseBody::Value {
                value: vec![0, 255]
            }
        );
        worker.join().unwrap();
    }

    #[test]
    fn typed_mismatched_response_discards_connection_without_replaying_mutation() {
        let (listener, client) = listener_and_client(&ProtocolConfig::default());
        let worker = thread::spawn(move || {
            let mut first = accept(&listener);
            assert_eq!(
                protocol::decode_request(&request(&mut first)).unwrap().id,
                9
            );
            response(
                &mut first,
                &protocol::encode_response(&protocol::Response {
                    id: Some(8),
                    result: ResponseBody::Ok,
                }),
            )
            .unwrap();
            let mut next = accept(&listener);
            assert_eq!(request(&mut next), "GET next");
            response(&mut next, "null").unwrap();
        });
        let result = client.send_request(&Request {
            id: 9,
            command: protocol::Command::Del { key: "a".into() },
        });
        assert!(result.is_err());
        assert!(!client.is_connected());
        assert_eq!(client.send("GET next").unwrap(), "null");
        worker.join().unwrap();
    }

    #[test]
    fn status_distinguishes_reachable_from_ready() {
        let (listener, client) = listener_and_client(&ProtocolConfig::default());
        let worker = thread::spawn(move || {
            let mut stream = accept(&listener);
            assert_eq!(request(&mut stream), "HEALTH");
            response(
                &mut stream,
                "{\"live\":true,\"ready\":false,\"replication_state\":\"syncing\"}",
            )
            .unwrap();
        });
        let snapshot = probe_status(&client);
        assert!(snapshot.reachable);
        assert!(!snapshot.ready);
        worker.join().unwrap();
    }

    #[test]
    fn lost_mutation_ack_is_not_replayed() {
        for command in ["DEL key", "SET key new EX 1", "NOOP", "GET close"] {
            let protocol = ProtocolConfig {
                close_command: "GET close".into(),
                ..ProtocolConfig::default()
            };
            let (listener, client) = listener_and_client(&protocol);
            let worker = thread::spawn(move || {
                let mut first = accept(&listener);
                assert_eq!(request(&mut first), command);
                // The server applied the operation, then lost its acknowledgment.
                drop(first);
                let mut next = accept(&listener);
                assert_eq!(
                    request(&mut next),
                    "GET key",
                    "the failed command was replayed"
                );
                response(&mut next, "null").unwrap();
            });

            let error = client.send(command).unwrap_err();
            assert!(error.to_string().contains("was not retried"));
            assert!(!client.is_connected());
            assert_eq!(client.send("GET key").unwrap(), "null");
            worker.join().unwrap();
        }
    }

    #[test]
    fn failed_second_read_attempt_cannot_supply_the_next_response() {
        let (listener, client) = listener_and_client(&ProtocolConfig::default());
        let (timeouts_done_tx, timeouts_done_rx) = mpsc::channel();
        let (late_response_tx, late_response_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut first = accept(&listener);
            assert_eq!(request(&mut first), "GET a");
            // A disconnected first attempt leaves budget for the one safe retry.
            drop(first);
            let mut second = accept(&listener);
            assert_eq!(request(&mut second), "GET a");
            // Hold the retry open until the shared request deadline expires.
            timeouts_done_rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            let _ = response(&mut second, "VALUE_OF_A");
            late_response_tx.send(()).unwrap();
            let mut third = accept(&listener);
            assert_eq!(request(&mut third), "GET b");
            response(&mut third, "VALUE_OF_B").unwrap();
        });

        assert!(client.send("GET a").is_err());
        timeouts_done_tx.send(()).unwrap();
        late_response_rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(!client.is_connected());
        assert_eq!(client.send("GET b").unwrap(), "VALUE_OF_B");
        worker.join().unwrap();
    }

    #[test]
    fn successful_exchanges_reuse_the_connection() {
        let (listener, client) = listener_and_client(&ProtocolConfig::default());
        let worker = thread::spawn(move || {
            let mut stream = accept(&listener);
            assert_eq!(request(&mut stream), "GET a");
            response(&mut stream, "A").unwrap();
            assert_eq!(request(&mut stream), "GET b");
            response(&mut stream, "B").unwrap();
        });
        assert_eq!(client.send("GET a").unwrap(), "A");
        assert_eq!(client.send("GET b").unwrap(), "B");
        worker.join().unwrap();
    }

    #[test]
    fn batch_stops_before_next_mutation_on_server_error() {
        let protocol = ProtocolConfig::default();
        let (listener, client) = listener_and_client(&protocol);
        let worker = thread::spawn(move || {
            let mut stream = accept(&listener);
            assert_eq!(request(&mut stream), "NOOP");
            response(&mut stream, "error:invalid command").unwrap();
            let mut next = [0; 1];
            assert_eq!(
                stream.read(&mut next).unwrap(),
                0,
                "batch executed another command"
            );
        });
        let mut input = io::Cursor::new("NOOP\nSET must_not_execute yes\n");
        let result = run_batch(&client, &protocol, &mut input, true, false);
        drop(client);
        assert!(result.unwrap_err().to_string().contains("--stop-on-error"));
        worker.join().unwrap();
    }

    #[test]
    fn batch_preserves_get_values_that_look_like_errors() {
        let protocol = ProtocolConfig::default();
        let (listener, client) = listener_and_client(&protocol);
        let worker = thread::spawn(move || {
            let mut stream = accept(&listener);
            for value in ["error:invalid command", "error:timeout", "error:my value"] {
                assert_eq!(request(&mut stream), "GET key");
                response(&mut stream, value).unwrap();
            }
            assert_eq!(request(&mut stream), "DEL error:key");
            response(&mut stream, "error:key").unwrap();
            assert_eq!(request(&mut stream), "SET executed yes");
            response(&mut stream, "ok").unwrap();
        });
        let mut input =
            io::Cursor::new("GET key\nGET key\nGET key\nDEL error:key\nSET executed yes\n");
        run_batch(&client, &protocol, &mut input, true, false).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn batch_response_checks_follow_command_contracts() {
        let protocol = ProtocolConfig {
            close_command: "QUIT".into(),
            invalid_command_response: "invalid".into(),
            ..ProtocolConfig::default()
        };
        for (command, response, failed) in [
            ("GET", "error:invalid command", true),
            ("GET two keys", "error:invalid command", true),
            ("SET key value EX huge", "error:set command invalid", true),
            (
                "SET key value",
                "error:readonly - this is a follower node",
                true,
            ),
            ("DEL key", "error:readonly - this is a follower node", true),
            ("EXIN key", "42", false),
            ("EXIN key", "error:timeout", true),
            ("INFO", "{}", false),
            ("INFO", "invalid", true),
            ("NOOP", "invalid", true),
            ("QUIT", "Closing connection", false),
        ] {
            assert_eq!(
                command_response_failed(command, response, &protocol),
                failed,
                "{command}"
            );
        }
    }
}
