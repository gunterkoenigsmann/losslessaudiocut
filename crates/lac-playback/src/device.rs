//! The audio device: a `cpal` output stream whose callback feeds from an
//! engine [`Output`].

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::PlaybackError;
use crate::engine::{Output, OutputFormat};

/// An open output stream. Dropping it stops the sound.
pub struct Device {
    /// Kept alive for the callback; never touched otherwise.
    _stream: cpal::Stream,
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Device")
    }
}

/// The default output device and its default format.
pub fn default_output() -> Result<(cpal::Device, cpal::SupportedStreamConfig), PlaybackError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(PlaybackError::NoDevice)?;
    let config = device
        .default_output_config()
        .map_err(|e| PlaybackError::Device(e.to_string()))?;
    Ok((device, config))
}

/// The engine format matching `config`.
pub fn format_of(config: &cpal::SupportedStreamConfig) -> OutputFormat {
    OutputFormat {
        sample_rate: config.sample_rate(),
        channels: usize::from(config.channels()),
    }
}

/// Opens a stream on `device` in `config` that plays `output`.
pub fn open(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    output: Output,
) -> Result<Device, PlaybackError> {
    let stream_config: StreamConfig = config.config();
    let stream = match config.sample_format() {
        SampleFormat::F32 => build::<f32>(device, stream_config, output),
        SampleFormat::I16 => build::<i16>(device, stream_config, output),
        SampleFormat::U16 => build::<u16>(device, stream_config, output),
        SampleFormat::I32 => build::<i32>(device, stream_config, output),
        SampleFormat::U8 => build::<u8>(device, stream_config, output),
        SampleFormat::F64 => build::<f64>(device, stream_config, output),
        other => {
            return Err(PlaybackError::Device(format!(
                "unsupported sample format {other}"
            )));
        }
    }?;
    stream
        .play()
        .map_err(|e| PlaybackError::Device(e.to_string()))?;
    Ok(Device { _stream: stream })
}

/// Builds the stream for device sample type `T`; the engine works in
/// `f32`, so other types are converted from a scratch buffer.
fn build<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut output: Output,
) -> Result<cpal::Stream, PlaybackError>
where
    T: SizedSample + FromSample<f32>,
{
    let mut scratch: Vec<f32> = Vec::new();
    device
        .build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], _| {
                if scratch.len() < data.len() {
                    // Once, or when the device enlarges its period.
                    scratch.resize(data.len(), 0.0);
                }
                let buf = &mut scratch[..data.len()];
                output.fill(buf);
                for (d, s) in data.iter_mut().zip(buf.iter()) {
                    *d = T::from_sample(*s);
                }
            },
            // Underruns and device changes: playback goes on or stays
            // silent; there is nobody to tell from this thread.
            |_| {},
            None,
        )
        .map_err(|e| PlaybackError::Device(e.to_string()))
}
