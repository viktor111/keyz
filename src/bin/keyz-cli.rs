#![allow(clippy::too_many_arguments)]

use std::{
    fmt, fs,
    io::{self, BufRead, Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand};
use config::{Config, ConfigContext, ConfigSource, ProtocolConfig, ServerConfig};
use rustyline::{error::ReadlineError, DefaultEditor};
use serde_json::json;

#[path = "../config.rs"]
mod config;

#[path = "../server/error.rs"]
mod server_error_impl;

mod server {
    pub mod error {
        pub use crate::server_error_impl::*;
    }
}

const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 3;
const DEFAULT_RESPONSE_TIMEOUT_SECS: u64 = 5;
const DEFAULT_STATUS_INTERVAL_SECS: u64 = 2;
const HEALTH_PROBE_KEY: &str = "__keyz_cli_health_check";
const DEFAULT_CONFIG_TEMPLATE: &str = r#"[server]
host = "127.0.0.1"
port = 7667

[protocol]
max_message_bytes = 4_194_304
idle_timeout_secs = 30
close_command = "CLOSE"
timeout_response = "error:timeout"
invalid_command_response = "error:invalid command"

[store]
compression_threshold = 512
cleanup_interval_ms = 250
# default_ttl_secs = 60
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
        help = "Response timeout in seconds"
    )]
    response_timeout: u64,
    #[arg(
        global = true,
        long,
        help = "Emit JSON where available for easier scripting"
    )]
    json: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Exec(ExecArgs),
    Commands(CommandsArgs),
    #[command(subcommand)]
    Config(ConfigCommand),
    Status(StatusArgs),
    Interactive(InteractiveArgs),
    Batch(BatchArgs),
    Metrics(MetricsArgs),
}

#[derive(Args)]
struct ExecArgs {
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

#[derive(Clone)]
struct KeyzClient {
    address: String,
    connect_timeout: Duration,
    response_timeout: Duration,
    max_message_bytes: u32,
}

impl KeyzClient {
    fn new(
        address: String,
        connect_timeout: Duration,
        response_timeout: Duration,
        max_message_bytes: u32,
    ) -> Self {
        Self {
            address,
            connect_timeout,
            response_timeout,
            max_message_bytes,
        }
    }

    fn send(&self, command: &str) -> Result<String> {
        if command.trim().is_empty() {
            return Err(anyhow!("command cannot be empty"));
        }

        if command.as_bytes().len() > self.max_message_bytes as usize {
            return Err(anyhow!(
                "command length {} exceeds configured max {} bytes",
                command.len(),
                self.max_message_bytes
            ));
        }

        let mut stream = self.connect()?;
        stream
            .set_read_timeout(Some(self.response_timeout))
            .context("unable to configure read timeout")?;
        stream
            .set_write_timeout(Some(self.response_timeout))
            .context("unable to configure write timeout")?;
        stream
            .set_nodelay(true)
            .context("unable to configure TCP_NODELAY")?;

        self.write_frame(&mut stream, command.as_bytes())?;
        self.read_frame(&mut stream)
    }

    fn connect(&self) -> Result<TcpStream> {
        let addrs: Vec<SocketAddr> = self
            .address
            .to_socket_addrs()
            .with_context(|| format!("failed to resolve address {}", self.address))?
            .collect();

        if addrs.is_empty() {
            return Err(anyhow!(
                "resolved address list for {} is empty",
                self.address
            ));
        }

        let mut last_err = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, self.connect_timeout) {
                Ok(stream) => return Ok(stream),
                Err(err) => last_err = Some((addr, err)),
            }
        }

        if let Some((addr, err)) = last_err {
            Err(anyhow!(
                "unable to connect to {} within {}s ({err})",
                addr,
                self.connect_timeout.as_secs()
            ))
        } else {
            Err(anyhow!("unable to connect to {}", self.address))
        }
    }

    fn write_frame(&self, stream: &mut TcpStream, payload: &[u8]) -> Result<()> {
        let len = payload.len();
        if len > u32::MAX as usize {
            return Err(anyhow!("payload too large to encode ({len} bytes)"));
        }

        let len_bytes = (len as u32).to_be_bytes();
        stream
            .write_all(&len_bytes)
            .context("failed to write frame length")?;
        stream
            .write_all(payload)
            .context("failed to write frame payload")?;
        Ok(())
    }

    fn read_frame(&self, stream: &mut TcpStream) -> Result<String> {
        let mut len_bytes = [0u8; 4];
        stream
            .read_exact(&mut len_bytes)
            .context("failed to read response length")?;

        let len = u32::from_be_bytes(len_bytes);
        if len == 0 {
            return Err(anyhow!("server returned empty frame"));
        }
        if len > self.max_message_bytes {
            return Err(anyhow!(
                "response length {} exceeds max_message_bytes {}",
                len,
                self.max_message_bytes
            ));
        }

        let mut buffer = vec![0u8; len as usize];
        stream
            .read_exact(&mut buffer)
            .context("failed to read response payload")?;
        let response = String::from_utf8(buffer)?;
        Ok(response)
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
];

#[derive(Clone)]
struct ResolvedAddress {
    host: String,
    port: u16,
}

impl fmt::Display for ResolvedAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
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
        protocol_cfg.max_message_bytes,
    );

    match cli.command {
        Commands::Exec(args) => handle_exec(&client, &protocol_cfg, args),
        Commands::Commands(args) => handle_commands(args, cli.json, &protocol_cfg),
        Commands::Config(ConfigCommand::Show) => {
            handle_config_show(&config, &source, cli.json, &endpoint)
        }
        Commands::Config(ConfigCommand::Init(args)) => handle_config_init(args),
        Commands::Status(args) => handle_status(&client, args, cli.json),
        Commands::Interactive(args) => handle_interactive(client, &protocol_cfg, args, &endpoint),
        Commands::Batch(args) => handle_batch(&client, args, cli.json),
        Commands::Metrics(args) => handle_metrics(&client, args, cli.json),
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
    let response = client.send(&command)?;
    let elapsed = start.elapsed();

    println!("{}", response);
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

    if json {
        let payload = json!({
            "source": source_desc,
            "endpoint": endpoint.to_string(),
            "using_file": !matches!(source, ConfigSource::Defaults),
            "server": {
                "host": config.server.host,
                "port": config.server.port,
            },
            "store": {
                "compression_threshold": config.store.compression_threshold,
                "cleanup_interval_ms": config.store.cleanup_interval_ms,
                "default_ttl_secs": config.store.default_ttl_secs,
            },
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

    println!("Configuration source : {}", source_desc);
    println!("Server endpoint      : {}", endpoint);
    println!(
        "Using config file    : {}",
        if matches!(source, ConfigSource::Defaults) {
            "no"
        } else {
            "yes"
        }
    );
    println!("--- server");
    println!("host = {}", config.server.host);
    println!("port = {}", config.server.port);
    println!("--- protocol");
    println!(
        "max_message_bytes       = {}",
        config.protocol.max_message_bytes
    );
    println!(
        "idle_timeout_secs       = {}",
        config.protocol.idle_timeout_secs
    );
    println!(
        "close_command           = {}",
        config.protocol.close_command
    );
    println!(
        "timeout_response        = {}",
        config.protocol.timeout_response
    );
    println!(
        "invalid_command_response= {}",
        config.protocol.invalid_command_response
    );
    println!("--- store");
    println!(
        "compression_threshold   = {}",
        config.store.compression_threshold
    );
    println!(
        "cleanup_interval_ms     = {}",
        config.store.cleanup_interval_ms
    );
    match config.store.default_ttl_secs {
        Some(ttl) => println!("default_ttl_secs        = {}", ttl),
        None => println!("default_ttl_secs        = (disabled)"),
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
    }
    Ok(())
}

#[derive(Debug)]
struct StatusSnapshot {
    reachable: bool,
    latency: Option<Duration>,
    response: Option<String>,
    error: Option<String>,
}

fn probe_status(client: &KeyzClient) -> StatusSnapshot {
    let sentinel_command = format!("GET {}", HEALTH_PROBE_KEY);

    let start = Instant::now();
    match client.send(&sentinel_command) {
        Ok(response) => StatusSnapshot {
            reachable: true,
            latency: Some(start.elapsed()),
            response: Some(response),
            error: None,
        },
        Err(err) => StatusSnapshot {
            reachable: false,
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
    client: KeyzClient,
    protocol: &ProtocolConfig,
    args: InteractiveArgs,
    endpoint: &ResolvedAddress,
) -> Result<()> {
    let mut editor = DefaultEditor::new()?;
    if let Some(path) = &args.history {
        if path.exists() {
            let _ = editor.load_history(path);
        }
    }

    println!(
        "Connected to {} (max frame {} bytes)",
        endpoint, protocol.max_message_bytes
    );
    println!("Type :help for assistance, :commands for a recap, :quit to exit.");

    loop {
        match editor.readline("keyz> ") {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(trimmed);
                if trimmed == ":quit" || trimmed == ":exit" {
                    break;
                } else if trimmed == ":help" {
                    println!("Commands: :help, :commands, :quit");
                    println!("Any other input is sent verbatim to the server.");
                    continue;
                } else if trimmed == ":commands" {
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

fn handle_batch(client: &KeyzClient, args: BatchArgs, json: bool) -> Result<()> {
    let mut reader: Box<dyn BufRead> = if let Some(path) = args.file {
        Box::new(io::BufReader::new(fs::File::open(&path).with_context(
            || format!("unable to open batch file {}", path.display()),
        )?))
    } else {
        Box::new(io::BufReader::new(io::stdin()))
    };

    let mut line = String::new();
    let mut index = 0usize;
    while reader.read_line(&mut line)? > 0 {
        index += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            line.clear();
            continue;
        }

        match client.send(trimmed) {
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

                if args.stop_on_error {
                    return Err(anyhow!("aborting due to --stop-on-error"));
                }
            }
        }

        line.clear();
    }

    Ok(())
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
