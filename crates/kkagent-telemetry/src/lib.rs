//! Telemetry service + console/file/cloud appenders (kimi-compatible shape).

pub mod cloud;
pub mod privacy;
pub mod service;

pub use cloud::{CloudAppender, CloudAppenderOptions};
pub use service::{
    ConsoleAppender, FileAppender, TelemetryEvent, TelemetryService, TelemetryServiceHandle,
};

// Keep unit tests out of the real ~/.kkagent home.
kkagent_config::install_test_home!();
