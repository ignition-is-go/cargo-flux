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
            Version {
                major: 0,
                minor: 0,
                patch: 0,
            }
        }
    }

    pub fn format(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }

    /// Whether `s` is a well-formed version to stamp: `MAJOR.MINOR.PATCH`,
    /// optionally with a leading `v` and a `-prerelease` suffix. Unlike
    /// [`parse`](Self::parse), which is lenient and falls back to `0.0.0`, this
    /// is strict — it exists to reject empty or garbage input before it is
    /// written into manifests and a git tag.
    pub fn is_valid(s: &str) -> bool {
        Regex::new(r"^v?\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$")
            .unwrap()
            .is_match(s)
    }
}

/// Compute the next version from a batch of commit subjects, or `None` when no
/// commit warrants a release.
///
/// Follows Conventional Commits v1.0.0 strictly: only `fix` (PATCH) and `feat`
/// (MINOR) correlate to a SemVer bump, and a `!` after the type/scope or a
/// `BREAKING CHANGE` footer on ANY type is a MAJOR bump. Every other type —
/// `docs`, `chore`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, and
/// non-conventional subjects — "has no implicit effect in Semantic Versioning
/// (unless they include a BREAKING CHANGE)", so a batch containing only those
/// releases nothing. That is what `None` means to the caller.
pub fn calculate_next_version(current: Version, commits: &[String]) -> Option<Version> {
    // `!` after the type/scope marks a breaking change on any type.
    let breaking_re = Regex::new(r"^[a-z]+(\([^)]*\))?!:").unwrap();
    let feat_re = Regex::new(r"^feat(\([^)]*\))?:").unwrap();
    let fix_re = Regex::new(r"^fix(\([^)]*\))?:").unwrap();

    let mut bump: Option<Bump> = None;
    for msg in commits {
        let this = if msg.contains("BREAKING CHANGE") || breaking_re.is_match(msg) {
            Some(Bump::Major)
        } else if feat_re.is_match(msg) {
            Some(Bump::Minor)
        } else if fix_re.is_match(msg) {
            Some(Bump::Patch)
        } else {
            None
        };
        // Option<Bump> orders None < Some, Patch < Minor < Major — the highest
        // bump across the batch wins, and stays `None` if nothing qualifies.
        bump = bump.max(this);
    }

    bump.map(|bump| match bump {
        Bump::Major => Version {
            major: current.major + 1,
            minor: 0,
            patch: 0,
        },
        Bump::Minor => Version {
            major: current.major,
            minor: current.minor + 1,
            patch: 0,
        },
        Bump::Patch => Version {
            major: current.major,
            minor: current.minor,
            patch: current.patch + 1,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_with_v_prefix() {
        let v = Version::parse("v3.1.0");
        assert_eq!(
            v,
            Version {
                major: 3,
                minor: 1,
                patch: 0
            }
        );
    }

    #[test]
    fn parses_version_without_prefix() {
        let v = Version::parse("3.1.0");
        assert_eq!(
            v,
            Version {
                major: 3,
                minor: 1,
                patch: 0
            }
        );
    }

    #[test]
    fn parses_version_with_prerelease_suffix() {
        let v = Version::parse("v3.1.0-beta.1");
        assert_eq!(
            v,
            Version {
                major: 3,
                minor: 1,
                patch: 0
            }
        );
    }

    #[test]
    fn formats_version_as_string() {
        let v = Version {
            major: 3,
            minor: 1,
            patch: 0,
        };
        assert_eq!(v.format(), "3.1.0");
    }

    #[test]
    fn calculates_patch_bump_for_fix_commits() {
        let current = Version::parse("v3.0.0");
        let commits = vec!["fix: something".to_string()];
        assert_eq!(
            calculate_next_version(current, &commits),
            Some(Version {
                major: 3,
                minor: 0,
                patch: 1
            })
        );
    }

    #[test]
    fn calculates_minor_bump_for_feat_commits() {
        let current = Version::parse("v3.0.0");
        let commits = vec!["feat: new feature".to_string()];
        assert_eq!(
            calculate_next_version(current, &commits),
            Some(Version {
                major: 3,
                minor: 1,
                patch: 0
            })
        );
    }

    #[test]
    fn calculates_major_bump_for_breaking_bang() {
        let current = Version::parse("v3.0.0");
        let commits = vec!["feat!: breaking change".to_string()];
        assert_eq!(
            calculate_next_version(current, &commits),
            Some(Version {
                major: 4,
                minor: 0,
                patch: 0
            })
        );
    }

    #[test]
    fn calculates_major_bump_for_breaking_change_footer() {
        let current = Version::parse("v3.0.0");
        let commits = vec!["fix: something\n\nBREAKING CHANGE: removed old API".to_string()];
        assert_eq!(
            calculate_next_version(current, &commits),
            Some(Version {
                major: 4,
                minor: 0,
                patch: 0
            })
        );
    }

    /// A breaking `!` on a non-releasing type (e.g. `refactor!`) is still MAJOR —
    /// the spec ties the major bump to the breaking marker, not to the type.
    #[test]
    fn breaking_bang_on_non_releasing_type_is_major() {
        let current = Version::parse("v3.4.5");
        let commits = vec!["refactor!: drop the old trait".to_string()];
        assert_eq!(
            calculate_next_version(current, &commits),
            Some(Version {
                major: 4,
                minor: 0,
                patch: 0
            })
        );
    }

    #[test]
    fn highest_bump_wins_across_mixed_commits() {
        let current = Version::parse("v1.0.0");
        let commits = vec![
            "fix: patch thing".to_string(),
            "feat: minor thing".to_string(),
            "chore: no bump".to_string(),
        ];
        assert_eq!(
            calculate_next_version(current, &commits),
            Some(Version {
                major: 1,
                minor: 1,
                patch: 0
            })
        );
    }

    /// The reason this whole change exists: a batch of only non-releasing types
    /// must release nothing. `docs`, `chore`, `style`, `refactor`, `perf`,
    /// `test`, `build`, `ci` have no implicit SemVer effect per the spec.
    #[test]
    fn non_releasing_types_produce_no_release() {
        let current = Version::parse("v1.2.3");
        for ty in [
            "docs", "chore", "style", "refactor", "perf", "test", "build", "ci",
        ] {
            let commits = vec![format!("{ty}: some change"), format!("{ty}(scope): more")];
            assert_eq!(
                calculate_next_version(current, &commits),
                None,
                "`{ty}:` commits must not trigger a release"
            );
        }
    }

    /// A docs commit alongside a fix still releases — as a PATCH, driven by the
    /// fix, not by the docs.
    #[test]
    fn releasing_commit_alongside_docs_uses_the_releasing_type() {
        let current = Version::parse("v1.2.3");
        let commits = vec!["docs: tidy readme".to_string(), "fix: real bug".to_string()];
        assert_eq!(
            calculate_next_version(current, &commits),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 4
            })
        );
    }

    #[test]
    fn non_conventional_commits_do_not_release() {
        let current = Version::parse("v1.0.0");
        let commits = vec!["update readme".to_string()];
        assert_eq!(calculate_next_version(current, &commits), None);
    }

    #[test]
    fn is_valid_accepts_real_versions_and_rejects_garbage() {
        for ok in ["2.0.1", "v2.0.1", "1.4.0-beta.3", "0.0.0", "10.20.30-rc.1"] {
            assert!(Version::is_valid(ok), "{ok} should be valid");
        }
        for bad in ["", " ", "v", "2.0", "2", "abc", "2.0.x", "-1.0.0", "2.0.1 "] {
            assert!(!Version::is_valid(bad), "{bad:?} should be rejected");
        }
    }
}
