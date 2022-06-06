// Crossover: clean crossovers as a multi-out plugin
// Copyright (C) 2022 Robbert van der Helm
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

#![cfg_attr(feature = "simd", feature(portable_simd))]

#[cfg(not(feature = "simd"))]
compile_error!("Compiling without SIMD support is currently not supported");

use crossover::fir::{FirCrossover, FirCrossoverType};
use crossover::iir::{IirCrossover, IirCrossoverType};
use nih_plug::buffer::ChannelSamples;
use nih_plug::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod biquad;
mod crossover;

/// The number of bands. Not used directly here, but this avoids hardcoding some constants in the
/// crossover implementations.
pub const NUM_BANDS: usize = 5;

const MIN_CROSSOVER_FREQUENCY: f32 = 40.0;
const MAX_CROSSOVER_FREQUENCY: f32 = 20_000.0;

struct Crossover {
    params: Arc<CrossoverParams>,

    buffer_config: BufferConfig,

    /// Provides the LR24 crossover.
    iir_crossover: IirCrossover,
    /// Provides the linear-phase LR24 crossover.
    fir_crossover: FirCrossover,
    /// Set when the number of bands has changed and the filters must be updated.
    should_update_filters: Arc<AtomicBool>,
}

#[derive(Params)]
struct CrossoverParams {
    /// The number of bands between 2 and 5
    #[id = "bandcnt"]
    pub num_bands: IntParam,

    // We'll only provide frequency controls, as gain, panning, solo, mute etc. is all already
    // provided by Bitwig's UI
    #[id = "xov1fq"]
    pub crossover_1_freq: FloatParam,
    #[id = "xov2fq"]
    pub crossover_2_freq: FloatParam,
    #[id = "xov3fq"]
    pub crossover_3_freq: FloatParam,
    #[id = "xov4fq"]
    pub crossover_4_freq: FloatParam,

    // Having this parameter first or after the number of bands makes more sense, but this way the
    // band control plus the four crossovers fits exactly in Bitwig's parameter list
    #[id = "xovtyp"]
    pub crossover_type: EnumParam<CrossoverType>,
}

// The `non_exhaustive` is to prevent adding cases for latency compensation when adding more types
// later
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
#[non_exhaustive]
enum CrossoverType {
    #[id = "lr24"]
    #[name = "LR24"]
    LinkwitzRiley24,
    #[id = "lr24-lp"]
    #[name = "LR24 (LP)"]
    LinkwitzRiley24LinearPhase,
}

impl CrossoverParams {
    fn new(should_update_filters: Arc<AtomicBool>) -> Self {
        let crossover_range = FloatRange::Skewed {
            min: MIN_CROSSOVER_FREQUENCY,
            max: MAX_CROSSOVER_FREQUENCY,
            factor: FloatRange::skew_factor(-1.0),
        };
        let crossover_smoothing_style = SmoothingStyle::Logarithmic(100.0);
        let crossover_value_to_string = formatters::v2s_f32_hz_then_khz(0);
        let crossover_string_to_value = formatters::s2v_f32_hz_then_khz();

        Self {
            num_bands: IntParam::new(
                "Band Count",
                2,
                IntRange::Linear {
                    min: 2,
                    max: NUM_BANDS as i32,
                },
            )
            .with_callback({
                let should_update_filters = should_update_filters.clone();

                Arc::new(move |_| should_update_filters.store(true, Ordering::Relaxed))
            }),

            // TODO: More sensible default frequencies
            crossover_1_freq: FloatParam::new("Crossover 1", 200.0, crossover_range)
                .with_smoother(crossover_smoothing_style)
                .with_value_to_string(crossover_value_to_string.clone())
                .with_string_to_value(crossover_string_to_value.clone()),
            crossover_2_freq: FloatParam::new("Crossover 2", 1000.0, crossover_range)
                .with_smoother(crossover_smoothing_style)
                .with_value_to_string(crossover_value_to_string.clone())
                .with_string_to_value(crossover_string_to_value.clone()),
            crossover_3_freq: FloatParam::new("Crossover 3", 5000.0, crossover_range)
                .with_smoother(crossover_smoothing_style)
                .with_value_to_string(crossover_value_to_string.clone())
                .with_string_to_value(crossover_string_to_value.clone()),
            crossover_4_freq: FloatParam::new("Crossover 4", 10000.0, crossover_range)
                .with_smoother(crossover_smoothing_style)
                .with_value_to_string(crossover_value_to_string.clone())
                .with_string_to_value(crossover_string_to_value.clone()),

            crossover_type: EnumParam::new("Type", CrossoverType::LinkwitzRiley24).with_callback(
                Arc::new(move |_| should_update_filters.store(true, Ordering::Relaxed)),
            ),
        }
    }
}

impl Default for Crossover {
    fn default() -> Self {
        let should_update_filters = Arc::new(AtomicBool::new(false));

        Crossover {
            params: Arc::new(CrossoverParams::new(should_update_filters.clone())),

            buffer_config: BufferConfig {
                sample_rate: 1.0,
                min_buffer_size: None,
                max_buffer_size: 0,
                process_mode: ProcessMode::Realtime,
            },

            iir_crossover: IirCrossover::new(IirCrossoverType::LinkwitzRiley24),
            fir_crossover: FirCrossover::new(FirCrossoverType::LinkwitzRiley24LinearPhase),
            should_update_filters,
        }
    }
}

impl Plugin for Crossover {
    const NAME: &'static str = "Crossover";
    const VENDOR: &'static str = "Robbert van der Helm";
    const URL: &'static str = "https://github.com/robbert-vdh/nih-plug";
    const EMAIL: &'static str = "mail@robbertvanderhelm.nl";

    const VERSION: &'static str = "0.1.0";

    const DEFAULT_NUM_INPUTS: u32 = 2;
    const DEFAULT_NUM_OUTPUTS: u32 = 2;

    const DEFAULT_AUX_OUTPUTS: Option<AuxiliaryIOConfig> = Some(AuxiliaryIOConfig {
        // Two to five of these busses will be used at a time
        num_busses: 5,
        num_channels: 2,
    });

    const PORT_NAMES: PortNames = PortNames {
        main_input: None,
        // We won't output any sound here
        main_output: Some("The Void"),
        aux_inputs: None,
        aux_outputs: Some(&["Band 1", "Band 2", "Band 3", "Band 4", "Band 5"]),
    };

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn accepts_bus_config(&self, config: &BusConfig) -> bool {
        // Only do stereo
        config.num_input_channels == 2
            && config.num_output_channels == 2
            && config.aux_output_busses.num_channels == 2
    }

    fn initialize(
        &mut self,
        _bus_config: &BusConfig,
        buffer_config: &BufferConfig,
        context: &mut impl InitContext,
    ) -> bool {
        self.buffer_config = *buffer_config;

        // Make sure the filter states match the current parameters
        self.update_filters();

        // The FIR filters are linear-phase and introduce latency
        match self.params.crossover_type.value() {
            CrossoverType::LinkwitzRiley24 => (),
            CrossoverType::LinkwitzRiley24LinearPhase => {
                context.set_latency_samples(self.fir_crossover.latency())
            }
        }

        true
    }

    fn reset(&mut self) {
        self.iir_crossover.reset();
        self.fir_crossover.reset();
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext,
    ) -> ProcessStatus {
        // Right now both crossover types only do 24 dB/octave Linkwitz-Riley style crossovers
        match self.params.crossover_type.value() {
            CrossoverType::LinkwitzRiley24 => {
                Self::do_process(buffer, aux, |main_channel_samples, bands| {
                    // Only update the filters when needed
                    self.maybe_update_filters();

                    self.iir_crossover.process(
                        self.params.num_bands.value as usize,
                        main_channel_samples,
                        bands,
                    );
                })
            }
            CrossoverType::LinkwitzRiley24LinearPhase => {
                context.set_latency_samples(self.fir_crossover.latency());

                Self::do_process(buffer, aux, |main_channel_samples, bands| {
                    self.maybe_update_filters();

                    self.fir_crossover.process(
                        self.params.num_bands.value as usize,
                        main_channel_samples,
                        bands,
                    );
                })
            }
        }

        ProcessStatus::Normal
    }
}

impl Crossover {
    /// Takes care of all of the boilerplate in zipping the outputs together to get a nice iterator
    /// friendly and SIMD-able interface for the processing function. Prevents havign to branch per
    /// sample. The closure receives an input sample and it should write the output samples for each
    /// band to the array.
    fn do_process(
        buffer: &mut Buffer,
        aux: &mut AuxiliaryBuffers,
        mut f: impl FnMut(&ChannelSamples, [ChannelSamples; NUM_BANDS]),
    ) {
        let aux_outputs = &mut aux.outputs;
        let (band_1_buffer, aux_outputs) = aux_outputs.split_first_mut().unwrap();
        let (band_2_buffer, aux_outputs) = aux_outputs.split_first_mut().unwrap();
        let (band_3_buffer, aux_outputs) = aux_outputs.split_first_mut().unwrap();
        let (band_4_buffer, aux_outputs) = aux_outputs.split_first_mut().unwrap();
        let (band_5_buffer, _) = aux_outputs.split_first_mut().unwrap();

        // Snoclists for days
        for (
            (
                (
                    ((main_channel_samples, band_1_channel_samples), band_2_channel_samples),
                    band_3_channel_samples,
                ),
                band_4_channel_samples,
            ),
            band_5_channel_samples,
        ) in buffer
            .iter_samples()
            .zip(band_1_buffer.iter_samples())
            .zip(band_2_buffer.iter_samples())
            .zip(band_3_buffer.iter_samples())
            .zip(band_4_buffer.iter_samples())
            .zip(band_5_buffer.iter_samples())
        {
            // We can avoid a lot of hardcoding and conditionals by restoring the original array structure
            let bands = [
                band_1_channel_samples,
                band_2_channel_samples,
                band_3_channel_samples,
                band_4_channel_samples,
                band_5_channel_samples,
            ];

            f(&main_channel_samples, bands);

            // The main output should be silent as the signal is already evenly split over the other
            // bands
            for sample in main_channel_samples {
                *sample = 0.0;
            }
        }
    }

    /// Update the filter coefficients for the crossovers, but only if it's needed.
    fn maybe_update_filters(&mut self) {
        if self
            .should_update_filters
            .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
            || self.params.crossover_1_freq.smoothed.is_smoothing()
            || self.params.crossover_2_freq.smoothed.is_smoothing()
            || self.params.crossover_3_freq.smoothed.is_smoothing()
            || self.params.crossover_4_freq.smoothed.is_smoothing()
        {
            self.update_filters();
        }
    }

    /// Update the filter coefficients for the crossovers.
    fn update_filters(&mut self) {
        let crossover_frequencies = [
            self.params.crossover_1_freq.smoothed.next(),
            self.params.crossover_2_freq.smoothed.next(),
            self.params.crossover_3_freq.smoothed.next(),
            self.params.crossover_4_freq.smoothed.next(),
        ];

        match self.params.crossover_type.value() {
            CrossoverType::LinkwitzRiley24 => self.iir_crossover.update(
                self.buffer_config.sample_rate,
                self.params.num_bands.value as usize,
                crossover_frequencies,
            ),
            CrossoverType::LinkwitzRiley24LinearPhase => self.fir_crossover.update(
                self.buffer_config.sample_rate,
                self.params.num_bands.value as usize,
                crossover_frequencies,
            ),
        }
    }
}

impl ClapPlugin for Crossover {
    const CLAP_ID: &'static str = "nl.robbertvanderhelm.crossover";
    const CLAP_DESCRIPTION: &'static str = "Cleanly split a signal into multiple bands";
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Stereo,
        ClapFeature::Utility,
    ];
    const CLAP_MANUAL_URL: &'static str = Self::URL;
    const CLAP_SUPPORT_URL: &'static str = Self::URL;
}

impl Vst3Plugin for Crossover {
    const VST3_CLASS_ID: [u8; 16] = *b"CrossoverRvdH...";
    const VST3_CATEGORIES: &'static str = "Fx|Tools";
}

nih_export_clap!(Crossover);
nih_export_vst3!(Crossover);
