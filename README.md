# VoxType

VoxType is a private, local voice-dictation utility for Apple Silicon Macs. Hold a key, speak, and release to paste the Qwen3-ASR transcript into the text field that was focused when recording began.

## Requirements

- Apple Silicon Mac with macOS 13 Ventura or newer
- 16 GB unified memory
- About 5.3 GB free disk space for the model download and installation headroom
- Rust 1.85+, Node.js, and npm

Audio and transcripts remain in memory and are discarded after paste. VoxType has no transcript history, telemetry, or post-install network use.

## Run from source

```sh
npm install
npm run tauri dev
```

The first launch guides you through:

1. Granting Accessibility permission so VoxType can observe the global push-to-talk key and issue Paste.
2. Granting microphone permission.
3. Downloading the pinned Qwen3-ASR-1.7B model (about 4.7 GB).
4. Testing push-to-talk.

Hold **Right Option** by default, speak, and release. Press Escape while recording to cancel. Right Option continues to behave normally when pressed with another key. The shortcut is configurable in Settings.

### Accessibility after replacing a local build

macOS binds Accessibility approval for an ad-hoc-signed app to that exact
build. If System Settings shows an older VoxType row as enabled while setup
still says permission is missing, remove the stale row and add
`/Applications/VoxType.app` again. VoxType requests the current build's entry
automatically at startup and refreshes its setup state while it runs.

## Build

```sh
# Optimized .app
npm run bundle:app

# Optimized .app and DMG
npm run bundle:dmg
```

Artifacts are written beneath `src-tauri/target/release/bundle/`. The packaging script applies an ad-hoc hardened-runtime signature, the
`com.nbutton.voxtype` identifier, and the microphone entitlement before it
creates the DMG.

Public distribution requires a Developer ID certificate and notarization, which are intentionally not configured in this repository.

## Validation

```sh
npm run check
npm run build
npm test
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
```

System-wide insertion should be manually exercised in TextEdit, Safari, Chrome, VS Code, Terminal, Slack, and the Copilot app because macOS Accessibility behavior differs across native, browser, Electron, and terminal controls.

## Recognition model

VoxType pins:

- `qwen3-asr` Rust crate `0.2.2` with Candle Metal acceleration
- `Qwen/Qwen3-ASR-1.7B` revision `7278e1e70fe206f11671096ffdd38061171dd6e5`

Downloads are resumable and written to Application Support only after exact size and SHA-256 verification of the model shards. VoxType does not bundle or redistribute model weights.

## Privacy and safety

- Microphone PCM is never written to disk.
- Transcripts are not logged or persisted.
- A failed paste may retain the final text only in process memory for Retry/Discard and automatically discards it after five minutes.
- Password and secure fields are blocked.
- The target field is locked when recording starts; focus changes prevent automatic paste.
- VoxType snapshots all clipboard representations and restores them only when its private transaction marker proves the clipboard was not concurrently changed.

## License

VoxType is available under the [MIT License](LICENSE). Qwen3-ASR model weights are licensed separately under Apache-2.0. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
