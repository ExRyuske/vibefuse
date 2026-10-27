//! A VST3 plug-in in the chain. The host must keep the order the spec sets:
//!   createInstance → initialize → setBusArrangements → activateBus →
//!   setupProcessing → setActive(true) → setProcessing(true) → process… →
//!   setProcessing(false) → setActive(false) → terminate
//! Breaking it is the usual cause of silence and of crashes in plug-ins.

use std::ffi::c_void;
use std::sync::Arc;

use vst3::Steinberg::Vst::{
    AudioBusBuffers, AudioBusBuffers__type0, BusDirections_, BusInfo, IAudioProcessor,
    IAudioProcessorTrait, IComponent, IComponentHandler, IComponentTrait, IEditController,
    IEditControllerTrait, IHostApplication, IParameterChanges, MediaTypes_, ProcessContext,
    ProcessContext_::StatesAndFlags_, ProcessData, ProcessModes_, ProcessSetup, SpeakerArrangement,
    SymbolicSampleSizes_, kSpeakerL, kSpeakerM, kSpeakerR,
};
use vst3::Steinberg::{FUnknown, IPluginBaseTrait, IPluginFactoryTrait, kResultOk};
use vst3::{ComPtr, ComWrapper, Interface};

use super::controller::{self, Controller, Vst3State};
use super::host::{ComponentHandler, HostApplication, Notifications};
use super::module::Module;
use super::params::{self, ControllerFormatter, ParamChanges};
use crate::fx::graph::{AudioBuffers, AudioProcessor, Loaded, ParamBridge, ParamChange};
use crate::fx::{Descriptor, Result, hex};

// The bindings declare these as the platform's C++ compiler sees them:
// signed on Windows, unsigned on macOS.
#[allow(clippy::unnecessary_cast)]
const MEDIA_AUDIO: i32 = MediaTypes_::kAudio as i32;
#[allow(clippy::unnecessary_cast)]
const INPUT: i32 = BusDirections_::kInput as i32;
#[allow(clippy::unnecessary_cast)]
const OUTPUT: i32 = BusDirections_::kOutput as i32;
#[allow(clippy::unnecessary_cast)]
const SAMPLE32: i32 = SymbolicSampleSizes_::kSample32 as i32;
#[allow(clippy::unnecessary_cast)]
const REALTIME: i32 = ProcessModes_::kRealtime as i32;
/// What the process context tells: a tempo, a time signature and a running
/// sample count, no transport.
#[allow(clippy::unnecessary_cast)]
const CONTEXT_STATE: u32 = (StatesAndFlags_::kTempoValid
    | StatesAndFlags_::kTimeSigValid
    | StatesAndFlags_::kContTimeValid) as u32;

/// An audio bus and its buffer: `channels × max_frames`, cut into channels.
struct Bus {
    channels: usize,
    data: Vec<f32>,
    /// The channels' starts, as VST3 takes them; rebuilt in `activate` only.
    ptrs: Vec<*mut f32>,
}

impl Bus {
    fn allocate(&mut self, max_frames: usize) {
        self.data = vec![0.0; self.channels * max_frames];
        let base = self.data.as_mut_ptr();
        // SAFETY: offsets within `data`.
        self.ptrs = (0..self.channels)
            .map(|ch| unsafe { base.add(ch * max_frames) })
            .collect();
    }

    fn buffers(&mut self) -> AudioBusBuffers {
        AudioBusBuffers {
            numChannels: self.channels as i32,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: self.ptrs.as_mut_ptr(),
            },
        }
    }
}

pub struct Vst3Instance {
    // Fields drop in order: the plug-in's pointers, then the host
    // application, the module last.
    processor: ComPtr<IAudioProcessor>,
    component: ComPtr<IComponent>,
    _host: ComWrapper<HostApplication>,
    _module: Arc<Module>,
    name: String,
    /// Every bus: the first is the main one, the others (side chains) only
    /// get valid, silent buffers, as the spec wants all described.
    inputs: Vec<Bus>,
    outputs: Vec<Bus>,
    in_bufs: Vec<AudioBusBuffers>,
    out_bufs: Vec<AudioBusBuffers>,
    param_changes: ComWrapper<ParamChanges>,
    param_changes_ptr: ComPtr<IParameterChanges>,
    changes: rtrb::Consumer<ParamChange>,
    max_frames: usize,
    active: bool,
    /// The time plug-ins may ask for: no transport, a running sample count.
    context: ProcessContext,
}

// SAFETY: the instance belongs to one thread at a time: made on the UI
// thread, then processed under the rack's lock.
unsafe impl Send for Vst3Instance {}

/// The controller: the component itself (single-component effects) or a
/// class of its own, which the host then initializes.
fn create_controller(
    module: &Module,
    component: &ComPtr<IComponent>,
    host: *mut IHostApplication,
) -> Option<(ComPtr<IEditController>, bool)> {
    if let Some(controller) = component.cast::<IEditController>() {
        return Some((controller, false));
    }
    let mut cid = [0; 16];
    // SAFETY: a TUID buffer.
    if unsafe { component.getControllerClassId(&mut cid) } != kResultOk {
        return None;
    }
    let mut object: *mut c_void = std::ptr::null_mut();
    // SAFETY: the module's factory, a class id from the component.
    let made = unsafe {
        module.factory().createInstance(
            cid.as_ptr(),
            IEditController::IID.as_ptr().cast(),
            &mut object,
        )
    };
    if made != kResultOk {
        return None;
    }
    // SAFETY: a new object with one reference, now ours.
    let controller = unsafe { ComPtr::from_raw(object.cast::<IEditController>()) }?;
    // SAFETY: the host application outlives the controller.
    if unsafe { controller.initialize(host.cast::<FUnknown>()) } != kResultOk {
        return None;
    }
    Some((controller, true))
}

pub fn create(module: Arc<Module>, descriptor: &Descriptor) -> Result<Loaded> {
    let fail = |reason: String| format!("{}: {reason}", descriptor.name);
    let cid: [u8; 16] = hex::decode(&descriptor.uid)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| fail(format!("bad class id {}", descriptor.uid)))?;
    let cid = cid.map(|b| b as std::ffi::c_char);
    let mut object: *mut c_void = std::ptr::null_mut();
    // SAFETY: 16-byte ids, an output pointer; the factory lives with the
    // module.
    let made = unsafe {
        module
            .factory()
            .createInstance(cid.as_ptr(), IComponent::IID.as_ptr().cast(), &mut object)
    };
    if made != kResultOk {
        return Err(fail(format!("createInstance returned {made}")));
    }
    // SAFETY: a new object with one reference, now ours.
    let component = unsafe { ComPtr::from_raw(object.cast::<IComponent>()) }
        .ok_or_else(|| fail("no component".into()))?;
    let host = HostApplication::new("VibeFuse");
    let host_ptr = host
        .to_com_ptr::<IHostApplication>()
        .ok_or_else(|| fail("no host application".into()))?;
    // SAFETY: the host application is kept with the instance.
    let started = unsafe { component.initialize(host_ptr.as_ptr().cast::<FUnknown>()) };
    if started != kResultOk {
        return Err(fail(format!("initialize returned {started}")));
    }
    // From here a failure terminates the component again.
    let abort = |reason: &str| {
        // SAFETY: initialized above, terminated once.
        unsafe { component.terminate() };
        fail(reason.to_owned())
    };
    let Some(processor) = component.cast::<IAudioProcessor>() else {
        return Err(abort("not an audio processor"));
    };
    // SAFETY: an initialized component.
    if unsafe { processor.canProcessSampleSize(SAMPLE32) } != kResultOk {
        return Err(abort("no 32-bit samples"));
    }
    let controller =
        create_controller(&module, &component, host_ptr.as_ptr()).map(|(controller, separate)| {
            // Without these some plug-ins refuse their editor, and meters
            // stay dead.
            if separate {
                controller::share_state(&component, &controller);
                controller::link(&component, &controller, true);
            }
            Controller::new(controller, component.clone(), separate, Arc::clone(&module))
        });
    let parameters = controller
        .as_ref()
        .map(|c| params::read_parameters(c.get()))
        .unwrap_or_default();
    let values = controller
        .as_ref()
        .map(|c| {
            parameters
                .iter()
                // SAFETY: ids of this controller.
                .map(|p| unsafe { c.get().getParamNormalized(p.id) })
                .collect()
        })
        .unwrap_or_default();
    let param_changes = ParamChanges::new(parameters.len().clamp(1, 64));
    let Some(param_changes_ptr) = param_changes.to_com_ptr::<IParameterChanges>() else {
        drop(controller);
        return Err(abort("no parameter queues"));
    };
    let (bridge, changes) = ParamBridge::for_parameters(parameters.len());
    // Before the editor: edits in it go nowhere without a handler.
    let handler = controller.as_ref().map(|c| {
        let handler = ComponentHandler::new(Arc::clone(&bridge));
        if let Some(ptr) = handler.to_com_ptr::<IComponentHandler>() {
            // SAFETY: the handler lives as long as the controller.
            unsafe { c.get().setComponentHandler(ptr.as_ptr()) };
        }
        handler
    });
    let state = Vst3State {
        component: component.clone(),
        controller: controller.clone(),
        _module: Arc::clone(&module),
    };
    Ok(Loaded {
        parameters,
        values,
        bridge,
        formatter: controller
            .clone()
            .map(|c| Box::new(ControllerFormatter(c)) as Box<_>),
        editor: controller.map(super::editor::new),
        notifications: handler.map(|h| Box::new(Notifications(h)) as Box<_>),
        state: Some(Box::new(state)),
        processor: Box::new(Vst3Instance {
            processor,
            component,
            _host: host,
            _module: module,
            name: descriptor.name.clone(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            in_bufs: Vec::new(),
            out_bufs: Vec::new(),
            param_changes,
            param_changes_ptr,
            changes,
            max_frames: 0,
            active: false,
            // SAFETY: plain numbers, for which zero is valid.
            context: unsafe { std::mem::zeroed() },
        }),
    })
}

/// The speakers of `channels` channels.
fn arrangement(channels: usize) -> SpeakerArrangement {
    if channels == 1 {
        kSpeakerM as SpeakerArrangement
    } else {
        (kSpeakerL | kSpeakerR) as SpeakerArrangement
    }
}

impl Vst3Instance {
    fn bus_count(&self, direction: i32) -> usize {
        // SAFETY: an initialized component.
        unsafe { self.component.getBusCount(MEDIA_AUDIO, direction) }.max(0) as usize
    }

    fn bus_info(&self, direction: i32, index: usize) -> Option<BusInfo> {
        let mut info = BusInfo {
            mediaType: 0,
            direction: 0,
            channelCount: 0,
            name: [0; 128],
            busType: 0,
            flags: 0,
        };
        // SAFETY: an index below the count.
        let ok = unsafe {
            self.component
                .getBusInfo(MEDIA_AUDIO, direction, index as i32, &mut info)
        } == kResultOk;
        ok.then_some(info)
    }

    /// Asks for the chain's layout on the main buses and takes whatever the
    /// plug-in settles on: only that tells how many channels it reads.
    fn configure_buses(&mut self, channels: usize) -> Result<()> {
        let current = |direction| -> Vec<SpeakerArrangement> {
            (0..self.bus_count(direction))
                .map(|index| {
                    let channels = self
                        .bus_info(direction, index)
                        .map_or(2, |info| info.channelCount.max(0) as usize);
                    arrangement(channels)
                })
                .collect()
        };
        let mut inputs = current(INPUT);
        let mut outputs = current(OUTPUT);
        if outputs.is_empty() {
            return Err(format!("{}: no audio outputs", self.name));
        }
        if let Some(first) = inputs.first_mut() {
            *first = arrangement(channels);
        }
        outputs[0] = arrangement(channels);
        // SAFETY: local arrays with their lengths. A refusal is fine: the
        // plug-in keeps its own layout, read below.
        unsafe {
            self.processor.setBusArrangements(
                inputs.as_mut_ptr(),
                inputs.len() as i32,
                outputs.as_mut_ptr(),
                outputs.len() as i32,
            );
        }
        let actual = |direction| -> Vec<Bus> {
            (0..self.bus_count(direction))
                .map(|index| {
                    let mut speakers: SpeakerArrangement = 0;
                    // SAFETY: an index below the count.
                    let ok = unsafe {
                        self.processor
                            .getBusArrangement(direction, index as i32, &mut speakers)
                    } == kResultOk;
                    let channels = if ok {
                        (speakers as u64).count_ones() as usize
                    } else {
                        self.bus_info(direction, index)
                            .map_or(0, |info| info.channelCount.max(0) as usize)
                    };
                    Bus {
                        channels: channels.max(1),
                        data: Vec::new(),
                        ptrs: Vec::new(),
                    }
                })
                .collect()
        };
        let (inputs, outputs) = (actual(INPUT), actual(OUTPUT));
        self.inputs = inputs;
        self.outputs = outputs;
        // Only the main buses: a side chain would wait for a signal we lack.
        for (direction, count) in [(INPUT, self.inputs.len()), (OUTPUT, self.outputs.len())] {
            for index in 0..count {
                // SAFETY: indices below the count.
                unsafe {
                    self.component.activateBus(
                        MEDIA_AUDIO,
                        direction,
                        index as i32,
                        u8::from(index == 0),
                    );
                }
            }
        }
        Ok(())
    }
}

impl AudioProcessor for Vst3Instance {
    fn activate(&mut self, sample_rate: f64, max_frames: usize, channels: usize) -> Result<()> {
        self.deactivate();
        self.max_frames = max_frames;
        self.configure_buses(channels)?;
        for bus in self.inputs.iter_mut().chain(&mut self.outputs) {
            bus.allocate(max_frames);
        }
        self.in_bufs = self.inputs.iter_mut().map(Bus::buffers).collect();
        self.out_bufs = self.outputs.iter_mut().map(Bus::buffers).collect();
        self.context.sampleRate = sample_rate;
        self.context.tempo = 120.0;
        self.context.timeSigNumerator = 4;
        self.context.timeSigDenominator = 4;
        self.context.state = CONTEXT_STATE;
        let mut setup = ProcessSetup {
            processMode: REALTIME,
            symbolicSampleSize: SAMPLE32,
            maxSamplesPerBlock: max_frames as i32,
            sampleRate: sample_rate,
        };
        // SAFETY: in the order the spec sets.
        unsafe {
            let ready = self.processor.setupProcessing(&mut setup);
            if ready != kResultOk {
                return Err(format!("{}: setupProcessing returned {ready}", self.name));
            }
            if self.component.setActive(1) != kResultOk {
                return Err(format!("{}: refused to start", self.name));
            }
            self.processor.setProcessing(1);
        }
        self.active = true;
        Ok(())
    }

    fn deactivate(&mut self) {
        if std::mem::take(&mut self.active) {
            // SAFETY: the reverse of `activate`.
            unsafe {
                self.processor.setProcessing(0);
                self.component.setActive(0);
            }
        }
    }

    fn process(&mut self, buffers: &mut AudioBuffers<'_>) {
        let frames = buffers.frames();
        let channels = buffers.channel_count();
        if !self.active || frames > self.max_frames || channels == 0 {
            return;
        }
        if let Some(bus) = self.inputs.first() {
            for (ch, &to) in bus.ptrs.iter().enumerate() {
                let from = buffers.channel_ref(buffers.source_index(ch));
                // SAFETY: buffers of `max_frames` made in `activate`.
                unsafe { std::ptr::copy_nonoverlapping(from.as_ptr(), to, frames) };
            }
        }
        // Changes that do not fit wait for the next block, hence the peek.
        self.param_changes.begin_block();
        while let Ok(&change) = self.changes.peek() {
            if !self.param_changes.add(change.id, change.value) {
                break;
            }
            let _ = self.changes.pop();
        }
        let input_changes = if self.param_changes.is_empty() {
            std::ptr::null_mut()
        } else {
            self.param_changes_ptr.as_ptr()
        };
        let mut data = ProcessData {
            processMode: REALTIME,
            symbolicSampleSize: SAMPLE32,
            numSamples: frames as i32,
            numInputs: self.in_bufs.len() as i32,
            numOutputs: self.out_bufs.len() as i32,
            inputs: self.in_bufs.as_mut_ptr(),
            outputs: self.out_bufs.as_mut_ptr(),
            inputParameterChanges: input_changes,
            outputParameterChanges: std::ptr::null_mut(),
            inputEvents: std::ptr::null_mut(),
            outputEvents: std::ptr::null_mut(),
            processContext: &mut self.context,
        };
        // SAFETY: every pointer in `data` is valid for the call.
        unsafe { self.processor.process(&mut data) };
        self.context.continousTimeSamples += frames as i64;
        if let Some(bus) = self.outputs.first() {
            for ch in 0..channels {
                let from = bus.ptrs[ch.min(bus.channels - 1)];
                // SAFETY: as above.
                unsafe {
                    std::ptr::copy_nonoverlapping(from, buffers.channel(ch).as_mut_ptr(), frames);
                }
            }
        }
    }
}

impl Drop for Vst3Instance {
    fn drop(&mut self) {
        self.deactivate();
        // SAFETY: pairs with `initialize`, once.
        unsafe { self.component.terminate() };
    }
}
