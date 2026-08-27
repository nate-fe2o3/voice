use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Onboarding,
    Downloading,
    Loading,
    Ready,
    Recording,
    Finalizing,
    PasteRecovery,
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSnapshot {
    pub phase: Phase,
    pub status: String,
    pub preview: String,
    pub input_level: f32,
    pub recording_ms: u64,
    pub model_progress: f32,
    pub model_installed: bool,
    pub model_loaded: bool,
    pub accessibility_trusted: bool,
    pub input_monitoring_trusted: bool,
    pub hotkey_listener_ready: bool,
    pub hotkey_listener_error: Option<String>,
    pub can_retry: bool,
    pub error: Option<String>,
}

impl Default for AppSnapshot {
    fn default() -> Self {
        Self {
            phase: Phase::Onboarding,
            status: "Finish setup to enable dictation".into(),
            preview: String::new(),
            input_level: 0.0,
            recording_ms: 0,
            model_progress: 0.0,
            model_installed: false,
            model_loaded: false,
            accessibility_trusted: false,
            input_monitoring_trusted: false,
            hotkey_listener_ready: false,
            hotkey_listener_error: None,
            can_retry: false,
            error: None,
        }
    }
}
