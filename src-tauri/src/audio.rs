use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig, SupportedStreamConfig};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use webrtc_vad::{SampleRate, Vad, VadMode};

use crate::model::ModelCommand;

const TARGET_SAMPLE_RATE: u32 = 16_000;
const VAD_FRAME_SAMPLES: usize = 160;
const PRE_ROLL_FRAMES: usize = 20;

#[derive(Debug, Clone)]
pub enum AudioEvent {
    Level(f32),
    Error(String),
}

pub struct AudioRecorder {
    stop: Arc<AtomicBool>,
    voice_detected: Arc<AtomicBool>,
    capture: Option<JoinHandle<()>>,
}

impl AudioRecorder {
    pub fn start(
        device_name: Option<&str>,
        model_sender: SyncSender<ModelCommand>,
        event_sender: SyncSender<AudioEvent>,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let voice_detected = Arc::new(AtomicBool::new(false));
        let capture_stop = stop.clone();
        let capture_voice = voice_detected.clone();
        let requested_device = device_name.map(str::to_string);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let capture = std::thread::Builder::new()
            .name("voxtype-audio-capture".into())
            .spawn(move || {
                let result: Result<()> = (|| {
                    let host = cpal::default_host();
                    let device = find_input_device(&host, requested_device.as_deref())?;
                    let supported = device
                        .default_input_config()
                        .context("read default microphone format")?;
                    let channels = supported.channels() as usize;
                    let source_rate = supported.sample_rate().0;
                    let (raw_sender, raw_receiver) = mpsc::sync_channel::<Vec<f32>>(64);
                    let processor = spawn_processor(
                        raw_receiver,
                        source_rate,
                        channels,
                        model_sender,
                        event_sender.clone(),
                        capture_stop.clone(),
                        capture_voice,
                    );
                    let error_events = event_sender;
                    let stream = build_input_stream(
                        &device,
                        &supported,
                        move |samples| {
                            let _ = raw_sender.try_send(samples);
                        },
                        move |error| {
                            let _ = error_events.try_send(AudioEvent::Error(error));
                        },
                        "open microphone stream",
                    )?;
                    stream.play().context("start microphone")?;
                    let _ = ready_sender.send(Ok(()));
                    while !capture_stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    drop(stream);
                    let _ = processor.join();
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = ready_sender.send(Err(format!("{error:#}")));
                }
            })?;
        match ready_receiver
            .recv_timeout(Duration::from_secs(5))
            .context("microphone did not start")?
        {
            Ok(()) => {}
            Err(error) => anyhow::bail!(error),
        }

        Ok(Self {
            stop,
            voice_detected,
            capture: Some(capture),
        })
    }

    pub fn stop(mut self) -> bool {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(capture) = self.capture.take() {
            let _ = capture.join();
        }
        self.voice_detected.load(Ordering::Relaxed)
    }
}

impl Drop for AudioRecorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(capture) = self.capture.take() {
            let _ = capture.join();
        }
    }
}

pub fn input_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let mut devices = Vec::new();
    for device in host.input_devices().context("enumerate microphones")? {
        let name = device
            .name()
            .unwrap_or_else(|_| "Unknown microphone".into());
        devices.push(name);
    }
    devices.sort();
    devices.dedup();
    Ok(devices)
}

pub fn test_microphone(
    device_name: Option<&str>,
    on_level: Arc<dyn Fn(f32) + Send + Sync>,
) -> Result<f32> {
    let host = cpal::default_host();
    let device = find_input_device(&host, device_name)?;
    let supported = device
        .default_input_config()
        .context("microphone is unavailable")?;
    let (events, receiver) = mpsc::sync_channel(16);
    let level_events = events.clone();
    let stream = build_input_stream(
        &device,
        &supported,
        move |samples| {
            let level = normalized_level(samples.into_iter());
            let _ = level_events.try_send(AudioEvent::Level(level));
        },
        move |error| {
            let _ = events.try_send(AudioEvent::Error(error));
        },
        "open microphone test stream",
    )?;
    stream.play().context("start microphone test")?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    let mut peak = 0.0_f32;

    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match receiver.recv_timeout(remaining.min(Duration::from_millis(60))) {
            Ok(AudioEvent::Level(level)) => {
                peak = peak.max(level);
                if last_emit.elapsed() >= Duration::from_millis(50) {
                    on_level(level);
                    last_emit = Instant::now();
                }
            }
            Ok(AudioEvent::Error(error)) => anyhow::bail!(error),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("microphone test stream stopped unexpectedly")
            }
        }
    }
    Ok(peak)
}

fn find_input_device(host: &cpal::Host, requested: Option<&str>) -> Result<Device> {
    if let Some(requested) = requested {
        for device in host.input_devices().context("enumerate microphones")? {
            let matches = device
                .name()
                .is_ok_and(|name| name == requested);
            if matches {
                return Ok(device);
            }
        }
        anyhow::bail!("the selected microphone is no longer available");
    }
    host.default_input_device()
        .context("no default microphone is configured")
}

fn build_input_stream<D, E>(
    device: &Device,
    config: &SupportedStreamConfig,
    on_data: D,
    on_error: E,
    error_context: &'static str,
) -> Result<Stream>
where
    D: FnMut(Vec<f32>) + Send + 'static,
    E: FnMut(String) + Send + 'static,
{
    let stream_config = config.clone().into();
    match config.sample_format() {
        SampleFormat::F32 => {
            build_typed_input_stream(device, &stream_config, |sample| sample, on_data, on_error)
        }
        SampleFormat::I16 => build_typed_input_stream(
            device,
            &stream_config,
            |sample: i16| sample as f32 / i16::MAX as f32,
            on_data,
            on_error,
        ),
        SampleFormat::U16 => build_typed_input_stream(
            device,
            &stream_config,
            |sample: u16| (sample as f32 - 32768.0) / 32768.0,
            on_data,
            on_error,
        ),
        format => anyhow::bail!("unsupported microphone sample format: {format:?}"),
    }
    .context(error_context)
}

fn build_typed_input_stream<T, F, D, E>(
    device: &Device,
    config: &StreamConfig,
    convert: F,
    mut on_data: D,
    mut on_error: E,
) -> std::result::Result<Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + Send + 'static,
    F: Fn(T) -> f32 + Send + Copy + 'static,
    D: FnMut(Vec<f32>) + Send + 'static,
    E: FnMut(String) + Send + 'static,
{
    device.build_input_stream(
        config,
        move |data: &[T], _| {
            on_data(data.iter().copied().map(convert).collect());
        },
        move |error| on_error(error.to_string()),
        None,
    )
}

fn normalized_level(samples: impl Iterator<Item = f32>) -> f32 {
    let (sum, count) = samples.fold((0.0, 0_usize), |(sum, count), sample| {
        (sum + sample * sample, count + 1)
    });
    ((sum / count.max(1) as f32).sqrt() * 8.0).clamp(0.0, 1.0)
}

fn spawn_processor(
    receiver: Receiver<Vec<f32>>,
    source_rate: u32,
    channels: usize,
    model_sender: SyncSender<ModelCommand>,
    events: SyncSender<AudioEvent>,
    stop: Arc<AtomicBool>,
    voice_detected: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("voxtype-audio".into())
        .spawn(move || {
            let mut vad =
                Vad::new_with_rate_and_mode(SampleRate::Rate16kHz, VadMode::Aggressive);
            let mut pending = Vec::<f32>::new();
            let mut pre_roll = VecDeque::<Vec<f32>>::with_capacity(PRE_ROLL_FRAMES);
            let mut model_batch = Vec::<f32>::with_capacity(1_600);
            let mut started = false;
            let mut last_level = Instant::now() - Duration::from_secs(1);

            while !stop.load(Ordering::Relaxed) {
                let chunk = match receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(chunk) => chunk,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let mono = downmix(&chunk, channels);
                let resampled = resample_linear(&mono, source_rate, TARGET_SAMPLE_RATE);
                if last_level.elapsed() >= Duration::from_millis(50) {
                    let level = normalized_level(resampled.iter().copied());
                    let _ = events.try_send(AudioEvent::Level(level));
                    last_level = Instant::now();
                }
                pending.extend_from_slice(&resampled);

                while pending.len() >= VAD_FRAME_SAMPLES {
                    let frame: Vec<f32> = pending.drain(..VAD_FRAME_SAMPLES).collect();
                    let pcm: Vec<i16> = frame
                        .iter()
                        .map(|sample| {
                            (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
                        })
                        .collect();
                    let voiced = vad.is_voice_segment(&pcm).unwrap_or(false);
                    if voiced {
                        voice_detected.store(true, Ordering::Relaxed);
                    }
                    if !started {
                        pre_roll.push_back(frame);
                        if pre_roll.len() > PRE_ROLL_FRAMES {
                            pre_roll.pop_front();
                        }
                        if voiced {
                            started = true;
                            let mut initial = Vec::with_capacity(
                                pre_roll.len() * VAD_FRAME_SAMPLES,
                            );
                            while let Some(frame) = pre_roll.pop_front() {
                                initial.extend_from_slice(&frame);
                            }
                            model_batch.extend_from_slice(&initial);
                        }
                    } else {
                        model_batch.extend_from_slice(&frame);
                    }
                    if model_batch.len() >= 1_600 {
                        let batch = std::mem::take(&mut model_batch);
                        if model_sender.send(ModelCommand::Audio(batch)).is_err() {
                            return;
                        }
                        model_batch.reserve(1_600);
                    }
                }
            }

            if started {
                model_batch.extend_from_slice(&pending);
                if !model_batch.is_empty() {
                    let _ = model_sender.send(ModelCommand::Audio(model_batch));
                }
            }
        })
        .expect("spawn audio processor")
}

fn downmix(samples: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }
    samples
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

fn resample_linear(samples: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
    if source_rate == target_rate || samples.len() < 2 {
        return samples.to_vec();
    }
    let output_length =
        ((samples.len() as u64 * target_rate as u64) / source_rate as u64) as usize;
    let ratio = source_rate as f64 / target_rate as f64;
    (0..output_length)
        .map(|index| {
            let source = index as f64 * ratio;
            let left = source.floor() as usize;
            let right = (left + 1).min(samples.len() - 1);
            let fraction = (source - left as f64) as f32;
            samples[left] * (1.0 - fraction) + samples[right] * fraction
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmixes_stereo() {
        assert_eq!(downmix(&[1.0, -1.0, 0.5, 0.5], 2), vec![0.0, 0.5]);
    }

    #[test]
    fn resamples_to_expected_length() {
        let input = vec![0.0; 48_000];
        assert_eq!(resample_linear(&input, 48_000, 16_000).len(), 16_000);
    }

    #[test]
    fn normalizes_microphone_level() {
        assert_eq!(normalized_level([0.0, 0.0].into_iter()), 0.0);
        assert!((normalized_level([0.125, -0.125].into_iter()) - 1.0).abs() < f32::EPSILON);
    }
}
