//! The operating system family, as data. Drivers state which ones they
//! support; selection checks it at run time instead of with `#[cfg]`.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Operating system family of the machine that runs the Runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostOs {
    /// Linux.
    Linux,
    /// macOS.
    MacOs,
    /// Any other system.
    Other,
}

impl HostOs {
    /// The system this binary was built for. This is the only compile-time
    /// platform check that driver selection needs.
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Other
        }
    }

    /// Name for people, as used in error texts.
    pub fn name(self) -> &'static str {
        match self {
            Self::Linux => "Linux",
            Self::MacOs => "macOS",
            Self::Other => "this platform",
        }
    }
}

impl fmt::Display for HostOs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
