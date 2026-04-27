//! Locate and load unit files by name across the search path.
//!
//! For a name like `foo.service`:
//! 1. Walk `user_search_paths()` looking for `foo.service` (case-sensitive).
//!    First hit becomes the main fragment.
//! 2. For each search path, look for `foo.service.d/*.conf` and stack them
//!    on top in alphabetical order.
//! 3. For instance names `foo@bar.service`, fall back to the template
//!    `foo@.service` if no instance-specific file exists.
//!
//! Drop-in handling is "name-aware": template `foo@.service.d/*.conf` are
//! also applied to instance `foo@bar.service`.

use crate::error::{ParseError, ParseWarning};
use crate::ini::parse as parse_ini;
use crate::name::UnitName;
use crate::search;
use crate::spec::{parse_unit_str, Unit, UnitTypeData};
use std::path::{Path, PathBuf};

/// Outcome of `load_unit`. Holds a fully-resolved `Unit` plus warnings from
/// parsing every fragment.
pub struct LoadedUnit {
    /// Resolved unit (main fragment + drop-ins applied).
    pub unit: Unit,
    /// Warnings collected from every fragment.
    pub warnings: Vec<ParseWarning>,
    /// True if the unit's main fragment came from a template (`foo@.service`).
    pub from_template: bool,
}

/// Load a unit by name from the standard user search paths.
pub fn load_unit(name: &UnitName) -> Result<LoadedUnit, ParseError> {
    load_unit_in(name, &search::user_search_paths())
}

/// Load a unit by name, searching the given list of base directories.
pub fn load_unit_in(
    name: &UnitName,
    base_dirs: &[PathBuf],
) -> Result<LoadedUnit, ParseError> {
    let mut warnings = Vec::new();
    let (main_path, from_template) = locate_main(name, base_dirs)?;
    let parsed = crate::spec::parse_unit_file(&main_path)?;
    warnings.extend(parsed.warnings);
    let mut unit = parsed.unit;
    unit.fragment_paths.clear();
    unit.fragment_paths.push(main_path.clone());

    // Apply drop-ins from every base dir (in priority order; later overrides
    // earlier). Both the literal name's `.d/` and (for instances) the
    // template `.d/` are checked.
    for base in base_dirs {
        for d in candidate_dropin_dirs(name, base) {
            apply_dropins(&d, &mut unit, &mut warnings)?;
        }
    }

    Ok(LoadedUnit {
        unit,
        warnings,
        from_template,
    })
}

fn locate_main(
    name: &UnitName,
    base_dirs: &[PathBuf],
) -> Result<(PathBuf, bool), ParseError> {
    let want = name.filename();
    for base in base_dirs {
        let candidate = base.join(&want);
        if candidate.is_file() {
            return Ok((candidate, false));
        }
    }
    // Instance fallback to template.
    if let Some(tpl) = name.template_form() {
        let want = tpl.filename();
        for base in base_dirs {
            let candidate = base.join(&want);
            if candidate.is_file() {
                return Ok((candidate, true));
            }
        }
    }
    Err(ParseError::Io {
        path: PathBuf::from(name.filename()),
        source: std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "unit file not found in any search path",
        ),
    })
}

fn candidate_dropin_dirs(name: &UnitName, base: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    v.push(base.join(format!("{}.d", name.filename())));
    if let Some(tpl) = name.template_form() {
        v.push(base.join(format!("{}.d", tpl.filename())));
    }
    // Type-wide drop-in dirs (.service.d/, .socket.d/) — systemd uses the
    // exact-suffix variant only, but we accept both for resilience.
    v.push(base.join(format!("{}.d", name.kind.suffix().trim_start_matches('.'))));
    v
}

fn apply_dropins(
    dir: &Path,
    unit: &mut Unit,
    warnings: &mut Vec<ParseWarning>,
) -> Result<(), ParseError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("conf"))
        .collect();
    paths.sort();
    for path in paths {
        let source = std::fs::read_to_string(&path).map_err(|e| ParseError::Io {
            path: path.clone(),
            source: e,
        })?;
        // Drop-ins are layered on top via re-parsing into the same unit's raw
        // ini. We lay each drop-in's sections under the existing ones.
        let added = parse_ini(&path, &source)?;
        for (section, entries) in added.sections {
            unit.raw
                .sections
                .entry(section)
                .or_default()
                .extend(entries);
        }
        unit.fragment_paths.push(path);
    }
    // Re-derive typed view from updated raw ini.
    let merged_source = render_ini(&unit.raw);
    let reparsed = parse_unit_str(unit.name.clone(), &merged_source, None)?;
    warnings.extend(reparsed.warnings);
    let saved_paths = std::mem::take(&mut unit.fragment_paths);
    *unit = reparsed.unit;
    unit.fragment_paths = saved_paths;
    Ok(())
}

fn render_ini(ini: &crate::ini::Ini) -> String {
    let mut s = String::new();
    for (section, entries) in &ini.sections {
        s.push('[');
        s.push_str(section);
        s.push_str("]\n");
        for e in entries {
            s.push_str(&e.key);
            s.push('=');
            s.push_str(&e.value);
            s.push('\n');
        }
        s.push('\n');
    }
    s
}

/// Convenience: walk every search path and list every unit-file basename.
#[must_use]
pub fn list_unit_files(base_dirs: &[PathBuf]) -> Vec<(UnitName, PathBuf)> {
    let mut out = Vec::new();
    for base in base_dirs {
        let entries = match std::fs::read_dir(base) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_file() {
                if let Ok(name) = UnitName::from_path(&p) {
                    out.push((name, p));
                }
            }
        }
    }
    out
}

/// True if the unit's per-type data is something we know how to activate.
#[must_use]
pub fn is_activatable(unit: &Unit) -> bool {
    matches!(
        unit.kind,
        UnitTypeData::Service(_)
            | UnitTypeData::Socket(_)
            | UnitTypeData::Timer(_)
            | UnitTypeData::Path(_)
            | UnitTypeData::Target(_)
            | UnitTypeData::Scope(_)
    )
}
