use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, Position};

use crate::audio::{AudioEvent, AudioRecorder};
use crate::clipboard::ClipboardTransaction;
use crate::macos::{self, FocusedTarget};
use crate::model::{
    self, ModelCommand, ModelEvent, ModelService,
};
use crate::settings::{settings_path, Settings};
use crate::state::{AppSnapshot, Phase};
use serde::Serialize;

const MINIMUM_HOLD: Duration = Duration::from_millis(250);
const MAXIMUM_RECORDING: Duration = Duration::from_secs(5 * 60);
const WARNING_TIME: Duration = Duration::from_secs(4 * 60 + 30);
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(5 * 60);

struct RecordingSession {
    id: u64,
    started: Instant,
    recorder: AudioRecorder,
    destination: RecordingDestination,
}

enum RecordingDestination {
    Dictation(FocusedTarget),
    SetupTest,
}

struct PasteRecovery {
    id: u64,
    transcript: String,
    target: FocusedTarget,
    expires: Instant,
}

trait RetryFocus {
    fn is_still_focused(&self) -> bool;
    fn restore_focus(&self) -> Result<()>;
}

impl RetryFocus for FocusedTarget {
    fn is_still_focused(&self) -> bool {
        FocusedTarget::is_still_focused(self)
    }

    fn restore_focus(&self) -> Result<()> {
        FocusedTarget::restore_focus(self)
    }
}

fn prepare_retry_focus(target: &impl RetryFocus) -> Result<()> {
    if target.is_still_focused() {
        return Ok(());
    }
    target
        .restore_focus()
        .context("return focus to the original text field")?;
    if !target.is_still_focused() {
        anyhow::bail!("the original text field did not regain focus");
    }
    Ok(())
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetupTestResult {
    transcript: String,
    status: String,
}

pub struct Controller {
    app: AppHandle,
    app_data_dir: PathBuf,
    settings_path: PathBuf,
    settings: Mutex<Settings>,
    snapshot: Mutex<AppSnapshot>,
    model: ModelService,
    model_events: SyncSender<ModelEvent>,
    audio_events: SyncSender<AudioEvent>,
    recording: Mutex<Option<RecordingSession>>,
    pending_destination: Mutex<Option<RecordingDestination>>,
    recovery: Mutex<Option<PasteRecovery>>,
    download_cancel: Mutex<Option<Arc<AtomicBool>>>,
    pointer: Mutex<(f64, f64)>,
    sequence: AtomicU64,
}

impl Controller {
    pub fn new(app: AppHandle) -> Result<Arc<Self>> {
        let app_data_dir = app
            .path()
            .app_data_dir()
            .context("resolve Application Support directory")?;
        let settings_path = settings_path(&app_data_dir);
        let settings = Settings::load(&settings_path).unwrap_or_default();
        let installed = model::model_is_installed(&app_data_dir);
        let accessibility_trusted = macos::accessibility_trusted();
        let input_monitoring_trusted = macos::input_monitoring_trusted();
        if !accessibility_trusted {
            let _ = macos::request_accessibility();
        }
        if !input_monitoring_trusted {
            let _ = macos::request_input_monitoring();
        }
        let (model_event_sender, model_event_receiver) = mpsc::sync_channel(128);
        let (audio_event_sender, audio_event_receiver) = mpsc::sync_channel(128);
        let model_service = ModelService::spawn(model_event_sender.clone());
        let mut snapshot = AppSnapshot {
            model_installed: installed,
            accessibility_trusted,
            input_monitoring_trusted,
            ..AppSnapshot::default()
        };
        if installed {
            snapshot.phase = Phase::Loading;
            snapshot.status = "Loading Qwen3-ASR…".into();
        } else if settings.onboarding_complete {
            snapshot.status = "Download the speech model to enable dictation".into();
        }

        let controller = Arc::new(Self {
            app,
            app_data_dir,
            settings_path,
            settings: Mutex::new(settings),
            snapshot: Mutex::new(snapshot),
            model: model_service,
            model_events: model_event_sender,
            audio_events: audio_event_sender,
            recording: Mutex::new(None),
            pending_destination: Mutex::new(None),
            recovery: Mutex::new(None),
            download_cancel: Mutex::new(None),
            pointer: Mutex::new((0.0, 0.0)),
            sequence: AtomicU64::new(1),
        });

        {
            let weak = Arc::downgrade(&controller);
            std::thread::Builder::new()
                .name("voxtype-model-events".into())
                .spawn(move || {
                    while let Ok(event) = model_event_receiver.recv() {
                        let Some(controller) = weak.upgrade() else {
                            break;
                        };
                        controller.handle_model_event(event);
                    }
                })?;
        }
        {
            let weak = Arc::downgrade(&controller);
            std::thread::Builder::new()
                .name("voxtype-audio-events".into())
                .spawn(move || {
                    while let Ok(event) = audio_event_receiver.recv() {
                        let Some(controller) = weak.upgrade() else {
                            break;
                        };
                        controller.handle_audio_event(event);
                    }
                })?;
        }
        if installed {
            controller
                .model
                .send(ModelCommand::Load(model::model_dir(&controller.app_data_dir)))?;
        }
        controller.emit_snapshot();
        Ok(controller)
    }

    pub fn snapshot(&self) -> AppSnapshot {
        self.snapshot.lock().expect("snapshot lock").clone()
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().expect("settings lock").clone()
    }

    pub fn save_settings(&self, mut settings: Settings) -> Result<()> {
        settings.normalize()?;
        settings.save(&self.settings_path)?;
        *self.settings.lock().expect("settings lock") = settings;
        self.refresh_permissions();
        Ok(())
    }

    pub fn complete_onboarding(&self) -> Result<()> {
        let mut settings = self.settings();
        settings.onboarding_complete = true;
        let ready_status = format!("Ready — hold {} to dictate", settings.hotkey.display_name());
        self.save_settings(settings)?;
        let installed = model::model_is_installed(&self.app_data_dir);
        self.mutate_snapshot(|snapshot| {
            snapshot.model_installed = installed;
            if snapshot.model_loaded
                && snapshot.accessibility_trusted
                && snapshot.input_monitoring_trusted
            {
                snapshot.phase = Phase::Ready;
                snapshot.status = ready_status;
            }
        });
        Ok(())
    }

    pub fn refresh_permissions(&self) {
        let trusted = macos::accessibility_trusted();
        let input_monitoring_trusted = macos::input_monitoring_trusted();
        let settings = self.settings();
        let ready_status = format!("Ready — hold {} to dictate", settings.hotkey.display_name());
        self.mutate_snapshot(|snapshot| {
            snapshot.accessibility_trusted = trusted;
            snapshot.input_monitoring_trusted = input_monitoring_trusted;
            if trusted
                && input_monitoring_trusted
                && snapshot.model_loaded
                && settings.onboarding_complete
            {
                snapshot.phase = Phase::Ready;
                snapshot.status = ready_status;
                snapshot.error = None;
            } else if (!trusted || !input_monitoring_trusted)
                && snapshot.phase == Phase::Ready
            {
                snapshot.phase = Phase::Onboarding;
                snapshot.status = if !trusted {
                    "Accessibility permission is required"
                } else {
                    "Input Monitoring permission is required"
                }
                .into();
            }
        });
    }

    pub fn set_hotkey_listener_status(&self, ready: bool, error: Option<String>) {
        self.mutate_snapshot(|snapshot| {
            snapshot.hotkey_listener_ready = ready;
            snapshot.hotkey_listener_error = error;
        });
    }

    pub fn begin_recording(self: &Arc<Self>) {
        let phase = self.snapshot().phase;
        if !can_begin_recording(&self.snapshot(), false) {
            if phase == Phase::Loading {
                self.show_overlay_with_status("Model warming up…");
            } else if matches!(phase, Phase::Finalizing | Phase::PasteRecovery | Phase::Recording) {
                self.show_overlay_with_status("VoxType is busy");
            } else {
                self.show_overlay_with_status(&self.snapshot().status);
            }
            return;
        }
        let settings = self.settings();
        let target = match FocusedTarget::capture(&settings.excluded_apps) {
            Ok(target) => target,
            Err(error) => {
                self.transient_error(error.to_string());
                return;
            }
        };
        if let Err(error) = self.start_recording(
            &settings,
            RecordingDestination::Dictation(target),
            true,
        ) {
            self.transient_error(error);
        }
    }

    pub fn begin_setup_test(self: &Arc<Self>) -> Result<()> {
        let snapshot = self.snapshot();
        if !can_begin_recording(&snapshot, true) {
            anyhow::bail!(
                "The speech model must be loaded and VoxType must be idle before testing"
            );
        }
        let settings = self.settings();
        self.start_recording(&settings, RecordingDestination::SetupTest, false)
            .map_err(anyhow::Error::msg)
    }

    fn start_recording(
        self: &Arc<Self>,
        settings: &Settings,
        destination: RecordingDestination,
        show_overlay: bool,
    ) -> std::result::Result<(), String> {
        if let Err(error) = self.model.send(ModelCommand::Start {
            language: (settings.language == "english").then(|| "english".into()),
            initial_text: settings.initial_context(),
        }) {
            return Err(error.to_string());
        }
        let recorder = match AudioRecorder::start(
            settings.microphone.as_deref(),
            self.model.command_sender(),
            self.audio_events.clone(),
        ) {
            Ok(recorder) => recorder,
            Err(error) => {
                let _ = self.model.send(ModelCommand::Cancel);
                return Err(format!("{error:#}"));
            }
        };
        let id = self.sequence.fetch_add(1, Ordering::Relaxed);
        *self.recording.lock().expect("recording lock") = Some(RecordingSession {
            id,
            started: Instant::now(),
            recorder,
            destination,
        });
        self.mutate_snapshot(|snapshot| {
            snapshot.phase = Phase::Recording;
            snapshot.status = "Listening…".into();
            snapshot.preview.clear();
            snapshot.recording_ms = 0;
            snapshot.input_level = 0.0;
            snapshot.error = None;
        });
        if show_overlay {
            self.show_overlay();
        }
        self.schedule_recording_limits(id);
        Ok(())
    }

    pub fn finish_recording(self: &Arc<Self>) {
        let session = self.recording.lock().expect("recording lock").take();
        let Some(session) = session else {
            return;
        };
        let elapsed = session.started.elapsed();
        let voiced = session.recorder.stop();
        if elapsed < MINIMUM_HOLD {
            let _ = self.model.send(ModelCommand::Cancel);
            let status = "Hold the button while speaking, then release";
            self.emit_empty_setup_test(&session.destination, status);
            self.return_ready(status);
            return;
        }
        if !voiced {
            let _ = self.model.send(ModelCommand::Cancel);
            let status = "No speech detected. Hold the button and try again.";
            self.emit_empty_setup_test(&session.destination, status);
            self.return_ready(status);
            return;
        }
        *self
            .pending_destination
            .lock()
            .expect("destination lock") = Some(session.destination);
        self.mutate_snapshot(|snapshot| {
            snapshot.phase = Phase::Finalizing;
            snapshot.status = "Finalizing…".into();
            snapshot.input_level = 0.0;
        });
        if let Err(error) = self.model.send(ModelCommand::Finish) {
            self.enter_recovery_or_error(None, error.to_string());
        }
    }

    pub fn cancel_recording(&self) {
        let session = self.recording.lock().expect("recording lock").take();
        if let Some(session) = session {
            session.recorder.stop();
            let _ = self.model.send(ModelCommand::Cancel);
            self.return_ready("Canceled");
        }
    }

    pub fn retry_paste(self: &Arc<Self>) -> Result<()> {
        let recovery = self.recovery.lock().expect("recovery lock").take();
        let Some(recovery) = recovery else {
            anyhow::bail!("there is no transcript awaiting retry");
        };
        if recovery.expires <= Instant::now() {
            self.return_ready("Transcript discarded");
            anyhow::bail!("the recovery window expired");
        }
        self.hide_overlay();
        if let Err(error) = prepare_retry_focus(&recovery.target) {
            *self.recovery.lock().expect("recovery lock") = Some(recovery);
            let message = format!("{error:#}");
            self.mutate_snapshot(|snapshot| {
                snapshot.status = "Could not return to the original text field".into();
                snapshot.error = Some(message);
            });
            self.show_overlay();
            return Err(error);
        }
        let transcript = recovery.transcript;
        let target = recovery.target;
        if let Err(error) = self.paste_transcript(transcript.clone(), target.clone()) {
            self.enter_recovery(transcript, target, &format!("{error:#}"));
            return Err(error);
        }
        Ok(())
    }

    pub fn discard_recovery(&self) {
        if self.recovery.lock().expect("recovery lock").take().is_some() {
            self.return_ready("Transcript discarded");
        }
    }

    pub fn start_download(self: &Arc<Self>) -> Result<()> {
        if model::model_is_installed(&self.app_data_dir) {
            anyhow::bail!("the model is already installed");
        }
        let mut cancel_guard = self.download_cancel.lock().expect("download lock");
        if cancel_guard.is_some() {
            anyhow::bail!("a model download is already running");
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        *cancel_guard = Some(cancelled.clone());
        drop(cancel_guard);
        self.mutate_snapshot(|snapshot| {
            snapshot.phase = Phase::Downloading;
            snapshot.status = "Downloading Qwen3-ASR…".into();
            snapshot.error = None;
            snapshot.model_progress = 0.0;
        });
        let app_data = self.app_data_dir.clone();
        let events = self.model_events.clone();
        let weak = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("voxtype-model-download".into())
            .spawn(move || {
                if let Err(error) = model::download_model(&app_data, &events, cancelled) {
                    let _ = events.send(ModelEvent::Error(format!("{error:#}")));
                }
                if let Some(controller) = weak.upgrade() {
                    controller
                        .download_cancel
                        .lock()
                        .expect("download lock")
                        .take();
                }
            })?;
        Ok(())
    }

    pub fn cancel_download(&self) {
        if let Some(cancelled) = self.download_cancel.lock().expect("download lock").as_ref() {
            cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub fn load_model(&self) -> Result<()> {
        if !model::model_is_installed(&self.app_data_dir) {
            anyhow::bail!("download the model first");
        }
        self.model
            .send(ModelCommand::Load(model::model_dir(&self.app_data_dir)))
    }

    pub fn unload_model(&self) -> Result<()> {
        if matches!(
            self.snapshot().phase,
            Phase::Recording | Phase::Finalizing | Phase::PasteRecovery
        ) {
            anyhow::bail!("finish dictation before unloading the model");
        }
        self.model.send(ModelCommand::Unload)
    }

    pub fn remove_model(&self) -> Result<()> {
        self.unload_model()?;
        model::remove_model(&self.app_data_dir)?;
        self.mutate_snapshot(|snapshot| {
            snapshot.model_installed = false;
            snapshot.model_loaded = false;
            snapshot.phase = Phase::Onboarding;
            snapshot.status = "Speech model removed".into();
            snapshot.model_progress = 0.0;
        });
        Ok(())
    }

    pub fn try_update_pointer(&self, x: f64, y: f64) {
        if let Ok(mut pointer) = self.pointer.try_lock() {
            *pointer = (x, y);
        }
    }

    pub fn start_privacy_watchdog(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("voxtype-privacy-watchdog".into())
            .spawn(move || {
                let mut last_check = SystemTime::now();
                let mut was_trusted = macos::accessibility_trusted();
                let mut was_input_monitoring_trusted = macos::input_monitoring_trusted();
                loop {
                    std::thread::sleep(Duration::from_secs(2));
                    let Some(controller) = weak.upgrade() else {
                        break;
                    };
                    let now = SystemTime::now();
                    let slept = now
                        .duration_since(last_check)
                        .is_ok_and(|elapsed| elapsed > Duration::from_secs(10));
                    let trusted = macos::accessibility_trusted();
                    let input_monitoring_trusted = macos::input_monitoring_trusted();
                    if slept
                        || trusted != was_trusted
                        || input_monitoring_trusted != was_input_monitoring_trusted
                    {
                        controller.cancel_sensitive_work("Dictation cleared after system change");
                        controller.refresh_permissions();
                    }
                    last_check = now;
                    was_trusted = trusted;
                    was_input_monitoring_trusted = input_monitoring_trusted;
                }
            })
            .expect("spawn privacy watchdog");
    }

    fn cancel_sensitive_work(&self, status: &str) {
        if let Some(session) = self.recording.lock().expect("recording lock").take() {
            session.recorder.stop();
        }
        let _ = self.model.send(ModelCommand::Cancel);
        self.pending_destination
            .lock()
            .expect("destination lock")
            .take();
        self.recovery.lock().expect("recovery lock").take();
        if matches!(
            self.snapshot().phase,
            Phase::Recording | Phase::Finalizing | Phase::PasteRecovery
        ) {
            self.return_ready(status);
        }
    }

    fn handle_model_event(self: &Arc<Self>, event: ModelEvent) {
        match event {
            ModelEvent::Loading => self.mutate_snapshot(|snapshot| {
                snapshot.phase = Phase::Loading;
                snapshot.status = "Loading Qwen3-ASR…".into();
                snapshot.error = None;
            }),
            ModelEvent::Loaded => {
                let settings = self.settings();
                let trusted = macos::accessibility_trusted();
                let input_monitoring_trusted = macos::input_monitoring_trusted();
                let ready_status =
                    format!("Ready — hold {} to dictate", settings.hotkey.display_name());
                self.mutate_snapshot(|snapshot| {
                    snapshot.model_loaded = true;
                    snapshot.model_installed = true;
                    snapshot.accessibility_trusted = trusted;
                    snapshot.input_monitoring_trusted = input_monitoring_trusted;
                    if settings.onboarding_complete && trusted && input_monitoring_trusted {
                        snapshot.phase = Phase::Ready;
                        snapshot.status = ready_status;
                    } else {
                        snapshot.phase = Phase::Onboarding;
                        snapshot.status = "Finish setup to enable dictation".into();
                    }
                });
            }
            ModelEvent::Unloaded => self.mutate_snapshot(|snapshot| {
                snapshot.model_loaded = false;
                snapshot.phase = Phase::Onboarding;
                snapshot.status = "Model unloaded".into();
            }),
            ModelEvent::Preview { text, language } => {
                self.mutate_snapshot(|snapshot| {
                    if snapshot.phase == Phase::Recording {
                        snapshot.preview = text;
                        snapshot.status = if language.is_empty() {
                            "Listening…".into()
                        } else {
                            format!("Listening · {language}")
                        };
                    }
                });
            }
            ModelEvent::Final { text, language } => {
                if self.snapshot().phase != Phase::Finalizing {
                    return;
                }
                let destination = self
                    .pending_destination
                    .lock()
                    .expect("destination lock")
                    .take();
                let Some(destination) = destination else {
                    return;
                };
                let transcript = normalize_transcript(&text, &language);
                match destination {
                    RecordingDestination::SetupTest => {
                        let status = if transcript.is_empty() {
                            "No speech detected. Hold the button and try again."
                        } else {
                            "Setup test complete."
                        };
                        let _ = self.app.emit(
                            "voxtype://setup-test-result",
                            SetupTestResult {
                                transcript: transcript.trim_end().into(),
                                status: status.into(),
                            },
                        );
                        self.return_ready(status);
                    }
                    RecordingDestination::Dictation(target) => {
                        if transcript.is_empty() {
                            self.return_ready("No speech detected");
                        } else if !target.is_still_focused() {
                            self.enter_recovery(
                                transcript,
                                target,
                                "Click Retry to return to the original text field",
                            );
                        } else if let Err(error) =
                            self.paste_transcript(transcript.clone(), target.clone())
                        {
                            self.enter_recovery(transcript, target, &format!("{error:#}"));
                        }
                    }
                }
            }
            ModelEvent::DownloadProgress { downloaded, total } => {
                self.mutate_snapshot(|snapshot| {
                    snapshot.model_progress = downloaded as f32 / total.max(1) as f32;
                    snapshot.status = format!(
                        "Downloading Qwen3-ASR · {:.1} / {:.1} GB",
                        downloaded as f64 / 1_000_000_000.0,
                        total as f64 / 1_000_000_000.0
                    );
                });
            }
            ModelEvent::Downloaded => {
                self.mutate_snapshot(|snapshot| {
                    snapshot.model_installed = true;
                    snapshot.model_progress = 1.0;
                    snapshot.phase = Phase::Loading;
                    snapshot.status = "Verifying and loading model…".into();
                });
                let _ = self.load_model();
            }
            ModelEvent::Error(error) => self.handle_runtime_error(error),
        }
    }

    fn handle_audio_event(&self, event: AudioEvent) {
        match event {
            AudioEvent::Level(level) => {
                let recording = self.recording.lock().expect("recording lock");
                let elapsed = recording
                    .as_ref()
                    .map(|session| session.started.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                drop(recording);
                self.mutate_snapshot(|snapshot| {
                    if snapshot.phase == Phase::Recording {
                        snapshot.input_level = level;
                        snapshot.recording_ms = elapsed;
                    }
                });
            }
            AudioEvent::Error(error) => {
                self.cancel_recording();
                self.transient_error(format!("Microphone disconnected: {error}"));
            }
        }
    }

    fn paste_transcript(self: &Arc<Self>, transcript: String, target: FocusedTarget) -> Result<()> {
        if !target.is_still_focused() {
            anyhow::bail!("the original text field no longer has focus");
        }
        let transaction = ClipboardTransaction::begin(&transcript)?;
        if let Err(error) = macos::synthesize_paste() {
            let _ = transaction.restore_if_owned();
            return Err(error);
        }
        std::thread::Builder::new()
            .name("voxtype-clipboard-restore".into())
            .spawn(move || {
                std::thread::sleep(Duration::from_millis(750));
                let _ = transaction.restore_if_owned();
            })?;
        self.return_ready("Pasted");
        Ok(())
    }

    fn emit_empty_setup_test(&self, destination: &RecordingDestination, status: &str) {
        if matches!(destination, RecordingDestination::SetupTest) {
            let _ = self.app.emit(
                "voxtype://setup-test-result",
                SetupTestResult {
                    transcript: String::new(),
                    status: status.into(),
                },
            );
        }
    }

    fn enter_recovery(&self, transcript: String, target: FocusedTarget, message: &str) {
        let id = self.sequence.fetch_add(1, Ordering::Relaxed);
        *self.recovery.lock().expect("recovery lock") = Some(PasteRecovery {
            id,
            transcript,
            target,
            expires: Instant::now() + RECOVERY_TIMEOUT,
        });
        self.mutate_snapshot(|snapshot| {
            snapshot.phase = Phase::PasteRecovery;
            snapshot.status = message.into();
            snapshot.can_retry = true;
            snapshot.error = Some(message.into());
        });
        self.show_overlay();
        self.schedule_recovery_expiry(id);
    }

    fn enter_recovery_or_error(&self, transcript: Option<String>, message: String) {
        let destination = self
            .pending_destination
            .lock()
            .expect("destination lock")
            .take();
        match (transcript, destination) {
            (Some(transcript), Some(RecordingDestination::Dictation(target))) => {
                self.enter_recovery(transcript, target, &message)
            }
            _ => self.handle_runtime_error(message),
        }
    }

    fn handle_runtime_error(&self, error: String) {
        self.recording.lock().expect("recording lock").take();
        self.pending_destination
            .lock()
            .expect("destination lock")
            .take();
        let downloading = self.download_cancel.lock().expect("download lock").is_some();
        self.mutate_snapshot(|snapshot| {
            snapshot.phase = if downloading {
                Phase::Onboarding
            } else {
                Phase::Error
            };
            snapshot.status = "VoxType needs attention".into();
            snapshot.error = Some(error);
            snapshot.model_loaded = false;
        });
        self.show_overlay();
    }

    fn transient_error(&self, message: String) {
        self.mutate_snapshot(|snapshot| {
            snapshot.status = message.clone();
            snapshot.error = Some(message);
        });
        self.show_overlay();
        self.hide_overlay_later(Duration::from_secs(3));
    }

    fn return_ready(&self, status: &str) {
        let settings = self.settings();
        self.pending_destination
            .lock()
            .expect("destination lock")
            .take();
        self.mutate_snapshot(|snapshot| {
            snapshot.phase = if snapshot.model_loaded
                && settings.onboarding_complete
                && snapshot.accessibility_trusted
                && snapshot.input_monitoring_trusted
            {
                Phase::Ready
            } else {
                Phase::Onboarding
            };
            snapshot.status = status.into();
            snapshot.preview.clear();
            snapshot.input_level = 0.0;
            snapshot.recording_ms = 0;
            snapshot.can_retry = false;
            snapshot.error = None;
        });
        self.hide_overlay_later(Duration::from_millis(850));
    }

    fn schedule_recording_limits(self: &Arc<Self>, id: u64) {
        let weak = Arc::downgrade(self);
        std::thread::spawn(move || {
            std::thread::sleep(WARNING_TIME);
            let Some(controller) = weak.upgrade() else {
                return;
            };
            if controller.recording_id() == Some(id) {
                controller.mutate_snapshot(|snapshot| {
                    snapshot.status = "30 seconds remaining".into();
                });
            }
            std::thread::sleep(MAXIMUM_RECORDING - WARNING_TIME);
            if controller.recording_id() == Some(id) {
                controller.finish_recording();
            }
        });
    }

    fn schedule_recovery_expiry(&self, id: u64) {
        let app = self.app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(RECOVERY_TIMEOUT);
            if let Some(controller) = app.try_state::<Arc<Controller>>() {
                let should_discard = controller
                    .recovery
                    .lock()
                    .expect("recovery lock")
                    .as_ref()
                    .is_some_and(|recovery| recovery.id == id);
                if should_discard {
                    controller.discard_recovery();
                }
            }
        });
    }

    fn recording_id(&self) -> Option<u64> {
        self.recording
            .lock()
            .expect("recording lock")
            .as_ref()
            .map(|session| session.id)
    }

    fn mutate_snapshot(&self, mutate: impl FnOnce(&mut AppSnapshot)) {
        let snapshot = {
            let mut snapshot = self.snapshot.lock().expect("snapshot lock");
            mutate(&mut snapshot);
            snapshot.clone()
        };
        let _ = self.app.emit("voxtype://snapshot", snapshot);
    }

    fn emit_snapshot(&self) {
        let _ = self.app.emit("voxtype://snapshot", self.snapshot());
    }

    fn show_overlay_with_status(&self, status: &str) {
        self.mutate_snapshot(|snapshot| snapshot.status = status.into());
        self.show_overlay();
        self.hide_overlay_later(Duration::from_secs(2));
    }

    fn show_overlay(&self) {
        let (pointer_x, pointer_y) = *self.pointer.lock().expect("pointer lock");
        let app = self.app.clone();
        let main_app = app.clone();
        let _ = app.run_on_main_thread(move || {
            let Some(window) = main_app.get_webview_window("overlay") else {
                return;
            };
            if let Ok(monitors) = main_app.available_monitors() {
                let monitor = monitors.iter().find(|monitor| {
                    let position = monitor.position();
                    let size = monitor.size();
                    pointer_x >= position.x as f64
                        && pointer_x < (position.x + size.width as i32) as f64
                        && pointer_y >= position.y as f64
                        && pointer_y < (position.y + size.height as i32) as f64
                });
                if let Some(monitor) = monitor.or_else(|| monitors.first()) {
                    let monitor_position = monitor.position();
                    let monitor_size = monitor.size();
                    let scale = monitor.scale_factor();
                    let width = (540.0 * scale) as i32;
                    let height = (116.0 * scale) as i32;
                    let x = monitor_position.x + (monitor_size.width as i32 - width) / 2;
                    let y = monitor_position.y + monitor_size.height as i32 - height - 72;
                    let _ =
                        window.set_position(Position::Physical(PhysicalPosition::new(x, y)));
                }
            }
            if macos::show_without_activation(&window).is_err() {
                let _ = window.show();
            }
            let _ = window.set_ignore_cursor_events(false);
        });
    }

    fn hide_overlay(&self) {
        if let Some(window) = self.app.get_webview_window("overlay") {
            let _ = window.hide();
        }
    }

    fn hide_overlay_later(&self, delay: Duration) {
        let app = self.app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let Some(controller) = app.try_state::<Arc<Controller>>() else {
                return;
            };
            if matches!(controller.snapshot().phase, Phase::Ready | Phase::Onboarding) {
                let main_app = app.clone();
                let _ = app.run_on_main_thread(move || {
                    if let Some(window) = main_app.get_webview_window("overlay") {
                        let _ = window.hide();
                    }
                });
            }
        });
    }
}

fn can_begin_recording(snapshot: &AppSnapshot, setup_test: bool) -> bool {
    if setup_test {
        snapshot.phase == Phase::Onboarding && snapshot.model_loaded
    } else {
        snapshot.phase == Phase::Ready
    }
}

pub fn normalize_transcript(text: &str, language: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let language = language.to_ascii_lowercase();
    let cjk_language = ["chinese", "japanese", "korean", "cantonese"]
        .iter()
        .any(|candidate| language.contains(candidate));
    let contains_cjk = trimmed.chars().any(|character| {
        matches!(
            character,
            '\u{3040}'..='\u{30ff}'
                | '\u{3400}'..='\u{4dbf}'
                | '\u{4e00}'..='\u{9fff}'
                | '\u{ac00}'..='\u{d7af}'
        )
    });
    if cjk_language || contains_cjk {
        trimmed.to_string()
    } else {
        format!("{trimmed} ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct FocusStolenByRetryClick {
        focused: Cell<bool>,
        restore_attempted: Cell<bool>,
    }

    impl RetryFocus for FocusStolenByRetryClick {
        fn is_still_focused(&self) -> bool {
            self.focused.get()
        }

        fn restore_focus(&self) -> Result<()> {
            self.restore_attempted.set(true);
            self.focused.set(true);
            Ok(())
        }
    }

    #[test]
    fn appends_space_for_english() {
        assert_eq!(normalize_transcript(" hello. \n", "English"), "hello. ");
    }

    #[test]
    fn does_not_append_space_for_cjk() {
        assert_eq!(normalize_transcript("你好。 ", "Chinese"), "你好。");
    }

    #[test]
    fn setup_test_can_record_before_onboarding_is_complete() {
        let snapshot = AppSnapshot {
            model_loaded: true,
            ..AppSnapshot::default()
        };
        assert!(can_begin_recording(&snapshot, true));
        assert!(!can_begin_recording(&snapshot, false));
    }

    #[test]
    fn retry_restores_focus_stolen_by_overlay_click() {
        let target = FocusStolenByRetryClick {
            focused: Cell::new(false),
            restore_attempted: Cell::new(false),
        };

        prepare_retry_focus(&target).unwrap();

        assert!(target.restore_attempted.get());
        assert!(target.focused.get());
    }
}
