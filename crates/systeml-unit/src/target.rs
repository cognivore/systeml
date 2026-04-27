//! `[Target]` (and `[Scope]`) sections. Targets are pure dependency
//! aggregators: no execution. Scope units exist only to register externally
//! launched processes.

/// `[Target]` directives. Currently empty — all interesting state is in
/// `[Unit]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TargetUnit;

/// `[Scope]` — also empty for now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ScopeUnit;
