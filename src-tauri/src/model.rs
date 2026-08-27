use anyhow::{Context, Result};
use qwen3_asr::{best_device, AsrInference, StreamingOptions, StreamingState};
use reqwest::blocking::Client;
use reqwest::header::RANGE;
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;

pub const MODEL_ID: &str = "Qwen/Qwen3-ASR-1.7B";
pub const MODEL_REVISION: &str = "7278e1e70fe206f11671096ffdd38061171dd6e5";
const MODEL_DIRECTORY: &str = "qwen3-asr-1.7b";
const DOWNLOAD_HEADROOM_BYTES: u64 = 500_000_000;

#[derive(Debug)]
pub enum ModelCommand {
    Load(PathBuf),
    Unload,
    Start {
        language: Option<String>,
        initial_text: Option<String>,
    },
    Audio(Vec<f32>),
    Finish,
    Cancel,
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum ModelEvent {
    Loading,
    Loaded,
    Unloaded,
    Preview { text: String, language: String },
    Final { text: String, language: String },
    DownloadProgress { downloaded: u64, total: u64 },
    Downloaded,
    Error(String),
}

pub struct ModelService {
    sender: SyncSender<ModelCommand>,
}

impl ModelService {
    pub fn spawn(event_sender: SyncSender<ModelEvent>) -> Self {
        let (sender, receiver) = mpsc::sync_channel(128);
        std::thread::Builder::new()
            .name("voxtype-asr".into())
            .spawn(move || run_model_worker(receiver, event_sender))
            .expect("spawn ASR worker");
        Self { sender }
    }

    pub fn send(&self, command: ModelCommand) -> Result<()> {
        self.sender
            .send(command)
            .context("ASR worker is unavailable")
    }

    pub fn command_sender(&self) -> SyncSender<ModelCommand> {
        self.sender.clone()
    }
}

impl Drop for ModelService {
    fn drop(&mut self) {
        let _ = self.sender.send(ModelCommand::Shutdown);
    }
}

fn run_model_worker(receiver: Receiver<ModelCommand>, events: SyncSender<ModelEvent>) {
    let mut model: Option<AsrInference> = None;
    let mut stream: Option<StreamingState> = None;

    while let Ok(command) = receiver.recv() {
        let result: Result<bool> = (|| match command {
            ModelCommand::Load(path) => {
                let _ = events.send(ModelEvent::Loading);
                let device = best_device();
                if device.is_cpu() {
                    anyhow::bail!("Metal is unavailable; CPU fallback is disabled");
                }
                model = Some(AsrInference::load(&path, device).context("load Qwen3-ASR")?);
                stream = None;
                let _ = events.send(ModelEvent::Loaded);
                Ok(true)
            }
            ModelCommand::Unload => {
                stream = None;
                model = None;
                let _ = events.send(ModelEvent::Unloaded);
                Ok(true)
            }
            ModelCommand::Start {
                language,
                initial_text,
            } => {
                let inference = model.as_ref().context("model is not loaded")?;
                let mut options = StreamingOptions::default().with_chunk_size_sec(2.0);
                if let Some(language) = language {
                    options = options.with_language(language);
                }
                if let Some(initial_text) = initial_text {
                    options = options.with_initial_text(initial_text);
                }
                stream = Some(inference.init_streaming(options));
                Ok(true)
            }
            ModelCommand::Audio(samples) => {
                let inference = model.as_ref().context("model is not loaded")?;
                let state = stream.as_mut().context("stream is not active")?;
                if let Some(result) = inference.feed_audio(state, &samples)? {
                    let _ = events.send(ModelEvent::Preview {
                        text: clean_asr_text(&result.text),
                        language: result.language,
                    });
                }
                Ok(true)
            }
            ModelCommand::Finish => {
                let inference = model.as_ref().context("model is not loaded")?;
                let mut state = stream.take().context("stream is not active")?;
                let result = inference.finish_streaming(&mut state)?;
                let _ = events.send(ModelEvent::Final {
                    text: clean_asr_text(&result.text),
                    language: result.language,
                });
                Ok(true)
            }
            ModelCommand::Cancel => {
                stream = None;
                Ok(true)
            }
            ModelCommand::Shutdown => Ok(false),
        })();

        match result {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                stream = None;
                let _ = events.send(ModelEvent::Error(format!("{error:#}")));
            }
        }
    }
}

fn clean_asr_text(text: &str) -> String {
    let mut cleaned = text.trim_start();
    while let Some(rest) = cleaned.strip_prefix("<asr_text>") {
        cleaned = rest.trim_start();
    }
    cleaned.to_string()
}

#[derive(Clone, Copy)]
struct ModelFile {
    name: &'static str,
    size: u64,
    sha256: Option<&'static str>,
}

const MODEL_FILES: &[ModelFile] = &[
    ModelFile {
        name: "config.json",
        size: 6_194,
        sha256: Some("2e74a751548b8ad7d7526d29365ad8144c345d8b412b1152d25dc6698452712f"),
    },
    ModelFile {
        name: "model.safetensors.index.json",
        size: 64_821,
        sha256: Some("f994739fe38e5210b9e3e8ce6c6307315e2ceac3cb630e7b7414d69dce520f60"),
    },
    ModelFile {
        name: "model-00001-of-00002.safetensors",
        size: 4_220_320_824,
        sha256: Some("a4cd1f1a04d90b757dc7f7dd26254e69a013b19e80efe590a83c6a3bde8608d6"),
    },
    ModelFile {
        name: "model-00002-of-00002.safetensors",
        size: 478_200_688,
        sha256: Some("6e0b9d9e09e2e0238e7ef3cc8a484ab387e91b90f1900bedf88bc92d7929ccfc"),
    },
    ModelFile {
        name: "tokenizer_config.json",
        size: 12_487,
        sha256: Some("4942d005604266809309cabc9f4e9cb89ce855d59b14681fdc0e1cc62ea26c4c"),
    },
    ModelFile {
        name: "vocab.json",
        size: 2_776_833,
        sha256: Some("ca10d7e9fb3ed18575dd1e277a2579c16d108e32f27439684afa0e10b1440910"),
    },
    ModelFile {
        name: "merges.txt",
        size: 1_671_853,
        sha256: Some("8831e4f1a044471340f7c0a83d7bd71306a5b867e95fd870f74d0c5308a904d5"),
    },
];

pub fn model_dir(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("models").join(MODEL_DIRECTORY)
}

pub fn model_is_installed(app_data_dir: &Path) -> bool {
    let directory = model_dir(app_data_dir);
    if !directory.join(".complete").exists() || !directory.join("tokenizer.json").exists() {
        return false;
    }
    MODEL_FILES
        .iter()
        .all(|file| directory.join(file.name).metadata().is_ok_and(|m| m.len() == file.size))
}

pub fn remove_model(app_data_dir: &Path) -> Result<()> {
    let directory = model_dir(app_data_dir);
    if directory.exists() {
        fs::remove_dir_all(directory).context("remove model files")?;
    }
    Ok(())
}

pub fn download_model(
    app_data_dir: &Path,
    events: &SyncSender<ModelEvent>,
    cancelled: Arc<AtomicBool>,
) -> Result<PathBuf> {
    let directory = model_dir(app_data_dir);
    fs::create_dir_all(&directory).context("create model directory")?;
    let available = fs2::available_space(&directory).context("check free disk space")?;
    let total: u64 = MODEL_FILES.iter().map(|file| file.size).sum();
    let remaining = total.saturating_sub(incomplete_download_size(app_data_dir));
    let required = remaining.saturating_add(DOWNLOAD_HEADROOM_BYTES);
    if available < required {
        anyhow::bail!(
            "At least {:.1} GB free disk space is required; {:.1} GB is available",
            required as f64 / 1_000_000_000.0,
            available as f64 / 1_000_000_000.0
        );
    }

    let client = Client::builder()
        .user_agent("VoxType/0.1")
        .build()
        .context("create download client")?;
    let mut completed = 0_u64;

    for file in MODEL_FILES {
        download_one(&client, &directory, *file, completed, total, events, &cancelled)?;
        completed += file.size;
    }
    build_tokenizer(&directory)?;

    let marker = serde_json::json!({
        "model": MODEL_ID,
        "revision": MODEL_REVISION
    });
    fs::write(
        directory.join(".complete"),
        serde_json::to_vec_pretty(&marker)?,
    )
    .context("write model completion marker")?;
    let _ = events.send(ModelEvent::Downloaded);
    Ok(directory)
}

fn download_one(
    client: &Client,
    directory: &Path,
    spec: ModelFile,
    completed: u64,
    total: u64,
    events: &SyncSender<ModelEvent>,
    cancelled: &AtomicBool,
) -> Result<()> {
    let final_path = directory.join(spec.name);
    if final_path.metadata().is_ok_and(|metadata| metadata.len() == spec.size)
        && verify_hash_if_present(&final_path, spec.sha256)?
    {
        let _ = events.send(ModelEvent::DownloadProgress {
            downloaded: completed + spec.size,
            total,
        });
        return Ok(());
    }

    let part_path = final_path.with_extension(format!(
        "{}.part",
        final_path.extension().and_then(|value| value.to_str()).unwrap_or("")
    ));
    let mut offset = part_path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    if offset > spec.size {
        fs::remove_file(&part_path).context("remove invalid partial download")?;
        offset = 0;
    }
    let url = format!(
        "https://huggingface.co/{MODEL_ID}/resolve/{MODEL_REVISION}/{}",
        spec.name
    );
    let mut request = client.get(url);
    if offset > 0 {
        request = request.header(RANGE, format!("bytes={offset}-"));
    }
    let mut response = request.send().context("start model download")?;
    if !response.status().is_success() {
        anyhow::bail!("model download returned HTTP {}", response.status());
    }
    let append = offset > 0 && response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if !append {
        offset = 0;
    }
    let mut output = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&part_path)
        .context("open partial model file")?;
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        if cancelled.load(Ordering::Relaxed) {
            anyhow::bail!("model download cancelled");
        }
        let read = response.read(&mut buffer).context("read model download")?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .context("write model download")?;
        offset += read as u64;
        let _ = events.send(ModelEvent::DownloadProgress {
            downloaded: completed + offset.min(spec.size),
            total,
        });
    }
    output.sync_all().context("flush model file")?;
    if offset != spec.size {
        anyhow::bail!(
            "{} has the wrong size: expected {}, received {}",
            spec.name,
            spec.size,
            offset
        );
    }
    if !verify_hash_if_present(&part_path, spec.sha256)? {
        let _ = fs::remove_file(&part_path);
        anyhow::bail!("{} failed SHA-256 verification", spec.name);
    }
    fs::rename(&part_path, &final_path).context("install downloaded model file")?;
    Ok(())
}

fn verify_hash_if_present(path: &Path, expected: Option<&str>) -> Result<bool> {
    let Some(expected) = expected else {
        return Ok(true);
    };
    let mut file = File::open(path).context("open model file for verification")?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()) == expected)
}

fn build_tokenizer(directory: &Path) -> Result<()> {
    let vocab: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("vocab.json"))?)?;
    let tokenizer_config: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("tokenizer_config.json"))?)?;
    let merges_content = fs::read_to_string(directory.join("merges.txt"))?;
    let merges: Vec<&str> = merges_content
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .collect();

    let mut added_tokens = Vec::new();
    if let Some(decoder) = tokenizer_config["added_tokens_decoder"].as_object() {
        let mut entries: Vec<(u64, &serde_json::Value)> = decoder
            .iter()
            .filter_map(|(id, token)| id.parse().ok().map(|id| (id, token)))
            .collect();
        entries.sort_by_key(|(id, _)| *id);
        for (id, token) in entries {
            added_tokens.push(serde_json::json!({
                "id": id,
                "content": token["content"],
                "single_word": false,
                "lstrip": false,
                "rstrip": false,
                "normalized": false,
                "special": token["special"]
            }));
        }
    }

    let tokenizer = serde_json::json!({
        "version": "1.0",
        "truncation": null,
        "padding": null,
        "added_tokens": added_tokens,
        "normalizer": {"type": "NFC"},
        "pre_tokenizer": {
            "type": "Sequence",
            "pretokenizers": [
                {
                    "type": "Split",
                    "pattern": {"Regex": "(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\\r\\n\\p{L}\\p{N}]?\\p{L}+|\\p{N}| ?[^\\s\\p{L}\\p{N}]+[\\r\\n]*|\\s*[\\r\\n]+|\\s+(?!\\S)|\\s+"},
                    "behavior": "Isolated",
                    "invert": false
                },
                {
                    "type": "ByteLevel",
                    "add_prefix_space": false,
                    "trim_offsets": false,
                    "use_regex": false
                }
            ]
        },
        "post_processor": {
            "type": "ByteLevel",
            "add_prefix_space": false,
            "trim_offsets": false,
            "use_regex": false
        },
        "decoder": {
            "type": "ByteLevel",
            "add_prefix_space": false,
            "trim_offsets": false,
            "use_regex": false
        },
        "model": {
            "type": "BPE",
            "dropout": null,
            "unk_token": null,
            "continuing_subword_prefix": "",
            "end_of_word_suffix": "",
            "fuse_unk": false,
            "byte_fallback": false,
            "ignore_merges": false,
            "vocab": vocab,
            "merges": merges
        }
    });
    fs::write(
        directory.join("tokenizer.json"),
        serde_json::to_vec(&tokenizer)?,
    )
    .context("write generated tokenizer")?;
    Ok(())
}

fn incomplete_download_size(app_data_dir: &Path) -> u64 {
    let directory = model_dir(app_data_dir);
    MODEL_FILES
        .iter()
        .filter_map(|file| {
            let final_path = directory.join(file.name);
            let part_path = final_path.with_extension(format!(
                "{}.part",
                final_path.extension().and_then(|value| value.to_str()).unwrap_or("")
            ));
            final_path
                .metadata()
                .or_else(|_| part_path.metadata())
                .ok()
                .map(|metadata| metadata.len().min(file.size))
        })
        .sum()
}

#[cfg(test)]
fn required_model_files() -> HashSet<&'static str> {
    MODEL_FILES.iter().map(|file| file.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_has_unique_names_and_expected_total() {
        let names = required_model_files();
        assert_eq!(names.len(), MODEL_FILES.len());
        assert_eq!(
            MODEL_FILES.iter().map(|file| file.size).sum::<u64>(),
            4_703_053_700
        );
    }

    #[test]
    fn incomplete_size_counts_partial_files() {
        let temp = tempfile::tempdir().unwrap();
        let directory = model_dir(temp.path());
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("config.json.part"), vec![0; 10]).unwrap();
        assert_eq!(incomplete_download_size(temp.path()), 10);
    }

    #[test]
    fn removes_qwen_asr_text_delimiter() {
        assert_eq!(
            clean_asr_text("<asr_text>Oh, hello! How are you today?"),
            "Oh, hello! How are you today?"
        );
    }
}
