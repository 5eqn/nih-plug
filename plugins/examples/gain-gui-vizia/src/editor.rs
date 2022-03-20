use atomic_float::AtomicF32;
use nih_plug::prelude::Editor;
use nih_plug_vizia::vizia::*;
use nih_plug_vizia::widgets::*;
use nih_plug_vizia::{assets, create_vizia_editor, ViziaState};
use std::pin::Pin;
use std::sync::Arc;

use crate::GainParams;

/// VIZIA uses points instead of pixels for text
const POINT_SCALE: f32 = 0.75;

const STYLE: &str = r#""#;

#[derive(Lens)]
// TODO: Lens requires everything to be marked as `pub`
pub struct Data {
    params: Pin<Arc<GainParams>>,
    peak_meter: Arc<AtomicF32>,
}

impl Model for Data {}

// Makes sense to also define this here, makes it a bit easier to keep track of
pub(crate) fn default_state() -> Arc<ViziaState> {
    ViziaState::from_size(200, 250)
}

pub(crate) fn create(
    params: Pin<Arc<GainParams>>,
    peak_meter: Arc<AtomicF32>,
    editor_state: Arc<ViziaState>,
) -> Option<Box<dyn Editor>> {
    create_vizia_editor(editor_state, move |cx, setter| {
        cx.add_theme(STYLE);

        Data {
            params: params.clone(),
            peak_meter: peak_meter.clone(),
        }
        .build(cx);

        VStack::new(cx, |cx| {
            Label::new(cx, "Gain GUI")
                .font(assets::NOTO_SANS_THIN)
                .font_size(40.0 * POINT_SCALE)
                .height(Pixels(50.0))
                .child_top(Stretch(1.0))
                .child_bottom(Pixels(0.0));
            // NOTE: VIZIA adds 1 pixel of additional height to these labels, so we'll need to
            //       compensate for that
            Label::new(cx, "Gain").bottom(Pixels(-1.0));

            VStack::new(cx, |cx| {
                ParamSlider::new(cx, Data::params, setter, |params| &params.gain);
                ParamSlider::new(cx, Data::params, setter, |params| &params.gain)
                    .set_style(ParamSliderStyle::FromLeft);
                ParamSlider::new(cx, Data::params, setter, |params| &params.foo);
                ParamSlider::new(cx, Data::params, setter, |params| &params.foo)
                    .set_style(ParamSliderStyle::CurrentStep);
                ParamSlider::new(cx, Data::params, setter, |params| &params.foo)
                    .set_style(ParamSliderStyle::CurrentStepLabeled);
            })
            .row_between(Pixels(5.0));

            // TODO: Add a peak meter
        })
        .row_between(Pixels(0.0))
        .child_left(Stretch(1.0))
        .child_right(Stretch(1.0));
    })
}
