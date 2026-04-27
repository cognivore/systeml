//! Test fixtures for the bus crate.
//!
//! Provides a [`fake_manager`] that produces a `Manager` pre-populated with
//! one fake `.service` unit. Used by the integration tests but kept public
//! so downstream crates (e.g. `systemlctl`) can reuse it for their own
//! integration tests against the bus.

use std::sync::Arc;
use systeml_runtime::Manager;
use systeml_unit::{parse_unit_str, LoadedUnit, UnitName};
use tokio::sync::RwLock;

/// Build a `Manager` containing a single `hello.service` unit.
///
/// The unit is parsed from a minimal in-memory source so this never touches
/// the filesystem.
#[must_use]
pub fn fake_manager() -> Arc<RwLock<Manager>> {
    let mut m = Manager::new();
    let name: UnitName = "hello.service".parse().expect("hardcoded name parses");
    let parsed = parse_unit_str(
        name.clone(),
        "[Unit]\nDescription=Hello fixture\n\
         [Service]\nType=simple\nExecStart=/bin/echo hi\n\
         [Install]\nWantedBy=default.target\n",
        None,
    )
    .expect("hardcoded unit source parses");
    let lu = LoadedUnit {
        unit: parsed.unit,
        warnings: parsed.warnings,
        from_template: false,
    };
    m.insert_loaded(name, lu);
    Arc::new(RwLock::new(m))
}
