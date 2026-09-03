use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cpal::traits::DeviceTrait;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use sonora::config::EchoCanceller;
use sonora::{AudioProcessing, Config, StreamConfig as SonoraStreamConfig};

use crate::devices::{self, Flow};
use crate::wasapi_io::{self, CaptureMode, Endpoint, StreamEvent, StreamKind};
use crate::{ReferenceMode, RunArgs};

const SAMPLE_RATE: u32 = 48_000;
const FRAME_SAMPLES: usize = 480;
const RING_SECONDS: usize = 2;

#[derive(Clone, Debug)]
pub enum BridgeEvent {
    Starting,
    StreamStarted {
        label: &'static str,
        buffer_frames: u32,
    },
    Running,
    Metrics(String),
    Stopped(String),
    Failed(String),
}

pub struct BridgeHandle {
    running: Arc<AtomicBool>,
    events: mpsc::Receiver<BridgeEvent>,
    worker: Option<thread::JoinHandle<()>>,
}

impl BridgeHandle {
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }

    pub fn drain_events(&self) -> Vec<BridgeEvent> {
        self.events.try_iter().collect()
    }

    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(thread::JoinHandle::is_finished)
    }

    pub fn reap_if_finished(&mut self) -> std::result::Result<bool, String> {
        if !self.is_finished() {
            return Ok(false);
        }
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| "AEC bridge supervisor panicked".to_owned())?;
        }
        Ok(true)
    }
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) mic_dropped: AtomicU64,
    pub(crate) reference_dropped: AtomicU64,
    pub(crate) output_dropped: AtomicU64,
    pub(crate) output_underruns: AtomicU64,
    pub(crate) mic_discontinuities: AtomicU64,
    pub(crate) reference_discontinuities: AtomicU64,
}

struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub fn spawn(args: RunArgs) -> Result<BridgeHandle> {
    let running = Arc::new(AtomicBool::new(true));
    let running_worker = running.clone();
    let (events_tx, events_rx) = mpsc::channel();
    let events_worker = events_tx.clone();
    let worker = thread::Builder::new()
        .name("aec-bridge-supervisor".to_owned())
        .spawn(move || {
            let _ = events_worker.send(BridgeEvent::Starting);
            match run_controlled(args, running_worker, Some(events_worker.clone()), false) {
                Ok(summary) => {
                    let _ = events_worker.send(BridgeEvent::Stopped(summary));
                }
                Err(error) => {
                    let _ = events_worker.send(BridgeEvent::Failed(format!("{error:#}")));
                }
            }
        })
        .context("failed to start AEC bridge supervisor")?;

    Ok(BridgeHandle {
        running,
        events: events_rx,
        worker: Some(worker),
    })
}

pub fn run(args: RunArgs) -> Result<()> {
    let running = Arc::new(AtomicBool::new(true));
    let running_ctrlc = running.clone();
    ctrlc::set_handler(move || running_ctrlc.store(false, Ordering::Release))
        .context("failed to install Ctrl+C handler")?;
    run_controlled(args, running, None, true).map(|_| ())
}

fn run_controlled(
    args: RunArgs,
    running: Arc<AtomicBool>,
    bridge_events: Option<mpsc::Sender<BridgeEvent>>,
    console: bool,
) -> Result<String> {
    let _stop_on_return = StopOnDrop(running.clone());
    let endpoints = devices::resolve_endpoints(&args.endpoints)?;
    let reference_flow = devices::reference_flow(args.endpoints.reference_mode);
    let mic_supported = devices::validate_format("microphone", &endpoints.mic, Flow::Capture)?;
    let reference_supported =
        devices::validate_format("reference", &endpoints.reference, reference_flow)?;
    let output_supported = devices::validate_format("output", &endpoints.output, Flow::Render)?;

    let mic_channels = usize::from(mic_supported.channels());
    let reference_channels = usize::from(reference_supported.channels());
    let output_channels = usize::from(output_supported.channels());
    if output_channels != 2 {
        bail!("the handoff render endpoint must expose two output channels");
    }

    let mic_endpoint = Endpoint::from_cpal(&endpoints.mic, mic_channels)?;
    let reference_endpoint = Endpoint::from_cpal(&endpoints.reference, reference_channels)?;
    let output_endpoint = Endpoint::from_cpal(&endpoints.output, output_channels)?;

    if console {
        print_startup_summary(
            &args,
            &endpoints,
            mic_channels,
            reference_channels,
            output_channels,
        )?;
    }

    let counters = Arc::new(Counters::default());

    let mic_ring_samples = SAMPLE_RATE as usize * mic_channels * RING_SECONDS;
    let reference_ring_samples = SAMPLE_RATE as usize * reference_channels * RING_SECONDS;
    let output_ring_samples = SAMPLE_RATE as usize * output_channels * RING_SECONDS;
    let (mic_producer, mut mic_consumer) = HeapRb::<f32>::new(mic_ring_samples).split();
    let (reference_producer, mut reference_consumer) =
        HeapRb::<f32>::new(reference_ring_samples).split();
    let (mut output_producer, output_consumer) = HeapRb::<f32>::new(output_ring_samples).split();

    let running_processor = running.clone();
    let counters_processor = counters.clone();
    let capture_delay_ms = args.capture_delay_ms;
    let stream_delay_ms = args.stream_delay_ms;
    let bypass = args.bypass;
    let bridge_events_processor = bridge_events.clone();
    let processor = thread::Builder::new()
        .name("aec-processing".to_owned())
        .spawn(move || -> Result<()> {
            let _stop_on_exit = StopOnDrop(running_processor.clone());
            let capture_config = SonoraStreamConfig::new(SAMPLE_RATE, 1);
            let render_config =
                SonoraStreamConfig::new(SAMPLE_RATE, reference_channels as u16);
            let config = Config {
                echo_canceller: (!bypass).then(EchoCanceller::default),
                ..Default::default()
            };
            let mut apm = AudioProcessing::builder()
                .config(config)
                .capture_config(capture_config)
                .render_config(render_config)
                .build();
            apm.set_stream_delay_ms(stream_delay_ms)
                .context("invalid stream delay")?;

            let mic_frame_len = FRAME_SAMPLES * mic_channels;
            let reference_frame_len = FRAME_SAMPLES * reference_channels;
            let capture_delay_samples = (SAMPLE_RATE as usize * capture_delay_ms as usize / 1_000)
                * mic_channels;

            let mut mic_interleaved = vec![0.0f32; mic_frame_len];
            let mut mic_mono = vec![0.0f32; FRAME_SAMPLES];
            let mut capture_out = vec![0.0f32; FRAME_SAMPLES];
            let mut output_interleaved = vec![0.0f32; FRAME_SAMPLES * output_channels];
            let mut reference_interleaved = vec![0.0f32; reference_frame_len];
            let mut reference_left = vec![0.0f32; FRAME_SAMPLES];
            let mut reference_right = vec![0.0f32; FRAME_SAMPLES];
            let mut render_out_left = vec![0.0f32; FRAME_SAMPLES];
            let mut render_out_right = vec![0.0f32; FRAME_SAMPLES];
            let mut last_metrics = Instant::now();

            while running_processor.load(Ordering::Acquire) {
                let mut did_work = false;

                while reference_consumer.occupied_len() >= reference_frame_len {
                    reference_consumer.pop_slice(&mut reference_interleaved);
                    deinterleave_reference(
                        &reference_interleaved,
                        reference_channels,
                        &mut reference_left,
                        &mut reference_right,
                    );
                    if !bypass {
                        if reference_channels == 1 {
                            apm.process_render_f32(
                                &[&reference_left],
                                &mut [&mut render_out_left],
                            )?;
                        } else {
                            apm.process_render_f32(
                                &[&reference_left, &reference_right],
                                &mut [&mut render_out_left, &mut render_out_right],
                            )?;
                        }
                    }
                    did_work = true;
                }

                while mic_consumer.occupied_len() >= mic_frame_len + capture_delay_samples {
                    mic_consumer.pop_slice(&mut mic_interleaved);
                    downmix_to_mono(&mic_interleaved, mic_channels, &mut mic_mono);

                    if bypass {
                        capture_out.copy_from_slice(&mic_mono);
                    } else {
                        apm.process_capture_f32(&[&mic_mono], &mut [&mut capture_out])?;
                    }

                    duplicate_mono_to_stereo(&capture_out, &mut output_interleaved);
                    let written = output_producer.push_slice(&output_interleaved);
                    if written < output_interleaved.len() {
                        counters_processor
                            .output_dropped
                            .fetch_add((output_interleaved.len() - written) as u64, Ordering::Relaxed);
                    }
                    did_work = true;
                }

                if !bypass && last_metrics.elapsed() >= Duration::from_secs(5) {
                    let stats = apm.statistics();
                    let summary = format!(
                        "ERL={} dB, ERLE={} dB, delay={} ms | drops mic/ref/out={}/{}/{} underruns={} discontinuities={}/{}",
                        display_metric(stats.echo_return_loss),
                        display_metric(stats.echo_return_loss_enhancement),
                        stats
                            .delay_ms
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "-".to_owned()),
                        counters_processor.mic_dropped.load(Ordering::Relaxed),
                        counters_processor.reference_dropped.load(Ordering::Relaxed),
                        counters_processor.output_dropped.load(Ordering::Relaxed),
                        counters_processor.output_underruns.load(Ordering::Relaxed),
                        counters_processor.mic_discontinuities.load(Ordering::Relaxed),
                        counters_processor.reference_discontinuities.load(Ordering::Relaxed),
                    );
                    if console {
                        println!("AEC: {summary}");
                    }
                    send_bridge_event(
                        &bridge_events_processor,
                        BridgeEvent::Metrics(summary),
                    );
                    last_metrics = Instant::now();
                }

                if !did_work {
                    thread::park_timeout(Duration::from_millis(2));
                }
            }
            Ok(())
        })
        .context("failed to start AEC processing thread")?;
    let processor_thread = processor.thread().clone();

    let (stream_events_tx, stream_events_rx) = mpsc::channel();
    let mut mic_thread = None;
    let mut reference_thread = None;
    let mut output_thread = None;
    let mut run_error = None;
    let mut startup_cancelled = false;

    match wasapi_io::spawn_capture(
        StreamKind::Reference,
        match args.endpoints.reference_mode {
            ReferenceMode::Loopback => CaptureMode::RenderLoopback,
            ReferenceMode::Capture => CaptureMode::Endpoint,
        },
        reference_endpoint,
        reference_producer,
        running.clone(),
        counters.clone(),
        processor_thread.clone(),
        stream_events_tx.clone(),
    ) {
        Ok(handle) => {
            reference_thread = Some(handle);
            match wait_for_stream_start(
                &stream_events_rx,
                &running,
                &bridge_events,
                console,
                StreamKind::Reference,
            ) {
                Ok(true) => {}
                Ok(false) => startup_cancelled = true,
                Err(error) => run_error = Some(error),
            }
        }
        Err(error) => run_error = Some(error),
    }

    if run_error.is_none() && !startup_cancelled {
        match wasapi_io::spawn_capture(
            StreamKind::Microphone,
            CaptureMode::Endpoint,
            mic_endpoint,
            mic_producer,
            running.clone(),
            counters.clone(),
            processor_thread.clone(),
            stream_events_tx.clone(),
        ) {
            Ok(handle) => {
                mic_thread = Some(handle);
                match wait_for_stream_start(
                    &stream_events_rx,
                    &running,
                    &bridge_events,
                    console,
                    StreamKind::Microphone,
                ) {
                    Ok(true) => {}
                    Ok(false) => startup_cancelled = true,
                    Err(error) => run_error = Some(error),
                }
            }
            Err(error) => run_error = Some(error),
        }
    }

    if run_error.is_none() && !startup_cancelled {
        match wasapi_io::spawn_render(
            output_endpoint,
            output_consumer,
            args.mute_output,
            running.clone(),
            counters.clone(),
            stream_events_tx.clone(),
        ) {
            Ok(handle) => {
                output_thread = Some(handle);
                match wait_for_stream_start(
                    &stream_events_rx,
                    &running,
                    &bridge_events,
                    console,
                    StreamKind::Output,
                ) {
                    Ok(true) => {}
                    Ok(false) => startup_cancelled = true,
                    Err(error) => run_error = Some(error),
                }
            }
            Err(error) => run_error = Some(error),
        }
    }
    drop(stream_events_tx);

    let started_successfully = run_error.is_none()
        && !startup_cancelled
        && mic_thread.is_some()
        && reference_thread.is_some()
        && output_thread.is_some();
    if started_successfully {
        if console {
            println!("\nAEC Bridge is live. Press Ctrl+C to stop.");
        }
        send_bridge_event(&bridge_events, BridgeEvent::Running);
        let started = Instant::now();
        while running.load(Ordering::Acquire) {
            while let Ok(event) = stream_events_rx.try_recv() {
                if let StreamEvent::Failed { kind, message } = event {
                    run_error = Some(anyhow::anyhow!(
                        "{} WASAPI stream failed: {message}",
                        kind.label()
                    ));
                    running.store(false, Ordering::Release);
                    break;
                }
            }
            if args.duration_seconds != 0
                && started.elapsed() >= Duration::from_secs(args.duration_seconds)
            {
                running.store(false, Ordering::Release);
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    running.store(false, Ordering::Release);
    processor_thread.unpark();
    if let Some(mic_thread) = mic_thread {
        collect_audio_thread_result("microphone", mic_thread, &mut run_error);
    }
    if let Some(reference_thread) = reference_thread {
        collect_audio_thread_result("reference", reference_thread, &mut run_error);
    }
    if let Some(output_thread) = output_thread {
        collect_audio_thread_result("handoff", output_thread, &mut run_error);
    }
    match processor.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) if run_error.is_none() => run_error = Some(error),
        Err(_) if run_error.is_none() => {
            run_error = Some(anyhow::anyhow!("AEC processing thread panicked"));
        }
        _ => {}
    }

    let summary = format!(
        "Dropped samples mic/ref/out={}/{}/{}; output underruns={}; capture discontinuities mic/ref={}/{}",
        counters.mic_dropped.load(Ordering::Relaxed),
        counters.reference_dropped.load(Ordering::Relaxed),
        counters.output_dropped.load(Ordering::Relaxed),
        counters.output_underruns.load(Ordering::Relaxed),
        counters.mic_discontinuities.load(Ordering::Relaxed),
        counters.reference_discontinuities.load(Ordering::Relaxed),
    );
    if console {
        println!("Stopped. {summary}");
    }
    if let Some(error) = run_error {
        Err(error)
    } else {
        Ok(summary)
    }
}

fn wait_for_stream_start(
    events: &mpsc::Receiver<StreamEvent>,
    running: &AtomicBool,
    bridge_events: &Option<mpsc::Sender<BridgeEvent>>,
    console: bool,
    expected: StreamKind,
) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(StreamEvent::Started {
                kind,
                buffer_frames,
            }) => {
                if console {
                    println!(
                        "WASAPI: {:<10} started with a {}-frame shared buffer",
                        kind.label(),
                        buffer_frames
                    );
                }
                send_bridge_event(
                    bridge_events,
                    BridgeEvent::StreamStarted {
                        label: kind.label(),
                        buffer_frames,
                    },
                );
                if kind == expected {
                    return Ok(true);
                }
            }
            Ok(StreamEvent::Failed { kind, message }) => {
                bail!("{} WASAPI stream failed: {message}", kind.label());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !running.load(Ordering::Acquire) {
                    return Ok(false);
                }
                if Instant::now() >= deadline {
                    bail!(
                        "timed out while opening the {} WASAPI stream",
                        expected.label()
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("audio stream workers stopped before startup completed");
            }
        }
    }
}

fn send_bridge_event(events: &Option<mpsc::Sender<BridgeEvent>>, event: BridgeEvent) {
    if let Some(events) = events {
        let _ = events.send(event);
    }
}

fn collect_audio_thread_result(
    label: &str,
    handle: thread::JoinHandle<Result<()>>,
    run_error: &mut Option<anyhow::Error>,
) {
    let result = match handle.join() {
        Ok(result) => result.with_context(|| format!("{label} WASAPI worker failed")),
        Err(_) => Err(anyhow::anyhow!("{label} WASAPI worker panicked")),
    };
    if run_error.is_none()
        && let Err(error) = result
    {
        *run_error = Some(error);
    }
}

fn print_startup_summary(
    args: &RunArgs,
    endpoints: &devices::ResolvedEndpoints,
    mic_channels: usize,
    reference_channels: usize,
    output_channels: usize,
) -> Result<()> {
    println!("AEC Bridge {}\n", env!("CARGO_PKG_VERSION"));
    println!("Microphone: {}", endpoints.mic.description()?.name());
    println!(
        "Reference:  {} ({})",
        endpoints.reference.description()?.name(),
        match args.endpoints.reference_mode {
            ReferenceMode::Loopback => "render loopback",
            ReferenceMode::Capture => "capture endpoint",
        }
    );
    println!("Handoff:    {}", endpoints.output.description()?.name());
    println!(
        "Format:     {SAMPLE_RATE} Hz; mic/reference/output channels={mic_channels}/{reference_channels}/{output_channels}"
    );
    println!(
        "Processing: {}; capture delay={} ms; stream delay={} ms",
        if args.bypass {
            "BYPASS"
        } else {
            "WebRTC M145 AEC3"
        },
        args.capture_delay_ms,
        args.stream_delay_ms
    );
    if args.mute_output {
        println!("Output:     MUTED safety probe (handoff receives silence)");
    }
    println!(
        "Routing: select the handoff endpoint's paired capture device in your downstream app."
    );
    println!(
        "Safety: never route the raw mic, handoff return, or processed mic into the AEC reference."
    );
    Ok(())
}

fn downmix_to_mono(input: &[f32], channels: usize, output: &mut [f32]) {
    for (out, frame) in output.iter_mut().zip(input.chunks_exact(channels)) {
        *out = frame.iter().copied().sum::<f32>() / channels as f32;
    }
}

fn deinterleave_reference(input: &[f32], channels: usize, left: &mut [f32], right: &mut [f32]) {
    for (index, frame) in input.chunks_exact(channels).enumerate() {
        left[index] = frame[0];
        right[index] = if channels == 1 { frame[0] } else { frame[1] };
    }
}

fn duplicate_mono_to_stereo(input: &[f32], output: &mut [f32]) {
    for (sample, frame) in input.iter().zip(output.as_chunks_mut::<2>().0) {
        frame[0] = *sample;
        frame[1] = *sample;
    }
}

fn display_metric(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.1}"))
        .unwrap_or_else(|| "-".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{deinterleave_reference, downmix_to_mono, duplicate_mono_to_stereo};

    #[test]
    fn downmixes_stereo() {
        let mut output = [0.0; 2];
        downmix_to_mono(&[1.0, 0.0, -1.0, 0.5], 2, &mut output);
        assert_eq!(output, [0.5, -0.25]);
    }

    #[test]
    fn deinterleaves_stereo() {
        let mut left = [0.0; 2];
        let mut right = [0.0; 2];
        deinterleave_reference(&[1.0, 2.0, 3.0, 4.0], 2, &mut left, &mut right);
        assert_eq!(left, [1.0, 3.0]);
        assert_eq!(right, [2.0, 4.0]);
    }

    #[test]
    fn duplicates_mono() {
        let mut output = [0.0; 4];
        duplicate_mono_to_stereo(&[0.25, -0.5], &mut output);
        assert_eq!(output, [0.25, 0.25, -0.5, -0.5]);
    }
}
