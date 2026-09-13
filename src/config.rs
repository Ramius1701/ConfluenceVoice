//! Configuration loaded from a TOML file next to the executable.
//!
//! Deliberately not environment variables: this is meant to run the way any other
//! Windows program does — download the exe, edit one file beside it, double-click
//! (or run from a shortcut). No system environment variables to set, no service
//! manager required.

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const CONFIG_FILENAME: &str = "confluencevoice.toml";

/// Everything the service needs, after defaults and path resolution.
pub struct Config {
    /// Public address our ICE host candidates must advertise.
    ///
    /// REQUIRED — there is deliberately no default. This must be an address that
    /// viewers on the public internet can reach, because it is the only address
    /// they ever learn: the Linden Lab voice protocol gives the server no way to
    /// trickle candidates, so our candidate list has to be complete in the SDP
    /// answer.
    pub public_ip: String,

    /// Where the JSON-RPC listener binds. TLS only, deliberately: the SDP carries
    /// this server's DTLS fingerprint, so an attacker able to rewrite signalling in
    /// flight could substitute their own and become the media endpoint. Restrict
    /// this port to your region hosts at the firewall as well — it has no
    /// authentication of its own.
    pub rpc_bind: String,

    /// Media port range. Must match the UDP range opened in your firewall, and is
    /// also the ceiling on concurrent sessions (one socket per peer connection).
    pub media_port_lo: u16,
    pub media_port_hi: u16,

    /// Ceiling on concurrent sessions, checked before a media port is allocated.
    pub max_sessions: usize,

    /// Resolved to an absolute path (relative entries are resolved against the
    /// directory the exe lives in, not the current working directory, so this
    /// works the same whether launched by double-click, shortcut, or Task
    /// Scheduler).
    pub tls_cert: PathBuf,
    pub tls_key: PathBuf,
}

#[derive(Deserialize, Default)]
struct RawConfig {
    public_ip: Option<String>,
    rpc_bind: Option<String>,
    media_port_lo: Option<u16>,
    media_port_hi: Option<u16>,
    max_sessions: Option<usize>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
}

const DEFAULT_RPC_BIND: &str = "0.0.0.0:9443";
const DEFAULT_MEDIA_PORT_LO: u16 = 40000;
const DEFAULT_MEDIA_PORT_HI: u16 = 40999;
const DEFAULT_MAX_SESSIONS: usize = 900;
const DEFAULT_TLS_CERT: &str = "tls\\fullchain.pem";
const DEFAULT_TLS_KEY: &str = "tls\\privkey.pem";

const TEMPLATE: &str = r#"# ConfluenceVoice configuration.
#
# Edit this file, then start confluencevoice.exe again. Relative paths below
# are resolved against the folder this file is in, not wherever you launched
# the exe from.

# REQUIRED. The address viewers on the public internet send voice media to.
# This cannot be guessed or defaulted: the Linden Lab voice protocol gives the
# server no way to trickle ICE candidates, so this has to be right in the very
# first answer we send. If this host has a public IP directly attached, use
# it as-is. Behind 1:1 NAT, use the PUBLIC address and forward the media port
# range (below) to this machine.
public_ip = ""

# Where the JSON-RPC/TLS listener binds. Restrict this port to your region
# hosts at the firewall — it has no authentication of its own.
rpc_bind = "0.0.0.0:9443"

# UDP port range for voice media. One port per concurrent session; this range
# must match what you open in Windows Firewall.
media_port_lo = 40000
media_port_hi = 40999

# Refuse new sessions past this count rather than exhausting the port range.
max_sessions = 900

# TLS certificate and private key, PEM format. Required — the viewer connects
# over HTTPS and the SDP's DTLS fingerprint depends on the same certificate.
# Paths are relative to this config file unless you give an absolute path.
tls_cert = "tls\\fullchain.pem"
tls_key = "tls\\privkey.pem"
"#;

impl Config {
    /// Load `confluencevoice.toml` from beside the running executable.
    ///
    /// If the file does not exist, a commented template is written in its place
    /// and this returns an error describing what to do next — the same "fail
    /// loudly rather than guessing" stance as the missing-public-IP check.
    pub fn load() -> Result<Config, String> {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let dir = exe
            .parent()
            .ok_or_else(|| "executable has no parent directory".to_string())?
            .to_path_buf();
        let path = dir.join(CONFIG_FILENAME);

        if !path.exists() {
            std::fs::write(&path, TEMPLATE)
                .map_err(|e| format!("writing default {}: {e}", path.display()))?;
            return Err(format!(
                "no configuration file existed, so one was created at:\n\n    {}\n\n\
                 Open it, set at least public_ip, and run confluencevoice again.",
                path.display()
            ));
        }

        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        let raw: RawConfig = toml::from_str(&text)
            .map_err(|e| format!("{} is not valid: {e}", path.display()))?;

        let public_ip = raw.public_ip.unwrap_or_default().trim().to_string();
        if public_ip.is_empty() {
            return Err(format!(
                "{} does not set public_ip.\n\n\
                 Set it to the PUBLIC IP address viewers should send voice media to,\n\
                 e.g. public_ip = \"203.0.113.10\"\n\n\
                 It cannot be guessed: it is the only address the viewer ever learns.",
                path.display()
            ));
        }

        let media_port_lo = raw.media_port_lo.unwrap_or(DEFAULT_MEDIA_PORT_LO);
        let media_port_hi = raw.media_port_hi.unwrap_or(DEFAULT_MEDIA_PORT_HI);
        if media_port_lo >= media_port_hi {
            return Err(format!(
                "{}: media_port_lo ({media_port_lo}) must be less than media_port_hi ({media_port_hi})",
                path.display()
            ));
        }

        Ok(Config {
            public_ip,
            rpc_bind: raw.rpc_bind.unwrap_or_else(|| DEFAULT_RPC_BIND.to_string()),
            media_port_lo,
            media_port_hi,
            max_sessions: raw.max_sessions.unwrap_or(DEFAULT_MAX_SESSIONS),
            tls_cert: resolve(&dir, raw.tls_cert.as_deref().unwrap_or(DEFAULT_TLS_CERT)),
            tls_key: resolve(&dir, raw.tls_key.as_deref().unwrap_or(DEFAULT_TLS_KEY)),
        })
    }
}

/// Resolve a config-file path against the config file's own directory, unless it
/// is already absolute.
fn resolve(base: &Path, entry: &str) -> PathBuf {
    let p = Path::new(entry);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}
