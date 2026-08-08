//! Telemetry service + console/file/cloud appenders (kimi-compatible shape).

pub mod privacy;
pub mod cloud;
pub mod service;

pub use service::{
    ConsoleAppender, FileAppender, TelemetryEvent, TelemetryService, TelemetryServiceHandle,
};
pub use cloud::{CloudAppender, CloudAppenderOptions};
