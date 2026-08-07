//! Backend contracts shared by Tyde agent integrations.
//!
//! This crate owns the interface between the host and agent backends. Concrete
//! backend implementations and Tyde server orchestration live outside it.

use std::collections::BTreeSet;

mod certification;
mod conformance;

pub use certification::{CertificationCase, CertificationTier};
pub use conformance::{BackendConformanceError, BackendConformanceValidator, ConformanceSnapshot};

/// A normalized adapter observation consumed by live conformance tests.
#[derive(Debug, Clone)]
pub enum BackendObservation {
    Chat(protocol::ChatEvent),
    ModelRequestTokenUsage(protocol::ModelRequestTokenUsage),
    Other,
}

/// A behavior that a live backend session promises to support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BackendCapability {
    ListSessions,
    ResumeSession,
    ForkSession,
    ImageInput,
    Interrupt,
    SessionSettings,
    StartupMcpServers,
    TurnUsageReported,
    ModelRequestUsageReported,
    ContextUsageReported,
    ContextBreakdownReported,
    Subagents,
    BackgroundTasks,
    AgentInitiatedTurns,
    MidTurnSteering,
    WorkspaceInstructions,
    Customization,
}

impl BackendCapability {
    pub const ALL: [Self; 17] = [
        Self::ListSessions,
        Self::ResumeSession,
        Self::ForkSession,
        Self::ImageInput,
        Self::Interrupt,
        Self::SessionSettings,
        Self::StartupMcpServers,
        Self::TurnUsageReported,
        Self::ModelRequestUsageReported,
        Self::ContextUsageReported,
        Self::ContextBreakdownReported,
        Self::Subagents,
        Self::BackgroundTasks,
        Self::AgentInitiatedTurns,
        Self::MidTurnSteering,
        Self::WorkspaceInstructions,
        Self::Customization,
    ];
}

/// The capabilities declared by a live backend session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackendCapabilities {
    capabilities: BTreeSet<BackendCapability>,
}

impl BackendCapabilities {
    pub fn new(capabilities: impl IntoIterator<Item = BackendCapability>) -> Self {
        Self {
            capabilities: capabilities.into_iter().collect(),
        }
    }

    pub fn contains(&self, capability: BackendCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn len(&self) -> usize {
        self.capabilities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = BackendCapability> + '_ {
        self.capabilities.iter().copied()
    }

    pub fn validate(&self) -> Result<(), CapabilityDeclarationError> {
        self.require(
            BackendCapability::ContextBreakdownReported,
            BackendCapability::ContextUsageReported,
        )?;
        self.require(
            BackendCapability::ModelRequestUsageReported,
            BackendCapability::TurnUsageReported,
        )?;
        Ok(())
    }

    fn require(
        &self,
        capability: BackendCapability,
        required: BackendCapability,
    ) -> Result<(), CapabilityDeclarationError> {
        if self.contains(capability) && !self.contains(required) {
            return Err(CapabilityDeclarationError {
                capability,
                required,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityDeclarationError {
    pub capability: BackendCapability,
    pub required: BackendCapability,
}

impl std::fmt::Display for CapabilityDeclarationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "capability {:?} requires {:?}",
            self.capability, self.required
        )
    }
}

impl std::error::Error for CapabilityDeclarationError {}

impl<const N: usize> From<[BackendCapability; N]> for BackendCapabilities {
    fn from(capabilities: [BackendCapability; N]) -> Self {
        Self::new(capabilities)
    }
}

#[cfg(test)]
mod tests {
    use super::{BackendCapabilities, BackendCapability};

    #[test]
    fn capabilities_are_unique_and_queryable() {
        let capabilities = BackendCapabilities::new([
            BackendCapability::ResumeSession,
            BackendCapability::ContextUsageReported,
            BackendCapability::ResumeSession,
        ]);

        assert!(capabilities.contains(BackendCapability::ResumeSession));
        assert!(capabilities.contains(BackendCapability::ContextUsageReported));
        assert!(!capabilities.contains(BackendCapability::ModelRequestUsageReported));
        assert_eq!(capabilities.iter().count(), 2);
        assert_eq!(capabilities.len(), 2);
        assert!(!capabilities.is_empty());
    }

    #[test]
    fn reported_context_breakdown_requires_reported_context_usage() {
        let capabilities = BackendCapabilities::from([BackendCapability::ContextBreakdownReported]);

        let error = capabilities.validate().expect_err("invalid declaration");
        assert_eq!(
            error.capability,
            BackendCapability::ContextBreakdownReported
        );
        assert_eq!(error.required, BackendCapability::ContextUsageReported);
    }

    #[test]
    fn model_request_usage_requires_turn_usage() {
        let capabilities =
            BackendCapabilities::from([BackendCapability::ModelRequestUsageReported]);

        let error = capabilities.validate().expect_err("invalid declaration");
        assert_eq!(
            error.capability,
            BackendCapability::ModelRequestUsageReported
        );
        assert_eq!(error.required, BackendCapability::TurnUsageReported);
    }
}
