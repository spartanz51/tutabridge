use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub email: String,
    pub imap_port: u16,
    pub smtp_port: u16,
    #[serde(default = "default_api_url")]
    pub api_url: String,
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
        }
    }
}

fn config_path() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tutabridge");
    dir.join("config.toml")
}

pub fn load_or_create_config() -> Result<Config, Box<dyn std::error::Error>> {
    let path = config_path();

    let mut cfg = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        toml::from_str(&content)?
    } else {
        std::fs::create_dir_all(path.parent().unwrap())?;
        Config::default()
    };

    if cfg.email.is_empty() {
        use std::io::{BufRead, Write};
        print!("Tuta email address: ");
        std::io::stdout().flush()?;
        let mut email = String::new();
        std::io::stdin().lock().read_line(&mut email)?;
        let email = email.trim().to_string();
        if email.is_empty() {
            return Err("Email address is required".into());
        }
        cfg.email = email;
        let content = toml::to_string_pretty(&cfg)?;
        std::fs::write(&path, &content)?;
    }

    Ok(cfg)
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
        };
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(cfg, deserialized);
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
