//! `[Unit] Condition*=` and `Assert*=` parsing & evaluation.
//!
//! Conditions cause a unit to be skipped (no error) when not met; asserts
//! cause hard failure. The split is purely about reporting — parsing is the
//! same.

use std::path::PathBuf;

/// One condition. Negation is folded into the `negate` field.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Condition {
    /// Kind / argument pair.
    pub check: ConditionCheck,
    /// `!`-prefix: invert the result.
    pub negate: bool,
    /// `|`-prefix: trigger-only; if set, the unit needs at least one such
    /// `|`-prefixed condition to be true (logical OR among triggers).
    pub trigger: bool,
}

/// All supported condition kinds. Linux-specific ones (KernelCommandLine,
/// SecurityHaveCapability, …) are recognised so they parse, but evaluation
/// returns a fixed result with a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ConditionCheck {
    /// `ConditionPathExists=`
    PathExists(PathBuf),
    /// `ConditionPathExistsGlob=`
    PathExistsGlob(String),
    /// `ConditionPathIsDirectory=`
    PathIsDirectory(PathBuf),
    /// `ConditionPathIsSymbolicLink=`
    PathIsSymbolicLink(PathBuf),
    /// `ConditionPathIsMountPoint=`
    PathIsMountPoint(PathBuf),
    /// `ConditionPathIsReadWrite=`
    PathIsReadWrite(PathBuf),
    /// `ConditionPathIsEncrypted=` (Linux-specific; always false on macOS).
    PathIsEncrypted(PathBuf),
    /// `ConditionDirectoryNotEmpty=`
    DirectoryNotEmpty(PathBuf),
    /// `ConditionFileNotEmpty=`
    FileNotEmpty(PathBuf),
    /// `ConditionFileIsExecutable=`
    FileIsExecutable(PathBuf),
    /// `ConditionUser=`
    User(String),
    /// `ConditionGroup=`
    Group(String),
    /// `ConditionHost=`
    Host(String),
    /// `ConditionArchitecture=`
    Architecture(String),
    /// `ConditionVirtualization=` (always "none" on macOS host).
    Virtualization(String),
    /// `ConditionEnvironment=`
    Environment(String),
    /// `ConditionFirstBoot=` (always false in our world).
    FirstBoot(bool),
    /// `ConditionKernelCommandLine=` (Linux-only; always false).
    KernelCommandLine(String),
    /// `ConditionKernelVersion=` (Linux-only; always false).
    KernelVersion(String),
    /// `ConditionACPower=` — hooks into IOPSCopyPowerSourcesInfo on macOS.
    AcPower(bool),
    /// `ConditionNeedsUpdate=` (always false).
    NeedsUpdate(String),
    /// `ConditionMemory=` — `>=N` style.
    Memory(String),
    /// `ConditionCPUs=`
    Cpus(String),
    /// `ConditionControlGroupController=` (Linux-only; always false).
    ControlGroupController(String),
    /// `ConditionSecurity=` (Linux-only; always false).
    Security(String),
    /// `ConditionCapability=` (Linux-only; always false).
    Capability(String),
    /// Unknown directive — preserved for round-tripping.
    Unknown {
        /// Original key with `Condition`/`Assert` prefix stripped.
        key: String,
        /// Raw value.
        value: String,
    },
}

impl Condition {
    /// Parse one `ConditionFoo=` or `AssertFoo=` value.
    ///
    /// `key` should be the directive key with the leading `Condition` or
    /// `Assert` already stripped (e.g. `PathExists`).
    ///
    /// Errors on malformed boolean values (e.g.
    /// `ConditionFirstBoot=maybe`). Unknown condition keys are *not* an
    /// error — they round-trip as `ConditionCheck::Unknown` for forward
    /// compatibility, matching upstream systemd's tolerance for newer
    /// conditions on older parsers.
    pub fn parse(key: &str, value: &str) -> Result<Self, String> {
        let mut neg = false;
        let mut trig = false;
        let mut v = value.trim();
        loop {
            if let Some(r) = v.strip_prefix('|') {
                trig = true;
                v = r.trim();
            } else if let Some(r) = v.strip_prefix('!') {
                neg = !neg;
                v = r.trim();
            } else {
                break;
            }
        }
        let check = match key {
            "PathExists" => ConditionCheck::PathExists(PathBuf::from(v)),
            "PathExistsGlob" => ConditionCheck::PathExistsGlob(v.into()),
            "PathIsDirectory" => ConditionCheck::PathIsDirectory(PathBuf::from(v)),
            "PathIsSymbolicLink" => ConditionCheck::PathIsSymbolicLink(PathBuf::from(v)),
            "PathIsMountPoint" => ConditionCheck::PathIsMountPoint(PathBuf::from(v)),
            "PathIsReadWrite" => ConditionCheck::PathIsReadWrite(PathBuf::from(v)),
            "PathIsEncrypted" => ConditionCheck::PathIsEncrypted(PathBuf::from(v)),
            "DirectoryNotEmpty" => ConditionCheck::DirectoryNotEmpty(PathBuf::from(v)),
            "FileNotEmpty" => ConditionCheck::FileNotEmpty(PathBuf::from(v)),
            "FileIsExecutable" => ConditionCheck::FileIsExecutable(PathBuf::from(v)),
            "User" => ConditionCheck::User(v.into()),
            "Group" => ConditionCheck::Group(v.into()),
            "Host" => ConditionCheck::Host(v.into()),
            "Architecture" => ConditionCheck::Architecture(v.into()),
            "Virtualization" => ConditionCheck::Virtualization(v.into()),
            "Environment" => ConditionCheck::Environment(v.into()),
            "FirstBoot" => ConditionCheck::FirstBoot(
                parse_bool(v)
                    .ok_or_else(|| format!("ConditionFirstBoot expects a boolean, got {v:?}"))?,
            ),
            "KernelCommandLine" => ConditionCheck::KernelCommandLine(v.into()),
            "KernelVersion" => ConditionCheck::KernelVersion(v.into()),
            "ACPower" => ConditionCheck::AcPower(
                parse_bool(v)
                    .ok_or_else(|| format!("ConditionACPower expects a boolean, got {v:?}"))?,
            ),
            "NeedsUpdate" => ConditionCheck::NeedsUpdate(v.into()),
            "Memory" => ConditionCheck::Memory(v.into()),
            "CPUs" => ConditionCheck::Cpus(v.into()),
            "ControlGroupController" => ConditionCheck::ControlGroupController(v.into()),
            "Security" => ConditionCheck::Security(v.into()),
            "Capability" => ConditionCheck::Capability(v.into()),
            other => ConditionCheck::Unknown {
                key: other.to_owned(),
                value: v.to_owned(),
            },
        };
        Ok(Self {
            check,
            negate: neg,
            trigger: trig,
        })
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    Some(match s.to_ascii_lowercase().as_str() {
        "1" | "yes" | "true" | "on" => true,
        "0" | "no" | "false" | "off" => false,
        _ => return None,
    })
}

/// All conditions/asserts on a unit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Conditions {
    /// `ConditionXxx=` — skip the unit if not satisfied.
    pub conditions: Vec<Condition>,
    /// `AssertXxx=` — fail the unit if not satisfied.
    pub asserts: Vec<Condition>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_negate() {
        let c = Condition::parse("PathExists", "!/etc/foo").unwrap();
        assert!(c.negate);
        assert!(matches!(c.check, ConditionCheck::PathExists(p) if p.to_str() == Some("/etc/foo")));
    }

    #[test]
    fn parse_trigger() {
        let c = Condition::parse("PathExists", "|/etc/foo").unwrap();
        assert!(c.trigger);
        assert!(!c.negate);
    }

    #[test]
    fn parse_trigger_and_negate() {
        let c = Condition::parse("PathExists", "|!/etc/foo").unwrap();
        assert!(c.trigger);
        assert!(c.negate);
    }

    #[test]
    fn unknown_preserved() {
        let c = Condition::parse("FooBarBaz", "wat").unwrap();
        assert!(matches!(c.check, ConditionCheck::Unknown { .. }));
    }

    #[test]
    fn malformed_bool_errors() {
        assert!(Condition::parse("FirstBoot", "maybe").is_err());
        assert!(Condition::parse("ACPower", "kinda").is_err());
    }
}
