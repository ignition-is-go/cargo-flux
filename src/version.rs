//! Semantic version parsing and calculation

use regex::Regex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Bump {
    Patch,
    Minor,
    Major,
}

impl Version {
    pub fn parse(tag: &str) -> Self {
        let re = Regex::new(r"^v?(\d+)\.(\d+)\.(\d+)").unwrap();
        if let Some(caps) = re.captures(tag) {
            Version {
                major: caps[1].parse().unwrap_or(0),
                minor: caps[2].parse().unwrap_or(0),
                patch: caps[3].parse().unwrap_or(0),
            }
        } else {
            Version { major: 0, minor: 0, patch: 0 }
        }
    }

    pub fn format(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

pub fn calculate_next_version(current: Version, commits: &[String]) -> Version {
    let mut bump = Bump::Patch;

    let feat_re = Regex::new(r"^feat(\([^)]*\))?!?:").unwrap();
    let breaking_re = Regex::new(r"^[a-z]+(\([^)]*\))?!:").unwrap();

    for msg in commits {
        if msg.contains("BREAKING CHANGE") || breaking_re.is_match(msg) {
            bump = Bump::Major;
            break;
        }
        if feat_re.is_match(msg) && bump < Bump::Minor {
            bump = Bump::Minor;
        }
    }

    match bump {
        Bump::Major => Version { major: current.major + 1, minor: 0, patch: 0 },
        Bump::Minor => Version { major: current.major, minor: current.minor + 1, patch: 0 },
        Bump::Patch => Version { major: current.major, minor: current.minor, patch: current.patch + 1 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_with_v_prefix() {
        let v = Version::parse("v3.1.0");
        assert_eq!(v, Version { major: 3, minor: 1, patch: 0 });
    }

    #[test]
    fn parses_version_without_prefix() {
        let v = Version::parse("3.1.0");
        assert_eq!(v, Version { major: 3, minor: 1, patch: 0 });
    }

    #[test]
    fn parses_version_with_prerelease_suffix() {
        let v = Version::parse("v3.1.0-beta.1");
        assert_eq!(v, Version { major: 3, minor: 1, patch: 0 });
    }

    #[test]
    fn formats_version_as_string() {
        let v = Version { major: 3, minor: 1, patch: 0 };
        assert_eq!(v.format(), "3.1.0");
    }

    #[test]
    fn calculates_patch_bump_for_fix_commits() {
        let current = Version::parse("v3.0.0");
        let commits = vec!["fix: something".to_string()];
        assert_eq!(calculate_next_version(current, &commits), Version { major: 3, minor: 0, patch: 1 });
    }

    #[test]
    fn calculates_minor_bump_for_feat_commits() {
        let current = Version::parse("v3.0.0");
        let commits = vec!["feat: new feature".to_string()];
        assert_eq!(calculate_next_version(current, &commits), Version { major: 3, minor: 1, patch: 0 });
    }

    #[test]
    fn calculates_major_bump_for_breaking_bang() {
        let current = Version::parse("v3.0.0");
        let commits = vec!["feat!: breaking change".to_string()];
        assert_eq!(calculate_next_version(current, &commits), Version { major: 4, minor: 0, patch: 0 });
    }

    #[test]
    fn calculates_major_bump_for_breaking_change_footer() {
        let current = Version::parse("v3.0.0");
        let commits = vec!["fix: something\n\nBREAKING CHANGE: removed old API".to_string()];
        assert_eq!(calculate_next_version(current, &commits), Version { major: 4, minor: 0, patch: 0 });
    }

    #[test]
    fn highest_bump_wins_across_mixed_commits() {
        let current = Version::parse("v1.0.0");
        let commits = vec![
            "fix: patch thing".to_string(),
            "feat: minor thing".to_string(),
            "chore: no bump".to_string(),
        ];
        assert_eq!(calculate_next_version(current, &commits), Version { major: 1, minor: 1, patch: 0 });
    }

    #[test]
    fn defaults_to_patch_for_non_conventional_commits() {
        let current = Version::parse("v1.0.0");
        let commits = vec!["update readme".to_string()];
        assert_eq!(calculate_next_version(current, &commits), Version { major: 1, minor: 0, patch: 1 });
    }
}
