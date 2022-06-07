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

use nih_plug::buffer::ChannelSamples;
use nih_plug::debug::*;
use std::f32;
use std::simd::{f32x2, StdFloat};

use crate::crossover::iir::biquad::{Biquad, BiquadCoefficients, NEUTRAL_Q};
use crate::NUM_BANDS;

// TODO: Move this to FFT convolution so we can increase the filter size and improve low latency performance

/// The size of the FIR filter window, or the number of taps. The low frequency performance is
/// greatly limited by this.
const FILTER_SIZE: usize = 121;
/// The size of the FIR filter's ring buffer. This is `FILTER_SIZE` rounded up to the next power of
/// two.
const RING_BUFFER_SIZE: usize = FILTER_SIZE.next_power_of_two();

#[derive(Debug)]
pub struct FirCrossover {
    /// The kind of crossover to use. `.update_filters()` must be called after changing this.
    mode: FirCrossoverType,

    /// Filters for each of the bands. Depending on the number of bands argument passed to
    /// `.process()` two to five of these may be used. The first one always contains a low-pass
    /// filter, the last one always contains a high-pass filter, while the other bands will contain
    /// band-pass filters.
    band_filters: [FirFilter; NUM_BANDS],
}

/// The type of FIR crossover to use.
#[derive(Debug, Clone, Copy)]
pub enum FirCrossoverType {
    /// Emulates the filter slope of [`super::iir::IirCrossoverType`], but with linear-phase FIR
    /// filters instead of minimum-phase IIR filters. The exact same filters are used to design the
    /// FIR filters.
    LinkwitzRiley24LinearPhase,
}

/// A single FIR filter that may be configured in any way. In this plugin this will be a
/// linear-phase low-pass, band-pass, or high-pass filter.
#[derive(Debug, Clone)]
struct FirFilter {
    /// The coefficients for this filter. The filters for both channels should be equivalent, this
    /// just avoids broadcasts in the filter process.
    ///
    /// TODO: Profile to see if storing this as f32x2 rather than f32s plus splatting makes any
    ///       difference in performance at all
    pub coefficients: FirCoefficients,

    /// A ring buffer storing the last `FILTER_SIZE - 1` samples. The capacity is `FILTER_SIZE`
    /// rounded up to the next power of two.
    delay_buffer: [f32x2; RING_BUFFER_SIZE],
    /// The index in `delay_buffer` to write the next sample to. Wrapping negative indices back to
    /// the end, the previous sample can be found at `delay_buffer[delay_buffer_next_idx - 1]`, the
    /// one before that at `delay_buffer[delay_buffer_next_idx - 2]`, and so on.
    delay_buffer_next_idx: usize,
}

/// Coefficients for an FIR filter. This struct includes ways to design the filter. Parameterized
/// over `f32x2` only for the time being since that's what we need here.
#[repr(transparent)]
#[derive(Debug, Clone)]
struct FirCoefficients([f32x2; FILTER_SIZE]);

impl Default for FirFilter {
    fn default() -> Self {
        Self {
            coefficients: FirCoefficients::default(),
            delay_buffer: [f32x2::default(); RING_BUFFER_SIZE],
            delay_buffer_next_idx: 0,
        }
    }
}

impl Default for FirCoefficients {
    fn default() -> Self {
        // Initialize this to a delay with the same amount of latency as we'd introduce with our
        // linear-phase filters
        let mut coefficients = [f32x2::default(); FILTER_SIZE];
        coefficients[FILTER_SIZE / 2] = f32x2::splat(1.0);

        Self(coefficients)
    }
}

impl FirCrossover {
    /// Create a new multiband crossover processor. All filters will be configured to pass audio
    /// through as is, albeit with a delay. `.update()` needs to be called first to set up the
    /// filters, and `.reset()` can be called whenever the filter state must be cleared.
    ///
    /// Make sure to add the latency reported by [`latency()`][Self::latency()] to the plugin's
    /// reported latency.
    pub fn new(mode: FirCrossoverType) -> Self {
        Self {
            mode,
            band_filters: Default::default(),
        }
    }

    /// Get the current latency in samples. This depends on the selected mode.
    pub fn latency(&self) -> u32 {
        // Actually, that's a lie, since we currently only do linear-phase filters with a constant
        // size
        match self.mode {
            FirCrossoverType::LinkwitzRiley24LinearPhase => (FILTER_SIZE / 2) as u32,
        }
    }

    /// Split the signal into bands using the crossovers previously configured through `.update()`.
    /// The split bands will be written to `band_outputs`. `main_io` is not written to, and should
    /// be cleared separately.
    pub fn process(
        &mut self,
        num_bands: usize,
        main_io: &ChannelSamples,
        band_outputs: [ChannelSamples; NUM_BANDS],
    ) {
        nih_debug_assert!(num_bands >= 2);
        nih_debug_assert!(num_bands <= NUM_BANDS);
        // Required for the SIMD, so we'll just do a hard assert or the unchecked conversions will
        // be unsound
        assert!(main_io.len() == 2);

        let samples: f32x2 = unsafe { main_io.to_simd_unchecked() };
        match self.mode {
            FirCrossoverType::LinkwitzRiley24LinearPhase => {
                // TODO: Everything is structured to be fast to compute for the IIR filters. Instead
                //       of doing two channels at the same time, it would probably be faster to use
                //       SIMD for the actual convolution so we can do 4 or 8 multiply-adds at the
                //       same time. Or perhaps a better way to spend the time, use FFT convolution
                //       for this.
                for (filter, mut output) in self
                    .band_filters
                    .iter_mut()
                    .zip(band_outputs)
                    .take(num_bands)
                {
                    let filtered_samples = filter.process(samples);

                    unsafe { output.from_simd_unchecked(filtered_samples) };
                }
            }
        }
    }

    /// Update the crossover frequencies for all filters. `num_bands` is assumed to be in `[2,
    /// NUM_BANDS]`.
    pub fn update(
        &mut self,
        sample_rate: f32,
        num_bands: usize,
        frequencies: [f32; NUM_BANDS - 1],
    ) {
        match self.mode {
            FirCrossoverType::LinkwitzRiley24LinearPhase => {
                // The goal here is to design 2-5 filters with the same frequency response
                // magnitudes as the split bands in the IIR LR24 crossover version with the same
                // center frequencies would have. The algorithm works in two stages. First, the IIR
                // low-pass filters for the 1-4 crossovers used in the equivalent IIR LR24 version
                // are computed and converted to equivalent linear-phase FIR filters using the
                // algorithm described below in `FirCoefficients`. Then these are used to build the
                // coefficients for the 2-5 bands:
                //
                // - The first band is always simply the first band's
                //   low-pass filter.
                // - The middle bands are band-pass filters. These are created by taking the next
                //   crossover's low-pass filter and subtracting the accumulated band impulse
                //   response up to that point. The accumulated band impulse response is initialized
                //   with the first band's low-pass filter, and the band-pass filter for every band
                //   after that gets added to it.
                // - The final band is a high-pass filter that's computed through spectral inversion
                //   from the accumulated band impulse response.

                // As explained above, we'll start with the low-pass band
                nih_debug_assert!(num_bands >= 2);
                let iir_coefs = BiquadCoefficients::lowpass(sample_rate, frequencies[0], NEUTRAL_Q);
                let lp_fir_coefs =
                    FirCoefficients::design_fourth_order_linear_phase_low_pass_from_biquad(
                        iir_coefs,
                    );
                self.band_filters[0].coefficients = lp_fir_coefs;

                // For the band-pass filters and the final high-pass filter, we need to keep track
                // of the accumulated impulse response
                let mut accumulated_ir = self.band_filters[0].coefficients.clone();
                for (split_frequency, band_filter) in frequencies
                    .iter()
                    .zip(self.band_filters.iter_mut())
                    // There are `num_bands` bands, so there are `num_bands - 1` crossovers. The
                    // last band is formed from the accumulated impulse response.
                    .take(num_bands - 1)
                    // And the first band is already taken care of
                    .skip(1)
                {
                    let iir_coefs =
                        BiquadCoefficients::lowpass(sample_rate, *split_frequency, NEUTRAL_Q);
                    let lp_fir_coefs =
                        FirCoefficients::design_fourth_order_linear_phase_low_pass_from_biquad(
                            iir_coefs,
                        );

                    // We want the band between the accumulated frequency response and the next
                    // crossover's low-pass filter
                    let mut fir_bp_coefs = lp_fir_coefs;
                    for (bp_coef, accumulated_coef) in
                        fir_bp_coefs.0.iter_mut().zip(accumulated_ir.0.iter_mut())
                    {
                        // At this poing `bp_coef` is the low-pass filter
                        *bp_coef -= *accumulated_coef;

                        // And the accumulated coefficients for the next band/for the high-pass
                        // filter should contain this band-pass filter. This becomes a bit weirder
                        // to read when it's a single loop, but essentially this is what's going on
                        // here:
                        //
                        //     fir_bp_coefs = fir_lp_coefs - accumulated_ir
                        //     accumulated_ir += fir_bp_coefs

                        *accumulated_coef += *bp_coef;
                    }

                    band_filter.coefficients = fir_bp_coefs;
                }

                // And finally we can do a spectral inversion of the accumulated IR to the the last
                // band's high-pass filter
                let mut fir_hp_coefs = accumulated_ir;
                for coef in fir_hp_coefs.0.iter_mut() {
                    *coef = -*coef;
                }
                fir_hp_coefs.0[FILTER_SIZE / 2] += f32x2::splat(1.0);

                self.band_filters[num_bands - 1].coefficients = fir_hp_coefs;
            }
        }
    }

    /// Reset the internal filter state for all crossovers.
    pub fn reset(&mut self) {
        for filter in &mut self.band_filters {
            filter.reset();
        }
    }
}

impl FirFilter {
    /// Process left and right audio samples through the filter.
    pub fn process(&mut self, samples: f32x2) -> f32x2 {
        // TODO: Replace direct convolution with FFT convolution, would make the implementation much
        //       more complex though because of the multi output part
        let coefficients = &self.coefficients.0;
        let mut result = coefficients[0] * samples;

        // Now multiply `self.coefficients[1..]` with the delay buffer starting at
        // `self.delay_buffer_next_idx - 1`, wrapping around to the end when that is reached
        // The end index is exclusive, and we already did the multiply+add for the first coefficient.
        let before_wraparound_start_idx = self
            .delay_buffer_next_idx
            .saturating_sub(coefficients.len() - 1);
        let before_wraparound_end_idx = self.delay_buffer_next_idx;
        let num_before_wraparound = before_wraparound_end_idx - before_wraparound_start_idx;
        for (coefficient, delayed_sample) in coefficients[1..1 + num_before_wraparound].iter().zip(
            self.delay_buffer[before_wraparound_start_idx..before_wraparound_end_idx]
                .iter()
                .rev(),
        ) {
            // `result += coefficient * sample`, but with explicit FMA
            result = coefficient.mul_add(*delayed_sample, result);
        }

        let after_wraparound_begin_idx =
            self.delay_buffer.len() - (coefficients.len() - num_before_wraparound);
        let after_wraparound_end_idx = self.delay_buffer.len();
        for (coefficient, delayed_sample) in coefficients[1 + num_before_wraparound..].iter().zip(
            self.delay_buffer[after_wraparound_begin_idx..after_wraparound_end_idx]
                .iter()
                .rev(),
        ) {
            result = coefficient.mul_add(*delayed_sample, result);
        }

        // And finally write the samples to the delay buffer for the enxt sample
        self.delay_buffer[self.delay_buffer_next_idx] = samples;
        self.delay_buffer_next_idx = (self.delay_buffer_next_idx + 1) % self.delay_buffer.len();

        result
    }

    /// Reset the internal filter state.
    pub fn reset(&mut self) {
        self.delay_buffer.fill(f32x2::default());
        self.delay_buffer_next_idx = 0;
    }
}

impl FirCoefficients {
    /// A somewhat crude but very functional and relatively fast way create linear phase FIR
    /// **low-pass** filter that matches the frequency response of a fourth order biquad low-pass
    /// filter. As in, this matches the frequency response magnitudes of applying those biquads to a
    /// signal twice. This only works for low-pass filters, as the function normalizes the result to
    /// hae unity gain at the DC bin. The algorithm works as follows:
    ///
    /// - An impulse function (so all zeroes except for the first element) of length `FILTER_LEN / 2
    ///   + 1` is filtered with the biquad.
    /// - The biquad's state is reset, and the impulse response is filtered in the opposite
    ///   direction.
    /// - At this point the bidirectionally filtered impulse response contains the **right** half of
    ///   a truncated linear phase FIR kernel.
    ///
    /// Since the FIR filter will be a symmetrical version of this impulse response, we can optimize
    /// the post-processing work slightly by windowing and normalizing this bidirectionally filtered
    /// impulse response instead.
    ///
    /// - A half Blackman window is applied to the impulse response. Since this is the right half,
    ///   this starts at unity gain for the first sample and then tapers off towards the right.
    /// - The impulse response is then normalized such that the final linear-phase FIR kernel has a
    ///   sum of 1.0. Since it will be symmetrical around the IRs first sample, the would-be final
    ///   sum can be computed as `ir.sum() * 2 - ir[0]`.
    ///
    /// Lastly the linear phase FIR filter simply needs to be constructed from this right half:
    ///
    /// - This bidirectionally filtered impulse response is then reversed, and placed at the start
    ///   of the `FILTER_LEN` size FIR coefficient array.
    /// - The non-reversed bidirectionally filtered impulse response is copied to the second half of
    ///   the coefficients. (one of the copies doesn't need to include the centermost coefficient)
    ///
    /// The corresponding high-pass filter can be computed through spectral inversion.
    pub fn design_fourth_order_linear_phase_low_pass_from_biquad(
        biquad_coefs: BiquadCoefficients<f32x2>,
    ) -> Self {
        const CENTER_IDX: usize = FILTER_SIZE / 2;

        // We'll start with an impulse (at exactly half of this odd sized buffer)...
        let mut impulse_response = [f32x2::default(); FILTER_SIZE];
        impulse_response[CENTER_IDX] = f32x2::splat(1.0);

        // ...and filter that in both directions
        let mut biquad = Biquad::default();
        biquad.coefficients = biquad_coefs;
        for sample in impulse_response.iter_mut().skip(CENTER_IDX - 1) {
            *sample = biquad.process(*sample);
        }

        biquad.reset();
        for sample in impulse_response.iter_mut().skip(CENTER_IDX - 1).rev() {
            *sample = biquad.process(*sample);
        }

        // Now the right half of `impulse_response` contains a truncated right half of the
        // linear-phase FIR filter. We can apply the window function here, and then fianlly
        // normalize it so that the the final FIR filter kernel sums to 1.

        // Adopted from `nih_plug::util::window`. We only end up applying the right half of the
        // window, starting at the top of the window.
        let blackman_scale_1 = (2.0 * f32::consts::PI) / (impulse_response.len() - 1) as f32;
        let blackman_scale_2 = blackman_scale_1 * 2.0;
        for (sample_idx, sample) in impulse_response.iter_mut().enumerate().skip(CENTER_IDX - 1) {
            let cos_1 = (blackman_scale_1 * sample_idx as f32).cos();
            let cos_2 = (blackman_scale_2 * sample_idx as f32).cos();
            *sample *= f32x2::splat(0.42 - (0.5 * cos_1) + (0.08 * cos_2));
        }

        // Since this final filter will be symmetrical around `impulse_response[CENTER_IDX]`, we
        // can simply normalize based on that fact:
        let would_be_impulse_response_sum =
            (impulse_response.iter().skip(CENTER_IDX).sum::<f32x2>() * f32x2::splat(2.0))
                - impulse_response[CENTER_IDX];
        let would_be_impulse_response_recip = would_be_impulse_response_sum.recip();
        for sample in &mut impulse_response {
            *sample *= would_be_impulse_response_recip;
        }

        // And finally we can simply copy the right half of the filter kernel to the left half
        // around the `CENTER_IDX`.
        for source_idx in CENTER_IDX + 1..impulse_response.len() {
            let target_idx = CENTER_IDX - (source_idx - CENTER_IDX);
            impulse_response[target_idx] = impulse_response[source_idx];
        }

        Self(impulse_response)
    }
}
