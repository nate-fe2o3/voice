import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import "./styles.css";

type Phase =
  | "onboarding"
  | "downloading"
  | "loading"
  | "ready"
  | "recording"
  | "finalizing"
  | "paste_recovery"
  | "error";

interface AppSnapshot {
  phase: Phase;
  status: string;
  preview: string;
  inputLevel: number;
  recordingMs: number;
  modelProgress: number;
  modelInstalled: boolean;
  modelLoaded: boolean;
  accessibilityTrusted: boolean;
  inputMonitoringTrusted: boolean;
  hotkeyListenerReady: boolean;
  hotkeyListenerError: string | null;
  canRetry: boolean;
  error: string | null;
}

interface Settings {
  hotkey: "right_option" | "f8" | "command_shift_space";
  launchAtLogin: boolean;
  tonesEnabled: boolean;
  overlayPreview: boolean;
  microphone: string | null;
  language: "auto" | "english";
  preferredTerms: string[];
  excludedApps: string[];
  onboardingComplete: boolean;
}

interface MicrophoneTestEvent {
  level: number;
}

interface SetupTestResult {
  transcript: string;
  status: string;
}

const app = document.querySelector<HTMLDivElement>("#app")!;
const overlayView = new URLSearchParams(location.search).get("view") === "overlay";
let snapshot: AppSnapshot;
let settings: Settings;
let microphones: string[] = [];
let activeSection = "general";
let previousPhase: Phase | undefined;
let downloadConfirmationOpen = false;
let uiMessage = "";
let microphoneTestRunning = false;
let microphoneTestLevel = 0;
let microphoneTestMessage = "";
let setupTestHeld = false;
let setupTestStart: Promise<boolean> | null = null;
let setupTestTranscript = "";
let setupTestMessage = "";

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function hotkeyName(hotkey: Settings["hotkey"]): string {
  return {
    right_option: "Right Option",
    f8: "F8",
    command_shift_space: "Command + Shift + Space",
  }[hotkey];
}

function phaseLabel(phase: Phase): string {
  return {
    onboarding: "Setup",
    downloading: "Downloading",
    loading: "Loading",
    ready: "Ready",
    recording: "Recording",
    finalizing: "Finalizing",
    paste_recovery: "Paste needs attention",
    error: "Error",
  }[phase];
}

function renderOverlay(): void {
  const preview =
    settings?.overlayPreview && snapshot.preview
      ? `<p class="overlay-preview">${escapeHtml(snapshot.preview)}</p>`
      : "";
  const recovery = snapshot.canRetry
    ? `<div class="overlay-actions">
        <button class="primary small" data-action="retry">Retry</button>
        <button class="small" data-action="discard">Discard</button>
      </div>`
    : "";
  const level = Math.round(snapshot.inputLevel * 100);
  app.innerHTML = `
    <main class="overlay phase-${snapshot.phase}" role="status">
      <div class="overlay-icon" aria-hidden="true">
        <span class="pulse"></span>
        <span class="mic-glyph">●</span>
      </div>
      <div class="overlay-copy">
        <div class="overlay-title-row">
          <strong>${escapeHtml(phaseLabel(snapshot.phase))}</strong>
          <span>${formatDuration(snapshot.recordingMs)}</span>
        </div>
        <p>${escapeHtml(snapshot.status)}</p>
        ${preview}
        <div class="meter" aria-label="Microphone level">
          <span style="width: ${level}%"></span>
        </div>
      </div>
      ${recovery}
    </main>`;
  wireOverlayActions();
}

function formatDuration(milliseconds: number): string {
  if (!milliseconds) return "";
  const seconds = Math.floor(milliseconds / 1000);
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

function renderSettings(): void {
  const needsSetup =
    !settings.onboardingComplete ||
    !snapshot.modelLoaded ||
    !snapshot.accessibilityTrusted ||
    !snapshot.inputMonitoringTrusted;
  app.innerHTML = `
    <div class="window-shell">
      <aside class="sidebar" aria-label="VoxType settings">
        <div class="brand">
          <div class="brand-icon" aria-hidden="true">V</div>
          <div><strong>VoxType</strong><span>Local dictation</span></div>
        </div>
        ${needsSetup ? `<button class="nav-item ${activeSection === "setup" ? "active" : ""}" data-section="setup">Setup</button>` : ""}
        <button class="nav-item ${activeSection === "general" ? "active" : ""}" data-section="general">General</button>
        <button class="nav-item ${activeSection === "audio" ? "active" : ""}" data-section="audio">Audio</button>
        <button class="nav-item ${activeSection === "recognition" ? "active" : ""}" data-section="recognition">Recognition</button>
        <button class="nav-item ${activeSection === "model" ? "active" : ""}" data-section="model">Model</button>
        <button class="nav-item ${activeSection === "privacy" ? "active" : ""}" data-section="privacy">Privacy</button>
        <button class="nav-item ${activeSection === "about" ? "active" : ""}" data-section="about">About</button>
        <div class="sidebar-status status-${snapshot.phase}">
          <span></span>
          <div><strong>${escapeHtml(phaseLabel(snapshot.phase))}</strong><small>${escapeHtml(snapshot.status)}</small></div>
        </div>
      </aside>
      <main class="content">
        ${renderSection()}
      </main>
    </div>`;
  wireSettingsActions();
}

function renderSection(): string {
  if (activeSection === "setup") return renderSetup();
  if (activeSection === "audio") return renderAudio();
  if (activeSection === "recognition") return renderRecognition();
  if (activeSection === "model") return renderModel();
  if (activeSection === "privacy") return renderPrivacy();
  if (activeSection === "about") return renderAbout();
  return renderGeneral();
}

function sectionHeader(title: string, description: string): string {
  return `<header class="section-header"><h1>${title}</h1><p>${description}</p></header>
    ${uiMessage ? `<div class="notice" role="status">${escapeHtml(uiMessage)}</div>` : ""}`;
}

function renderDownloadConfirmation(): string {
  if (!downloadConfirmationOpen) return "";
  return `<section class="download-confirmation" role="dialog" aria-labelledby="download-title">
    <div>
      <strong id="download-title">Download Qwen3-ASR 1.7B?</strong>
      <p>VoxType will download about 4.7 GB to Application Support, verify every file, then load the model locally.</p>
    </div>
    <button data-action="cancel-download-confirmation">Cancel</button>
    <button class="primary" data-action="start-download">Start download</button>
  </section>`;
}

function renderSetup(): string {
  const permissionClass = snapshot.accessibilityTrusted ? "complete" : "pending";
  const inputMonitoringClass = snapshot.inputMonitoringTrusted ? "complete" : "pending";
  const modelClass = snapshot.modelLoaded ? "complete" : "pending";
  const setupTestBusy =
    setupTestHeld || snapshot.phase === "recording" || snapshot.phase === "finalizing";
  return `
    ${sectionHeader("Set up VoxType", "Audio stays on this Mac. Complete these steps once to enable system-wide dictation.")}
    ${renderDownloadConfirmation()}
    <div class="setup-list">
      <section class="setup-card ${permissionClass}">
        <span class="step-icon">${snapshot.accessibilityTrusted ? "✓" : "1"}</span>
        <div><h2>Allow Accessibility</h2><p>${snapshot.accessibilityTrusted
          ? "VoxType can target and paste into the focused text field."
          : "Required to target and paste into text fields. If VoxType already looks enabled, remove the old row and add /Applications/VoxType.app again—the previous local build had a different signature."
        }</p></div>
        <button data-action="accessibility">${snapshot.accessibilityTrusted ? "Open Settings" : "Allow…"}</button>
        <button data-action="refresh-permissions">Refresh</button>
      </section>
      <section class="setup-card ${inputMonitoringClass}">
        <span class="step-icon">${snapshot.inputMonitoringTrusted ? "✓" : "2"}</span>
        <div><h2>Allow Input Monitoring</h2><p>${snapshot.inputMonitoringTrusted
          ? "VoxType can receive the selected push-to-talk key globally."
          : "Required for F8, Right Option, or Command + Shift + Space to work outside VoxType."
        }</p></div>
        <button data-action="input-monitoring">${snapshot.inputMonitoringTrusted ? "Open Settings" : "Allow…"}</button>
        <button data-action="refresh-permissions">Refresh</button>
      </section>
      <section class="setup-card">
        <span class="step-icon">3</span>
        <div><h2>Allow microphone access</h2><p>VoxType records only while the push-to-talk key is held.</p>
          ${renderMicrophoneTest()}
        </div>
        <button data-action="test-microphone" ${microphoneTestRunning ? "disabled" : ""}>${microphoneTestRunning ? "Testing…" : "Test microphone"}</button>
      </section>
      <section class="setup-card ${modelClass}">
        <span class="step-icon">${snapshot.modelLoaded ? "✓" : "4"}</span>
        <div><h2>Install Qwen3-ASR 1.7B</h2><p>4.7 GB download. The model runs locally with Metal and stays on this Mac.</p>
          ${snapshot.phase === "downloading" ? `<progress max="1" value="${snapshot.modelProgress}"></progress>` : ""}
        </div>
        ${
          snapshot.phase === "downloading"
            ? `<button data-action="cancel-download">Cancel</button>`
            : snapshot.modelInstalled
              ? `<button data-action="load-model">${snapshot.modelLoaded ? "Loaded" : "Load"}</button>`
              : `<button class="primary" data-action="request-download">Download…</button>`
        }
      </section>
      <section class="setup-card">
        <span class="step-icon">5</span>
        <div><h2>Try push-to-talk</h2><p>Hold ${hotkeyName(settings.hotkey)}, speak, then release.</p>
          <textarea id="setupTestInput" rows="2" placeholder="Your transcript will appear here" readonly>${escapeHtml(setupTestTranscript || snapshot.preview)}</textarea>
          <div class="meter setup-test-meter" aria-label="Setup test microphone level"><span style="width:${Math.round(snapshot.inputLevel * 100)}%"></span></div>
          <small class="setup-test-status" role="status">${escapeHtml(setupTestMessage || "This test does not enable dictation until you finish setup.")}</small>
        </div>
        <button class="hold-button" data-action="hold-test" ${!snapshot.modelLoaded || setupTestBusy ? "disabled" : ""}>${setupTestHeld ? "Listening…" : snapshot.phase === "finalizing" ? "Transcribing…" : "Hold to test"}</button>
      </section>
    </div>
    <div class="form-footer">
      <span>${snapshot.hotkeyListenerError ? escapeHtml(snapshot.hotkeyListenerError) : snapshot.error ? escapeHtml(snapshot.error) : snapshot.hotkeyListenerReady ? "No speech or transcript is retained." : "Starting the global shortcut listener…"}</span>
      <button class="primary" data-action="complete-setup" ${!snapshot.modelLoaded || !snapshot.accessibilityTrusted || !snapshot.inputMonitoringTrusted || !snapshot.hotkeyListenerReady ? "disabled" : ""}>Finish setup</button>
    </div>`;
}

function renderGeneral(): string {
  return `
    ${sectionHeader("General", "Choose how VoxType starts and how push-to-talk behaves.")}
    ${!snapshot.hotkeyListenerReady ? `<div class="notice" role="status">${escapeHtml(snapshot.hotkeyListenerError || "Starting the global shortcut listener…")}</div>` : ""}
    <section class="form-card">
      <label class="field"><span>Push-to-talk shortcut<small>${snapshot.hotkeyListenerReady ? "Global shortcut listener is active." : "The shortcut listener is unavailable."} Right Option remains available for normal shortcuts when used with another key.</small></span>
        <select id="hotkey">
          <option value="right_option" ${settings.hotkey === "right_option" ? "selected" : ""}>Right Option</option>
          <option value="f8" ${settings.hotkey === "f8" ? "selected" : ""}>F8</option>
          <option value="command_shift_space" ${settings.hotkey === "command_shift_space" ? "selected" : ""}>Command + Shift + Space</option>
        </select>
      </label>
      ${toggle("launchAtLogin", "Launch at login", "Keep VoxType ready without opening a Dock window.", settings.launchAtLogin)}
      ${toggle("tonesEnabled", "Recording sounds", "Play subtle start, finish, and cancel tones.", settings.tonesEnabled)}
      ${toggle("overlayPreview", "Show transcript preview", "Display interim text in the floating overlay.", settings.overlayPreview)}
    </section>
    ${saveFooter()}`;
}

function renderAudio(): string {
  const options = [
    `<option value="" ${settings.microphone === null ? "selected" : ""}>Follow system default</option>`,
    ...microphones.map(
      (microphone) =>
        `<option value="${escapeHtml(microphone)}" ${settings.microphone === microphone ? "selected" : ""}>${escapeHtml(microphone)}</option>`,
    ),
  ].join("");
  return `
    ${sectionHeader("Audio", "Use the system microphone automatically or pin a specific device.")}
    <section class="form-card">
      <label class="field"><span>Microphone<small>If a pinned device disconnects, VoxType cancels safely instead of pasting partial text.</small></span>
        <select id="microphone">${options}</select>
      </label>
      ${renderMicrophoneTest()}
      <button data-action="test-microphone" ${microphoneTestRunning ? "disabled" : ""}>${microphoneTestRunning ? "Testing…" : "Test selected microphone"}</button>
    </section>
    ${saveFooter()}`;
}

function renderMicrophoneTest(): string {
  return `<div class="microphone-test">
    <div class="level-preview"><span>Input level</span><div class="meter" aria-label="Microphone input level"><span data-microphone-level style="width:${Math.round(microphoneTestLevel * 100)}%"></span></div></div>
    <small role="status">${escapeHtml(microphoneTestMessage || "Click Test microphone, then speak normally.")}</small>
  </div>`;
}

function renderRecognition(): string {
  return `
    ${sectionHeader("Recognition", "Qwen3-ASR runs locally and supports automatic language detection.")}
    <section class="form-card">
      <label class="field"><span>Language<small>Pin English only if automatic detection chooses the wrong language.</small></span>
        <select id="language">
          <option value="auto" ${settings.language === "auto" ? "selected" : ""}>Detect automatically</option>
          <option value="english" ${settings.language === "english" ? "selected" : ""}>English</option>
        </select>
      </label>
      <label class="stacked-field"><span>Preferred terms<small>One name, acronym, or technical term per line. Used as context; never uploaded.</small></span>
        <textarea id="preferredTerms" rows="9" maxlength="8100" placeholder="VoxType&#10;Qwen&#10;Tauri">${escapeHtml(settings.preferredTerms.join("\n"))}</textarea>
      </label>
    </section>
    ${saveFooter()}`;
}

function renderModel(): string {
  const downloadControls =
    snapshot.phase === "downloading"
      ? `<div class="download-status">
          <progress max="1" value="${snapshot.modelProgress}"></progress>
          <span>${escapeHtml(snapshot.status)}</span>
          <button data-action="cancel-download">Cancel download</button>
        </div>`
      : "";
  return `
    ${sectionHeader("Speech model", "Qwen3-ASR 1.7B · revision 7278e1e · Apache-2.0")}
    ${renderDownloadConfirmation()}
    <section class="model-card">
      <div class="model-graphic">Q3</div>
      <div><h2>Qwen3-ASR 1.7B</h2><p>Official BF16 weights · Metal acceleration · about 4.6 GB memory</p>
        <span class="pill ${snapshot.modelLoaded ? "success" : ""}">${snapshot.modelLoaded ? "Loaded" : snapshot.modelInstalled ? "Installed" : "Not installed"}</span>
      </div>
    </section>
    ${downloadControls}
    <section class="form-card button-row">
      ${!snapshot.modelInstalled && snapshot.phase !== "downloading" ? `<button class="primary" data-action="request-download">Download model…</button>` : ""}
      ${snapshot.modelInstalled && !snapshot.modelLoaded ? `<button class="primary" data-action="load-model">Load model</button>` : ""}
      ${snapshot.modelLoaded ? `<button data-action="unload-model">Unload model</button>` : ""}
      ${snapshot.modelInstalled ? `<button class="danger" data-action="remove-model">Remove model…</button>` : ""}
    </section>
    <p class="fine-print">Model updates are never installed silently. Normal dictation makes no network requests after installation.</p>`;
}

function renderPrivacy(): string {
  return `
    ${sectionHeader("Privacy", "VoxType is designed to forget each dictation after it is pasted.")}
    <div class="privacy-grid">
      <section><strong>Memory-only audio</strong><p>Microphone samples are never written to disk.</p></section>
      <section><strong>No transcript history</strong><p>Final text is discarded immediately after paste.</p></section>
      <section><strong>No telemetry</strong><p>No analytics, crash upload, or content logging.</p></section>
      <section><strong>Offline recognition</strong><p>Only the explicit model download uses the network.</p></section>
    </div>
    <section class="form-card">
      <label class="stacked-field"><span>Excluded applications<small>Enter one macOS bundle identifier per line, for example com.apple.Notes.</small></span>
        <textarea id="excludedApps" rows="8" maxlength="26000" placeholder="com.example.private-app">${escapeHtml(settings.excludedApps.join("\n"))}</textarea>
      </label>
    </section>
    ${saveFooter()}`;
}

function renderAbout(): string {
  return `
    ${sectionHeader("About VoxType", "Private, local voice dictation for macOS.")}
    <section class="about-card">
      <div class="brand-icon large">V</div>
      <h2>VoxType 0.1.0</h2>
      <p>Built with Rust, Tauri, Candle, and Qwen3-ASR.</p>
      <p>VoxType is MIT licensed. Qwen3-ASR model weights are licensed separately under Apache-2.0.</p>
      <a href="https://huggingface.co/Qwen/Qwen3-ASR-1.7B" data-external>Model information</a>
    </section>`;
}

function toggle(id: string, title: string, detail: string, checked: boolean): string {
  return `<label class="field toggle"><span>${title}<small>${detail}</small></span><input id="${id}" type="checkbox" ${checked ? "checked" : ""}><i></i></label>`;
}

function saveFooter(): string {
  return `<div class="form-footer"><span id="form-message"></span><button class="primary" data-action="save">Save changes</button></div>`;
}

function wireOverlayActions(): void {
  document.querySelector('[data-action="retry"]')?.addEventListener("click", async () => {
    await run("retry_paste");
  });
  document.querySelector('[data-action="discard"]')?.addEventListener("click", async () => {
    await run("discard_recovery");
  });
}

function wireSettingsActions(): void {
  document.querySelectorAll<HTMLElement>("[data-section]").forEach((element) => {
    element.addEventListener("click", () => {
      activeSection = element.dataset.section!;
      renderSettings();
    });
  });
  document.querySelectorAll<HTMLAnchorElement>("[data-external]").forEach((anchor) => {
    anchor.addEventListener("click", (event) => {
      event.preventDefault();
      void openUrl(anchor.href);
    });
  });
  bindAction("accessibility", async () => {
    await run("open_accessibility_settings");
  });
  bindAction("input-monitoring", async () => {
    await run("open_input_monitoring_settings");
  });
  bindAction("refresh-permissions", async () => {
    snapshot = await invoke<AppSnapshot>("refresh_permissions");
    renderSettings();
  });
  bindAction("test-microphone", async () => {
    const device = readMicrophone();
    microphoneTestRunning = true;
    microphoneTestLevel = 0;
    microphoneTestMessage = "Listening for three seconds—speak now.";
    uiMessage = "";
    renderSettings();
    try {
      const peak = await invoke<number>("test_microphone", { device });
      microphoneTestMessage =
        peak >= 0.02
          ? "Microphone is working and audio was detected."
          : "The microphone opened, but no sound was detected. Check its input level and try again.";
      uiMessage = "";
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      microphoneTestMessage = message;
      uiMessage = message;
    } finally {
      microphoneTestRunning = false;
      microphoneTestLevel = 0;
      renderSettings();
    }
  });
  bindAction("request-download", () => {
    downloadConfirmationOpen = true;
    uiMessage = "";
    renderSettings();
  });
  bindAction("cancel-download-confirmation", () => {
    downloadConfirmationOpen = false;
    renderSettings();
  });
  bindAction("start-download", async () => {
    downloadConfirmationOpen = false;
    uiMessage = "Starting model download…";
    renderSettings();
    const started = await run("start_model_download");
    if (!started) renderSettings();
  });
  bindAction("cancel-download", () => run("cancel_model_download"));
  bindAction("load-model", () => run("load_model"));
  bindAction("unload-model", () => run("unload_model"));
  bindAction("remove-model", async () => {
    if (confirm("Remove the local Qwen3-ASR model files?")) {
      await run("remove_model");
    }
  });
  bindAction("complete-setup", async () => {
    await run("complete_onboarding");
    settings.onboardingComplete = true;
    activeSection = "general";
    renderSettings();
  });
  bindAction("save", saveForm);
  const hold = document.querySelector<HTMLElement>('[data-action="hold-test"]');
  hold?.addEventListener("pointerdown", (event) => {
    event.preventDefault();
    if (setupTestHeld) return;
    setupTestHeld = true;
    setupTestTranscript = "";
    setupTestMessage = "Listening… release when you finish speaking.";
    setupTestStart = run("begin_setup_test");
    void setupTestStart.then((started) => {
      if (!started) {
        setupTestHeld = false;
        setupTestMessage = uiMessage || "Could not start the setup test.";
        renderSettings();
      }
    });
    renderSettings();
  });
}

function bindAction(action: string, callback: () => unknown | Promise<unknown>): void {
  document
    .querySelector(`[data-action="${action}"]`)
    ?.addEventListener("click", () => void callback());
}

async function saveForm(): Promise<void> {
  const hotkey = document.querySelector<HTMLSelectElement>("#hotkey");
  const microphone = document.querySelector<HTMLSelectElement>("#microphone");
  const language = document.querySelector<HTMLSelectElement>("#language");
  const preferredTerms = document.querySelector<HTMLTextAreaElement>("#preferredTerms");
  const excludedApps = document.querySelector<HTMLTextAreaElement>("#excludedApps");
  const launchAtLogin = document.querySelector<HTMLInputElement>("#launchAtLogin");
  const tonesEnabled = document.querySelector<HTMLInputElement>("#tonesEnabled");
  const overlayPreview = document.querySelector<HTMLInputElement>("#overlayPreview");
  if (hotkey) settings.hotkey = hotkey.value as Settings["hotkey"];
  if (microphone) settings.microphone = microphone.value || null;
  if (language) settings.language = language.value as Settings["language"];
  if (preferredTerms) settings.preferredTerms = lines(preferredTerms.value);
  if (excludedApps) settings.excludedApps = lines(excludedApps.value);
  if (launchAtLogin) settings.launchAtLogin = launchAtLogin.checked;
  if (tonesEnabled) settings.tonesEnabled = tonesEnabled.checked;
  if (overlayPreview) settings.overlayPreview = overlayPreview.checked;
  await run("save_settings", { settings });
  setFormMessage("Saved.");
}

function lines(value: string): string[] {
  return value
    .split("\n")
    .map((line) => line.trim())
    .filter((line, index, values) => line && values.indexOf(line) === index);
}

function readMicrophone(): string | null {
  return document.querySelector<HTMLSelectElement>("#microphone")?.value || settings.microphone;
}

function setFormMessage(message: string): void {
  const element = document.querySelector("#form-message");
  if (element) element.textContent = message;
}

async function run(
  command: string,
  args?: Record<string, unknown>,
): Promise<boolean> {
  try {
    await invoke(command, args);
    uiMessage = "";
    return true;
  } catch (error) {
    if (overlayView) return false;
    const message = error instanceof Error ? error.message : String(error);
    uiMessage = message;
    setFormMessage(message);
    if (!document.querySelector("#form-message")) alert(message);
    return false;
  }
}

function playPhaseTone(next: Phase): void {
  if (!settings?.tonesEnabled || next === previousPhase) return;
  if (!["recording", "finalizing", "paste_recovery"].includes(next)) return;
  try {
    const audio = new AudioContext();
    const oscillator = audio.createOscillator();
    const gain = audio.createGain();
    oscillator.frequency.value =
      next === "recording" ? 660 : next === "finalizing" ? 520 : 240;
    gain.gain.setValueAtTime(0.045, audio.currentTime);
    gain.gain.exponentialRampToValueAtTime(0.001, audio.currentTime + 0.09);
    oscillator.connect(gain).connect(audio.destination);
    oscillator.start();
    oscillator.stop(audio.currentTime + 0.1);
  } catch {
    // Visual state remains available when audio output cannot be initialized.
  }
}

function render(): void {
  playPhaseTone(snapshot.phase);
  previousPhase = snapshot.phase;
  if (overlayView) renderOverlay();
  else renderSettings();
}

async function initialize(): Promise<void> {
  [snapshot, settings] = await Promise.all([
    invoke<AppSnapshot>("get_snapshot"),
    invoke<Settings>("get_settings"),
  ]);
  if (
    !settings.onboardingComplete ||
    !snapshot.accessibilityTrusted ||
    !snapshot.inputMonitoringTrusted
  ) {
    activeSection = "setup";
  }
  if (!overlayView) {
    try {
      microphones = await invoke<string[]>("list_microphones");
    } catch {
      microphones = [];
    }
  } else {
    document.body.classList.add("overlay-body");
  }
  await listen<AppSnapshot>("voxtype://snapshot", (event) => {
    snapshot = event.payload;
    render();
  });
  await listen<MicrophoneTestEvent>("voxtype://microphone-test", (event) => {
    if (!microphoneTestRunning) return;
    microphoneTestLevel = event.payload.level;
    document.querySelectorAll<HTMLElement>("[data-microphone-level]").forEach((meter) => {
      meter.style.width = `${Math.round(microphoneTestLevel * 100)}%`;
    });
  });
  await listen<SetupTestResult>("voxtype://setup-test-result", (event) => {
    setupTestHeld = false;
    setupTestTranscript = event.payload.transcript;
    setupTestMessage = event.payload.status;
    renderSettings();
  });
  const finishSetupTest = (): void => {
    if (!setupTestHeld) return;
    setupTestHeld = false;
    setupTestMessage = "Transcribing…";
    const start = setupTestStart;
    setupTestStart = null;
    renderSettings();
    void start?.then(async (started) => {
      if (started) await run("finish_setup_test");
    });
  };
  window.addEventListener("pointerup", finishSetupTest);
  window.addEventListener("pointercancel", finishSetupTest);
  render();
}

void initialize();
