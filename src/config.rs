//! Application configuration: typed defaults, tolerant YAML loading (unknown
//! and superseded keys warn rather than error), and the config-file lookup
//! chain.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmScope {
    #[default]
    LastUser,
    All,
    None,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LocalConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f64,
    pub max_tokens: u32,
    /// Sent as `reasoning_effort` on chat requests when non-empty. Thinking
    /// models (e.g. Gemma 4) otherwise spend the entire output budget on
    /// hidden reasoning and return truncated/empty content. Set to "" if your
    /// OpenAI-compatible server rejects the field.
    pub reasoning_effort: String,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:11434/v1".to_string(),
            api_key: "ollama".to_string(),
            model: "gemma4:12b-mlx".to_string(),
            temperature: 0.1,
            max_tokens: 2048,
            reasoning_effort: "none".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct EncoderConfig {
    pub mode: String,
    pub target_ratio: f64,
    pub protect_system_under_chars: usize,
    pub min_chars_to_compress: usize,
    pub rules_enabled: bool,
    pub llm_scope: LlmScope,
    pub llm_timeout_s: f64,
    pub list_trim_enabled: bool,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            mode: "hybrid".to_string(),
            target_ratio: 0.45,
            protect_system_under_chars: 800,
            min_chars_to_compress: 400,
            rules_enabled: true,
            llm_scope: LlmScope::LastUser,
            llm_timeout_s: 15.0,
            list_trim_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CacheConfig {
    pub max_entries: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { max_entries: 4096 }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub host: String,
    pub port: u16,
    pub upstream_base_url: String,
    pub upstream_api_key_env: String,
    pub pass_client_auth: bool,
    pub require_client_auth: bool,
    pub log_stats: bool,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8787,
            upstream_base_url: "https://api.x.ai/v1".to_string(),
            upstream_api_key_env: "X_API_KEY".to_string(),
            pass_client_auth: true,
            require_client_auth: false,
            log_stats: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct StatsConfig {
    pub usd_per_mtok_input: f64,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            usd_per_mtok_input: 3.0,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AppConfig {
    pub local: LocalConfig,
    pub encoder: EncoderConfig,
    pub cache: CacheConfig,
    pub proxy: ProxyConfig,
    pub stats: StatsConfig,
}

/// A loaded config plus provenance: what file (if any) it came from, and any
/// unknown/superseded keys that were dropped along the way.
#[derive(Debug)]
pub struct LoadedConfig {
    pub config: AppConfig,
    /// Human-readable provenance: the file path used, or "built-in defaults".
    /// Carried into proxy `AppState` and reported by `/health` (Task 11).
    pub source: String,
    pub warnings: Vec<String>,
}

/// section name -> its known (non-superseded) keys.
const KNOWN_SECTIONS: &[(&str, &[&str])] = &[
    (
        "local",
        &[
            "base_url",
            "api_key",
            "model",
            "temperature",
            "max_tokens",
            "reasoning_effort",
        ],
    ),
    (
        "encoder",
        &[
            "mode",
            "target_ratio",
            "protect_system_under_chars",
            "min_chars_to_compress",
            "rules_enabled",
            "llm_scope",
            "llm_timeout_s",
            "list_trim_enabled",
        ],
    ),
    ("cache", &["max_entries"]),
    (
        "proxy",
        &[
            "host",
            "upstream_base_url",
            "upstream_api_key_env",
            "pass_client_auth",
            "require_client_auth",
            "log_stats",
            "port",
        ],
    ),
    ("stats", &["usd_per_mtok_input"]),
];

/// Keys that existed in v1 and are silently accepted-but-ignored in v2,
/// with an explicit message pointing at the replacement (or explaining why
/// there isn't one).
fn superseded_message(section: &str, key: &str) -> Option<String> {
    match (section, key) {
        ("local", "timeout_s") => {
            Some("local.timeout_s is ignored in v2; use encoder.llm_timeout_s".to_string())
        }
        ("encoder", "roles") => {
            Some("encoder.roles is ignored; role policy is fixed in v2".to_string())
        }
        ("stats", "encoding") => {
            Some("stats.encoding is ignored in v2; tokenizer is fixed to cl100k_base".to_string())
        }
        _ => None,
    }
}

/// Walk the raw YAML mapping, dropping unknown sections/keys and superseded
/// keys while recording a human-readable warning for each. The returned
/// `Value` only contains keys `AppConfig` knows how to deserialize.
fn sanitize(mut value: Value, warnings: &mut Vec<String>) -> Value {
    if matches!(value, Value::Null) {
        return Value::Mapping(Default::default());
    }
    let Value::Mapping(map) = &mut value else {
        return value;
    };
    let top_keys: Vec<String> = map
        .keys()
        .filter_map(|k| k.as_str().map(str::to_string))
        .collect();
    for section_name in &top_keys {
        match KNOWN_SECTIONS
            .iter()
            .find(|(name, _)| *name == section_name.as_str())
        {
            Some((_, known_keys)) => {
                if let Some(Value::Mapping(section_map)) = map.get_mut(section_name.as_str()) {
                    let inner_keys: Vec<String> = section_map
                        .keys()
                        .filter_map(|k| k.as_str().map(str::to_string))
                        .collect();
                    for inner_key in &inner_keys {
                        if known_keys.contains(&inner_key.as_str()) {
                            continue;
                        }
                        let msg =
                            superseded_message(section_name, inner_key).unwrap_or_else(|| {
                                format!("unknown config key ignored: {section_name}.{inner_key}")
                            });
                        warnings.push(msg);
                        section_map.remove(inner_key.as_str());
                    }
                }
            }
            None => {
                warnings.push(format!("unknown config section ignored: {section_name}"));
                map.remove(section_name.as_str());
            }
        }
    }
    value
}

/// Load and validate a config file at an exact path. Unknown/superseded keys
/// produce warnings, not errors; a genuinely malformed YAML file (bad syntax,
/// wrong types) is an error.
pub fn load_config_from(path: &Path) -> anyhow::Result<LoadedConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    let value: Value = serde_yaml::from_str(&raw)
        .with_context(|| format!("failed to parse YAML config: {}", path.display()))?;
    let mut warnings = Vec::new();
    let value = sanitize(value, &mut warnings);
    let config: AppConfig = serde_yaml::from_value(value)
        .with_context(|| format!("failed to load config: {}", path.display()))?;
    // Known-but-unimplemented knob: accepted so v1 configs load cleanly, but
    // the user deserves to know it does nothing.
    if config.encoder.list_trim_enabled {
        warnings.push(
            "encoder.list_trim_enabled is reserved and not implemented in v2; ignored".to_string(),
        );
    }
    Ok(LoadedConfig {
        config,
        source: path.display().to_string(),
        warnings,
    })
}

/// Resolve config given an explicit path override plus an ordered list of
/// fallback candidates. Kept separate from `resolve_config` so the
/// fall-through-to-defaults path is testable without mutating process cwd.
fn resolve_config_with_candidates(
    explicit: Option<PathBuf>,
    candidates: &[PathBuf],
) -> anyhow::Result<LoadedConfig> {
    if let Some(path) = explicit {
        if !path.exists() {
            anyhow::bail!("config file not found: {}", path.display());
        }
        let loaded = load_config_from(&path)?;
        tracing::info!(source = %loaded.source, "loaded config");
        return Ok(loaded);
    }
    for candidate in candidates {
        if candidate.exists() {
            let loaded = load_config_from(candidate)?;
            tracing::info!(source = %loaded.source, "loaded config");
            return Ok(loaded);
        }
    }
    tracing::info!(source = "built-in defaults", "loaded config");
    Ok(LoadedConfig {
        config: AppConfig::default(),
        source: "built-in defaults".to_string(),
        warnings: Vec::new(),
    })
}

/// Lookup order: explicit -> $PROMPT_CODEC_CONFIG -> ./config.yaml ->
/// ~/.config/prompt-codec/config.yaml -> built-in defaults. An explicit path
/// that doesn't exist is a hard error (the user asked for that file);
/// missing files earlier in the search chain just fall through.
pub fn resolve_config(explicit: Option<PathBuf>) -> anyhow::Result<LoadedConfig> {
    let mut candidates = Vec::new();
    if let Ok(env_path) = std::env::var("PROMPT_CODEC_CONFIG") {
        candidates.push(PathBuf::from(env_path));
    }
    candidates.push(PathBuf::from("config.yaml"));
    if let Some(home) = dirs::home_dir() {
        candidates.push(
            home.join(".config")
                .join("prompt-codec")
                .join("config.yaml"),
        );
    }
    resolve_config_with_candidates(explicit, &candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = AppConfig::default();
        assert_eq!(c.local.model, "gemma4:12b-mlx");
        assert_eq!(c.encoder.llm_timeout_s, 15.0);
        assert_eq!(c.encoder.llm_scope, LlmScope::LastUser);
        assert_eq!(c.proxy.port, 8787);
        assert!(!c.encoder.list_trim_enabled);
    }

    #[test]
    fn loads_yaml_and_warns_on_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        std::fs::write(
            &p,
            "local:\n  model: qwen3:8b\n  timeout_s: 120\ndecoder:\n  enabled: true\nencoder:\n  roles: [user]\n",
        )
        .unwrap();
        let loaded = load_config_from(&p).unwrap();
        assert_eq!(loaded.config.local.model, "qwen3:8b");
        assert_eq!(loaded.config.encoder.llm_timeout_s, 15.0); // local.timeout_s ignored
        assert!(loaded.source.contains("config.yaml")); // source records the file path
        let joined = loaded.warnings.join("\n");
        assert!(joined.contains("decoder"));
        assert!(joined.contains("local.timeout_s"));
        assert!(joined.contains("encoder.roles"));
    }

    #[test]
    fn explicit_missing_file_is_error_and_source_tracks_defaults() {
        assert!(resolve_config(Some("/nonexistent/x.yaml".into())).is_err());
    }

    #[test]
    fn empty_candidate_chain_falls_through_to_defaults() {
        // Avoids cwd games in a parallel test run: exercise the internal
        // search-chain fn directly with an empty candidate list.
        let loaded = resolve_config_with_candidates(None, &[]).unwrap();
        assert_eq!(loaded.source, "built-in defaults");
        assert_eq!(loaded.config.local.model, AppConfig::default().local.model);
    }

    #[test]
    fn enabling_reserved_list_trim_knob_warns() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        std::fs::write(&p, "encoder:\n  list_trim_enabled: true\n").unwrap();
        let loaded = load_config_from(&p).unwrap();
        assert!(loaded.config.encoder.list_trim_enabled); // still deserializes
        assert!(loaded
            .warnings
            .join("\n")
            .contains("encoder.list_trim_enabled is reserved and not implemented in v2; ignored"));
        // The default (false) stays quiet.
        std::fs::write(&p, "encoder:\n  list_trim_enabled: false\n").unwrap();
        let loaded = load_config_from(&p).unwrap();
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn superseded_stats_encoding_key_warns() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        std::fs::write(
            &p,
            "stats:\n  encoding: cl100k_base\n  usd_per_mtok_input: 5.0\n",
        )
        .unwrap();
        let loaded = load_config_from(&p).unwrap();
        assert_eq!(loaded.config.stats.usd_per_mtok_input, 5.0);
        assert!(loaded.warnings.join("\n").contains("stats.encoding"));
    }

    #[test]
    fn known_sections_map_covers_every_struct_field() {
        // Anti-drift guard: serialize the default AppConfig (every section and
        // field present by construction) and push it through the sanitize walk.
        // Any struct field missing from KNOWN_SECTIONS would be stripped with a
        // warning — so zero warnings proves the map is complete.
        let value = serde_yaml::to_value(AppConfig::default()).unwrap();
        let mut warnings = Vec::new();
        let sanitized = sanitize(value, &mut warnings);
        assert_eq!(
            warnings,
            Vec::<String>::new(),
            "KNOWN_SECTIONS is out of sync with the config structs"
        );
        // And the sanitized value must still round-trip into an AppConfig.
        let roundtripped: AppConfig = serde_yaml::from_value(sanitized).unwrap();
        assert_eq!(roundtripped.local.model, AppConfig::default().local.model);
    }

    #[test]
    fn wrong_scalar_type_is_clean_error_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        std::fs::write(&p, "proxy:\n  port: \"abc\"\n").unwrap();
        let err = load_config_from(&p).unwrap_err();
        assert!(format!("{err:#}").contains("config.yaml"));
    }

    #[test]
    fn section_as_non_mapping_is_clean_error_with_path() {
        // Pins today's behavior: a known section that isn't a mapping survives
        // the sanitize walk untouched and fails typed deserialization with a
        // clean, path-bearing error (no panic).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        std::fs::write(&p, "proxy: [1, 2]\n").unwrap();
        let err = load_config_from(&p).unwrap_err();
        assert!(format!("{err:#}").contains("config.yaml"));
    }
}
