//! Configuration loaded from a TOML file next to the executable.
//!
//! Deliberately not environment variables: this is meant to run the way any other
//! Windows program does — download the exe, edit one file beside it, double-click
//! (or run from a shortcut). No system environment variables to set, no service
//! manager required.

use serde::Deserialize;
use std::net::IpAddr;
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

    /// Local address the media UDP sockets bind to. Unset means "bind to
    /// public_ip", which is right when the public address is attached to this
    /// machine (a VPS). Behind a router (NAT) the public address is not on any
    /// local network card, so set this to the machine's LAN address: sockets bind
    /// there and public_ip is advertised to viewers as well.
    pub bind_ip: Option<String>,

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

    /// If non-empty, only requests to the JSON-RPC/TLS port from one of these source
    /// addresses are accepted — everything else is refused before the TLS handshake
    /// even starts. Empty (the default) accepts from anywhere, matching the previous
    /// behaviour; the port itself has no other authentication of its own, so this is
    /// the one thing standing between it and the open internet. Only the region hosts
    /// that actually call in need to be listed, not viewers — viewers never speak to
    /// this port directly.
    pub allowed_region_ips: Vec<IpAddr>,

    /// TURN server(s) to hand viewers as a relay of last resort, for networks that
    /// block outbound UDP to anywhere but a known relay. Optional: without one, those
    /// viewers get no voice at all, same as upstream wolfvoice. ConfluenceVoice does
    /// not run a TURN server itself — point this at your own (e.g. coturn) or a paid
    /// TURN provider.
    pub turn_urls: Vec<String>,
    pub turn_username: Option<String>,
    pub turn_credential: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawConfig {
    public_ip: Option<String>,
    bind_ip: Option<String>,
    rpc_bind: Option<String>,
    media_port_lo: Option<u16>,
    media_port_hi: Option<u16>,
    max_sessions: Option<usize>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    #[serde(default)]
    allowed_region_ips: Vec<String>,
    #[serde(default)]
    turn_urls: Vec<String>,
    turn_username: Option<String>,
    turn_credential: Option<String>,
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

# OPTIONAL. Only for machines BEHIND A ROUTER (home or office NAT): the address of
# this machine on your own network, e.g. "192.168.1.20". Voice sockets bind here,
# while public_ip above is what viewers outside your network are told to use. Viewers
# on your own network keep using this address directly. Leave it commented out on a
# server that has its public address attached directly.
# bind_ip = "192.168.1.20"

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

# OPTIONAL but recommended. This port has no authentication of its own — anything
# that can reach it can pretend to be your region. If set, only these source
# addresses may connect at all (rejected before the TLS handshake); everything
# else is refused. List your region hosts' own addresses, not viewers' — viewers
# never talk to this port directly.
# allowed_region_ips = ["203.0.113.10"]

# OPTIONAL. A TURN relay for viewers whose network blocks outbound UDP to anywhere
# but a known relay server — without one, those viewers get no voice at all.
# ConfluenceVoice does not run a TURN server itself; point this at your own
# (e.g. coturn) or a paid TURN provider.
# turn_urls = ["turn:turn.example.com:3478"]
# turn_username = "user"
# turn_credential = "password"
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

        let allowed_region_ips = parse_allowed_ips(&raw.allowed_region_ips)
            .map_err(|e| format!("{}: {e}", path.display()))?;

        validate_turn_config(&raw.turn_urls, raw.turn_username.is_some(), raw.turn_credential.is_some())
            .map_err(|e| format!("{}: {e}", path.display()))?;

        Ok(Config {
            public_ip,
            bind_ip: raw
                .bind_ip
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            rpc_bind: raw.rpc_bind.unwrap_or_else(|| DEFAULT_RPC_BIND.to_string()),
            media_port_lo,
            media_port_hi,
            max_sessions: raw.max_sessions.unwrap_or(DEFAULT_MAX_SESSIONS),
            tls_cert: resolve(&dir, raw.tls_cert.as_deref().unwrap_or(DEFAULT_TLS_CERT)),
            tls_key: resolve(&dir, raw.tls_key.as_deref().unwrap_or(DEFAULT_TLS_KEY)),
            allowed_region_ips,
            turn_urls: raw.turn_urls,
            turn_username: raw.turn_username,
            turn_credential: raw.turn_credential,
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

fn parse_allowed_ips(entries: &[String]) -> Result<Vec<IpAddr>, String> {
    entries
        .iter()
        .map(|s| {
            s.trim()
                .parse::<IpAddr>()
                .map_err(|e| format!("allowed_region_ips entry {s:?} is not a valid IP address: {e}"))
        })
        .collect()
}

/// Partial TURN config (a URL with no credentials, or credentials with no URL) is
/// almost certainly a mistake, not a deliberate choice, so this fails loudly rather
/// than silently sending viewers a TURN server they cannot authenticate to, or
/// credentials with nowhere to use them.
fn validate_turn_config(turn_urls: &[String], has_username: bool, has_credential: bool) -> Result<(), String> {
    let has_urls = !turn_urls.is_empty();
    let has_creds = has_username || has_credential;
    if has_urls != has_creds {
        return Err(
            "turn_urls, turn_username and turn_credential must all be set together, or all left out"
                .to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_ips_parses_v4_and_v6_and_trims_whitespace() {
        let ips = parse_allowed_ips(&[" 203.0.113.10".to_string(), "2001:db8::1 ".to_string()]).unwrap();
        assert_eq!(ips, vec!["203.0.113.10".parse::<IpAddr>().unwrap(), "2001:db8::1".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn allowed_ips_rejects_a_hostname() {
        // A common mistake: this field takes IP addresses, not DNS names — that
        // distinction matters because the check runs before any DNS lookup would
        // happen, so a hostname here would just always fail to match.
        let err = parse_allowed_ips(&["region.example.org".to_string()]).unwrap_err();
        assert!(err.contains("region.example.org"), "{err}");
    }

    #[test]
    fn allowed_ips_empty_is_fine() {
        assert_eq!(parse_allowed_ips(&[]).unwrap(), Vec::<IpAddr>::new());
    }

    #[test]
    fn turn_config_all_present_or_all_absent_is_valid() {
        assert!(validate_turn_config(&[], false, false).is_ok(), "none set");
        assert!(
            validate_turn_config(&["turn:example.org:3478".to_string()], true, true).is_ok(),
            "all set"
        );
    }

    #[test]
    fn turn_config_rejects_partial_setup() {
        assert!(
            validate_turn_config(&["turn:example.org:3478".to_string()], false, false).is_err(),
            "url with no credentials"
        );
        assert!(
            validate_turn_config(&[], true, true).is_err(),
            "credentials with no url"
        );
        assert!(
            validate_turn_config(&[], true, false).is_err(),
            "username alone, no url, no credential"
        );
    }
}
