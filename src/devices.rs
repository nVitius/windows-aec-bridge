use std::str::FromStr;

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait};

use crate::{EndpointArgs, ReferenceMode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flow {
    Capture,
    Render,
}

#[derive(Clone, Debug)]
pub struct EndpointDescriptor {
    pub id: String,
    pub name: String,
    pub format: String,
    pub compatible: bool,
}

#[derive(Debug, Default)]
pub struct EndpointCatalog {
    pub capture: Vec<EndpointDescriptor>,
    pub render: Vec<EndpointDescriptor>,
}

pub struct ResolvedEndpoints {
    pub mic: cpal::Device,
    pub reference: cpal::Device,
    pub output: cpal::Device,
}

pub fn print_devices() -> Result<()> {
    let host = cpal::default_host();
    let default_input = host
        .default_input_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let default_output = host
        .default_output_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());

    println!("WASAPI audio endpoints\n");
    println!(
        "Default capture: {}",
        default_input.as_deref().unwrap_or("<none>")
    );
    println!(
        "Default render:  {}\n",
        default_output.as_deref().unwrap_or("<none>")
    );

    let devices = host
        .devices()
        .context("failed to enumerate WASAPI endpoints")?;
    for device in devices {
        let id = device
            .id()
            .map(|id| id.to_string())
            .unwrap_or_else(|error| format!("<unavailable: {error}>"));
        let description = device
            .description()
            .map(|description| description.to_string())
            .unwrap_or_else(|error| format!("<unavailable: {error}>"));

        let input = device
            .default_input_config()
            .map(|config| format_config(&config))
            .unwrap_or_else(|_| "-".to_owned());
        let output = device
            .default_output_config()
            .map(|config| format_config(&config))
            .unwrap_or_else(|_| "-".to_owned());

        println!("{description}");
        println!("  ID:      {id}");
        println!("  Capture: {input}");
        println!("  Render:  {output}\n");
    }

    Ok(())
}

pub fn check_endpoints(args: &EndpointArgs) -> Result<()> {
    let endpoints = resolve_endpoints(args)?;
    let reference_flow = reference_flow(args.reference_mode);

    println!("Endpoint validation succeeded. No audio streams were opened.\n");
    print_resolved("Microphone", &endpoints.mic, Flow::Capture)?;
    print_resolved(
        match args.reference_mode {
            ReferenceMode::Loopback => "Reference loopback",
            ReferenceMode::Capture => "Reference capture",
        },
        &endpoints.reference,
        reference_flow,
    )?;
    print_resolved("Handoff render", &endpoints.output, Flow::Render)?;

    validate_format("microphone", &endpoints.mic, Flow::Capture)?;
    validate_format("reference", &endpoints.reference, reference_flow)?;
    validate_format("output", &endpoints.output, Flow::Render)?;

    println!("\nAll bridge endpoints use compatible 48 kHz f32 shared-mode audio.");
    Ok(())
}

pub fn endpoint_catalog() -> Result<EndpointCatalog> {
    let host = cpal::default_host();
    let mut catalog = EndpointCatalog::default();

    for device in host
        .devices()
        .context("failed to enumerate WASAPI endpoints")?
    {
        let Ok(id) = device.id() else {
            continue;
        };
        let Ok(description) = device.description() else {
            continue;
        };

        for flow in [Flow::Capture, Flow::Render] {
            if !supports_flow(&device, flow) {
                continue;
            }
            let (format, compatible) = match stream_config(&device, flow) {
                Ok(config) => {
                    let compatible = is_compatible_format(&config, flow);
                    (format_config(&config), compatible)
                }
                Err(error) => (format!("unavailable: {error:#}"), false),
            };
            let endpoint = EndpointDescriptor {
                id: id.to_string(),
                name: description.name().to_owned(),
                format,
                compatible,
            };
            match flow {
                Flow::Capture => catalog.capture.push(endpoint),
                Flow::Render => catalog.render.push(endpoint),
            }
        }
    }

    catalog.capture.sort_by_key(|item| item.name.to_lowercase());
    catalog.render.sort_by_key(|item| item.name.to_lowercase());
    Ok(catalog)
}

pub fn resolve_endpoints(args: &EndpointArgs) -> Result<ResolvedEndpoints> {
    let host = cpal::default_host();
    let reference_flow = reference_flow(args.reference_mode);
    Ok(ResolvedEndpoints {
        mic: resolve_device(&host, &args.mic, Flow::Capture)
            .with_context(|| format!("could not resolve microphone '{}'", args.mic))?,
        reference: resolve_device(&host, &args.reference, reference_flow)
            .with_context(|| format!("could not resolve reference '{}'", args.reference))?,
        output: resolve_device(&host, &args.output, Flow::Render)
            .with_context(|| format!("could not resolve output '{}'", args.output))?,
    })
}

pub fn reference_flow(mode: ReferenceMode) -> Flow {
    match mode {
        ReferenceMode::Loopback => Flow::Render,
        ReferenceMode::Capture => Flow::Capture,
    }
}

pub fn stream_config(device: &cpal::Device, flow: Flow) -> Result<cpal::SupportedStreamConfig> {
    match flow {
        Flow::Capture => device
            .default_input_config()
            .context("endpoint has no default capture format"),
        Flow::Render => device
            .default_output_config()
            .context("endpoint has no default render format"),
    }
}

pub fn validate_format(
    label: &str,
    device: &cpal::Device,
    flow: Flow,
) -> Result<cpal::SupportedStreamConfig> {
    let config = stream_config(device, flow)?;
    if config.sample_rate() != 48_000 {
        bail!(
            "{label} must use 48 kHz shared-mode audio, but its default is {} Hz",
            config.sample_rate()
        );
    }
    if config.sample_format() != cpal::SampleFormat::F32 {
        bail!(
            "{label} must use f32 shared-mode audio, but its default is {}",
            config.sample_format()
        );
    }
    match flow {
        Flow::Capture if !matches!(config.channels(), 1 | 2) => {
            bail!(
                "{label} must expose one or two channels, but it exposes {}",
                config.channels()
            );
        }
        Flow::Render if config.channels() != 2 => {
            bail!(
                "{label} must expose two channels, but it exposes {}",
                config.channels()
            );
        }
        _ => {}
    }
    Ok(config)
}

fn is_compatible_format(config: &cpal::SupportedStreamConfig, flow: Flow) -> bool {
    config.sample_rate() == 48_000
        && config.sample_format() == cpal::SampleFormat::F32
        && match flow {
            Flow::Capture => matches!(config.channels(), 1 | 2),
            Flow::Render => config.channels() == 2,
        }
}

fn resolve_device(host: &cpal::Host, selector: &str, flow: Flow) -> Result<cpal::Device> {
    if let Ok(id) = cpal::DeviceId::from_str(selector) {
        let Some(device) = host.device_by_id(&id) else {
            bail!("no active endpoint has ID '{selector}'");
        };
        ensure_flow(&device, flow)?;
        return Ok(device);
    }

    let selector_folded = selector.to_lowercase();
    let mut exact = Vec::new();
    let mut partial = Vec::new();

    for device in host
        .devices()
        .context("failed to enumerate WASAPI endpoints")?
    {
        if !supports_flow(&device, flow) {
            continue;
        }
        let Ok(description) = device.description() else {
            continue;
        };
        let name = description.name();
        if name.eq_ignore_ascii_case(selector) {
            exact.push(device);
        } else if name.to_lowercase().contains(&selector_folded) {
            partial.push(device);
        }
    }

    let matches = if exact.is_empty() { partial } else { exact };
    match matches.len() {
        0 => bail!("no active {:?} endpoint matches '{selector}'", flow),
        1 => Ok(matches.into_iter().next().expect("one match disappeared")),
        _ => {
            let names = matches
                .iter()
                .filter_map(|device| device.description().ok())
                .map(|description| description.name().to_owned())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("selector '{selector}' is ambiguous; it matches: {names}")
        }
    }
}

fn print_resolved(label: &str, device: &cpal::Device, flow: Flow) -> Result<()> {
    let description = device.description()?;
    let id = device.id()?;
    let config = stream_config(device, flow)?;
    println!("{label}: {}", description.name());
    println!("  ID:     {id}");
    println!("  Format: {}\n", format_config(&config));
    Ok(())
}

fn ensure_flow(device: &cpal::Device, flow: Flow) -> Result<()> {
    if supports_flow(device, flow) {
        Ok(())
    } else {
        bail!("endpoint exists but does not support {:?} audio", flow)
    }
}

fn supports_flow(device: &cpal::Device, flow: Flow) -> bool {
    match flow {
        Flow::Capture => device.supports_input(),
        Flow::Render => device.supports_output(),
    }
}

pub fn format_config(config: &cpal::SupportedStreamConfig) -> String {
    format!(
        "{} Hz, {} channel(s), {}, buffer {:?}",
        config.sample_rate(),
        config.channels(),
        config.sample_format(),
        config.buffer_size()
    )
}
