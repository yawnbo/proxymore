#[macro_use]
extern crate log;

use proxymore::{
    cert::{init_ca, CertificateAuthority},
    filter::parse_title_filters,
    server::{PrintMode, ServerBuilder, WEB_PREFIX},
    tui,
};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use simplelog::{format_description, ConfigBuilder, LevelFilter, WriteLogger};
use std::{
    fs,
    io::{self, IsTerminal},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
};
use tokio::net::TcpListener;
use wreq_util::Emulation;

const CA_CERT_FILENAME: &str = "proxymore-ca-cert.cer";
const PRIVATE_KEY_FILENAME: &str = "proxymore-key.pem";

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_dir = ensure_config_dir()?;
    setup_logger(&config_dir)?;
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    let ca = setup_ca(&config_dir)?;
    let (ip, port) =
        parse_addr(&cli.listen).ok_or_else(|| anyhow!("Invalid addr '{}'", cli.listen))?;
    let addr = format!("{}:{}", ip, port);
    let reverse_proxy_url = cli.reverse_proxy_url.map(sanitize_reverse_proxy_url);
    let emulation = parse_emulation(&cli.emulation)?;
    let title_filters = parse_title_filters(&cli.filters)?;
    let mime_filters: Vec<String> = cli.mime_filters.iter().map(|v| v.to_lowercase()).collect();
    let listener = TcpListener::bind(SocketAddr::new(ip, port)).await?;
    let is_tui = io::stdout().is_terminal() && (cli.tui || (!cli.dump && !cli.web));
    let is_dump = cli.dump || (!is_tui && !cli.web);
    let print_mode = if is_tui {
        PrintMode::Nothing
    } else if is_dump {
        PrintMode::Markdown
    } else {
        PrintMode::Oneline
    };
    let server = ServerBuilder::new(ca)
        .reverse_proxy_url(reverse_proxy_url)
        .emulation(emulation)
        .title_filters(title_filters)
        .mime_filters(mime_filters)
        .web(cli.web)
        .print_mode(print_mode)
        .build();
    let state = server.state();
    let stop_server = server.run(listener).await?;
    info!("HTTP(S) proxy listening at {addr}");
    if is_tui {
        let addr = addr.clone();
        tui::run(state, &addr).await.context("Failed to run TUI")?;
    } else {
        eprintln!("HTTP(S) proxy listening at {addr}");
        if cli.web {
            eprintln!(
                "Web interface accessible at http://{}:{}{}/",
                ip, port, WEB_PREFIX
            );
        }
        shutdown_signal().await;
    }
    let _ = stop_server.send(());
    Ok(())
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Listening ip and port address
    #[clap(short = 'l', long, value_name = "ADDR", default_value = "0.0.0.0:8080")]
    listen: String,
    /// Only inspect http(s) traffic whose `{method} {uri}` matches the regex
    #[clap(short = 'f', long, value_name = "REGEX")]
    filters: Vec<String>,
    /// Only inspect http(s) traffic whose content-type matches the value
    #[clap(short = 'm', long, value_name = "VALUE")]
    mime_filters: Vec<String>,
    /// Enable user-friendly web interface
    #[clap(short = 'W', long)]
    web: bool,
    /// Eenter TUI
    #[clap(short = 'T', long)]
    tui: bool,
    /// Dump all traffics
    #[clap(short = 'D', long)]
    dump: bool,
    /// Browser TLS fingerprint emulation (firefox, chrome, safari, edge, opera)
    #[clap(short = 'E', long, value_name = "BROWSER", default_value = "firefox")]
    emulation: String,
    /// Reverse proxy url
    #[clap(value_name = "URL")]
    reverse_proxy_url: Option<String>,
}

fn setup_ca(config_dir: &Path) -> Result<CertificateAuthority> {
    let ca_cert_file = config_dir.join(CA_CERT_FILENAME);
    let private_key_file = config_dir.join(PRIVATE_KEY_FILENAME);
    let ca = init_ca(&ca_cert_file, &private_key_file)?;
    Ok(ca)
}

fn setup_logger(config_dir: &Path) -> Result<()> {
    let log_level = if cfg!(debug_assertions) {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };
    let crate_name = env!("CARGO_CRATE_NAME");
    let config = ConfigBuilder::new()
        .add_filter_allow(crate_name.to_string())
        .set_time_format_custom(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .set_thread_level(LevelFilter::Off)
        .build();
    let log_path = config_dir.join(format!("{crate_name}.log"));
    let log_file = fs::File::create(log_path)?;
    WriteLogger::init(log_level, config, log_file)?;
    Ok(())
}

fn ensure_config_dir() -> Result<PathBuf> {
    let mut config_dir = dirs::home_dir().ok_or_else(|| anyhow!("No home dir"))?;
    config_dir.push(".proxymore");
    if !config_dir.exists() {
        fs::create_dir_all(&config_dir).map_err(|err| {
            anyhow!(
                "Failed to create config dir '{}', {err}",
                config_dir.display()
            )
        })?;
    }
    Ok(config_dir)
}

fn parse_addr(value: &str) -> Option<(IpAddr, u16)> {
    if let Ok(port) = value.parse() {
        Some(("0.0.0.0".parse().unwrap(), port))
    } else if let Ok(ip) = value.parse() {
        Some((ip, 8080))
    } else if let Some((ip, port)) = value.rsplit_once(':') {
        if let (Some(ip), Some(port)) = (ip.parse().ok(), port.parse().ok()) {
            Some((ip, port))
        } else {
            None
        }
    } else {
        None
    }
}

fn parse_emulation(s: &str) -> Result<Emulation> {
    match s.to_lowercase().as_str() {
        // set to some default versions up here
        "firefox" | "ff" => Ok(Emulation::Firefox147),
        "chrome" | "cr" => Ok(Emulation::Chrome145),
        "safari" | "sf" => Ok(Emulation::Safari26_2),
        "edge" => Ok(Emulation::Edge145),
        "opera" => Ok(Emulation::Opera119),
        other => {
            let lower = other.to_lowercase();
            if lower.starts_with("chrome") {
                let ver = lower.strip_prefix("chrome").unwrap_or("");
                // enums are found at https://docs.rs/wreq-util/latest/wreq_util/enum.Emulation.html
                return match ver {
                    "100" => Ok(Emulation::Chrome100),
                    "101" => Ok(Emulation::Chrome101),
                    "104" => Ok(Emulation::Chrome104),
                    "105" => Ok(Emulation::Chrome105),
                    "106" => Ok(Emulation::Chrome106),
                    "107" => Ok(Emulation::Chrome107),
                    "108" => Ok(Emulation::Chrome108),
                    "109" => Ok(Emulation::Chrome109),
                    "110" => Ok(Emulation::Chrome110),
                    "114" => Ok(Emulation::Chrome114),
                    "116" => Ok(Emulation::Chrome116),
                    "117" => Ok(Emulation::Chrome117),
                    "118" => Ok(Emulation::Chrome118),
                    "119" => Ok(Emulation::Chrome119),
                    "120" => Ok(Emulation::Chrome120),
                    "123" => Ok(Emulation::Chrome123),
                    "124" => Ok(Emulation::Chrome124),
                    "126" => Ok(Emulation::Chrome126),
                    "127" => Ok(Emulation::Chrome127),
                    "128" => Ok(Emulation::Chrome128),
                    "129" => Ok(Emulation::Chrome129),
                    "130" => Ok(Emulation::Chrome130),
                    "131" => Ok(Emulation::Chrome131),
                    "132" => Ok(Emulation::Chrome132),
                    "133" => Ok(Emulation::Chrome133),
                    "134" => Ok(Emulation::Chrome134),
                    "135" => Ok(Emulation::Chrome135),
                    "136" => Ok(Emulation::Chrome136),
                    "137" => Ok(Emulation::Chrome137),
                    "138" => Ok(Emulation::Chrome138),
                    "139" => Ok(Emulation::Chrome139),
                    "140" => Ok(Emulation::Chrome140),
                    "141" => Ok(Emulation::Chrome141),
                    "142" => Ok(Emulation::Chrome142),
                    "143" => Ok(Emulation::Chrome143),
                    "144" => Ok(Emulation::Chrome144),
                    "145" => Ok(Emulation::Chrome145),
                    _ => Err(anyhow!("Unknown Chrome version: {}", s)),
                };
            }
            if lower.starts_with("firefox") {
                let ver = lower.strip_prefix("firefox").unwrap_or("");
                return match ver {
                    "109" => Ok(Emulation::Firefox109),
                    "117" => Ok(Emulation::Firefox117),
                    "128" => Ok(Emulation::Firefox128),
                    "133" => Ok(Emulation::Firefox133),
                    "135" => Ok(Emulation::Firefox135),
                    "136" => Ok(Emulation::Firefox136),
                    "139" => Ok(Emulation::Firefox139),
                    "142" => Ok(Emulation::Firefox142),
                    "143" => Ok(Emulation::Firefox143),
                    "144" => Ok(Emulation::Firefox144),
                    "145" => Ok(Emulation::Firefox145),
                    "146" => Ok(Emulation::Firefox146),
                    "147" => Ok(Emulation::Firefox147),
                    _ => Err(anyhow!("Unknown Firefox version: {}", s)),
                };
            }
            if lower.starts_with("safari") {
                let ver = lower.strip_prefix("safari").unwrap_or("");
                return match ver {
                    "15.3" => Ok(Emulation::Safari15_3),
                    "15.5" => Ok(Emulation::Safari15_5),
                    "16" => Ok(Emulation::Safari16),
                    "16.5" => Ok(Emulation::Safari16_5),
                    "17.0" => Ok(Emulation::Safari17_0),
                    "17.5" => Ok(Emulation::Safari17_5),
                    "18" => Ok(Emulation::Safari18),
                    "18.2" => Ok(Emulation::Safari18_2),
                    "18.3" => Ok(Emulation::Safari18_3),
                    "18.5" => Ok(Emulation::Safari18_5),
                    "26" => Ok(Emulation::Safari26),
                    "26.1" => Ok(Emulation::Safari26_1),
                    "26.2" => Ok(Emulation::Safari26_2),
                    _ => Err(anyhow!("Unknown Safari version: {}", s)),
                };
            }
            if lower.starts_with("edge") {
                let ver = lower.strip_prefix("edge").unwrap_or("");
                return match ver {
                    "101" => Ok(Emulation::Edge101),
                    "122" => Ok(Emulation::Edge122),
                    "127" => Ok(Emulation::Edge127),
                    "131" => Ok(Emulation::Edge131),
                    "134" => Ok(Emulation::Edge134),
                    "135" => Ok(Emulation::Edge135),
                    "136" => Ok(Emulation::Edge136),
                    "137" => Ok(Emulation::Edge137),
                    "138" => Ok(Emulation::Edge138),
                    "139" => Ok(Emulation::Edge139),
                    "140" => Ok(Emulation::Edge140),
                    "141" => Ok(Emulation::Edge141),
                    "142" => Ok(Emulation::Edge142),
                    "143" => Ok(Emulation::Edge143),
                    "144" => Ok(Emulation::Edge144),
                    "145" => Ok(Emulation::Edge145),
                    _ => Err(anyhow!("Unknown Edge version: {}", s)),
                };
            }
            if lower.starts_with("opera") {
                let ver = lower.strip_prefix("opera").unwrap_or("");
                return match ver {
                    "116" => Ok(Emulation::Opera116),
                    "117" => Ok(Emulation::Opera117),
                    "118" => Ok(Emulation::Opera118),
                    "119" => Ok(Emulation::Opera119),
                    _ => Err(anyhow!("Unknown Opera version: {}", s)),
                };
            }
            Err(anyhow!(
                "Unknown browser emulation: '{}'. Use firefox, chrome, safari, edge, or opera (optionally with version, e.g. firefox136)",
                s
            ))
        }
    }
}

fn sanitize_reverse_proxy_url(url: String) -> String {
    let url = if !url.starts_with("http://") && !url.starts_with("https://") {
        format!("http://{}", url)
    } else {
        url
    };
    if let Some(url) = url.strip_suffix('/') {
        url.to_string()
    } else {
        url
    }
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install CTRL+C signal handler")
}
