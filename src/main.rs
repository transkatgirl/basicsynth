use nih_plug::prelude::*;

use basicsynth::PolyModSynth;

fn main() {
    nih_export_standalone::<PolyModSynth>();
}
