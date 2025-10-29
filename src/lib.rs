use nih_plug::{prelude::*, util::db_to_gain};
use std::{collections::VecDeque, f32::consts::TAU, sync::Arc};

// ! This needs a lot of code cleanup; many comments are incorrect

/// The maximum size of an audio block. We'll split up the audio in blocks and render smoothed
/// values to buffers since these values may need to be reused for multiple voices.
const MAX_BLOCK_SIZE: usize = 64;

pub struct PolyModSynth {
    params: Arc<PolyModSynthParams>,
    voices: Vec<Voice>,
}

#[derive(Params)]
struct PolyModSynthParams {
    #[id = "gain"]
    gain: FloatParam,
    #[id = "vrange"]
    velocity_range: FloatParam,
    #[id = "sine"]
    sine_wave: BoolParam,
}

#[derive(Debug, Clone)]
struct Voice {
    active: bool,
    note: u8,
    channel: u8,
    frequency: f32,
    velocities: VecDeque<(u32, f32)>,
    pannings: VecDeque<(u32, f32)>,
    gains: VecDeque<(u32, f32)>,
    phase: f32,
}

impl Default for PolyModSynth {
    fn default() -> Self {
        Self {
            params: Arc::new(PolyModSynthParams::default()),
            voices: (0..=16)
                .flat_map(|channel| {
                    (0..=127).map(move |note| Voice {
                        active: false,
                        note,
                        channel,
                        frequency: util::midi_note_to_freq(note),
                        velocities: VecDeque::with_capacity(65535),
                        pannings: VecDeque::with_capacity(65535),
                        gains: VecDeque::with_capacity(65535),
                        phase: 0.0,
                    })
                })
                .collect(),
        }
    }
}

impl Default for PolyModSynthParams {
    fn default() -> Self {
        Self {
            gain: FloatParam::new(
                "Gain",
                1.0 / 128.0,
                FloatRange::Skewed {
                    min: util::db_to_gain(-100.0),
                    max: util::db_to_gain(0.0),
                    factor: FloatRange::gain_skew_factor(-100.0, 0.0),
                },
            )
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_gain_to_db(2))
            .with_string_to_value(formatters::s2v_f32_gain_to_db()),
            velocity_range: FloatParam::new(
                "Velocity Range",
                40.0,
                FloatRange::Linear {
                    min: 0.0,
                    max: 100.0,
                },
            )
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_rounded(2)),
            sine_wave: BoolParam::new("Generate Sine Wave Output", true),
        }
    }
}

impl Plugin for PolyModSynth {
    const NAME: &'static str = "Basic Synth";
    const VENDOR: &'static str = "transkatgirl";
    const URL: &'static str = ""; // TODO
    const EMAIL: &'static str = "08detour_dial@icloud.com";

    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(2),
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];

    const MIDI_INPUT: MidiConfig = MidiConfig::MidiCCs;
    const SAMPLE_ACCURATE_AUTOMATION: bool = true;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    // If the synth as a variable number of voices, you will need to call
    // `context.set_current_voice_capacity()` in `initialize()` and in `process()` (when the
    // capacity changes) to inform the host about this.
    fn reset(&mut self) {
        for voice in &mut self.voices {
            voice.active = false;
        }
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // NIH-plug has a block-splitting adapter for `Buffer`. While this works great for effect
        // plugins, for polyphonic synths the block size should be `min(MAX_BLOCK_SIZE,
        // num_remaining_samples, next_event_idx - block_start_idx)`. Because blocks also need to be
        // split on note events, it's easier to work with raw audio here and to do the splitting by
        // hand.
        let num_samples = buffer.samples();
        let sample_rate = context.transport().sample_rate;
        let output = buffer.as_slice();

        let sine_wave = self.params.sine_wave.value();
        let velocity_range = self.params.velocity_range.value();

        let mut next_event = context.next_event();
        let mut block_start: usize = 0;
        let mut block_end: usize = MAX_BLOCK_SIZE.min(num_samples);
        while block_start < num_samples {
            // First of all, handle all note events that happen at the start of the block, and cut
            // the block short if another event happens before the end of it. To handle polyphonic
            // modulation for new notes properly, we'll keep track of the next internal note index
            // at the block's start. If we receive polyphonic modulation that matches a voice that
            // has an internal note ID that's great than or equal to this one, then we should start
            // the note's smoother at the new value instead of fading in from the global value.
            'events: loop {
                match next_event {
                    // If the event happens now, then we'll keep processing events
                    Some(event) if (event.timing() as usize) <= block_start => {
                        // This synth doesn't support any of the polyphonic expression events. A
                        // real synth plugin however will want to support those.
                        match event {
                            NoteEvent::NoteOn {
                                timing,
                                voice_id: _,
                                channel,
                                note,
                                velocity,
                            } => {
                                let voice = self.start_voice(context, timing, channel, note);
                                voice.velocities.push_back((timing, velocity));
                            }
                            NoteEvent::PolyPressure {
                                timing,
                                voice_id: _,
                                channel,
                                note,
                                pressure,
                            } => {
                                let voice =
                                    &mut self.voices[(channel as usize * 128) + note as usize];
                                voice.velocities.push_back((timing, pressure));
                            }
                            NoteEvent::PolyVolume {
                                timing,
                                voice_id: _,
                                channel,
                                note,
                                gain,
                            } => {
                                let voice =
                                    &mut self.voices[(channel as usize * 128) + note as usize];
                                voice.gains.push_back((timing, gain));
                            }
                            NoteEvent::PolyPan {
                                timing,
                                voice_id: _,
                                channel,
                                note,
                                pan,
                            } => {
                                let voice =
                                    &mut self.voices[(channel as usize * 128) + note as usize];
                                voice.pannings.push_back((timing, pan));
                            }
                            NoteEvent::NoteOff {
                                timing,
                                voice_id: _,
                                channel,
                                note,
                                velocity: _,
                            } => {
                                self.stop_voices(context, timing, channel, note);
                            }
                            NoteEvent::Choke {
                                timing,
                                voice_id: _,
                                channel,
                                note,
                            } => {
                                self.stop_voices(context, timing, channel, note);
                            }
                            _ => (),
                        };

                        next_event = context.next_event();
                    }
                    // If the event happens before the end of the block, then the block should be cut
                    // short so the next block starts at the event
                    Some(event) if (event.timing() as usize) < block_end => {
                        block_end = event.timing() as usize;
                        break 'events;
                    }
                    _ => break 'events,
                }
            }

            // We'll start with silence, and then add the output from the active voices
            output[0][block_start..block_end].fill(0.0);
            output[1][block_start..block_end].fill(0.0);

            let global_gain = self.params.gain.value();

            for voice in self.voices.iter_mut() {
                if !voice.active {
                    continue;
                }

                for sample_idx in block_start..block_end {
                    while voice.gains.len() > 1 && voice.gains[1].0 > sample_idx as u32 {
                        voice.gains.pop_front();
                    }

                    while voice.velocities.len() > 1 && voice.velocities[1].0 > sample_idx as u32 {
                        voice.velocities.pop_front();
                    }

                    while voice.pannings.len() > 1 && voice.pannings[1].0 > sample_idx as u32 {
                        voice.pannings.pop_front();
                    }

                    let gain = if !voice.gains.is_empty() {
                        voice.gains[0].1
                    } else {
                        1.0
                    };

                    let velocity = if !voice.velocities.is_empty() {
                        voice.velocities[0].1
                    } else {
                        1.0
                    };

                    let pan = if !voice.pannings.is_empty() {
                        voice.pannings[0].1
                    } else {
                        0.0
                    };

                    let velocity_multiplier =
                        db_to_gain(map_value_f32(velocity, 0.0, 1.0, -velocity_range, 0.0));

                    let amp = velocity_multiplier * gain * global_gain;

                    let sample = if sine_wave {
                        (voice.phase * TAU).sin()
                    } else {
                        (voice.phase * 2.0).round() - 1.0
                    } * amp;

                    voice.phase += voice.frequency / sample_rate;
                    if voice.phase >= 1.0 {
                        voice.phase -= 1.0;
                    }

                    let (left, right) = constant_power_pan(sample, pan);

                    output[0][sample_idx] += left;
                    output[1][sample_idx] += right;
                }
            }

            // And then just keep processing blocks until we've run out of buffer to fill
            block_start = block_end;
            block_end = (block_start + MAX_BLOCK_SIZE).min(num_samples);
        }

        ProcessStatus::Normal
    }
}

impl PolyModSynth {
    fn start_voice(
        &mut self,
        _context: &mut impl ProcessContext<Self>,
        _sample_offset: u32,
        channel: u8,
        note: u8,
    ) -> &mut Voice {
        let voice = &mut self.voices[(channel as usize * 128) + note as usize];

        debug_assert_eq!(voice.channel, channel);
        debug_assert_eq!(voice.note, note);

        voice.active = true;
        voice.velocities.clear();
        voice.pannings.clear();
        voice.gains.clear();

        voice
    }
    fn stop_voices(
        &mut self,
        context: &mut impl ProcessContext<Self>,
        sample_offset: u32,
        channel: u8,
        note: u8,
    ) {
        let voice = &mut self.voices[(channel as usize * 128) + note as usize];

        debug_assert_eq!(voice.channel, channel);
        debug_assert_eq!(voice.note, note);

        context.send_event(NoteEvent::VoiceTerminated {
            timing: sample_offset,
            voice_id: Some((channel as i32 * 128) + note as i32),
            channel,
            note,
        });

        voice.active = false;
        voice.phase = 0.0;
    }
}

fn map_value_f32(x: f32, min: f32, max: f32, target_min: f32, target_max: f32) -> f32 {
    (x - min) / (max - min) * (target_max - target_min) + target_min
}

fn constant_power_pan(value: f32, pan: f32) -> (f32, f32) {
    if pan == 0.0 {
        (value, value)
    } else {
        let angle = (pan * 2.0 * 45.0).to_radians();
        let coeff = f32::sqrt(2.0) / 2.0;
        let cos = f32::cos(angle);
        let sin = f32::sin(angle);

        (coeff * (cos - sin) * value, coeff * (cos + sin) * value)
    }
}

impl ClapPlugin for PolyModSynth {
    const CLAP_ID: &'static str = "com.transkatgirl.basicsynth";
    const CLAP_DESCRIPTION: Option<&'static str> = None;
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::Instrument,
        ClapFeature::Synthesizer,
        ClapFeature::Stereo,
    ];
}

impl Vst3Plugin for PolyModSynth {
    const VST3_CLASS_ID: [u8; 16] = *b"transkatgirlSynt";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Instrument,
        Vst3SubCategory::Synth,
        Vst3SubCategory::Stereo,
    ];
}

nih_export_clap!(PolyModSynth);
nih_export_vst3!(PolyModSynth);
