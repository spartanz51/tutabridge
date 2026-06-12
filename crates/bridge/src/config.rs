use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub email: String,
    pub imap_port: u16,
    pub smtp_port: u16,
    #[serde(default = "default_api_url")]
    pub api_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_password: Option<String>,
    #[serde(default = "default_sync_limit")]
    pub sync_limit: usize,
    /// Read-only MCP server permission. `Disabled` (the default) means the
    /// server isn't started at all.
    #[serde(default)]
    pub mcp_permission: McpPermission,
    #[serde(default = "default_mcp_port")]
    pub mcp_port: u16,
}

/// What a connected LLM may read over the MCP server. Strictly read-only — the
/// server never exposes a tool that sends, moves, deletes or otherwise mutates
/// mail. The tiers differ only in how much they let a client *read*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum McpPermission {
    /// Server off.
    #[default]
    Disabled,
    /// Folder list, metadata search (subject / sender / date), and message
    /// headers only — never body content.
    Metadata,
    /// Everything `Metadata` allows, plus full-text body search and message
    /// body content.
    Full,
}

impl McpPermission {
    /// May tools read message body content (and search bodies)?
    pub fn allows_body(self) -> bool {
        matches!(self, McpPermission::Full)
    }

    /// Is the MCP server enabled at all?
    pub fn is_enabled(self) -> bool {
        !matches!(self, McpPermission::Disabled)
    }
}

fn default_sync_limit() -> usize {
    500
}

fn default_mcp_port() -> u16 {
    1944
}

fn default_api_url() -> String {
    "https://app.tuta.com".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            email: String::new(),
            imap_port: 1143,
            smtp_port: 1025,
            api_url: default_api_url(),
            bridge_password: None,
            sync_limit: default_sync_limit(),
            mcp_permission: McpPermission::default(),
            mcp_port: default_mcp_port(),
        }
    }
}

fn data_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tutabridge")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

pub fn store_db_path() -> PathBuf {
    data_dir().join("store.db")
}

pub fn store_mails_dir() -> PathBuf {
    data_dir().join("mails")
}

pub fn load_config() -> Result<Option<Config>, Box<dyn std::error::Error>> {
    let path = config_path();
    if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        Ok(Some(toml::from_str(&content)?))
    } else {
        Ok(None)
    }
}

pub fn save_config(cfg: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let path = config_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let content = toml::to_string_pretty(cfg)?;
    std::fs::write(&path, &content)?;
    Ok(())
}

pub fn ensure_bridge_password(config: &mut Config) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(ref pw) = config.bridge_password {
        return Ok(pw.clone());
    }
    let password = generate_bridge_password();
    config.bridge_password = Some(password.clone());
    save_config(config)?;
    Ok(password)
}

pub fn regenerate_bridge_password(
    config: &mut Config,
) -> Result<String, Box<dyn std::error::Error>> {
    let password = generate_bridge_password();
    config.bridge_password = Some(password.clone());
    save_config(config)?;
    Ok(password)
}

fn generate_bridge_password() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    let mut group = || -> String {
        (0..5)
            .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
            .collect()
    };
    format!("{}-{}-{}-{}", group(), group(), group(), group())
}

#[cfg(test)]
fn parse_config(content: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.email, "");
        assert_eq!(cfg.imap_port, 1143);
        assert_eq!(cfg.smtp_port, 1025);
        assert_eq!(cfg.api_url, "https://app.tuta.com");
    }

    #[test]
    fn test_parse_full_config() {
        let toml = r#"
email = "test@tuta.com"
imap_port = 1993
smtp_port = 1587
api_url = "https://custom.tuta.com"
"#;
        let cfg = parse_config(toml).unwrap();
        assert_eq!(cfg.email, "test@tuta.com");
        assert_eq!(cfg.imap_port, 1993);
        assert_eq!(cfg.smtp_port, 1587);
        assert_eq!(cfg.api_url, "https://custom.tuta.com");
    }

    #[test]
    fn test_parse_minimal_config() {
        let toml = r#"
email = "test@tuta.com"
imap_port = 1143
smtp_port = 1025
"#;
        let cfg = parse_config(toml).unwrap();
        assert_eq!(cfg.email, "test@tuta.com");
        assert_eq!(cfg.api_url, "https://app.tuta.com");
    }

    #[test]
    fn test_parse_config_missing_email() {
        let toml = r#"
imap_port = 1143
smtp_port = 1025
"#;
        let result = parse_config(toml);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_config_invalid_port_type() {
        let toml = r#"
email = "test@tuta.com"
imap_port = "not_a_number"
smtp_port = 1025
"#;
        let result = parse_config(toml);
        assert!(result.is_err());
    }

    #[test]
    fn test_config_roundtrip() {
        let cfg = Config {
            email: "roundtrip@tuta.com".to_string(),
            imap_port: 2143,
            smtp_port: 2025,
            api_url: "https://app.tuta.com".to_string(),
            bridge_password: None,
            sync_limit: 500,
            mcp_permission: McpPermission::Full,
            mcp_port: 1944,
        };
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(cfg, deserialized);
    }

    #[test]
    fn test_mcp_permission_defaults_disabled() {
        let toml = r#"
email = "test@tuta.com"
imap_port = 1143
smtp_port = 1025
"#;
        let cfg = parse_config(toml).unwrap();
        assert_eq!(cfg.mcp_permission, McpPermission::Disabled);
        assert!(!cfg.mcp_permission.is_enabled());
        assert_eq!(cfg.mcp_port, 1944);
    }

    #[test]
    fn test_mcp_permission_parses_and_gates_body() {
        let toml = r#"
email = "test@tuta.com"
imap_port = 1143
smtp_port = 1025
mcp_permission = "metadata"
mcp_port = 9999
"#;
        let cfg = parse_config(toml).unwrap();
        assert_eq!(cfg.mcp_permission, McpPermission::Metadata);
        assert!(cfg.mcp_permission.is_enabled());
        assert!(!cfg.mcp_permission.allows_body());
        assert_eq!(cfg.mcp_port, 9999);
    }

    #[test]
    fn test_parse_config_extra_fields_ignored() {
        let toml = r#"
email = "test@tuta.com"
imap_port = 1143
smtp_port = 1025
unknown_field = "ignored"
"#;
        // toml by default errors on unknown fields with deny_unknown_fields,
        // but serde default is to ignore them
        let result = parse_config(toml);
        assert!(result.is_ok());
    }
}
