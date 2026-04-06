//! Channel configuration and branch resolution

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelConfig {
    pub channel: String,
    pub prerelease: bool,
}

/// Parse `[channels]` table from a TOML value.
/// Supports:
/// - Shorthand: `main = "production"` (prerelease = false)
/// - Table: `"release/beta" = { channel = "beta", prerelease = true }`
pub fn parse_channels(table: &toml::Value) -> HashMap<String, ChannelConfig> {
    let mut channels = HashMap::new();
    let Some(table) = table.as_table() else {
        return channels;
    };

    for (key, value) in table {
        let config = if let Some(channel) = value.as_str() {
            ChannelConfig {
                prerelease: channel != "production",
                channel: channel.to_string(),
            }
        } else if let Some(table) = value.as_table() {
            ChannelConfig {
                channel: table
                    .get("channel")
                    .and_then(|v| v.as_str())
                    .unwrap_or(key)
                    .to_string(),
                prerelease: table
                    .get("prerelease")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            }
        } else {
            continue;
        };
        channels.insert(key.clone(), config);
    }

    channels
}

/// Resolve a git branch name to a channel config.
/// Exact matches take priority over glob patterns.
/// Glob patterns support trailing `*` only.
pub fn resolve_channel(
    branch: &str,
    channels: &HashMap<String, ChannelConfig>,
) -> Option<ChannelConfig> {
    // Exact match first
    if let Some(config) = channels.get(branch) {
        return Some(config.clone());
    }

    // Glob match (trailing * only)
    for (pattern, config) in channels {
        if let Some(prefix) = pattern.strip_suffix('*') {
            if branch.starts_with(prefix) {
                return Some(config.clone());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_channels_table() -> toml::Value {
        toml::from_str(
            r#"
main = "production"
dev = "canary"
"release/beta" = { channel = "beta", prerelease = true }
"release/*" = { channel = "rc", prerelease = true }
"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_shorthand_channel() {
        let channels = parse_channels(&sample_channels_table());
        assert_eq!(
            channels.get("main"),
            Some(&ChannelConfig {
                channel: "production".to_string(),
                prerelease: false,
            })
        );
    }

    #[test]
    fn parses_table_channel() {
        let channels = parse_channels(&sample_channels_table());
        assert_eq!(
            channels.get("release/beta"),
            Some(&ChannelConfig {
                channel: "beta".to_string(),
                prerelease: true,
            })
        );
    }

    #[test]
    fn resolves_exact_match() {
        let channels = parse_channels(&sample_channels_table());
        let config = resolve_channel("main", &channels);
        assert_eq!(
            config,
            Some(ChannelConfig {
                channel: "production".to_string(),
                prerelease: false,
            })
        );
    }

    #[test]
    fn resolves_glob_match() {
        let channels = parse_channels(&sample_channels_table());
        let config = resolve_channel("release/foo", &channels);
        assert_eq!(
            config,
            Some(ChannelConfig {
                channel: "rc".to_string(),
                prerelease: true,
            })
        );
    }

    #[test]
    fn exact_match_takes_priority_over_glob() {
        let channels = parse_channels(&sample_channels_table());
        let config = resolve_channel("release/beta", &channels);
        assert_eq!(
            config,
            Some(ChannelConfig {
                channel: "beta".to_string(),
                prerelease: true,
            })
        );
    }

    #[test]
    fn returns_none_for_unmapped_branch() {
        let channels = parse_channels(&sample_channels_table());
        let config = resolve_channel("feature/xyz", &channels);
        assert_eq!(config, None);
    }

    #[test]
    fn handles_empty_channels_table() {
        let table: toml::Value = toml::from_str("").unwrap();
        let channels = parse_channels(&table);
        assert!(channels.is_empty());
    }
}
