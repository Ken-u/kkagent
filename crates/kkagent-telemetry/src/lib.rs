//! Telemetry service + console/file/cloud appenders (kimi-compatible shape).

pub mod cloud;
pub mod privacy;
pub mod service;

pub use cloud::{CloudAppender, CloudAppenderOptions};
pub use service::{
    ConsoleAppender, FileAppender, TelemetryEvent, TelemetryService, TelemetryServiceHandle,
};
