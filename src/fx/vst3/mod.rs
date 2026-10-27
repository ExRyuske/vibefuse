//! VST3. The `vst3` crate has the COM bindings generated from Steinberg's
//! headers (MIT since VST 3.8); the host's side is here.

mod controller;
mod editor;
mod host;
mod instance;
mod module;
mod params;

use std::ffi::c_char;
use std::path::Path;

use vst3::Steinberg::{
    IPluginFactory2, IPluginFactory2Trait, IPluginFactoryTrait, PClassInfo, PClassInfo2, kResultOk,
};

use super::graph::{Loaded, PluginFormat};
use super::{Descriptor, Format, Result, hex};
use module::Module;

/// The category of effects and instruments (`kVstAudioEffectClass`).
const AUDIO_MODULE_CLASS: &str = "Audio Module Class";

pub struct Vst3Format;

impl PluginFormat for Vst3Format {
    fn format(&self) -> Format {
        Format::Vst3
    }

    fn probe(&self, path: &Path) -> Result<Vec<Descriptor>> {
        let module = Module::shared(path)?;
        classes(&module)
    }

    fn instantiate(&self, descriptor: &Descriptor, _sample_rate: f64) -> Result<Loaded> {
        instance::create(Module::shared(&descriptor.path)?, descriptor)
    }
}

/// The effects and instruments a module offers.
fn classes(module: &Module) -> Result<Vec<Descriptor>> {
    let factory = module.factory();
    // Vendors and subcategories come with the second version.
    let factory2 = factory.cast::<IPluginFactory2>();
    // SAFETY: plain calls on a live factory.
    let count = unsafe { factory.countClasses() };
    let mut found = Vec::new();
    for index in 0..count {
        let mut info = PClassInfo {
            cid: [0; 16],
            cardinality: 0,
            category: [0; 32],
            name: [0; 64],
        };
        // SAFETY: an output of the right type.
        if unsafe { factory.getClassInfo(index, &mut info) } != kResultOk
            || text(&info.category) != AUDIO_MODULE_CLASS
        {
            continue;
        }
        let mut descriptor = Descriptor {
            format: Format::Vst3,
            path: module.path().to_path_buf(),
            uid: hex::encode(&info.cid.map(|b| b as u8)),
            name: text(&info.name),
            vendor: String::new(),
            is_instrument: false,
        };
        if let Some(factory2) = &factory2 {
            let mut info2 = PClassInfo2 {
                cid: [0; 16],
                cardinality: 0,
                category: [0; 32],
                name: [0; 64],
                classFlags: 0,
                subCategories: [0; 128],
                vendor: [0; 64],
                version: [0; 64],
                sdkVersion: [0; 64],
            };
            // SAFETY: as above.
            if unsafe { factory2.getClassInfo2(index, &mut info2) } == kResultOk {
                descriptor.vendor = text(&info2.vendor);
                // "Fx|Dynamics", "Instrument|Synth".
                descriptor.is_instrument = text(&info2.subCategories)
                    .split('|')
                    .any(|s| s.eq_ignore_ascii_case("Instrument"));
            }
        }
        found.push(descriptor);
    }
    Ok(found)
}

/// A fixed `char` array up to its terminating zero.
fn text(raw: &[c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_stops_at_nul() {
        let raw = [b'a', b'b', 0, b'X'].map(|b| b as c_char);
        assert_eq!(text(&raw), "ab");
    }
}
