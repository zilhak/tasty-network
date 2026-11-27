//! `tasty-rendezvous`: the rendezvous server executable (docs/design/rendezvous.md).
//!
//! ```text
//! tasty-rendezvous --config <file>            run the server
//! tasty-rendezvous settings --config <file>   print each setting with its value and source
//! tasty-rendezvous keygen --dir <dir>         create the server key (or show the existing one)
//! ```
//!
//! The settings file is TOML: the server settings (docs/design/rendezvous.md "서버 설정값")
//! plus `key_dir` (the directory of the server key, made by `keygen`) and an `[access]` table
//! with `policy = "everyone"` (default) or `policy = "allowlist"` and `file = <path>` (one
//! device ID per line). Relative paths are relative to the settings file. The log level is
//! `info`; `TASTY_RENDEZVOUS_LOG=debug` adds addresses.

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use tasty_net_identity::DeviceKey;
use tasty_net_rendezvous::access::{AccessPolicy, Allowlist, Everyone};
use tasty_net_rendezvous::server::{Server, ServerConfig};
use tasty_net_rendezvous::settings::ServerSettings;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

const USAGE: &str = "usage: tasty-rendezvous --config <file>
       tasty-rendezvous settings --config <file>
       tasty-rendezvous keygen --dir <dir>";

/// Prints events to stderr, one line each.
struct StderrLog {
    max: Level,
}

struct Line(String);

impl Visit for Line {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ignored = write!(self.0, " {value:?}");
        } else {
            let _ignored = write!(self.0, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            let _ignored = write!(self.0, " {value}");
        } else {
            let _ignored = write!(self.0, " {}={value}", field.name());
        }
    }
}

impl Subscriber for StderrLog {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= &self.max
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(u64::MAX)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut line = Line(format!(
            "{} {}:",
            event.metadata().level(),
            event.metadata().target()
        ));
        event.record(&mut line);
        eprintln!("{}", line.0);
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

/// The executable's settings file.
struct FileConfig {
    settings: ServerSettings,
    key_dir: Option<PathBuf>,
    access: Arc<dyn AccessPolicy>,
    access_name: String,
}

fn load(path: &Path) -> Result<FileConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| format!("{}: {e}", path.display()))?;
    let base = path.parent().unwrap_or(Path::new("."));
    let relative = |p: &str| {
        let p = PathBuf::from(p);
        if p.is_absolute() { p } else { base.join(p) }
    };
    let key_dir = match table.remove("key_dir") {
        None => None,
        Some(toml::Value::String(s)) => Some(relative(&s)),
        Some(other) => return Err(format!("key_dir must be a string, got {other:?}")),
    };
    let (access, access_name): (Arc<dyn AccessPolicy>, String) = match table.remove("access") {
        None => (Arc::new(Everyone), "everyone".to_owned()),
        Some(toml::Value::Table(mut t)) => {
            let policy = match t.remove("policy") {
                None => "everyone".to_owned(),
                Some(toml::Value::String(s)) => s,
                Some(other) => {
                    return Err(format!("access.policy must be a string, got {other:?}"));
                }
            };
            let file = t.remove("file");
            if let Some(k) = t.keys().next() {
                return Err(format!("unknown key access.{k}"));
            }
            match (policy.as_str(), file) {
                ("everyone", None) => (Arc::new(Everyone), policy),
                ("allowlist", Some(toml::Value::String(f))) => {
                    let file = relative(&f);
                    let list = Allowlist::from_file(&file)
                        .map_err(|e| format!("{}: {e}", file.display()))?;
                    let name = format!("allowlist ({} devices, {})", list.len(), file.display());
                    (Arc::new(list), name)
                }
                ("allowlist", _) => {
                    return Err("access.policy = \"allowlist\" needs access.file".into());
                }
                ("everyone", Some(_)) => {
                    return Err("access.file is only for the allowlist policy".into());
                }
                (other, _) => return Err(format!("unknown access.policy {other}")),
            }
        }
        Some(other) => return Err(format!("access must be a table, got {other:?}")),
    };
    let settings = ServerSettings::from_toml(&table).map_err(|e| e.to_string())?;
    Ok(FileConfig {
        settings,
        key_dir,
        access,
        access_name,
    })
}

fn config_arg(args: &[String]) -> Result<PathBuf, String> {
    match args {
        [flag, path] if flag == "--config" => Ok(PathBuf::from(path)),
        _ => Err(USAGE.to_owned()),
    }
}

fn print_settings(path: &Path) -> Result<(), String> {
    let config = load(path)?;
    // Check the values the same way the server does at start.
    config.settings.resolve().map_err(|e| e.to_string())?;
    for e in config.settings.entries() {
        println!("{e}");
    }
    match &config.key_dir {
        Some(dir) => println!("key_dir = {}", dir.display()),
        None => println!("key_dir = (not set)"),
    }
    println!("access = {}", config.access_name);
    Ok(())
}

fn keygen(args: &[String]) -> Result<(), String> {
    let dir = match args {
        [flag, dir] if flag == "--dir" => PathBuf::from(dir),
        _ => return Err(USAGE.to_owned()),
    };
    let existed = dir.join(tasty_net_identity::DEVICE_KEY_FILE).exists();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let key = DeviceKey::load_or_create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if existed {
        println!("existing server key kept");
    } else {
        println!("server key created");
    }
    println!("public key: {}", key.id());
    Ok(())
}

fn run(path: &Path) -> Result<(), String> {
    let config = load(path)?;
    let key_dir = config
        .key_dir
        .ok_or_else(|| "key_dir is not set (make a key with tasty-rendezvous keygen)".to_owned())?;
    if !key_dir.join(tasty_net_identity::DEVICE_KEY_FILE).exists() {
        return Err(format!(
            "no server key in {} (make one with tasty-rendezvous keygen --dir {})",
            key_dir.display(),
            key_dir.display()
        ));
    }
    let key =
        DeviceKey::load_or_create(&key_dir).map_err(|e| format!("{}: {e}", key_dir.display()))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    runtime.block_on(async move {
        let server = Server::start(ServerConfig {
            key,
            settings: config.settings,
            access: config.access,
        })
        .await
        .map_err(|e| e.to_string())?;
        tracing::info!(access = %config.access_name, "access policy");
        tokio::signal::ctrl_c()
            .await
            .map_err(|e| format!("signal: {e}"))?;
        tracing::info!("shutting down");
        server.shutdown().await;
        Ok(())
    })
}

fn main() -> ExitCode {
    let max = match std::env::var("TASTY_RENDEZVOUS_LOG").as_deref() {
        Ok("debug") => Level::DEBUG,
        Ok("trace") => Level::TRACE,
        Ok("warn") => Level::WARN,
        _ => Level::INFO,
    };
    if tracing::subscriber::set_global_default(StderrLog { max }).is_err() {
        eprintln!("log subscriber was already set");
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        Some((cmd, rest)) if cmd == "settings" => config_arg(rest).and_then(|p| print_settings(&p)),
        Some((cmd, rest)) if cmd == "keygen" => keygen(rest),
        Some((cmd, _)) if cmd == "-h" || cmd == "--help" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => config_arg(&args).and_then(|p| run(&p)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tasty-rendezvous: {e}");
            ExitCode::FAILURE
        }
    }
}
