use modkit_macros::domain_model;
/// Service for the static audit plugin.
///
/// When `enabled` is `false`, all emit methods return `Ok(())` immediately
/// without writing to the log.
#[domain_model]
pub struct Service {
    pub enabled: bool,
}
