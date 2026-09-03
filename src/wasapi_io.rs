use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle, Thread};

use anyhow::{Context, Result, bail};
use cpal::traits::DeviceTrait;
use ringbuf::traits::{Consumer, Producer};
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WasapiError, WaveFormat};

use crate::bridge::Counters;

const EVENT_TIMEOUT_MS: u32 = 100;

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub channels: usize,
}

impl Endpoint {
    pub fn from_cpal(device: &cpal::Device, channels: usize) -> Result<Self> {
        Ok(Self {
            id: device.id()?.to_string(),
            name: device.description()?.name().to_owned(),
            channels,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamKind {
    Microphone,
    Reference,
    Output,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureMode {
    Endpoint,
    RenderLoopback,
}

impl StreamKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Microphone => "microphone",
            Self::Reference => "reference",
            Self::Output => "handoff",
        }
    }
}

#[derive(Debug)]
pub enum StreamEvent {
    Started {
        kind: StreamKind,
        buffer_frames: u32,
    },
    Failed {
        kind: StreamKind,
        message: String,
    },
}

struct ComGuard;

impl ComGuard {
    fn initialize() -> Result<Self> {
        wasapi::initialize_mta()
            .ok()
            .context("failed to initialize COM for WASAPI")?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_capture<P>(
    kind: StreamKind,
    mode: CaptureMode,
    endpoint: Endpoint,
    producer: P,
    running: Arc<AtomicBool>,
    counters: Arc<Counters>,
    processor_thread: Thread,
    events: mpsc::Sender<StreamEvent>,
) -> Result<JoinHandle<Result<()>>>
where
    P: Producer<Item = f32> + Send + 'static,
{
    let thread_name = format!("wasapi-{}-capture", kind.label());
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let result = capture_loop(
                kind,
                mode,
                &endpoint,
                producer,
                &running,
                &counters,
                &processor_thread,
                &events,
            );
            if let Err(error) = &result {
                running.store(false, Ordering::Release);
                processor_thread.unpark();
                let _ = events.send(StreamEvent::Failed {
                    kind,
                    message: format!("{error:#}"),
                });
            }
            result
        })
        .with_context(|| format!("failed to start {} WASAPI thread", kind.label()))
}

pub fn spawn_render<C>(
    endpoint: Endpoint,
    consumer: C,
    mute_output: bool,
    running: Arc<AtomicBool>,
    counters: Arc<Counters>,
    events: mpsc::Sender<StreamEvent>,
) -> Result<JoinHandle<Result<()>>>
where
    C: Consumer<Item = f32> + Send + 'static,
{
    thread::Builder::new()
        .name("wasapi-handoff-render".to_owned())
        .spawn(move || {
            let result = render_loop(
                &endpoint,
                consumer,
                mute_output,
                &running,
                &counters,
                &events,
            );
            if let Err(error) = &result {
                running.store(false, Ordering::Release);
                let _ = events.send(StreamEvent::Failed {
                    kind: StreamKind::Output,
                    message: format!("{error:#}"),
                });
            }
            result
        })
        .context("failed to start handoff WASAPI thread")
}

#[allow(clippy::too_many_arguments)]
fn capture_loop<P>(
    kind: StreamKind,
    mode: CaptureMode,
    endpoint: &Endpoint,
    mut producer: P,
    running: &AtomicBool,
    counters: &Counters,
    processor_thread: &Thread,
    events: &mpsc::Sender<StreamEvent>,
) -> Result<()>
where
    P: Producer<Item = f32>,
{
    let _com = ComGuard::initialize()?;
    let enumerator = DeviceEnumerator::new()?;
    let device = enumerator
        .get_device(raw_wasapi_id(&endpoint.id)?)
        .with_context(|| {
            format!(
                "could not open {} endpoint '{}'",
                kind.label(),
                endpoint.name
            )
        })?;
    let required_direction = match mode {
        CaptureMode::Endpoint => Direction::Capture,
        CaptureMode::RenderLoopback => Direction::Render,
    };
    if device.get_direction() != required_direction {
        bail!(
            "'{}' is not a {:?} endpoint",
            endpoint.name,
            required_direction
        );
    }

    let mut audio_client = device.get_iaudioclient()?;
    let format = audio_client.get_mixformat()?;
    validate_mix_format(&format, endpoint)?;
    let mode = StreamMode::EventsShared {
        autoconvert: false,
        buffer_duration_hns: 0,
    };
    audio_client
        .initialize_client(&format, &Direction::Capture, &mode)
        .with_context(|| format!("failed to initialize '{}' for capture", endpoint.name))?;
    let event = audio_client.set_get_eventhandle()?;
    let buffer_frames = audio_client.get_buffer_size()?;
    let capture_client = audio_client.get_audiocaptureclient()?;
    let block_align = format.get_blockalign() as usize;
    let mut bytes = vec![0u8; buffer_frames as usize * block_align];
    let mut samples = vec![0.0f32; buffer_frames as usize * endpoint.channels];

    audio_client.start_stream()?;
    events
        .send(StreamEvent::Started {
            kind,
            buffer_frames,
        })
        .ok();

    let stream_result = (|| -> Result<()> {
        while running.load(Ordering::Acquire) {
            match event.wait_for_event(EVENT_TIMEOUT_MS) {
                Ok(()) | Err(WasapiError::EventTimeout) => {}
                Err(error) => return Err(error.into()),
            }

            loop {
                let packet_frames = capture_client.get_next_packet_size()?.unwrap_or(0);
                if packet_frames == 0 {
                    break;
                }
                let sample_count = packet_frames as usize * endpoint.channels;
                let byte_count = packet_frames as usize * block_align;
                if bytes.len() < byte_count {
                    bytes.resize(byte_count, 0);
                }
                if samples.len() < sample_count {
                    samples.resize(sample_count, 0.0);
                }

                let (frames_read, info) = capture_client.read_from_device(&mut bytes)?;
                let samples_read = frames_read as usize * endpoint.channels;
                if info.flags.silent {
                    samples[..samples_read].fill(0.0);
                } else {
                    decode_f32_le(
                        &bytes[..samples_read * size_of::<f32>()],
                        &mut samples[..samples_read],
                    );
                }
                if info.flags.data_discontinuity {
                    capture_discontinuity_counter(counters, kind).fetch_add(1, Ordering::Relaxed);
                }

                let written = producer.push_slice(&samples[..samples_read]);
                if written < samples_read {
                    capture_drop_counter(counters, kind)
                        .fetch_add((samples_read - written) as u64, Ordering::Relaxed);
                }
                processor_thread.unpark();
            }
        }
        Ok(())
    })();

    let _ = audio_client.stop_stream();
    stream_result
}

fn render_loop<C>(
    endpoint: &Endpoint,
    mut consumer: C,
    mute_output: bool,
    running: &AtomicBool,
    counters: &Counters,
    events: &mpsc::Sender<StreamEvent>,
) -> Result<()>
where
    C: Consumer<Item = f32>,
{
    let _com = ComGuard::initialize()?;
    let enumerator = DeviceEnumerator::new()?;
    let device = enumerator
        .get_device(raw_wasapi_id(&endpoint.id)?)
        .with_context(|| format!("could not open handoff endpoint '{}'", endpoint.name))?;
    if device.get_direction() != Direction::Render {
        bail!("'{}' is not a render endpoint", endpoint.name);
    }

    let mut audio_client = device.get_iaudioclient()?;
    let format = audio_client.get_mixformat()?;
    validate_mix_format(&format, endpoint)?;
    let mode = StreamMode::EventsShared {
        autoconvert: false,
        buffer_duration_hns: 0,
    };
    audio_client
        .initialize_client(&format, &Direction::Render, &mode)
        .with_context(|| format!("failed to initialize '{}' for rendering", endpoint.name))?;
    let event = audio_client.set_get_eventhandle()?;
    let buffer_frames = audio_client.get_buffer_size()?;
    let render_client = audio_client.get_audiorenderclient()?;
    let mut samples = Vec::<f32>::new();
    let mut bytes = Vec::<u8>::new();

    let initial_frames = audio_client.get_available_space_in_frames()? as usize;
    write_render_frames(
        &render_client,
        initial_frames,
        endpoint.channels,
        &mut consumer,
        mute_output,
        false,
        counters,
        &mut samples,
        &mut bytes,
    )?;
    audio_client.start_stream()?;
    events
        .send(StreamEvent::Started {
            kind: StreamKind::Output,
            buffer_frames,
        })
        .ok();

    let stream_result = (|| -> Result<()> {
        while running.load(Ordering::Acquire) {
            match event.wait_for_event(EVENT_TIMEOUT_MS) {
                Ok(()) | Err(WasapiError::EventTimeout) => {}
                Err(error) => return Err(error.into()),
            }
            let frames = audio_client.get_available_space_in_frames()? as usize;
            write_render_frames(
                &render_client,
                frames,
                endpoint.channels,
                &mut consumer,
                mute_output,
                true,
                counters,
                &mut samples,
                &mut bytes,
            )?;
        }
        Ok(())
    })();

    let _ = audio_client.stop_stream();
    stream_result
}

#[allow(clippy::too_many_arguments)]
fn write_render_frames<C>(
    render_client: &wasapi::AudioRenderClient,
    frames: usize,
    channels: usize,
    consumer: &mut C,
    mute_output: bool,
    count_underrun: bool,
    counters: &Counters,
    samples: &mut Vec<f32>,
    bytes: &mut Vec<u8>,
) -> Result<()>
where
    C: Consumer<Item = f32>,
{
    if frames == 0 {
        return Ok(());
    }
    let sample_count = frames * channels;
    samples.resize(sample_count, 0.0);
    let read = consumer.pop_slice(samples);
    if read < sample_count {
        samples[read..].fill(0.0);
        if count_underrun {
            counters.output_underruns.fetch_add(1, Ordering::Relaxed);
        }
    }
    if mute_output {
        samples.fill(0.0);
    }

    encode_f32_le(samples, bytes);
    render_client.write_to_device(frames, bytes, None)?;
    Ok(())
}

fn validate_mix_format(format: &WaveFormat, endpoint: &Endpoint) -> Result<()> {
    if format.get_samplespersec() != 48_000 {
        bail!(
            "'{}' uses {} Hz instead of 48000 Hz",
            endpoint.name,
            format.get_samplespersec()
        );
    }
    if format.get_nchannels() as usize != endpoint.channels {
        bail!(
            "'{}' changed from {} to {} channels",
            endpoint.name,
            endpoint.channels,
            format.get_nchannels()
        );
    }
    if format.get_bitspersample() != 32 || format.get_subformat()? != SampleType::Float {
        bail!("'{}' is not 32-bit floating-point audio", endpoint.name);
    }
    Ok(())
}

fn raw_wasapi_id(id: &str) -> Result<&str> {
    id.strip_prefix("wasapi:")
        .with_context(|| format!("'{id}' is not a WASAPI endpoint ID"))
}

fn capture_drop_counter(counters: &Counters, kind: StreamKind) -> &std::sync::atomic::AtomicU64 {
    match kind {
        StreamKind::Microphone => &counters.mic_dropped,
        StreamKind::Reference => &counters.reference_dropped,
        StreamKind::Output => unreachable!("output is not a capture stream"),
    }
}

fn capture_discontinuity_counter(
    counters: &Counters,
    kind: StreamKind,
) -> &std::sync::atomic::AtomicU64 {
    match kind {
        StreamKind::Microphone => &counters.mic_discontinuities,
        StreamKind::Reference => &counters.reference_discontinuities,
        StreamKind::Output => unreachable!("output is not a capture stream"),
    }
}

fn decode_f32_le(bytes: &[u8], samples: &mut [f32]) {
    for (sample, encoded) in samples
        .iter_mut()
        .zip(bytes.as_chunks::<{ size_of::<f32>() }>().0)
    {
        *sample = f32::from_le_bytes(*encoded);
    }
}

fn encode_f32_le(samples: &[f32], bytes: &mut Vec<u8>) {
    bytes.resize(std::mem::size_of_val(samples), 0);
    for (encoded, sample) in bytes
        .as_chunks_mut::<{ size_of::<f32>() }>()
        .0
        .iter_mut()
        .zip(samples)
    {
        encoded.copy_from_slice(&sample.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_f32_le, encode_f32_le};

    #[test]
    fn f32_byte_round_trip() {
        let expected = [0.0, 0.25, -0.5, 1.0];
        let mut bytes = Vec::new();
        encode_f32_le(&expected, &mut bytes);
        let mut actual = [0.0; 4];
        decode_f32_le(&bytes, &mut actual);
        assert_eq!(actual, expected);
    }
}
