// VibeFuse Mixer: the software mixer of VibeFuse for macOS.
//
// An AudioServerPlugIn that publishes the same virtual devices Arturia's
// Windows driver creates for the MiniFuse 1 and mixes them the same way:
//
//   outputs  VibeFuse Main 1/2, Virtual 3/4, Virtual 5/6, Virtual 7/8
//   inputs   VibeFuse Mix 3/4, Mix 5/6, Loopback Main 7/8
//
// A hidden "bridge" device connects the mixer to the hardware: VibeFuse
// runs it in an aggregate device with the MiniFuse, feeds the card's inputs
// into it and plays the "Monitor & Phones" mix it returns on the card's
// outputs.
//
// Every device runs on one clock, so a sample time means the same moment on
// all of them. Each output device writes into a ring buffer; when an input
// device is read, its mix is computed from the rings at that time. The gain
// matrix uses the channel addressing and the property format of the Windows
// driver's mixer (see src/device/sls.rs): VibeFuse sends the same
// bytes to both, as the custom property 'mfgn' here.

#include <CoreAudio/AudioServerPlugIn.h>
#include <dispatch/dispatch.h>
#include <mach/mach_time.h>
#include <math.h>
#include <os/log.h>
#include <pthread.h>
#include <stddef.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <string.h>

#define MIXER_VERSION 1

// The factory in Info.plist.
void *VibeFuseMixer_Create(CFAllocatorRef allocator, CFUUIDRef requested_type);

#pragma mark Layout

enum {
    kDeviceCount = 8,
    kObjectsPerDevice = 8,
    // Object IDs within a device's block.
    kDeviceObject = 0,
    kInputStream = 1,
    kOutputStream = 2,
    kVolumeControl = 3,
    kMuteControl = 4,

    kChannels = 2,
    kSources = 10,  // mono mixer inputs
    kOutputs = 8,   // mono mixer outputs
    kRings = 5,     // stereo inputs of the mixer, one per writing device
    kRingFrames = 32768,
    kZeroTimeStampPeriod = 16384,
    kMaxFrames = 16384,

    kClockDomain = 'MFmx',
};

// Custom properties of the plug-in object, all CFData.
enum {
    kPropertyStatus = 'mfst',   // get: version, running devices, bridge running, rate
    kPropertyGains = 'mfgn',    // set: gains in the Windows driver's format
    kPropertyMeters = 'mfmt',   // get: meters in the Windows driver's format
    kPropertyRunning = 'mfrn',  // get: user devices with IO running (notifies)
};

// Windows driver property layout.
enum {
    kMagic = 0x4D,
    kCrosspoints = 200,
    kOutputGains = 201,
    kUnity = 1 << 24,
    kMaxGain = 1 << 26,
};

typedef struct {
    const char *uid;
    const char *name;
    int ring;   // ring the output stream writes, -1 without output
    int mix;    // first mixer output the input stream reads, -1 without input
    bool hidden;
} DeviceSpec;

static const DeviceSpec kDevices[kDeviceCount] = {
    {"VibeFuse:Main12", "VibeFuse Main 1/2", 0, -1, false},
    {"VibeFuse:Virtual34", "VibeFuse Virtual 3/4", 2, -1, false},
    {"VibeFuse:Virtual56", "VibeFuse Virtual 5/6", 3, -1, false},
    {"VibeFuse:Virtual78", "VibeFuse Virtual 7/8", 4, -1, false},
    {"VibeFuse:Mix34", "VibeFuse Mix 3/4", -1, 2, false},
    {"VibeFuse:Mix56", "VibeFuse Mix 5/6", -1, 4, false},
    {"VibeFuse:Loopback78", "VibeFuse Loopback Main 7/8", -1, 6, false},
    // Hardware inputs in, "Monitor & Phones" out.
    {"VibeFuse:Bridge", "VibeFuse Bridge", 1, 0, true},
};

static const Float64 kRates[] = {44100, 48000, 88200, 96000, 176400, 192000};
enum { kRateCount = sizeof(kRates) / sizeof(kRates[0]) };

// Mixer inputs as the Windows driver numbers them: (kind, index). Source s
// lives in ring s / 2, channel s % 2.
static const uint8_t kSourceAddress[kSources][2] = {
    {0, 0}, {0, 1},  // Main 1/2 (host playback)
    {1, 0}, {1, 1},  // hardware inputs (device inputs)
    {1, 2}, {1, 3},  // Virtual 3/4
    {2, 0}, {2, 1},  // Virtual 5/6 (virtual playback)
    {2, 2}, {2, 3},  // Virtual 7/8
};

// Mixer outputs: (kind, index).
static const uint8_t kOutputAddress[kOutputs][2] = {
    {1, 0}, {1, 1},  // Monitor & Phones (device outputs)
    {0, 2}, {0, 3},  // Mix 3/4 (host recording)
    {2, 0}, {2, 1},  // Mix 5/6 (virtual recording)
    {2, 2}, {2, 3},  // Loopback Main 7/8
};

static int source_index(uint8_t kind, uint8_t index) {
    for (int s = 0; s < kSources; s++) {
        if (kSourceAddress[s][0] == kind && kSourceAddress[s][1] == index) return s;
    }
    return -1;
}

static int output_index(uint8_t kind, uint8_t index) {
    for (int o = 0; o < kOutputs; o++) {
        if (kOutputAddress[o][0] == kind && kOutputAddress[o][1] == index) return o;
    }
    return -1;
}

static inline AudioObjectID object_id(int device, int part) {
    return (AudioObjectID)(2 + device * kObjectsPerDevice + part);
}

// The device and part an object ID belongs to; false for unknown IDs.
static bool split_id(AudioObjectID id, int *device, int *part) {
    if (id < 2) return false;
    int d = (int)(id - 2) / kObjectsPerDevice;
    int p = (int)(id - 2) % kObjectsPerDevice;
    if (d >= kDeviceCount) return false;
    const DeviceSpec *spec = &kDevices[d];
    bool user_output = spec->ring >= 0 && !spec->hidden;
    bool exists = p == kDeviceObject || (p == kInputStream && spec->mix >= 0) ||
                  (p == kOutputStream && spec->ring >= 0) ||
                  ((p == kVolumeControl || p == kMuteControl) && user_output);
    if (!exists) return false;
    *device = d;
    *part = p;
    return true;
}

static bool has_controls(int d) { return kDevices[d].ring >= 0 && !kDevices[d].hidden; }

#pragma mark State

typedef struct {
    float data[kChannels][kRingFrames];
    // Mixer time of the first and one past the last frame written in the
    // current uninterrupted run.
    _Atomic int64_t run_start;
    _Atomic int64_t written_end;
} Ring;

typedef struct {
    // Guarded by gStateMutex.
    UInt32 io_count;
    Float64 start_host;
    Float64 ticks_per_frame;
    int64_t offset;  // device sample time + offset = mixer time
    // Touched only by the HAL's IO thread of this device.
    UInt64 timestamps;
    float ramp[kChannels][kSources];
    float scratch[kMaxFrames];
    bool stream_active[2];
    // Controls of output devices.
    _Atomic uint32_t volume_bits;  // Float32 scalar
    _Atomic uint32_t muted;
} DeviceState;

static AudioServerPlugInHostRef gHost;
static pthread_mutex_t gStateMutex = PTHREAD_MUTEX_INITIALIZER;
static UInt32 gRefCount;
static Float64 gSampleRate = 48000;
static Float64 gAnchorHost;
static UInt32 gRunningDevices;
static UInt32 gRunningUserDevices;
static DeviceState gDevice[kDeviceCount];
static Ring gRing[kRings];
static _Atomic int32_t gCrosspoint[kOutputs][kSources];
static _Atomic int32_t gOutputGain[kOutputs];
// Peaks since the last meter query, as Float32 bits (non-negative floats
// order like integers).
static _Atomic uint32_t gSourcePeak[kSources];
static _Atomic uint32_t gOutputPeak[kOutputs];
static float gSourceLevel[kSources];
static float gOutputLevel[kOutputs];
static UInt64 gLastMeterQuery;
static bool gStorageDirty;
static UInt64 gLastStorageWrite;
static dispatch_queue_t gNotifyQueue;

static Float64 host_ticks_per_second(void) {
    static Float64 ticks;
    if (ticks == 0) {
        mach_timebase_info_data_t info;
        mach_timebase_info(&info);
        ticks = 1e9 * (Float64)info.denom / (Float64)info.numer;
    }
    return ticks;
}

static Float64 seconds_since(UInt64 then) {
    return (Float64)(mach_absolute_time() - then) / host_ticks_per_second();
}

static inline float load_float(_Atomic uint32_t *bits) {
    uint32_t raw = atomic_load_explicit(bits, memory_order_relaxed);
    float value;
    memcpy(&value, &raw, sizeof value);
    return value;
}

static inline void store_float(_Atomic uint32_t *bits, float value) {
    uint32_t raw;
    memcpy(&raw, &value, sizeof raw);
    atomic_store_explicit(bits, raw, memory_order_relaxed);
}

static inline void raise_peak(_Atomic uint32_t *peak, float value) {
    uint32_t raw;
    memcpy(&raw, &value, sizeof raw);
    uint32_t old = atomic_load_explicit(peak, memory_order_relaxed);
    while (raw > old &&
           !atomic_compare_exchange_weak_explicit(peak, &old, raw, memory_order_relaxed,
                                                  memory_order_relaxed)) {
    }
}

static void notify(AudioObjectID object, AudioObjectPropertySelector selector) {
    if (gHost == NULL) return;
    AudioObjectPropertyAddress address = {selector, kAudioObjectPropertyScopeGlobal,
                                          kAudioObjectPropertyElementMain};
    // The Host must not be called back from within the call that changed it.
    dispatch_async(gNotifyQueue, ^{
      gHost->PropertiesChanged(gHost, object, 1, &address);
    });
}

static void notify2(AudioObjectID object, AudioObjectPropertySelector a,
                    AudioObjectPropertySelector b) {
    notify(object, a);
    notify(object, b);
}

#pragma mark Gains

// The factory preset of Arturia's MiniFuse Control Center: Main 1/2 on Monitor &
// Phones (and its mirror on Loopback Main 7/8), Virtual 3/4 on Mix 3/4,
// Virtual 5/6 on Mix 5/6.
static void factory_gains(void) {
    for (int o = 0; o < kOutputs; o++) {
        for (int s = 0; s < kSources; s++) atomic_store(&gCrosspoint[o][s], 0);
        atomic_store(&gOutputGain[o], kUnity);
    }
    static const int pairs[][2] = {{0, 0}, {0, 6}, {4, 2}, {6, 4}};  // source, output
    for (size_t i = 0; i < sizeof pairs / sizeof pairs[0]; i++) {
        int s = pairs[i][0], o = pairs[i][1];
        atomic_store(&gCrosspoint[o][s], kUnity);
        atomic_store(&gCrosspoint[o + 1][s + 1], kUnity);
    }
}

static inline int32_t clamp_gain(int32_t gain) {
    return gain < 0 ? 0 : (gain > kMaxGain ? kMaxGain : gain);
}

static int32_t read_i32(const UInt8 *p) {
    return (int32_t)((uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 |
                     (uint32_t)p[3] << 24);
}

static void write_i32(UInt8 *p, int32_t value) {
    uint32_t v = (uint32_t)value;
    p[0] = (UInt8)v;
    p[1] = (UInt8)(v >> 8);
    p[2] = (UInt8)(v >> 16);
    p[3] = (UInt8)(v >> 24);
}

// Applies a property in the Windows driver's format. Unknown channels are
// skipped, like the driver does.
static OSStatus apply_gains(const UInt8 *bytes, CFIndex length) {
    if (length < 8 || read_i32(bytes) != kMagic) return kAudioHardwareIllegalOperationError;
    int32_t id = read_i32(bytes + 4);
    const UInt8 *entry = bytes + 8;
    CFIndex left = length - 8;
    if (id == kCrosspoints) {
        for (; left >= 8; entry += 8, left -= 8) {
            int s = source_index(entry[0], entry[1]);
            int o = output_index(entry[2], entry[3]);
            if (s >= 0 && o >= 0) {
                atomic_store_explicit(&gCrosspoint[o][s], clamp_gain(read_i32(entry + 4)),
                                      memory_order_relaxed);
            }
        }
    } else if (id == kOutputGains) {
        for (; left >= 6; entry += 6, left -= 6) {
            int o = output_index(entry[0], entry[1]);
            if (o >= 0) {
                atomic_store_explicit(&gOutputGain[o], clamp_gain(read_i32(entry + 2)),
                                      memory_order_relaxed);
            }
        }
    } else {
        return kAudioHardwareIllegalOperationError;
    }
    return 0;
}

#pragma mark Storage

static CFStringRef const kStorageKey = CFSTR("mixer");
static const int32_t kStorageMagic = 0x56464D58;  // "VFMX"
static const int32_t kStorageVersion = 1;
enum { kStorageWords = 2 + kOutputs * kSources + kOutputs + 2 * 4 };

static void save_state(void) {
    if (gHost == NULL) return;
    UInt8 bytes[kStorageWords * 4];
    UInt8 *p = bytes;
    write_i32(p, kStorageMagic), p += 4;
    write_i32(p, kStorageVersion), p += 4;
    for (int o = 0; o < kOutputs; o++) {
        for (int s = 0; s < kSources; s++) write_i32(p, atomic_load(&gCrosspoint[o][s])), p += 4;
    }
    for (int o = 0; o < kOutputs; o++) write_i32(p, atomic_load(&gOutputGain[o])), p += 4;
    for (int d = 0; d < 4; d++) {
        write_i32(p, (int32_t)atomic_load(&gDevice[d].volume_bits)), p += 4;
        write_i32(p, (int32_t)atomic_load(&gDevice[d].muted)), p += 4;
    }
    CFDataRef data = CFDataCreate(NULL, bytes, sizeof bytes);
    if (data != NULL) {
        gHost->WriteToStorage(gHost, kStorageKey, data);
        CFRelease(data);
    }
    gStorageDirty = false;
    gLastStorageWrite = mach_absolute_time();
}

// Writes at most once a second while faders move; the rest is written by
// the next meter query or when IO stops.
static void state_changed(void) {
    gStorageDirty = true;
    if (seconds_since(gLastStorageWrite) >= 1) save_state();
}

static void flush_state(void) {
    if (gStorageDirty && seconds_since(gLastStorageWrite) >= 1) save_state();
}

static void load_state(void) {
    CFPropertyListRef stored = NULL;
    if (gHost == NULL || gHost->CopyFromStorage(gHost, kStorageKey, &stored) != 0 ||
        stored == NULL) {
        return;
    }
    if (CFGetTypeID(stored) == CFDataGetTypeID() &&
        CFDataGetLength(stored) == kStorageWords * 4) {
        const UInt8 *p = CFDataGetBytePtr(stored);
        if (read_i32(p) == kStorageMagic && read_i32(p + 4) == kStorageVersion) {
            p += 8;
            for (int o = 0; o < kOutputs; o++) {
                for (int s = 0; s < kSources; s++) {
                    atomic_store(&gCrosspoint[o][s], clamp_gain(read_i32(p))), p += 4;
                }
            }
            for (int o = 0; o < kOutputs; o++) atomic_store(&gOutputGain[o], clamp_gain(read_i32(p))), p += 4;
            for (int d = 0; d < 4; d++) {
                uint32_t bits = (uint32_t)read_i32(p);
                float volume;
                memcpy(&volume, &bits, sizeof volume);
                if (volume >= 0 && volume <= 1) atomic_store(&gDevice[d].volume_bits, bits);
                atomic_store(&gDevice[d].muted, read_i32(p + 4) != 0);
                p += 8;
            }
        }
    }
    CFRelease(stored);
}

#pragma mark Volume

static const Float32 kVolumeMinDB = -96;

static Float32 scalar_to_db(Float32 scalar) {
    if (scalar <= 0) return kVolumeMinDB;
    Float32 db = 40 * log10f(scalar);
    return db < kVolumeMinDB ? kVolumeMinDB : (db > 0 ? 0 : db);
}

static Float32 db_to_scalar(Float32 db) {
    if (db <= kVolumeMinDB) return 0;
    if (db > 0) db = 0;
    return powf(10, db / 40);
}

// Linear gain of an output device's volume control.
static float device_gain(int d) {
    if (!has_controls(d)) return 1;
    if (atomic_load_explicit(&gDevice[d].muted, memory_order_relaxed)) return 0;
    float scalar = load_float(&gDevice[d].volume_bits);
    return scalar * scalar;
}

#pragma mark Rings and mixing

static void reset_rings(void) {
    for (int r = 0; r < kRings; r++) {
        atomic_store(&gRing[r].run_start, INT64_MIN / 2);
        atomic_store(&gRing[r].written_end, INT64_MIN / 2);
    }
}

static void write_ring(int r, int64_t time, const float *frames, UInt32 count, float gain) {
    Ring *ring = &gRing[r];
    if (atomic_load_explicit(&ring->written_end, memory_order_relaxed) != time) {
        atomic_store_explicit(&ring->run_start, time, memory_order_release);
        atomic_store_explicit(&ring->written_end, time, memory_order_release);
    }
    float peak[kChannels] = {0, 0};
    for (UInt32 i = 0; i < count; i++) {
        size_t at = (size_t)((time + i) & (kRingFrames - 1));
        for (int c = 0; c < kChannels; c++) {
            float x = frames[i * kChannels + c] * gain;
            ring->data[c][at] = x;
            float a = fabsf(x);
            if (a > peak[c]) peak[c] = a;
        }
    }
    atomic_store_explicit(&ring->written_end, time + count, memory_order_release);
    for (int c = 0; c < kChannels; c++) raise_peak(&gSourcePeak[r * 2 + c], peak[c]);
}

// Copies frames [time, time + count) of a source into `out`; frames that
// were not written in the current run are silent.
static void read_source(int s, int64_t time, UInt32 count, float *out) {
    Ring *ring = &gRing[s / 2];
    int64_t end = atomic_load_explicit(&ring->written_end, memory_order_acquire);
    int64_t start = atomic_load_explicit(&ring->run_start, memory_order_acquire);
    // Keep clear of the part the writer may be overwriting.
    int64_t oldest = end - (kRingFrames - kMaxFrames);
    if (start < oldest) start = oldest;
    const float *data = ring->data[s % 2];
    for (UInt32 i = 0; i < count; i++) {
        int64_t t = time + i;
        out[i] = (t >= start && t < end) ? data[(size_t)(t & (kRingFrames - 1))] : 0;
    }
}

// Renders the stereo mix starting at mixer output `first` into an
// interleaved buffer. Gain changes are ramped over the buffer.
static void render_mix(DeviceState *device, int first, int64_t time, UInt32 count, float *out) {
    memset(out, 0, sizeof(float) * kChannels * count);
    float peak[kChannels] = {0, 0};
    for (int s = 0; s < kSources; s++) {
        float target[kChannels];
        bool silent = true;
        for (int c = 0; c < kChannels; c++) {
            int o = first + c;
            int32_t xp = atomic_load_explicit(&gCrosspoint[o][s], memory_order_relaxed);
            int32_t og = atomic_load_explicit(&gOutputGain[o], memory_order_relaxed);
            target[c] = (float)((double)xp / kUnity * (double)og / kUnity);
            if (target[c] != 0 || device->ramp[c][s] != 0) silent = false;
        }
        if (silent) continue;
        float *x = device->scratch;
        read_source(s, time, count, x);
        for (int c = 0; c < kChannels; c++) {
            float gain = device->ramp[c][s];
            float step = (target[c] - gain) / (float)count;
            for (UInt32 i = 0; i < count; i++) {
                gain += step;
                out[i * kChannels + c] += x[i] * gain;
            }
            device->ramp[c][s] = target[c];
        }
    }
    for (UInt32 i = 0; i < count; i++) {
        for (int c = 0; c < kChannels; c++) {
            float a = fabsf(out[i * kChannels + c]);
            if (a > peak[c]) peak[c] = a;
        }
    }
    for (int c = 0; c < kChannels; c++) raise_peak(&gOutputPeak[first + c], peak[c]);
}

#pragma mark Meters

static const float kMeterFloor = -120;
static const float kMeterFall = 26;  // dB per second, as in the app

static float meter_level(_Atomic uint32_t *peak, float *level, float elapsed) {
    uint32_t raw = atomic_exchange_explicit(peak, 0, memory_order_relaxed);
    float amplitude;
    memcpy(&amplitude, &raw, sizeof amplitude);
    float db = amplitude > 0 ? 20 * log10f(amplitude) : kMeterFloor;
    float fallen = *level - kMeterFall * elapsed;
    *level = db > fallen ? db : fallen;
    if (*level < kMeterFloor) *level = kMeterFloor;
    return *level;
}

static void append_meter(UInt8 *p, uint32_t direction, const uint8_t address[2], float db) {
    write_i32(p, (int32_t)direction);
    p[4] = address[0];
    p[5] = address[1];
    int16_t centi = (int16_t)lrintf(db * 100);
    p[6] = (UInt8)centi;
    p[7] = (UInt8)((uint16_t)centi >> 8);
}

// Every meter in the reply format of the Windows driver: `u32 0x4D, u32
// 301`, then `dir: u32, kind, ch: u8, centi-dB: i16` per channel.
static CFDataRef copy_meters(void) {
    UInt64 now = mach_absolute_time();
    float elapsed = gLastMeterQuery == 0 ? 1 : (float)seconds_since(gLastMeterQuery);
    gLastMeterQuery = now;
    UInt8 bytes[8 + (kSources + kOutputs) * 8];
    write_i32(bytes, kMagic);
    write_i32(bytes + 4, 301);
    UInt8 *p = bytes + 8;
    for (int s = 0; s < kSources; s++, p += 8) {
        append_meter(p, 0, kSourceAddress[s], meter_level(&gSourcePeak[s], &gSourceLevel[s], elapsed));
    }
    for (int o = 0; o < kOutputs; o++, p += 8) {
        append_meter(p, 1, kOutputAddress[o], meter_level(&gOutputPeak[o], &gOutputLevel[o], elapsed));
    }
    flush_state();
    return CFDataCreate(NULL, bytes, sizeof bytes);
}

static CFDataRef copy_status(void) {
    UInt8 bytes[24];
    pthread_mutex_lock(&gStateMutex);
    write_i32(bytes, MIXER_VERSION);
    write_i32(bytes + 4, (int32_t)gRunningUserDevices);
    write_i32(bytes + 8, gDevice[kDeviceCount - 1].io_count > 0);
    write_i32(bytes + 12, (int32_t)gSampleRate);
    Float64 rate = gSampleRate;
    pthread_mutex_unlock(&gStateMutex);
    memcpy(bytes + 16, &rate, sizeof rate);
    return CFDataCreate(NULL, bytes, sizeof bytes);
}

static CFDataRef copy_running(void) {
    UInt8 bytes[4];
    pthread_mutex_lock(&gStateMutex);
    write_i32(bytes, (int32_t)gRunningUserDevices);
    pthread_mutex_unlock(&gStateMutex);
    return CFDataCreate(NULL, bytes, sizeof bytes);
}

#pragma mark Formats

static AudioStreamBasicDescription stream_format(Float64 rate) {
    AudioStreamBasicDescription format = {0};
    format.mSampleRate = rate;
    format.mFormatID = kAudioFormatLinearPCM;
    format.mFormatFlags = kAudioFormatFlagsNativeFloatPacked;
    format.mBytesPerPacket = 4 * kChannels;
    format.mFramesPerPacket = 1;
    format.mBytesPerFrame = 4 * kChannels;
    format.mChannelsPerFrame = kChannels;
    format.mBitsPerChannel = 32;
    return format;
}

static bool is_supported_rate(Float64 rate) {
    for (int i = 0; i < kRateCount; i++) {
        if (kRates[i] == rate) return true;
    }
    return false;
}

static OSStatus request_rate(Float64 rate) {
    if (!is_supported_rate(rate)) return kAudioHardwareIllegalOperationError;
    pthread_mutex_lock(&gStateMutex);
    bool same = rate == gSampleRate;
    pthread_mutex_unlock(&gStateMutex);
    if (same) return 0;
    // All devices share the clock, so they all change.
    UInt64 action = (UInt64)rate;
    dispatch_async(gNotifyQueue, ^{
      for (int d = 0; d < kDeviceCount; d++) {
          gHost->RequestDeviceConfigurationChange(gHost, object_id(d, kDeviceObject), action, NULL);
      }
    });
    return 0;
}

#pragma mark IUnknown

static HRESULT QueryInterface(void *driver, REFIID uuid, LPVOID *out);
static ULONG AddRef(void *driver);
static ULONG Release(void *driver);
static OSStatus Initialize(AudioServerPlugInDriverRef driver, AudioServerPlugInHostRef host);
static OSStatus CreateDevice(AudioServerPlugInDriverRef driver, CFDictionaryRef description,
                             const AudioServerPlugInClientInfo *client, AudioObjectID *out);
static OSStatus DestroyDevice(AudioServerPlugInDriverRef driver, AudioObjectID device);
static OSStatus AddDeviceClient(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                const AudioServerPlugInClientInfo *client);
static OSStatus RemoveDeviceClient(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                   const AudioServerPlugInClientInfo *client);
static OSStatus PerformDeviceConfigurationChange(AudioServerPlugInDriverRef driver,
                                                 AudioObjectID device, UInt64 action, void *info);
static OSStatus AbortDeviceConfigurationChange(AudioServerPlugInDriverRef driver,
                                               AudioObjectID device, UInt64 action, void *info);
static Boolean HasProperty(AudioServerPlugInDriverRef driver, AudioObjectID object, pid_t client,
                           const AudioObjectPropertyAddress *address);
static OSStatus IsPropertySettable(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                   pid_t client, const AudioObjectPropertyAddress *address,
                                   Boolean *out);
static OSStatus GetPropertyDataSize(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                    pid_t client, const AudioObjectPropertyAddress *address,
                                    UInt32 qualifier_size, const void *qualifier, UInt32 *out);
static OSStatus GetPropertyData(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                pid_t client, const AudioObjectPropertyAddress *address,
                                UInt32 qualifier_size, const void *qualifier, UInt32 size,
                                UInt32 *out_size, void *out);
static OSStatus SetPropertyData(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                pid_t client, const AudioObjectPropertyAddress *address,
                                UInt32 qualifier_size, const void *qualifier, UInt32 size,
                                const void *data);
static OSStatus StartIO(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client);
static OSStatus StopIO(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client);
static OSStatus GetZeroTimeStamp(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                 UInt32 client, Float64 *sample_time, UInt64 *host_time,
                                 UInt64 *seed);
static OSStatus WillDoIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                  UInt32 client, UInt32 operation, Boolean *will_do,
                                  Boolean *in_place);
static OSStatus BeginIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                 UInt32 client, UInt32 operation, UInt32 frames,
                                 const AudioServerPlugInIOCycleInfo *cycle);
static OSStatus DoIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                              AudioObjectID stream, UInt32 client, UInt32 operation, UInt32 frames,
                              const AudioServerPlugInIOCycleInfo *cycle, void *main,
                              void *secondary);
static OSStatus EndIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                               UInt32 client, UInt32 operation, UInt32 frames,
                               const AudioServerPlugInIOCycleInfo *cycle);

static AudioServerPlugInDriverInterface gInterface = {
    NULL,
    QueryInterface,
    AddRef,
    Release,
    Initialize,
    CreateDevice,
    DestroyDevice,
    AddDeviceClient,
    RemoveDeviceClient,
    PerformDeviceConfigurationChange,
    AbortDeviceConfigurationChange,
    HasProperty,
    IsPropertySettable,
    GetPropertyDataSize,
    GetPropertyData,
    SetPropertyData,
    StartIO,
    StopIO,
    GetZeroTimeStamp,
    WillDoIOOperation,
    BeginIOOperation,
    DoIOOperation,
    EndIOOperation,
};
static AudioServerPlugInDriverInterface *gInterfacePointer = &gInterface;
static AudioServerPlugInDriverRef const gDriver = &gInterfacePointer;

void *VibeFuseMixer_Create(CFAllocatorRef allocator, CFUUIDRef requested_type) {
    (void)allocator;
    if (!CFEqual(requested_type, kAudioServerPlugInTypeUUID)) return NULL;
    return (void *)gDriver;
}

static HRESULT QueryInterface(void *driver, REFIID uuid, LPVOID *out) {
    if (driver != gDriver || out == NULL) return kAudioHardwareBadObjectError;
    CFUUIDRef requested = CFUUIDCreateFromUUIDBytes(NULL, uuid);
    if (requested == NULL) return kAudioHardwareIllegalOperationError;
    HRESULT result = E_NOINTERFACE;
    if (CFEqual(requested, IUnknownUUID) || CFEqual(requested, kAudioServerPlugInDriverInterfaceUUID)) {
        pthread_mutex_lock(&gStateMutex);
        gRefCount++;
        pthread_mutex_unlock(&gStateMutex);
        *out = (LPVOID)gDriver;
        result = S_OK;
    }
    CFRelease(requested);
    return result;
}

static ULONG AddRef(void *driver) {
    if (driver != gDriver) return 0;
    pthread_mutex_lock(&gStateMutex);
    ULONG count = ++gRefCount;
    pthread_mutex_unlock(&gStateMutex);
    return count;
}

static ULONG Release(void *driver) {
    if (driver != gDriver) return 0;
    pthread_mutex_lock(&gStateMutex);
    if (gRefCount > 0) gRefCount--;
    ULONG count = gRefCount;
    pthread_mutex_unlock(&gStateMutex);
    return count;
}

#pragma mark Basic operations

static OSStatus Initialize(AudioServerPlugInDriverRef driver, AudioServerPlugInHostRef host) {
    if (driver != gDriver) return kAudioHardwareBadObjectError;
    gHost = host;
    if (gNotifyQueue == NULL) {
        gNotifyQueue = dispatch_queue_create("VibeFuseMixer.notify", DISPATCH_QUEUE_SERIAL);
    }
    factory_gains();
    for (int d = 0; d < kDeviceCount; d++) {
        store_float(&gDevice[d].volume_bits, 1);
        atomic_store(&gDevice[d].muted, 0);
        gDevice[d].stream_active[0] = gDevice[d].stream_active[1] = true;
    }
    for (int s = 0; s < kSources; s++) gSourceLevel[s] = kMeterFloor;
    for (int o = 0; o < kOutputs; o++) gOutputLevel[o] = kMeterFloor;
    reset_rings();
    load_state();
    return 0;
}

static OSStatus CreateDevice(AudioServerPlugInDriverRef driver, CFDictionaryRef description,
                             const AudioServerPlugInClientInfo *client, AudioObjectID *out) {
    (void)driver, (void)description, (void)client, (void)out;
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus DestroyDevice(AudioServerPlugInDriverRef driver, AudioObjectID device) {
    (void)driver, (void)device;
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus AddDeviceClient(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                const AudioServerPlugInClientInfo *client) {
    (void)device, (void)client;
    return driver == gDriver ? 0 : kAudioHardwareBadObjectError;
}

static OSStatus RemoveDeviceClient(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                   const AudioServerPlugInClientInfo *client) {
    (void)device, (void)client;
    return driver == gDriver ? 0 : kAudioHardwareBadObjectError;
}

static OSStatus PerformDeviceConfigurationChange(AudioServerPlugInDriverRef driver,
                                                 AudioObjectID device, UInt64 action, void *info) {
    (void)info;
    int d, part;
    if (driver != gDriver || !split_id(device, &d, &part)) return kAudioHardwareBadObjectError;
    Float64 rate = (Float64)action;
    if (!is_supported_rate(rate)) return kAudioHardwareIllegalOperationError;
    pthread_mutex_lock(&gStateMutex);
    bool changed = rate != gSampleRate;
    gSampleRate = rate;
    pthread_mutex_unlock(&gStateMutex);
    if (changed) reset_rings();
    return 0;
}

static OSStatus AbortDeviceConfigurationChange(AudioServerPlugInDriverRef driver,
                                               AudioObjectID device, UInt64 action, void *info) {
    (void)device, (void)action, (void)info;
    return driver == gDriver ? 0 : kAudioHardwareBadObjectError;
}

#pragma mark Properties

// Writes a value of type T into `out` if it fits.
#define RETURN_VALUE(T, value)                                         \
    do {                                                               \
        if (size < sizeof(T)) return kAudioHardwareBadPropertySizeError; \
        *(T *)out = (value);                                           \
        *out_size = sizeof(T);                                         \
        return 0;                                                      \
    } while (0)

static bool is_custom(AudioObjectPropertySelector selector) {
    return selector == kPropertyStatus || selector == kPropertyGains ||
           selector == kPropertyMeters || selector == kPropertyRunning;
}

static Boolean plugin_has(const AudioObjectPropertyAddress *a) {
    switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
        case kAudioObjectPropertyOwner:
        case kAudioObjectPropertyManufacturer:
        case kAudioObjectPropertyOwnedObjects:
        case kAudioObjectPropertyCustomPropertyInfoList:
        case kAudioPlugInPropertyBoxList:
        case kAudioPlugInPropertyTranslateUIDToBox:
        case kAudioPlugInPropertyDeviceList:
        case kAudioPlugInPropertyTranslateUIDToDevice:
        case kAudioPlugInPropertyResourceBundle:
            return true;
        default:
            return is_custom(a->mSelector);
    }
}

static bool scope_has_stream(int d, AudioObjectPropertyScope scope) {
    const DeviceSpec *spec = &kDevices[d];
    if (scope == kAudioObjectPropertyScopeInput) return spec->mix >= 0;
    if (scope == kAudioObjectPropertyScopeOutput) return spec->ring >= 0;
    return spec->mix >= 0 || spec->ring >= 0;
}

static Boolean device_has(const AudioObjectPropertyAddress *a) {
    switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
        case kAudioObjectPropertyOwner:
        case kAudioObjectPropertyName:
        case kAudioObjectPropertyManufacturer:
        case kAudioObjectPropertyOwnedObjects:
        case kAudioObjectPropertyControlList:
        case kAudioDevicePropertyDeviceUID:
        case kAudioDevicePropertyModelUID:
        case kAudioDevicePropertyTransportType:
        case kAudioDevicePropertyRelatedDevices:
        case kAudioDevicePropertyClockDomain:
        case kAudioDevicePropertyDeviceIsAlive:
        case kAudioDevicePropertyDeviceIsRunning:
        case kAudioDevicePropertyNominalSampleRate:
        case kAudioDevicePropertyAvailableNominalSampleRates:
        case kAudioDevicePropertyIsHidden:
        case kAudioDevicePropertyZeroTimeStampPeriod:
        case kAudioDevicePropertyStreams:
        case kAudioDevicePropertyClockIsStable:
        // The HAL asks for these in both directions, also on a device that
        // only has one: an unknown property there makes it drop the device.
        case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
        case kAudioDevicePropertyLatency:
        case kAudioDevicePropertySafetyOffset:
        case kAudioDevicePropertyPreferredChannelsForStereo:
        case kAudioDevicePropertyPreferredChannelLayout:
            return true;
        default:
            return false;
    }
}

static Boolean stream_has(const AudioObjectPropertyAddress *a) {
    switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
        case kAudioObjectPropertyOwner:
        case kAudioObjectPropertyOwnedObjects:
        case kAudioStreamPropertyIsActive:
        case kAudioStreamPropertyDirection:
        case kAudioStreamPropertyTerminalType:
        case kAudioStreamPropertyStartingChannel:
        case kAudioStreamPropertyLatency:
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat:
        case kAudioStreamPropertyAvailableVirtualFormats:
        case kAudioStreamPropertyAvailablePhysicalFormats:
            return true;
        default:
            return false;
    }
}

static Boolean control_has(int part, const AudioObjectPropertyAddress *a) {
    switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
        case kAudioObjectPropertyOwner:
        case kAudioObjectPropertyOwnedObjects:
        case kAudioControlPropertyScope:
        case kAudioControlPropertyElement:
            return true;
        case kAudioLevelControlPropertyScalarValue:
        case kAudioLevelControlPropertyDecibelValue:
        case kAudioLevelControlPropertyDecibelRange:
        case kAudioLevelControlPropertyConvertScalarToDecibels:
        case kAudioLevelControlPropertyConvertDecibelsToScalar:
            return part == kVolumeControl;
        case kAudioBooleanControlPropertyValue:
            return part == kMuteControl;
        default:
            return false;
    }
}

static Boolean HasProperty(AudioServerPlugInDriverRef driver, AudioObjectID object, pid_t client,
                           const AudioObjectPropertyAddress *address) {
    (void)client;
    if (driver != gDriver || address == NULL) return false;
    if (object == kAudioObjectPlugInObject) return plugin_has(address);
    int d, part;
    if (!split_id(object, &d, &part)) return false;
    switch (part) {
        case kDeviceObject:
            return device_has(address);
        case kInputStream:
        case kOutputStream:
            return stream_has(address);
        default:
            return control_has(part, address);
    }
}

static OSStatus IsPropertySettable(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                   pid_t client, const AudioObjectPropertyAddress *address,
                                   Boolean *out) {
    if (!HasProperty(driver, object, client, address)) return kAudioHardwareUnknownPropertyError;
    if (out == NULL) return kAudioHardwareIllegalOperationError;
    AudioObjectPropertySelector s = address->mSelector;
    if (object == kAudioObjectPlugInObject) {
        *out = s == kPropertyGains;
        return 0;
    }
    int d, part;
    split_id(object, &d, &part);
    switch (part) {
        case kDeviceObject:
            *out = s == kAudioDevicePropertyNominalSampleRate;
            break;
        case kInputStream:
        case kOutputStream:
            *out = s == kAudioStreamPropertyIsActive || s == kAudioStreamPropertyVirtualFormat ||
                   s == kAudioStreamPropertyPhysicalFormat;
            break;
        default:
            *out = s == kAudioLevelControlPropertyScalarValue ||
                   s == kAudioLevelControlPropertyDecibelValue ||
                   s == kAudioBooleanControlPropertyValue;
            break;
    }
    return 0;
}

// Objects a device owns in a scope.
static UInt32 device_objects(int d, AudioObjectPropertyScope scope, bool streams_only,
                             AudioObjectID ids[4]) {
    UInt32 n = 0;
    bool input = scope == kAudioObjectPropertyScopeGlobal || scope == kAudioObjectPropertyScopeInput;
    bool output =
        scope == kAudioObjectPropertyScopeGlobal || scope == kAudioObjectPropertyScopeOutput;
    if (input && kDevices[d].mix >= 0) ids[n++] = object_id(d, kInputStream);
    if (output && kDevices[d].ring >= 0) ids[n++] = object_id(d, kOutputStream);
    if (!streams_only && output && has_controls(d)) {
        ids[n++] = object_id(d, kVolumeControl);
        ids[n++] = object_id(d, kMuteControl);
    }
    return n;
}

static UInt32 control_list(int d, AudioObjectID ids[2]) {
    if (!has_controls(d)) return 0;
    ids[0] = object_id(d, kVolumeControl);
    ids[1] = object_id(d, kMuteControl);
    return 2;
}

static OSStatus GetPropertyDataSize(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                    pid_t client, const AudioObjectPropertyAddress *address,
                                    UInt32 qualifier_size, const void *qualifier, UInt32 *out) {
    (void)qualifier_size, (void)qualifier;
    if (!HasProperty(driver, object, client, address)) return kAudioHardwareUnknownPropertyError;
    if (out == NULL) return kAudioHardwareIllegalOperationError;
    AudioObjectPropertySelector s = address->mSelector;
    if (object == kAudioObjectPlugInObject) {
        switch (s) {
            case kAudioObjectPropertyBaseClass:
            case kAudioObjectPropertyClass:
                *out = sizeof(AudioClassID);
                break;
            case kAudioObjectPropertyOwner:
            case kAudioPlugInPropertyTranslateUIDToBox:
            case kAudioPlugInPropertyTranslateUIDToDevice:
                *out = sizeof(AudioObjectID);
                break;
            case kAudioObjectPropertyManufacturer:
            case kAudioPlugInPropertyResourceBundle:
                *out = sizeof(CFStringRef);
                break;
            case kAudioObjectPropertyOwnedObjects:
            case kAudioPlugInPropertyDeviceList:
                *out = kDeviceCount * sizeof(AudioObjectID);
                break;
            case kAudioPlugInPropertyBoxList:
                *out = 0;
                break;
            case kAudioObjectPropertyCustomPropertyInfoList:
                *out = 4 * sizeof(AudioServerPlugInCustomPropertyInfo);
                break;
            default:
                *out = sizeof(CFPropertyListRef);
                break;
        }
        return 0;
    }
    int d, part;
    split_id(object, &d, &part);
    AudioObjectID ids[4];
    switch (part) {
        case kDeviceObject:
            switch (s) {
                case kAudioObjectPropertyBaseClass:
                case kAudioObjectPropertyClass:
                    *out = sizeof(AudioClassID);
                    break;
                case kAudioObjectPropertyOwner:
                    *out = sizeof(AudioObjectID);
                    break;
                case kAudioObjectPropertyName:
                case kAudioObjectPropertyManufacturer:
                case kAudioDevicePropertyDeviceUID:
                case kAudioDevicePropertyModelUID:
                    *out = sizeof(CFStringRef);
                    break;
                case kAudioObjectPropertyOwnedObjects:
                    *out = device_objects(d, address->mScope, false, ids) * sizeof(AudioObjectID);
                    break;
                case kAudioDevicePropertyStreams:
                    *out = device_objects(d, address->mScope, true, ids) * sizeof(AudioObjectID);
                    break;
                case kAudioObjectPropertyControlList:
                    *out = control_list(d, ids) * sizeof(AudioObjectID);
                    break;
                case kAudioDevicePropertyRelatedDevices:
                    *out = sizeof(AudioObjectID);
                    break;
                case kAudioDevicePropertyNominalSampleRate:
                    *out = sizeof(Float64);
                    break;
                case kAudioDevicePropertyAvailableNominalSampleRates:
                    *out = kRateCount * sizeof(AudioValueRange);
                    break;
                case kAudioDevicePropertyPreferredChannelsForStereo:
                    *out = 2 * sizeof(UInt32);
                    break;
                case kAudioDevicePropertyPreferredChannelLayout:
                    *out = offsetof(AudioChannelLayout, mChannelDescriptions) +
                           kChannels * sizeof(AudioChannelDescription);
                    break;
                default:
                    *out = sizeof(UInt32);
                    break;
            }
            return 0;
        case kInputStream:
        case kOutputStream:
            switch (s) {
                case kAudioObjectPropertyBaseClass:
                case kAudioObjectPropertyClass:
                    *out = sizeof(AudioClassID);
                    break;
                case kAudioObjectPropertyOwner:
                    *out = sizeof(AudioObjectID);
                    break;
                case kAudioObjectPropertyOwnedObjects:
                    *out = 0;
                    break;
                case kAudioStreamPropertyVirtualFormat:
                case kAudioStreamPropertyPhysicalFormat:
                    *out = sizeof(AudioStreamBasicDescription);
                    break;
                case kAudioStreamPropertyAvailableVirtualFormats:
                case kAudioStreamPropertyAvailablePhysicalFormats:
                    *out = kRateCount * sizeof(AudioStreamRangedDescription);
                    break;
                default:
                    *out = sizeof(UInt32);
                    break;
            }
            return 0;
        default:
            switch (s) {
                case kAudioObjectPropertyBaseClass:
                case kAudioObjectPropertyClass:
                    *out = sizeof(AudioClassID);
                    break;
                case kAudioObjectPropertyOwner:
                    *out = sizeof(AudioObjectID);
                    break;
                case kAudioObjectPropertyOwnedObjects:
                    *out = 0;
                    break;
                case kAudioControlPropertyScope:
                    *out = sizeof(AudioObjectPropertyScope);
                    break;
                case kAudioControlPropertyElement:
                    *out = sizeof(AudioObjectPropertyElement);
                    break;
                case kAudioLevelControlPropertyDecibelRange:
                    *out = sizeof(AudioValueRange);
                    break;
                case kAudioBooleanControlPropertyValue:
                    *out = sizeof(UInt32);
                    break;
                default:
                    *out = sizeof(Float32);
                    break;
            }
            return 0;
    }
}

static CFStringRef copy_string(const char *text) {
    return CFStringCreateWithCString(NULL, text, kCFStringEncodingUTF8);
}

static OSStatus write_ids(const AudioObjectID *ids, UInt32 count, UInt32 size, UInt32 *out_size,
                          void *out) {
    UInt32 n = size / sizeof(AudioObjectID);
    if (n > count) n = count;
    memcpy(out, ids, n * sizeof(AudioObjectID));
    *out_size = n * sizeof(AudioObjectID);
    return 0;
}

static OSStatus plugin_get(const AudioObjectPropertyAddress *a, UInt32 qualifier_size,
                           const void *qualifier, UInt32 size, UInt32 *out_size, void *out) {
    switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
            RETURN_VALUE(AudioClassID, kAudioObjectClassID);
        case kAudioObjectPropertyClass:
            RETURN_VALUE(AudioClassID, kAudioPlugInClassID);
        case kAudioObjectPropertyOwner:
            RETURN_VALUE(AudioObjectID, kAudioObjectUnknown);
        case kAudioObjectPropertyManufacturer:
            RETURN_VALUE(CFStringRef, CFSTR("VibeFuse"));
        case kAudioPlugInPropertyResourceBundle:
            RETURN_VALUE(CFStringRef, CFSTR(""));
        case kAudioObjectPropertyOwnedObjects:
        case kAudioPlugInPropertyDeviceList: {
            AudioObjectID ids[kDeviceCount];
            for (int d = 0; d < kDeviceCount; d++) ids[d] = object_id(d, kDeviceObject);
            return write_ids(ids, kDeviceCount, size, out_size, out);
        }
        case kAudioPlugInPropertyBoxList:
            *out_size = 0;
            return 0;
        case kAudioPlugInPropertyTranslateUIDToBox:
            RETURN_VALUE(AudioObjectID, kAudioObjectUnknown);
        case kAudioPlugInPropertyTranslateUIDToDevice: {
            if (qualifier_size != sizeof(CFStringRef) || qualifier == NULL) {
                return kAudioHardwareBadPropertySizeError;
            }
            CFStringRef uid = *(const CFStringRef *)qualifier;
            AudioObjectID found = kAudioObjectUnknown;
            for (int d = 0; d < kDeviceCount && uid != NULL; d++) {
                CFStringRef candidate = copy_string(kDevices[d].uid);
                if (candidate != NULL) {
                    if (CFStringCompare(uid, candidate, 0) == kCFCompareEqualTo) {
                        found = object_id(d, kDeviceObject);
                    }
                    CFRelease(candidate);
                }
            }
            RETURN_VALUE(AudioObjectID, found);
        }
        case kAudioObjectPropertyCustomPropertyInfoList: {
            AudioServerPlugInCustomPropertyInfo info[4] = {
                {kPropertyStatus, kAudioServerPlugInCustomPropertyDataTypeCFPropertyList,
                 kAudioServerPlugInCustomPropertyDataTypeNone},
                {kPropertyGains, kAudioServerPlugInCustomPropertyDataTypeCFPropertyList,
                 kAudioServerPlugInCustomPropertyDataTypeNone},
                {kPropertyMeters, kAudioServerPlugInCustomPropertyDataTypeCFPropertyList,
                 kAudioServerPlugInCustomPropertyDataTypeNone},
                {kPropertyRunning, kAudioServerPlugInCustomPropertyDataTypeCFPropertyList,
                 kAudioServerPlugInCustomPropertyDataTypeNone},
            };
            UInt32 n = size / sizeof info[0];
            if (n > 4) n = 4;
            memcpy(out, info, n * sizeof info[0]);
            *out_size = n * sizeof info[0];
            return 0;
        }
        case kPropertyStatus:
            RETURN_VALUE(CFPropertyListRef, copy_status());
        case kPropertyMeters:
            RETURN_VALUE(CFPropertyListRef, copy_meters());
        case kPropertyRunning:
            RETURN_VALUE(CFPropertyListRef, copy_running());
        case kPropertyGains:
            // Write-only; reads get an empty value.
            RETURN_VALUE(CFPropertyListRef, CFDataCreate(NULL, NULL, 0));
        default:
            return kAudioHardwareUnknownPropertyError;
    }
}

static OSStatus device_get(int d, const AudioObjectPropertyAddress *a, UInt32 size,
                           UInt32 *out_size, void *out) {
    const DeviceSpec *spec = &kDevices[d];
    switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
            RETURN_VALUE(AudioClassID, kAudioObjectClassID);
        case kAudioObjectPropertyClass:
            RETURN_VALUE(AudioClassID, kAudioDeviceClassID);
        case kAudioObjectPropertyOwner:
            RETURN_VALUE(AudioObjectID, kAudioObjectPlugInObject);
        case kAudioObjectPropertyName:
            RETURN_VALUE(CFStringRef, copy_string(spec->name));
        case kAudioObjectPropertyManufacturer:
            RETURN_VALUE(CFStringRef, CFSTR("VibeFuse"));
        case kAudioDevicePropertyDeviceUID:
            RETURN_VALUE(CFStringRef, copy_string(spec->uid));
        case kAudioDevicePropertyModelUID:
            RETURN_VALUE(CFStringRef, CFSTR("VibeFuse:Model"));
        case kAudioObjectPropertyOwnedObjects:
        case kAudioDevicePropertyStreams: {
            AudioObjectID ids[4];
            bool streams = a->mSelector == kAudioDevicePropertyStreams;
            UInt32 n = device_objects(d, a->mScope, streams, ids);
            return write_ids(ids, n, size, out_size, out);
        }
        case kAudioObjectPropertyControlList: {
            AudioObjectID ids[2];
            return write_ids(ids, control_list(d, ids), size, out_size, out);
        }
        case kAudioDevicePropertyTransportType:
            RETURN_VALUE(UInt32, kAudioDeviceTransportTypeVirtual);
        case kAudioDevicePropertyRelatedDevices: {
            AudioObjectID self = object_id(d, kDeviceObject);
            return write_ids(&self, 1, size, out_size, out);
        }
        case kAudioDevicePropertyClockDomain:
            RETURN_VALUE(UInt32, kClockDomain);
        case kAudioDevicePropertyDeviceIsAlive:
        case kAudioDevicePropertyClockIsStable:
            RETURN_VALUE(UInt32, 1);
        case kAudioDevicePropertyDeviceIsRunning: {
            pthread_mutex_lock(&gStateMutex);
            UInt32 running = gDevice[d].io_count > 0;
            pthread_mutex_unlock(&gStateMutex);
            RETURN_VALUE(UInt32, running);
        }
        case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
            RETURN_VALUE(UInt32, !spec->hidden && scope_has_stream(d, a->mScope));
        case kAudioDevicePropertyLatency:
        case kAudioDevicePropertySafetyOffset:
            RETURN_VALUE(UInt32, 0);
        case kAudioDevicePropertyNominalSampleRate: {
            pthread_mutex_lock(&gStateMutex);
            Float64 rate = gSampleRate;
            pthread_mutex_unlock(&gStateMutex);
            RETURN_VALUE(Float64, rate);
        }
        case kAudioDevicePropertyAvailableNominalSampleRates: {
            UInt32 n = size / sizeof(AudioValueRange);
            if (n > kRateCount) n = kRateCount;
            AudioValueRange *ranges = out;
            for (UInt32 i = 0; i < n; i++) ranges[i] = (AudioValueRange){kRates[i], kRates[i]};
            *out_size = n * sizeof(AudioValueRange);
            return 0;
        }
        case kAudioDevicePropertyIsHidden:
            RETURN_VALUE(UInt32, spec->hidden);
        case kAudioDevicePropertyZeroTimeStampPeriod:
            RETURN_VALUE(UInt32, kZeroTimeStampPeriod);
        case kAudioDevicePropertyPreferredChannelsForStereo: {
            if (size < 2 * sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            ((UInt32 *)out)[0] = 1;
            ((UInt32 *)out)[1] = 2;
            *out_size = 2 * sizeof(UInt32);
            return 0;
        }
        case kAudioDevicePropertyPreferredChannelLayout: {
            UInt32 need = offsetof(AudioChannelLayout, mChannelDescriptions) +
                          kChannels * sizeof(AudioChannelDescription);
            if (size < need) return kAudioHardwareBadPropertySizeError;
            AudioChannelLayout *layout = out;
            memset(layout, 0, need);
            layout->mChannelLayoutTag = kAudioChannelLayoutTag_UseChannelDescriptions;
            layout->mNumberChannelDescriptions = kChannels;
            layout->mChannelDescriptions[0].mChannelLabel = kAudioChannelLabel_Left;
            layout->mChannelDescriptions[1].mChannelLabel = kAudioChannelLabel_Right;
            *out_size = need;
            return 0;
        }
        default:
            return kAudioHardwareUnknownPropertyError;
    }
}

static OSStatus stream_get(int d, int part, const AudioObjectPropertyAddress *a, UInt32 size,
                           UInt32 *out_size, void *out) {
    bool input = part == kInputStream;
    switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
            RETURN_VALUE(AudioClassID, kAudioObjectClassID);
        case kAudioObjectPropertyClass:
            RETURN_VALUE(AudioClassID, kAudioStreamClassID);
        case kAudioObjectPropertyOwner:
            RETURN_VALUE(AudioObjectID, object_id(d, kDeviceObject));
        case kAudioObjectPropertyOwnedObjects:
            *out_size = 0;
            return 0;
        case kAudioStreamPropertyIsActive: {
            pthread_mutex_lock(&gStateMutex);
            UInt32 active = gDevice[d].stream_active[input ? 0 : 1];
            pthread_mutex_unlock(&gStateMutex);
            RETURN_VALUE(UInt32, active);
        }
        case kAudioStreamPropertyDirection:
            RETURN_VALUE(UInt32, input ? 1 : 0);
        case kAudioStreamPropertyTerminalType:
            RETURN_VALUE(UInt32, kAudioStreamTerminalTypeLine);
        case kAudioStreamPropertyStartingChannel:
            RETURN_VALUE(UInt32, 1);
        case kAudioStreamPropertyLatency:
            RETURN_VALUE(UInt32, 0);
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat: {
            pthread_mutex_lock(&gStateMutex);
            Float64 rate = gSampleRate;
            pthread_mutex_unlock(&gStateMutex);
            RETURN_VALUE(AudioStreamBasicDescription, stream_format(rate));
        }
        case kAudioStreamPropertyAvailableVirtualFormats:
        case kAudioStreamPropertyAvailablePhysicalFormats: {
            UInt32 n = size / sizeof(AudioStreamRangedDescription);
            if (n > kRateCount) n = kRateCount;
            AudioStreamRangedDescription *formats = out;
            for (UInt32 i = 0; i < n; i++) {
                formats[i].mFormat = stream_format(kRates[i]);
                formats[i].mSampleRateRange = (AudioValueRange){kRates[i], kRates[i]};
            }
            *out_size = n * sizeof(AudioStreamRangedDescription);
            return 0;
        }
        default:
            return kAudioHardwareUnknownPropertyError;
    }
}

static OSStatus control_get(int d, int part, const AudioObjectPropertyAddress *a, UInt32 qualifier_size,
                            const void *qualifier, UInt32 size, UInt32 *out_size, void *out) {
    (void)qualifier_size, (void)qualifier;
    bool volume = part == kVolumeControl;
    switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
            RETURN_VALUE(AudioClassID, volume ? kAudioLevelControlClassID : kAudioBooleanControlClassID);
        case kAudioObjectPropertyClass:
            RETURN_VALUE(AudioClassID, volume ? kAudioVolumeControlClassID : kAudioMuteControlClassID);
        case kAudioObjectPropertyOwner:
            RETURN_VALUE(AudioObjectID, object_id(d, kDeviceObject));
        case kAudioObjectPropertyOwnedObjects:
            *out_size = 0;
            return 0;
        case kAudioControlPropertyScope:
            RETURN_VALUE(AudioObjectPropertyScope, kAudioObjectPropertyScopeOutput);
        case kAudioControlPropertyElement:
            RETURN_VALUE(AudioObjectPropertyElement, kAudioObjectPropertyElementMain);
        case kAudioLevelControlPropertyScalarValue:
            RETURN_VALUE(Float32, load_float(&gDevice[d].volume_bits));
        case kAudioLevelControlPropertyDecibelValue:
            RETURN_VALUE(Float32, scalar_to_db(load_float(&gDevice[d].volume_bits)));
        case kAudioLevelControlPropertyDecibelRange:
            RETURN_VALUE(AudioValueRange, ((AudioValueRange){kVolumeMinDB, 0}));
        case kAudioLevelControlPropertyConvertScalarToDecibels:
            // The value to convert comes in `out`.
            if (size < sizeof(Float32)) return kAudioHardwareBadPropertySizeError;
            *(Float32 *)out = scalar_to_db(*(Float32 *)out);
            *out_size = sizeof(Float32);
            return 0;
        case kAudioLevelControlPropertyConvertDecibelsToScalar:
            if (size < sizeof(Float32)) return kAudioHardwareBadPropertySizeError;
            *(Float32 *)out = db_to_scalar(*(Float32 *)out);
            *out_size = sizeof(Float32);
            return 0;
        case kAudioBooleanControlPropertyValue:
            RETURN_VALUE(UInt32, atomic_load(&gDevice[d].muted));
        default:
            return kAudioHardwareUnknownPropertyError;
    }
}

// The HAL logs failed reads without saying which property it wanted.
static void log_unknown(AudioObjectID object, const AudioObjectPropertyAddress *a) {
    char selector[5], scope[5];
    for (int i = 0; i < 4; i++) {
        selector[i] = (char)(a->mSelector >> (24 - 8 * i));
        scope[i] = (char)(a->mScope >> (24 - 8 * i));
    }
    selector[4] = scope[4] = 0;
    os_log(OS_LOG_DEFAULT, "VibeFuse Mixer: object %u has no property '%{public}s' in scope '%{public}s'",
           (unsigned)object, selector, scope);
}

static OSStatus GetPropertyData(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                pid_t client, const AudioObjectPropertyAddress *address,
                                UInt32 qualifier_size, const void *qualifier, UInt32 size,
                                UInt32 *out_size, void *out) {
    if (!HasProperty(driver, object, client, address)) {
        log_unknown(object, address);
        return kAudioHardwareUnknownPropertyError;
    }
    if (out_size == NULL || (out == NULL && size > 0)) return kAudioHardwareIllegalOperationError;
    if (object == kAudioObjectPlugInObject) {
        return plugin_get(address, qualifier_size, qualifier, size, out_size, out);
    }
    int d, part;
    split_id(object, &d, &part);
    switch (part) {
        case kDeviceObject:
            return device_get(d, address, size, out_size, out);
        case kInputStream:
        case kOutputStream:
            return stream_get(d, part, address, size, out_size, out);
        default:
            return control_get(d, part, address, qualifier_size, qualifier, size, out_size, out);
    }
}

static OSStatus SetPropertyData(AudioServerPlugInDriverRef driver, AudioObjectID object,
                                pid_t client, const AudioObjectPropertyAddress *address,
                                UInt32 qualifier_size, const void *qualifier, UInt32 size,
                                const void *data) {
    (void)qualifier_size, (void)qualifier;
    Boolean settable = false;
    OSStatus status = IsPropertySettable(driver, object, client, address, &settable);
    if (status != 0) return status;
    if (!settable) return kAudioHardwareUnsupportedOperationError;
    if (data == NULL) return kAudioHardwareIllegalOperationError;
    AudioObjectPropertySelector s = address->mSelector;
    if (object == kAudioObjectPlugInObject) {
        if (size < sizeof(CFPropertyListRef)) return kAudioHardwareBadPropertySizeError;
        CFPropertyListRef value = *(const CFPropertyListRef *)data;
        if (value == NULL || CFGetTypeID(value) != CFDataGetTypeID()) {
            return kAudioHardwareIllegalOperationError;
        }
        status = apply_gains(CFDataGetBytePtr(value), CFDataGetLength(value));
        if (status == 0) state_changed();
        return status;
    }
    int d, part;
    split_id(object, &d, &part);
    switch (part) {
        case kDeviceObject:
            if (size < sizeof(Float64)) return kAudioHardwareBadPropertySizeError;
            return request_rate(*(const Float64 *)data);
        case kInputStream:
        case kOutputStream:
            if (s == kAudioStreamPropertyIsActive) {
                if (size < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
                pthread_mutex_lock(&gStateMutex);
                gDevice[d].stream_active[part == kInputStream ? 0 : 1] = *(const UInt32 *)data != 0;
                pthread_mutex_unlock(&gStateMutex);
                notify(object, kAudioStreamPropertyIsActive);
                return 0;
            } else {
                if (size < sizeof(AudioStreamBasicDescription)) return kAudioHardwareBadPropertySizeError;
                const AudioStreamBasicDescription *format = data;
                AudioStreamBasicDescription ours = stream_format(format->mSampleRate);
                if (format->mFormatID != ours.mFormatID || format->mChannelsPerFrame != kChannels ||
                    format->mBitsPerChannel != 32 ||
                    (format->mFormatFlags & kAudioFormatFlagIsFloat) == 0) {
                    return kAudioDeviceUnsupportedFormatError;
                }
                return request_rate(format->mSampleRate);
            }
        case kVolumeControl: {
            if (size < sizeof(Float32)) return kAudioHardwareBadPropertySizeError;
            Float32 value = *(const Float32 *)data;
            Float32 scalar = s == kAudioLevelControlPropertyDecibelValue ? db_to_scalar(value) : value;
            scalar = scalar < 0 ? 0 : (scalar > 1 ? 1 : scalar);
            store_float(&gDevice[d].volume_bits, scalar);
            notify2(object, kAudioLevelControlPropertyScalarValue, kAudioLevelControlPropertyDecibelValue);
            state_changed();
            return 0;
        }
        default: {
            if (size < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            atomic_store(&gDevice[d].muted, *(const UInt32 *)data != 0);
            notify(object, kAudioBooleanControlPropertyValue);
            state_changed();
            return 0;
        }
    }
}

#pragma mark IO

static OSStatus StartIO(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client) {
    (void)client;
    int d, part;
    if (driver != gDriver || !split_id(device, &d, &part) || part != kDeviceObject) {
        return kAudioHardwareBadObjectError;
    }
    bool user_change = false;
    pthread_mutex_lock(&gStateMutex);
    DeviceState *state = &gDevice[d];
    if (state->io_count == 0) {
        Float64 ticks = host_ticks_per_second() / gSampleRate;
        Float64 now = (Float64)mach_absolute_time();
        if (gRunningDevices == 0) gAnchorHost = now;
        // Start on a whole frame of the shared clock, so sample times of all
        // devices differ by an integer.
        int64_t frames = (int64_t)floor((now - gAnchorHost) / ticks);
        state->offset = frames;
        state->start_host = gAnchorHost + (Float64)frames * ticks;
        state->ticks_per_frame = ticks;
        state->timestamps = 0;
        memset(state->ramp, 0, sizeof state->ramp);
        gRunningDevices++;
        if (!kDevices[d].hidden) {
            gRunningUserDevices++;
            user_change = true;
        }
    }
    state->io_count++;
    pthread_mutex_unlock(&gStateMutex);
    if (user_change) notify(kAudioObjectPlugInObject, kPropertyRunning);
    notify(device, kAudioDevicePropertyDeviceIsRunning);
    return 0;
}

static OSStatus StopIO(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client) {
    (void)client;
    int d, part;
    if (driver != gDriver || !split_id(device, &d, &part) || part != kDeviceObject) {
        return kAudioHardwareBadObjectError;
    }
    bool user_change = false;
    pthread_mutex_lock(&gStateMutex);
    DeviceState *state = &gDevice[d];
    if (state->io_count > 0 && --state->io_count == 0) {
        gRunningDevices--;
        if (!kDevices[d].hidden) {
            gRunningUserDevices--;
            user_change = true;
        }
    }
    pthread_mutex_unlock(&gStateMutex);
    if (user_change) notify(kAudioObjectPlugInObject, kPropertyRunning);
    notify(device, kAudioDevicePropertyDeviceIsRunning);
    flush_state();
    return 0;
}

static OSStatus GetZeroTimeStamp(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                 UInt32 client, Float64 *sample_time, UInt64 *host_time,
                                 UInt64 *seed) {
    (void)client;
    int d, part;
    if (driver != gDriver || !split_id(device, &d, &part)) return kAudioHardwareBadObjectError;
    DeviceState *state = &gDevice[d];
    Float64 period = state->ticks_per_frame * kZeroTimeStampPeriod;
    Float64 now = (Float64)mach_absolute_time();
    while (state->start_host + (Float64)(state->timestamps + 1) * period <= now) {
        state->timestamps++;
    }
    *sample_time = (Float64)(state->timestamps * kZeroTimeStampPeriod);
    *host_time = (UInt64)(state->start_host + (Float64)state->timestamps * period);
    *seed = 1;
    return 0;
}

static OSStatus WillDoIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                  UInt32 client, UInt32 operation, Boolean *will_do,
                                  Boolean *in_place) {
    (void)client;
    int d, part;
    if (driver != gDriver || !split_id(device, &d, &part)) return kAudioHardwareBadObjectError;
    bool yes = (operation == kAudioServerPlugInIOOperationReadInput && kDevices[d].mix >= 0) ||
               (operation == kAudioServerPlugInIOOperationWriteMix && kDevices[d].ring >= 0);
    if (will_do) *will_do = yes;
    if (in_place) *in_place = true;
    return 0;
}

static OSStatus BeginIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                                 UInt32 client, UInt32 operation, UInt32 frames,
                                 const AudioServerPlugInIOCycleInfo *cycle) {
    (void)device, (void)client, (void)operation, (void)frames, (void)cycle;
    return driver == gDriver ? 0 : kAudioHardwareBadObjectError;
}

static OSStatus DoIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                              AudioObjectID stream, UInt32 client, UInt32 operation, UInt32 frames,
                              const AudioServerPlugInIOCycleInfo *cycle, void *main,
                              void *secondary) {
    (void)stream, (void)client, (void)secondary;
    int d, part;
    if (driver != gDriver || !split_id(device, &d, &part)) return kAudioHardwareBadObjectError;
    if (main == NULL || cycle == NULL) return 0;
    DeviceState *state = &gDevice[d];
    // Larger buffers are handled in pieces.
    for (UInt32 done = 0; done < frames;) {
        UInt32 count = frames - done;
        if (count > kMaxFrames) count = kMaxFrames;
        float *buffer = (float *)main + (size_t)done * kChannels;
        if (operation == kAudioServerPlugInIOOperationWriteMix && kDevices[d].ring >= 0) {
            int64_t time = (int64_t)llround(cycle->mOutputTime.mSampleTime) + state->offset + done;
            write_ring(kDevices[d].ring, time, buffer, count, device_gain(d));
        } else if (operation == kAudioServerPlugInIOOperationReadInput && kDevices[d].mix >= 0) {
            int64_t time = (int64_t)llround(cycle->mInputTime.mSampleTime) + state->offset + done;
            render_mix(state, kDevices[d].mix, time, count, buffer);
        }
        done += count;
    }
    return 0;
}

static OSStatus EndIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device,
                               UInt32 client, UInt32 operation, UInt32 frames,
                               const AudioServerPlugInIOCycleInfo *cycle) {
    (void)device, (void)client, (void)operation, (void)frames, (void)cycle;
    return driver == gDriver ? 0 : kAudioHardwareBadObjectError;
}
