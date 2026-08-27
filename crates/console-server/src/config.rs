//! TOML configuration. Key-file discipline mirrors the engine's
//! `require_private_file` / `read_api_key`: regular non-symlink file, mode
//! 0600 or stricter, 1..=4096 bytes with at least one non-whitespace byte,
//! value trimmed of leading/trailing ASCII whitespace.

use std::io::BufReader;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::http::HeaderValue;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    listen: Option<String>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    access_key_file: PathBuf,
    ui_dir: Option<PathBuf>,
    history: Option<RawHistory>,
    #[serde(default)]
    instance: Vec<RawInstance>,
    #[serde(default)]
    agent: Vec<RawAgent>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHistory {
    db_path: Option<PathBuf>,
    retention_days: Option<u64>,
    enabled: Option<bool>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInstance {
    name: String,
    base_url: String,
    api_key_file: PathBuf,
    ca_file: Option<PathBuf>,
}

/// A phase-4 sidecar exporter. `deny_unknown_fields` is what makes the
/// "agents hold no credential" rule mechanical: an `api_key_file` here is a
/// config error, not a key the console would then have somewhere to send.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    name: String,
    base_url: String,
    instance: String,
    kind: String,
}

pub struct Config {
    pub listen: SocketAddr,
    pub tls: Option<ConsoleTls>,
    pub access_key: Vec<u8>,
    pub ui_dir: PathBuf,
    pub history: HistoryConfig,
    /// The first instance is the default served at the root-anchored routes.
    pub instances: Vec<Instance>,
    /// Sidecar exporters, in config order. Each names the instance whose
    /// hardware it measures; the sampler stores its samples under that
    /// instance's name so the fleet/instance join keeps working.
    pub agents: Vec<Agent>,
    /// Keyless agent HTTPS uses platform roots only. It is a different
    /// rustls config and a different client from every engine client, so a
    /// custom engine CA can never become agent trust.
    pub agent_tls_config: Arc<rustls::ClientConfig>,
}

/// Console-terminated TLS material. The key bytes came from an O_NOFOLLOW,
/// exact-mode-0600 read and are never written back out by the process.
pub struct ConsoleTls {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

/// The `[history]` table. The store holds the fleet's telemetry, so its
/// path is anchored against the config directory exactly like the key
/// files are — a relative `db_path` never resolves against the process's
/// working directory.
pub struct HistoryConfig {
    pub db_path: PathBuf,
    pub retention_days: u64,
    pub enabled: bool,
    /// Sampling cadence, deliberately absent from the TOML surface: 1 s is
    /// part of the history contract (docs/engine-contract.md pins the
    /// engine's own 1 Hz tick), and an operator-tunable scrape rate would
    /// make "within one sample interval" mean different things on
    /// different consoles. Tests shorten it to keep runs quick.
    pub sample_interval_ms: u64,
}

pub const DEFAULT_DB_FILE: &str = "console-history.sqlite";
pub const DEFAULT_RETENTION_DAYS: u64 = 7;
pub const DEFAULT_SAMPLE_INTERVAL_MS: u64 = 1000;

impl HistoryConfig {
    fn resolve(raw: Option<RawHistory>, base_dir: &Path) -> Result<HistoryConfig, String> {
        let raw = raw.unwrap_or(RawHistory {
            db_path: None,
            retention_days: None,
            enabled: None,
        });
        let retention_days = raw.retention_days.unwrap_or(DEFAULT_RETENTION_DAYS);
        if retention_days == 0 {
            return Err("history.retention_days must be at least 1".to_owned());
        }
        Ok(HistoryConfig {
            db_path: anchor(
                base_dir,
                &raw.db_path
                    .unwrap_or_else(|| PathBuf::from(DEFAULT_DB_FILE)),
            ),
            retention_days,
            enabled: raw.enabled.unwrap_or(true),
            sample_interval_ms: DEFAULT_SAMPLE_INTERVAL_MS,
        })
    }
}

pub struct Instance {
    pub name: String,
    /// Normalized `http[s]://<authority>`, no trailing slash.
    pub base_url: String,
    pub authority: String,
    pub api_key: Vec<u8>,
    /// Prebuilt `Bearer <key>` header value, validated at load time.
    pub bearer: HeaderValue,
    pub is_https: bool,
    /// One trust store per instance. Even two instances with the same CA get
    /// distinct configs, making cross-instance trust bleed impossible.
    pub tls_config: Arc<rustls::ClientConfig>,
}

/// Which exporter is behind an agent's `/metrics`. The kind is descriptive —
/// it rides `/v1/fleet` so the UI can name the sidecar — and never changes
/// how the exposition is parsed: the console stores the series the exporter
/// actually published, whichever kind it claims to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentKind {
    Gx10,
    Mac,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::Gx10 => "gx10",
            AgentKind::Mac => "mac",
        }
    }

    fn parse(text: &str) -> Option<AgentKind> {
        match text {
            "gx10" => Some(AgentKind::Gx10),
            "mac" => Some(AgentKind::Mac),
            _ => None,
        }
    }
}

/// A sidecar exporter. Deliberately keyless: the exporters serve no secrets,
/// so there is no credential to hold and the console never sends one to an
/// agent — not the console's own access key, and not any engine key.
pub struct Agent {
    pub name: String,
    /// Normalized `http[s]://<authority>`, no trailing slash.
    pub base_url: String,
    pub authority: String,
    /// Name of the configured instance whose hardware this agent measures.
    pub instance: String,
    pub kind: AgentKind,
    pub is_https: bool,
}

impl Config {
    pub fn load(path: &Path, listen_override: Option<&str>) -> Result<Config, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("read config {}: {error}", path.display()))?;
        let base_dir = path.parent().unwrap_or(Path::new("."));
        Config::parse(&text, base_dir, listen_override)
    }

    /// `base_dir` anchors relative paths in the config file.
    pub fn parse(
        text: &str,
        base_dir: &Path,
        listen_override: Option<&str>,
    ) -> Result<Config, String> {
        let raw: RawConfig =
            toml::from_str(text).map_err(|error| format!("parse config: {error}"))?;

        let listen_text = listen_override
            .map(str::to_owned)
            .or(raw.listen)
            .unwrap_or_else(|| "127.0.0.1:5959".to_owned());
        let listen: SocketAddr = listen_text
            .parse()
            .map_err(|error| format!("listen address '{listen_text}': {error}"))?;

        let tls = resolve_console_tls(
            raw.tls_cert.as_deref().map(|path| anchor(base_dir, path)),
            raw.tls_key.as_deref().map(|path| anchor(base_dir, path)),
            listen,
        )?;

        let access_key = load_key_file(&anchor(base_dir, &raw.access_key_file), "access-key file")?;
        let history = HistoryConfig::resolve(raw.history, base_dir)?;

        let ui_dir = anchor(base_dir, &raw.ui_dir.unwrap_or_else(|| PathBuf::from("ui")));
        if !ui_dir.join("muser-dashboard.html").is_file() {
            return Err(format!(
                "ui_dir {} must contain muser-dashboard.html",
                ui_dir.display()
            ));
        }

        if raw.instance.is_empty() {
            return Err("at least one [[instance]] is required".to_owned());
        }
        let mut instances = Vec::with_capacity(raw.instance.len());
        for entry in &raw.instance {
            let instance = validate_instance(entry, base_dir)?;
            if instances
                .iter()
                .any(|existing: &Instance| existing.name == instance.name)
            {
                return Err(format!("duplicate instance name '{}'", instance.name));
            }
            instances.push(instance);
        }

        // Agents are validated after the instances so an agent can be
        // checked against the fleet it claims to belong to.
        let mut agents: Vec<Agent> = Vec::with_capacity(raw.agent.len());
        for entry in &raw.agent {
            let agent = validate_agent(entry, &instances)?;
            if agents.iter().any(|existing| existing.name == agent.name) {
                return Err(format!("duplicate agent name '{}'", agent.name));
            }
            // Series names carry the device index but not the agent, so two
            // agents of one kind on one instance would overwrite each
            // other's rows and show one machine's readings under the
            // other's name. Refuse the config rather than lose data
            // silently; agent-scoped series would be the way to support it.
            if let Some(existing) = agents
                .iter()
                .find(|existing| existing.instance == agent.instance && existing.kind == agent.kind)
            {
                return Err(format!(
                    "agent '{}' is a second {} agent on instance '{}' (agent '{}' already \
                     claims it); their series names would collide, so only one {} agent per \
                     instance is supported",
                    agent.name,
                    agent.kind.as_str(),
                    agent.instance,
                    existing.name,
                    agent.kind.as_str()
                ));
            }
            agents.push(agent);
        }

        let agent_tls_config = if agents.iter().any(|agent| agent.is_https) {
            build_client_tls(None, "agents")?
        } else {
            build_empty_client_tls("agents")?
        };

        Ok(Config {
            listen,
            tls,
            access_key,
            ui_dir,
            history,
            instances,
            agents,
            agent_tls_config,
        })
    }

    /// Agents attached to one instance, in config order.
    pub fn agents_for<'a>(&'a self, instance: &'a str) -> impl Iterator<Item = &'a Agent> {
        self.agents
            .iter()
            .filter(move |agent| agent.instance == instance)
    }
}

fn anchor(base_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        base_dir.join(path)
    }
}

/// `[A-Za-z0-9_-]{1,64}` — the same alphabet for instance and agent names, so
/// both are safe to match exact-string in a path segment with no decoding.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn validate_instance(raw: &RawInstance, base_dir: &Path) -> Result<Instance, String> {
    if !valid_name(&raw.name) {
        return Err(format!(
            "instance name '{}' must match [A-Za-z0-9_-]{{1,64}}",
            raw.name
        ));
    }
    let label = format!("instance '{}'", raw.name);
    let parsed = validate_base_url(&label, &raw.base_url, true)?;
    if raw.ca_file.is_some() && !parsed.is_https {
        return Err(format!(
            "{label} ca_file is only valid with an https:// base_url"
        ));
    }
    let ca_file = raw.ca_file.as_deref().map(|path| anchor(base_dir, path));
    let tls_config = if parsed.is_https {
        build_client_tls(ca_file.as_deref(), &label)?
    } else {
        // Preserve key-only loopback HTTP operation on hosts without a
        // platform certificate store. This client's configured origin can
        // never select TLS, so an empty trust store is the honest minimum.
        build_empty_client_tls(&label)?
    };
    let api_key = load_key_file(
        &anchor(base_dir, &raw.api_key_file),
        &format!("instance '{}' API-key file", raw.name),
    )?;
    let mut bearer_bytes = b"Bearer ".to_vec();
    bearer_bytes.extend_from_slice(&api_key);
    let mut bearer = HeaderValue::from_bytes(&bearer_bytes).map_err(|_| {
        format!(
            "instance '{}' API key contains bytes not permitted in an Authorization header",
            raw.name
        )
    })?;
    bearer.set_sensitive(true);
    Ok(Instance {
        name: raw.name.clone(),
        base_url: parsed.base_url,
        authority: parsed.authority,
        api_key,
        bearer,
        is_https: parsed.is_https,
        tls_config,
    })
}

/// Validates an agent's `[[agent]]` entry against the already-validated
/// instance list. An agent that names an instance the console does not serve
/// would store samples nothing could ever join against, so it is a config
/// error rather than a silently orphaned scraper.
fn validate_agent(raw: &RawAgent, instances: &[Instance]) -> Result<Agent, String> {
    if !valid_name(&raw.name) {
        return Err(format!(
            "agent name '{}' must match [A-Za-z0-9_-]{{1,64}}",
            raw.name
        ));
    }
    let parsed = validate_base_url(&format!("agent '{}'", raw.name), &raw.base_url, false)?;
    let Some(kind) = AgentKind::parse(&raw.kind) else {
        return Err(format!(
            "agent '{}' kind '{}' must be \"gx10\" or \"mac\"",
            raw.name, raw.kind
        ));
    };
    if !instances
        .iter()
        .any(|instance| instance.name == raw.instance)
    {
        return Err(format!(
            "agent '{}' names instance '{}', which is not a configured [[instance]]",
            raw.name, raw.instance
        ));
    }
    Ok(Agent {
        name: raw.name.clone(),
        base_url: parsed.base_url,
        authority: parsed.authority,
        instance: raw.instance.clone(),
        kind,
        is_https: parsed.is_https,
    })
}

struct ParsedBaseUrl {
    base_url: String,
    authority: String,
    is_https: bool,
}

/// Parses one origin-only upstream URL. Engines have the stricter plaintext
/// rule: HTTP is accepted only when the URL spells a literal loopback IP.
/// In particular, `localhost` is not treated as loopback by assumption.
fn validate_base_url(
    label: &str,
    raw: &str,
    secure_remote_engine: bool,
) -> Result<ParsedBaseUrl, String> {
    let (scheme, rest, is_https) = if let Some(rest) = raw.strip_prefix("https://") {
        ("https", rest, true)
    } else if let Some(rest) = raw.strip_prefix("http://") {
        ("http", rest, false)
    } else {
        return Err(format!(
            "{label} base_url must start with http:// or https://"
        ));
    };
    if rest.contains('#') {
        return Err(format!("{label} base_url must not contain a fragment"));
    }
    if rest.contains('?') {
        return Err(format!("{label} base_url must not contain a query"));
    }
    if rest.contains('@') {
        return Err(format!("{label} base_url must not contain userinfo"));
    }
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() || rest.contains('/') {
        return Err(format!(
            "{label} base_url must be {scheme}://host[:port] with no path"
        ));
    }
    let authority: axum::http::uri::Authority = rest
        .parse()
        .map_err(|error| format!("{label} base_url authority: {error}"))?;
    if authority.as_str() != rest {
        return Err(format!(
            "{label} base_url authority is not in canonical form"
        ));
    }
    if secure_remote_engine && !is_https {
        let authority_host = authority.host();
        let host_text = authority_host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(authority_host);
        let host: IpAddr = host_text.parse().map_err(|_| {
            format!(
                "{label} plaintext base_url must use a literal loopback IP; \
                 non-loopback engines require HTTPS"
            )
        })?;
        if !host.is_loopback() {
            return Err(format!(
                "{label} plaintext base_url is not a literal loopback IP; \
                 non-loopback engines require HTTPS"
            ));
        }
    }
    Ok(ParsedBaseUrl {
        base_url: format!("{scheme}://{rest}"),
        authority: rest.to_owned(),
        is_https,
    })
}

fn resolve_console_tls(
    cert_path: Option<PathBuf>,
    key_path: Option<PathBuf>,
    listen: SocketAddr,
) -> Result<Option<ConsoleTls>, String> {
    match (cert_path, key_path) {
        (None, None) if listen.ip().is_loopback() => Ok(None),
        (None, None) => Err(format!(
            "listen address '{listen}' is not loopback; tls_cert and tls_key are required"
        )),
        (Some(_), None) | (None, Some(_)) => {
            Err("tls_cert and tls_key must be configured together".to_owned())
        }
        (Some(cert_path), Some(key_path)) => {
            let cert_pem = read_regular_file_bounded(&cert_path, "TLS certificate", 1024 * 1024)?;
            if cert_pem.is_empty() {
                return Err(format!(
                    "TLS certificate {} must not be empty",
                    cert_path.display()
                ));
            }
            let key_pem = read_private_file(&key_path, "TLS private key", 64 * 1024 + 1, true)?;
            if key_pem.is_empty() {
                return Err(format!(
                    "TLS private key {} must not be empty",
                    key_path.display()
                ));
            }
            if key_pem.len() > 64 * 1024 {
                return Err(format!(
                    "TLS private key {} exceeds the 65536-byte limit",
                    key_path.display()
                ));
            }
            Ok(Some(ConsoleTls { cert_pem, key_pem }))
        }
    }
}

fn build_client_tls(
    ca_file: Option<&Path>,
    label: &str,
) -> Result<Arc<rustls::ClientConfig>, String> {
    let mut roots = rustls::RootCertStore::empty();
    if let Some(path) = ca_file {
        let pem = read_regular_file_bounded(path, &format!("{label} CA file"), 1024 * 1024)?;
        let certificates: Result<Vec<_>, _> =
            rustls_pemfile::certs(&mut BufReader::new(pem.as_slice())).collect();
        let certificates = certificates
            .map_err(|error| format!("read {label} CA file {}: {error}", path.display()))?;
        if certificates.is_empty() {
            return Err(format!(
                "{label} CA file {} contains no certificates",
                path.display()
            ));
        }
        for certificate in certificates {
            roots.add(certificate).map_err(|error| {
                format!("parse {label} CA certificate {}: {error}", path.display())
            })?;
        }
    } else {
        let native = rustls_native_certs::load_native_certs();
        let (accepted, _) = roots.add_parsable_certificates(native.certs);
        if accepted == 0 {
            let detail = native.errors.first().map_or_else(
                || "no certificates were found".to_owned(),
                ToString::to_string,
            );
            return Err(format!("load platform roots for {label}: {detail}"));
        }
    }

    finish_client_tls(roots, label)
}

fn build_empty_client_tls(label: &str) -> Result<Arc<rustls::ClientConfig>, String> {
    finish_client_tls(rustls::RootCertStore::empty(), label)
}

fn finish_client_tls(
    roots: rustls::RootCertStore,
    label: &str,
) -> Result<Arc<rustls::ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("configure TLS for {label}: {error}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

fn load_key_file(path: &Path, label: &str) -> Result<Vec<u8>, String> {
    let value = read_private_file(path, label, 4097, false)?;
    if value.iter().all(u8::is_ascii_whitespace) || value.len() > 4096 {
        return Err(format!(
            "{label} {} must contain 1..=4096 non-whitespace bytes",
            path.display()
        ));
    }
    let start = value
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    Ok(value[start..end].to_vec())
}

/// O_NOFOLLOW open + fstat on the opened fd: the symlink, regular-file, and
/// mode checks see the same inode the bytes are read from, so there is no
/// check-then-read window. Deliberate divergence from the engine's
/// path-based `require_private_file` + `read_api_key` pair.
#[cfg(unix)]
fn read_private_file(
    path: &Path,
    label: &str,
    max_bytes: u64,
    exact_0600: bool,
) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            format!(
                "open {label} {}: {error} (the key file must be a regular file, not a symlink)",
                path.display()
            )
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect {label} {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!("{label} {} must be a regular file", path.display()));
    }
    let mode = metadata.permissions().mode() & 0o7777;
    if exact_0600 && mode != 0o600 {
        return Err(format!(
            "{label} {} must have exact mode 0600",
            path.display()
        ));
    }
    if !exact_0600 && mode & 0o077 != 0 {
        return Err(format!(
            "{label} {} must have mode 0600 or stricter",
            path.display()
        ));
    }
    let mut value = Vec::new();
    (&mut file)
        .take(max_bytes)
        .read_to_end(&mut value)
        .map_err(|error| format!("read {label} {}: {error}", path.display()))?;
    Ok(value)
}

#[cfg(not(unix))]
fn read_private_file(
    path: &Path,
    label: &str,
    max_bytes: u64,
    _exact_0600: bool,
) -> Result<Vec<u8>, String> {
    if !path.is_file() {
        return Err(format!("{label} {} must be a regular file", path.display()));
    }
    use std::io::Read as _;
    let file = std::fs::File::open(path)
        .map_err(|error| format!("read {label} {}: {error}", path.display()))?;
    let mut value = Vec::new();
    file.take(max_bytes)
        .read_to_end(&mut value)
        .map_err(|error| format!("read {label} {}: {error}", path.display()))?;
    Ok(value)
}

fn read_regular_file_bounded(path: &Path, label: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("open {label} {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect {label} {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!("{label} {} must be a regular file", path.display()));
    }
    if metadata.len() > max_bytes {
        return Err(format!(
            "{label} {} exceeds the {max_bytes}-byte limit",
            path.display()
        ));
    }
    let mut value = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut value)
        .map_err(|error| format!("read {label} {}: {error}", path.display()))?;
    if value.len() as u64 > max_bytes {
        return Err(format!(
            "{label} {} exceeds the {max_bytes}-byte limit",
            path.display()
        ));
    }
    Ok(value)
}
