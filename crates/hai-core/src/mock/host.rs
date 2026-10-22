//! `HostBackend` mock: a generous fake host and an always-ready Home Assistant.

use crate::types::{HaSetupStatus, SystemInfo};
use crate::{HostBackend, Result};

use super::BackendMock;

impl HostBackend for BackendMock {
    fn system_info(&self) -> Result<SystemInfo> {
        Ok(SystemInfo {
            cpu_cores: 10,
            memory_mb: 32768,
        })
    }

    async fn check_ha_ready(&self, _ip: &str) -> bool {
        true
    }

    async fn check_ha_updated(&self, _ip: &str) -> bool {
        true
    }

    async fn ha_setup_status(&self, _ip: &str) -> Option<HaSetupStatus> {
        // Home Assistant is always ready, so its landing page is gone
        None
    }
}
