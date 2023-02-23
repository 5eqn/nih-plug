use std::num::NonZeroU32;

use anyhow::{Context, Result};
use cpal::{
    traits::*, Device, InputCallbackInfo, OutputCallbackInfo, Sample, SampleFormat, Stream,
    StreamConfig,
};
use crossbeam::sync::{Parker, Unparker};
use rtrb::RingBuffer;

use super::super::config::WrapperConfig;
use super::Backend;
use crate::audio_setup::{AudioIOLayout, AuxiliaryBuffers};
use crate::buffer::Buffer;
use crate::context::process::Transport;
use crate::midi::{MidiConfig, PluginNoteEvent};
use crate::plugin::Plugin;

/// Uses CPAL for audio and midir for MIDI.
pub struct Cpal {
    config: WrapperConfig,
    audio_io_layout: AudioIOLayout,

    input: Option<(Device, StreamConfig, SampleFormat)>,

    output_device: Device,
    output_config: StreamConfig,
    output_sample_format: SampleFormat,
    // TODO: MIDI
}

impl<P: Plugin> Backend<P> for Cpal {
    fn run(
        &mut self,
        cb: impl FnMut(
                &mut Buffer,
                &mut AuxiliaryBuffers,
                Transport,
                &[PluginNoteEvent<P>],
                &mut Vec<PluginNoteEvent<P>>,
            ) -> bool
            + 'static
            + Send,
    ) {
        // The CPAL audio devices may not accept floating point samples, so all of the actual audio
        // handling and buffer management handles in the `build_*_data_callback()` functions defined
        // below.

        // CPAL does not support duplex streams, so audio input (when enabled, inputs aren't
        // connected by default) waits a read a period of data before starting the output stream
        let mut _input_stream: Option<Stream> = None;
        let mut input_rb_consumer: Option<rtrb::Consumer<f32>> = None;
        if let Some((input_device, input_config, input_sample_format)) = &self.input {
            // Data is sent to the output data callback using a wait-free ring buffer
            let (rb_producer, rb_consumer) = RingBuffer::new(
                self.output_config.channels as usize * self.config.period_size as usize,
            );
            input_rb_consumer = Some(rb_consumer);

            let input_parker = Parker::new();
            let input_unparker = input_parker.unparker().clone();
            let error_cb = {
                let input_unparker = input_unparker.clone();
                move |err| {
                    nih_error!("Error during capture: {err:#}");
                    input_unparker.clone().unpark();
                }
            };

            let stream = match input_sample_format {
                SampleFormat::I16 => input_device.build_input_stream(
                    input_config,
                    self.build_input_data_callback::<i16>(input_unparker, rb_producer),
                    error_cb,
                ),
                SampleFormat::U16 => input_device.build_input_stream(
                    input_config,
                    self.build_input_data_callback::<u16>(input_unparker, rb_producer),
                    error_cb,
                ),
                SampleFormat::F32 => input_device.build_input_stream(
                    input_config,
                    self.build_input_data_callback::<f32>(input_unparker, rb_producer),
                    error_cb,
                ),
            }
            .expect("Fatal error creating the capture stream");
            stream
                .play()
                .expect("Fatal error trying to start the capture stream");
            _input_stream = Some(stream);

            // Playback is delayed one period if we're capturing audio so it has something to process
            input_parker.park()
        }

        // This thread needs to be blocked until audio processing ends as CPAL processes the streams
        // on another thread instead of blocking
        let parker = Parker::new();
        let unparker = parker.unparker().clone();
        let error_cb = {
            let unparker = unparker.clone();
            move |err| {
                nih_error!("Error during playback: {err:#}");
                unparker.clone().unpark();
            }
        };

        let output_stream = match self.output_sample_format {
            SampleFormat::I16 => self.output_device.build_output_stream(
                &self.output_config,
                self.build_output_data_callback::<P, i16>(unparker, input_rb_consumer, cb),
                error_cb,
            ),
            SampleFormat::U16 => self.output_device.build_output_stream(
                &self.output_config,
                self.build_output_data_callback::<P, u16>(unparker, input_rb_consumer, cb),
                error_cb,
            ),
            SampleFormat::F32 => self.output_device.build_output_stream(
                &self.output_config,
                self.build_output_data_callback::<P, f32>(unparker, input_rb_consumer, cb),
                error_cb,
            ),
        }
        .expect("Fatal error creating the output stream");

        // TODO: Wait a period before doing this when also reading the input
        output_stream
            .play()
            .expect("Fatal error trying to start the output stream");

        // Wait for the audio thread to exit
        parker.park();
    }
}

impl Cpal {
    /// Initialize the backend with the specified host. Returns an error if this failed for whatever
    /// reason.
    pub fn new<P: Plugin>(config: WrapperConfig, cpal_host_id: cpal::HostId) -> Result<Self> {
        let audio_io_layout = config.audio_io_layout_or_exit::<P>();
        let host = cpal::host_from_id(cpal_host_id).context("The Audio API is unavailable")?;

        if config.input_device.is_none() && audio_io_layout.main_input_channels.is_some() {
            nih_log!(
                "Audio inputs are not connected automatically to prevent feedback. Use the \
                 '--input-device' option to choose an input device."
            )
        }

        // No input device is connected unless requested by the user to avoid feedback loops
        let input_device = config
            .input_device
            .as_ref()
            .map(|name| -> Result<Device> {
                let device = host
                    .input_devices()
                    .context("No audio input devices available")?
                    // `.name()` returns a `Result` with a non-Eq error type so you can't compare this
                    // directly
                    .find(|d| d.name().as_deref().map(|n| n == name).unwrap_or(false))
                    .with_context(|| {
                        // This is a bit awkward, but instead of adding a dedicated option we'll just
                        // list all of the available devices in the error message when the chosen device
                        // does not exist
                        let mut message =
                            format!("Unknown input device '{name}'. Available devices are:");
                        for device_name in host.input_devices().unwrap().flat_map(|d| d.name()) {
                            message.push_str(&format!("\n{device_name}"))
                        }

                        message
                    })?;

                Ok(device)
            })
            .transpose()?;

        let output_device = match config.output_device.as_ref() {
            Some(name) => host
                .output_devices()
                .context("No audio output devices available")?
                .find(|d| d.name().as_deref().map(|n| n == name).unwrap_or(false))
                .with_context(|| {
                    let mut message =
                        format!("Unknown output device '{name}'. Available devices are:");
                    for device_name in host.output_devices().unwrap().flat_map(|d| d.name()) {
                        message.push_str(&format!("\n{device_name}"))
                    }

                    message
                })?,
            None => host
                .default_output_device()
                .context("No default audio output device available")?,
        };

        let requested_sample_rate = cpal::SampleRate(config.sample_rate as u32);
        let requested_buffer_size = cpal::BufferSize::Fixed(config.period_size);
        let num_input_channels = audio_io_layout
            .main_input_channels
            .map(NonZeroU32::get)
            .unwrap_or_default() as usize;
        let input = input_device
            .map(|device| -> Result<(Device, StreamConfig, SampleFormat)> {
                let input_configs: Vec<_> = device
                    .supported_input_configs()
                    .context("Could not get supported audio input configurations")?
                    .filter(|c| match c.buffer_size() {
                        cpal::SupportedBufferSize::Range { min, max } => {
                            c.channels() as usize == num_input_channels
                                && (c.min_sample_rate()..=c.max_sample_rate())
                                    .contains(&requested_sample_rate)
                                && (min..=max).contains(&&config.period_size)
                        }
                        cpal::SupportedBufferSize::Unknown => false,
                    })
                    .collect();
                let input_config_range = input_configs
                    .iter()
                    // Prefer floating point samples to avoid conversions
                    .find(|c| c.sample_format() == SampleFormat::F32)
                    .or_else(|| input_configs.first())
                    .cloned()
                    .with_context(|| {
                        format!(
                            "The audio input device does not support {} audio channels at a \
                             sample rate of {} Hz and a period size of {} samples",
                            num_input_channels, config.sample_rate, config.period_size,
                        )
                    })?;

                // We already checked that these settings are valid
                let input_config = StreamConfig {
                    channels: input_config_range.channels(),
                    sample_rate: requested_sample_rate,
                    buffer_size: requested_buffer_size.clone(),
                };
                let input_sample_format = input_config_range.sample_format();

                Ok((device, input_config, input_sample_format))
            })
            .transpose()?;

        let num_output_channels = audio_io_layout
            .main_output_channels
            .map(NonZeroU32::get)
            .unwrap_or_default() as usize;
        let output_configs: Vec<_> = output_device
            .supported_output_configs()
            .context("Could not get supported audio output configurations")?
            .filter(|c| match c.buffer_size() {
                cpal::SupportedBufferSize::Range { min, max } => {
                    c.channels() as usize == num_output_channels
                        && (c.min_sample_rate()..=c.max_sample_rate())
                            .contains(&requested_sample_rate)
                        && (min..=max).contains(&&config.period_size)
                }
                cpal::SupportedBufferSize::Unknown => false,
            })
            .collect();
        let output_config_range = output_configs
            .iter()
            .find(|c| c.sample_format() == SampleFormat::F32)
            .or_else(|| output_configs.first())
            .cloned()
            .with_context(|| {
                format!(
                    "The audio output device does not support {} audio channels at a sample rate \
                     of {} Hz and a period size of {} samples",
                    num_output_channels, config.sample_rate, config.period_size,
                )
            })?;
        let output_config = StreamConfig {
            channels: output_config_range.channels(),
            sample_rate: requested_sample_rate,
            buffer_size: requested_buffer_size,
        };
        let output_sample_format = output_config_range.sample_format();

        // TODO: Implement MIDI support
        if P::MIDI_INPUT >= MidiConfig::Basic || P::MIDI_OUTPUT >= MidiConfig::Basic {
            nih_log!("Audio-only, MIDI input and output has not been implemented yet.");
        }

        // There's no obvious way to do sidechain inputs and additional outputs with the CPAL
        // backends like there is with JACK. So we'll just provide empty buffers instead.
        if !audio_io_layout.aux_input_ports.is_empty() {
            nih_warn!("Sidechain inputs are not supported with this audio backend");
        }
        if !audio_io_layout.aux_output_ports.is_empty() {
            nih_warn!("Auxiliary outputs are not supported with this audio backend");
        }

        Ok(Cpal {
            config,
            audio_io_layout,

            input,

            output_device,
            output_config,
            output_sample_format,
        })
    }

    fn build_input_data_callback<T: Sample>(
        &self,
        input_unparker: Unparker,
        mut input_rb_producer: rtrb::Producer<f32>,
    ) -> impl FnMut(&[T], &InputCallbackInfo) + Send + 'static {
        // This callback needs to copy input samples to a ring buffer that can be read from in the
        // output data callback
        move |data, _info| {
            for sample in data {
                // If for whatever reason the input callback is fired twice before an output
                // callback, then just spin on this until the push succeeds
                while input_rb_producer.push(sample.to_f32()).is_err() {}
            }

            // The run function is blocked until a single period has been processed here. After this
            // point output playback can start.
            input_unparker.unpark();
        }
    }

    fn build_output_data_callback<P: Plugin, T: Sample>(
        &self,
        unparker: Unparker,
        mut input_rb_consumer: Option<rtrb::Consumer<f32>>,
        mut cb: impl FnMut(
                &mut Buffer,
                &mut AuxiliaryBuffers,
                Transport,
                &[PluginNoteEvent<P>],
                &mut Vec<PluginNoteEvent<P>>,
            ) -> bool
            + 'static
            + Send,
    ) -> impl FnMut(&mut [T], &OutputCallbackInfo) + Send + 'static {
        // We'll receive interlaced input samples from CPAL. These need to converted to deinterlaced
        // channels, processed, and then copied those back to an interlaced buffer for the output.
        // This needs to be wrapped in a struct like this and boxed because the `channels` vectors
        // need to live just as long as `buffer` when they get moved into the closure.
        // FIXME: This is pretty nasty, come up with a cleaner alternative
        let num_output_channels = self
            .audio_io_layout
            .main_output_channels
            .map(NonZeroU32::get)
            .unwrap_or_default() as usize;
        let mut channels =
            vec![vec![0.0f32; self.config.period_size as usize]; num_output_channels];
        let mut buffer = Buffer::default();
        unsafe {
            buffer.set_slices(0, |output_slices| {
                // Pre-allocate enough storage, the pointers are set in the data callback because
                // `channels` will have been moved between now and the next callback
                output_slices.resize_with(channels.len(), || &mut []);
            })
        }

        // We'll do the same thing for auxiliary inputs and outputs, so the plugin always gets the
        // buffers it expects
        let mut aux_input_storage: Vec<Vec<Vec<f32>>> = Vec::new();
        let mut aux_input_buffers: Vec<Buffer> = Vec::new();
        for channel_count in self.audio_io_layout.aux_input_ports {
            aux_input_storage.push(vec![
                vec![0.0f32; self.config.period_size as usize];
                channel_count.get() as usize
            ]);

            // We'll preallocate the slices, but we'll only assign them to point to
            // `aux_input_storage` at the start of the audio callback
            let mut aux_buffer = Buffer::default();
            unsafe {
                aux_buffer.set_slices(self.config.period_size as usize, |output_slices| {
                    output_slices.resize_with(channel_count.get() as usize, || &mut []);
                })
            }
            aux_input_buffers.push(aux_buffer);
        }

        let mut aux_output_storage: Vec<Vec<Vec<f32>>> = Vec::new();
        let mut aux_output_buffers: Vec<Buffer> = Vec::new();
        for channel_count in self.audio_io_layout.aux_output_ports {
            aux_output_storage.push(vec![
                vec![0.0f32; self.config.period_size as usize];
                channel_count.get() as usize
            ]);

            let mut aux_buffer = Buffer::default();
            unsafe {
                aux_buffer.set_slices(self.config.period_size as usize, |output_slices| {
                    output_slices.resize_with(channel_count.get() as usize, || &mut []);
                })
            }
            aux_output_buffers.push(aux_buffer);
        }

        // TODO: MIDI input and output
        let midi_input_events = Vec::with_capacity(1024);
        let mut midi_output_events = Vec::with_capacity(1024);

        // Can't borrow from `self` in the callback
        let config = self.config.clone();
        let mut num_processed_samples = 0;
        move |data, _info| {
            // Things may have been moved in between callbacks, so these pointers need to be set up
            // again on each invocation
            unsafe {
                buffer.set_slices(config.period_size as usize, |output_slices| {
                    for (output_slice, channel) in output_slices.iter_mut().zip(channels.iter_mut())
                    {
                        // SAFETY: `channels` is no longer used directly after this, and it outlives
                        // the data closure
                        *output_slice = &mut *(channel.as_mut_slice() as *mut [f32]);
                    }
                })
            }

            for (aux_buffer, aux_storage) in aux_input_buffers
                .iter_mut()
                .zip(aux_input_storage.iter_mut())
            {
                unsafe {
                    aux_buffer.set_slices(config.period_size as usize, |output_slices| {
                        for (output_slice, channel) in
                            output_slices.iter_mut().zip(aux_storage.iter_mut())
                        {
                            // SAFETY: `aux_input_storage` is no longer used directly after this,
                            //         and it outlives the data closure
                            *output_slice = &mut *(channel.as_mut_slice() as *mut [f32]);
                        }
                    })
                }
            }
            for (aux_buffer, aux_storage) in aux_output_buffers
                .iter_mut()
                .zip(aux_output_storage.iter_mut())
            {
                unsafe {
                    aux_buffer.set_slices(config.period_size as usize, |output_slices| {
                        for (output_slice, channel) in
                            output_slices.iter_mut().zip(aux_storage.iter_mut())
                        {
                            // SAFETY: `aux_output_storage` is no longer used directly after this,
                            //         and it outlives the data closure
                            *output_slice = &mut *(channel.as_mut_slice() as *mut [f32]);
                        }
                    })
                }
            }

            let mut transport = Transport::new(config.sample_rate);
            transport.pos_samples = Some(num_processed_samples);
            transport.tempo = Some(config.tempo as f64);
            transport.time_sig_numerator = Some(config.timesig_num as i32);
            transport.time_sig_denominator = Some(config.timesig_denom as i32);
            transport.playing = true;

            // If an input was configured, then the output buffer is filled with (interleaved) input
            // samples. Otherwise it gets filled with silence.
            match &mut input_rb_consumer {
                Some(input_rb_consumer) => {
                    for channels in buffer.iter_samples() {
                        for sample in channels {
                            loop {
                                // Keep spinning on this if the output callback somehow outpaces the
                                // input callback
                                if let Ok(input_sample) = input_rb_consumer.pop() {
                                    *sample = input_sample;
                                    break;
                                }
                            }
                        }
                    }
                }
                None => {
                    for channel in buffer.as_slice() {
                        channel.fill(0.0);
                    }
                }
            }

            // The CPAL backends don't support auxiliary IO, so we'll just zero them out. The
            // buffers are still provided to the wrapped plugin since it should not expect the
            // wrapper/host to deviate from its audio IO layouts.
            for aux_buffer in &mut aux_input_buffers {
                for channel in aux_buffer.as_slice() {
                    channel.fill(0.0);
                }
            }
            for aux_buffer in &mut aux_output_buffers {
                for channel in aux_buffer.as_slice() {
                    channel.fill(0.0);
                }
            }

            // SAFETY: Shortening these borrows is safe as even if the plugin overwrites the
            //         slices (which it cannot do without using unsafe code), then they
            //         would still be reset on the next iteration
            let mut aux = unsafe {
                AuxiliaryBuffers {
                    inputs: &mut *(aux_input_buffers.as_mut_slice() as *mut [Buffer]),
                    outputs: &mut *(aux_output_buffers.as_mut_slice() as *mut [Buffer]),
                }
            };

            midi_output_events.clear();
            if !cb(
                &mut buffer,
                &mut aux,
                transport,
                &midi_input_events,
                &mut midi_output_events,
            ) {
                // TODO: Some way to immediately terminate the stream here would be nice
                unparker.unpark();
                return;
            }

            // The buffer's samples need to be written to `data` in an interlaced format
            for (output_sample, buffer_sample) in data.iter_mut().zip(
                buffer
                    .iter_samples()
                    .flat_map(|channels| channels.into_iter()),
            ) {
                *output_sample = T::from(buffer_sample);
            }

            // TODO: Handle MIDI output events

            num_processed_samples += buffer.samples() as i64;
        }
    }
}
