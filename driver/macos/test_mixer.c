// Tests of VibeFuseMixer.c without the audio server: this program plays the
// Host, the way coreaudiod calls a plug-in. `cargo test` builds and runs it.

#include "VibeFuseMixer.c"

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static int failures;

#define CHECK(condition)                                                           \
    do {                                                                           \
        if (!(condition)) {                                                        \
            fprintf(stderr, "%s:%d: check failed: %s\n", __FILE__, __LINE__, #condition); \
            failures++;                                                            \
        }                                                                          \
    } while (0)

#define CHECK_NEAR(a, b, eps) CHECK(fabs((double)(a) - (double)(b)) <= (eps))

// The Host.

static CFMutableDictionaryRef gStorage;
static int gChanged[64];
static int gChangedCount;
static int gConfigRequests;

static OSStatus host_properties_changed(AudioServerPlugInHostRef host, AudioObjectID object,
                                        UInt32 count, const AudioObjectPropertyAddress *addresses) {
    (void)host, (void)object;
    for (UInt32 i = 0; i < count && gChangedCount < 64; i++) gChanged[gChangedCount++] = (int)addresses[i].mSelector;
    return 0;
}

static OSStatus host_copy(AudioServerPlugInHostRef host, CFStringRef key, CFPropertyListRef *out) {
    (void)host;
    CFPropertyListRef value = CFDictionaryGetValue(gStorage, key);
    *out = value ? CFRetain(value) : NULL;
    return 0;
}

static OSStatus host_write(AudioServerPlugInHostRef host, CFStringRef key, CFPropertyListRef data) {
    (void)host;
    CFDictionarySetValue(gStorage, key, data);
    return 0;
}

static OSStatus host_delete(AudioServerPlugInHostRef host, CFStringRef key) {
    (void)host;
    CFDictionaryRemoveValue(gStorage, key);
    return 0;
}

static OSStatus host_request(AudioServerPlugInHostRef host, AudioObjectID device, UInt64 action,
                             void *info) {
    (void)host;
    gConfigRequests++;
    // The real Host stops IO, lets the device change and starts IO again.
    return gInterface.PerformDeviceConfigurationChange(gDriver, device, action, info);
}

static AudioServerPlugInHostInterface gTestHost = {
    host_properties_changed, host_copy, host_write, host_delete, host_request,
};

// Helpers.

static void drain(void) { dispatch_sync(gNotifyQueue, ^{}); }

static bool changed(AudioObjectPropertySelector selector) {
    drain();
    for (int i = 0; i < gChangedCount; i++) {
        if ((AudioObjectPropertySelector)gChanged[i] == selector) return true;
    }
    return false;
}

static AudioObjectPropertyAddress address(AudioObjectPropertySelector selector,
                                          AudioObjectPropertyScope scope) {
    return (AudioObjectPropertyAddress){selector, scope, kAudioObjectPropertyElementMain};
}

static OSStatus get(AudioObjectID object, AudioObjectPropertySelector selector,
                    AudioObjectPropertyScope scope, UInt32 size, void *out, UInt32 *out_size) {
    AudioObjectPropertyAddress a = address(selector, scope);
    UInt32 written = 0;
    OSStatus status = gInterface.GetPropertyData(gDriver, object, 0, &a, 0, NULL, size, &written, out);
    if (out_size) *out_size = written;
    return status;
}

static UInt32 get_u32(AudioObjectID object, AudioObjectPropertySelector selector,
                      AudioObjectPropertyScope scope) {
    UInt32 value = 0xFFFFFFFF;
    CHECK(get(object, selector, scope, sizeof value, &value, NULL) == 0);
    return value;
}

static CFDataRef get_data(AudioObjectPropertySelector selector) {
    CFPropertyListRef value = NULL;
    CHECK(get(kAudioObjectPlugInObject, selector, kAudioObjectPropertyScopeGlobal, sizeof value,
              &value, NULL) == 0);
    CHECK(value != NULL && CFGetTypeID(value) == CFDataGetTypeID());
    return value;
}

static OSStatus send_gains(const UInt8 *bytes, CFIndex length) {
    CFDataRef data = CFDataCreate(NULL, bytes, length);
    AudioObjectPropertyAddress a = address(kPropertyGains, kAudioObjectPropertyScopeGlobal);
    OSStatus status =
        gInterface.SetPropertyData(gDriver, kAudioObjectPlugInObject, 0, &a, 0, NULL, sizeof data, &data);
    CFRelease(data);
    return status;
}

static AudioServerPlugInIOCycleInfo cycle_at(Float64 sample_time) {
    AudioServerPlugInIOCycleInfo cycle = {0};
    cycle.mInputTime.mSampleTime = sample_time;
    cycle.mOutputTime.mSampleTime = sample_time;
    return cycle;
}

static void write_device(int d, Float64 time, float *frames, UInt32 count) {
    AudioServerPlugInIOCycleInfo cycle = cycle_at(time);
    CHECK(gInterface.DoIOOperation(gDriver, object_id(d, kDeviceObject), object_id(d, kOutputStream), 0,
                                   kAudioServerPlugInIOOperationWriteMix, count, &cycle, frames,
                                   NULL) == 0);
}

static void read_device(int d, Float64 time, float *frames, UInt32 count) {
    AudioServerPlugInIOCycleInfo cycle = cycle_at(time);
    CHECK(gInterface.DoIOOperation(gDriver, object_id(d, kDeviceObject), object_id(d, kInputStream), 0,
                                   kAudioServerPlugInIOOperationReadInput, count, &cycle, frames,
                                   NULL) == 0);
}

static void start(int d) { CHECK(gInterface.StartIO(gDriver, object_id(d, kDeviceObject), 1) == 0); }
static void stop(int d) { CHECK(gInterface.StopIO(gDriver, object_id(d, kDeviceObject), 1) == 0); }

// Device sample time on `to` of sample time `time` on `from`.
static Float64 same_moment(int from, int to, Float64 time) {
    return time + (Float64)(gDevice[from].offset - gDevice[to].offset);
}

static void put_entry(UInt8 *p, uint8_t in_kind, uint8_t in, uint8_t out_kind, uint8_t out, int32_t gain) {
    p[0] = in_kind, p[1] = in, p[2] = out_kind, p[3] = out;
    write_i32(p + 4, gain);
}

enum { kMain = 0, kVirtual34 = 1, kVirtual56 = 2, kVirtual78 = 3, kMix34 = 4, kMix56 = 5, kLoop78 = 6, kBridge = 7 };
enum { N = 512 };

// Tests.

static void test_objects(void) {
    AudioObjectID devices[16];
    UInt32 size = 0;
    CHECK(get(kAudioObjectPlugInObject, kAudioPlugInPropertyDeviceList, kAudioObjectPropertyScopeGlobal,
              sizeof devices, devices, &size) == 0);
    CHECK(size == kDeviceCount * sizeof(AudioObjectID));

    CFStringRef uid = CFSTR("VibeFuse:Mix56");
    AudioObjectPropertyAddress a = address(kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal);
    AudioObjectID found = 0;
    UInt32 written = 0;
    CHECK(gInterface.GetPropertyData(gDriver, kAudioObjectPlugInObject, 0, &a, sizeof uid, &uid,
                                     sizeof found, &written, &found) == 0);
    CHECK(found == object_id(kMix56, kDeviceObject));

    CFStringRef name = NULL;
    CHECK(get(object_id(kVirtual56, kDeviceObject), kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
              sizeof name, &name, NULL) == 0);
    CHECK(name != NULL && CFStringCompare(name, CFSTR("VibeFuse Virtual 5/6"), 0) == kCFCompareEqualTo);
    if (name) CFRelease(name);

    CHECK(get_u32(object_id(kBridge, kDeviceObject), kAudioDevicePropertyIsHidden, kAudioObjectPropertyScopeGlobal) == 1);
    CHECK(get_u32(object_id(kMain, kDeviceObject), kAudioDevicePropertyIsHidden, kAudioObjectPropertyScopeGlobal) == 0);
    CHECK(get_u32(object_id(kMain, kDeviceObject), kAudioDevicePropertyClockDomain, kAudioObjectPropertyScopeGlobal) == kClockDomain);

    // An output device has an output stream and two controls, no input.
    AudioObjectID streams[4];
    CHECK(get(object_id(kMain, kDeviceObject), kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput,
              sizeof streams, streams, &size) == 0);
    CHECK(size == sizeof(AudioObjectID) && streams[0] == object_id(kMain, kOutputStream));
    CHECK(get(object_id(kMain, kDeviceObject), kAudioDevicePropertyStreams, kAudioObjectPropertyScopeInput,
              sizeof streams, streams, &size) == 0);
    CHECK(size == 0);
    // coreaudiod asks every device about both directions and drops a device
    // that does not answer, even when it has only one.
    static const AudioObjectPropertySelector both_ways[] = {
        kAudioDevicePropertyDeviceCanBeDefaultDevice, kAudioDevicePropertyDeviceCanBeDefaultSystemDevice,
        kAudioDevicePropertyLatency, kAudioDevicePropertySafetyOffset,
        kAudioDevicePropertyPreferredChannelsForStereo, kAudioDevicePropertyPreferredChannelLayout,
        kAudioDevicePropertyStreams,
    };
    static const AudioObjectPropertyScope scopes[] = {
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput,
    };
    for (int d = 0; d < kDeviceCount; d++) {
        for (size_t i = 0; i < sizeof both_ways / sizeof both_ways[0]; i++) {
            for (size_t j = 0; j < sizeof scopes / sizeof scopes[0]; j++) {
                AudioObjectPropertyAddress p = address(both_ways[i], scopes[j]);
                UInt32 need = 0;
                UInt8 value[256];
                CHECK(gInterface.HasProperty(gDriver, object_id(d, kDeviceObject), 0, &p));
                CHECK(gInterface.GetPropertyDataSize(gDriver, object_id(d, kDeviceObject), 0, &p, 0, NULL, &need) == 0);
                CHECK(need <= sizeof value);
                CHECK(get(object_id(d, kDeviceObject), both_ways[i], scopes[j], need, value, NULL) == 0);
            }
        }
    }
    CHECK(get_u32(object_id(kMain, kDeviceObject), kAudioDevicePropertyDeviceCanBeDefaultDevice, kAudioObjectPropertyScopeOutput) == 1);
    CHECK(get_u32(object_id(kMain, kDeviceObject), kAudioDevicePropertyDeviceCanBeDefaultDevice, kAudioObjectPropertyScopeInput) == 0);
    CHECK(get_u32(object_id(kMix56, kDeviceObject), kAudioDevicePropertyDeviceCanBeDefaultDevice, kAudioObjectPropertyScopeInput) == 1);
    CHECK(get_u32(object_id(kBridge, kDeviceObject), kAudioDevicePropertyDeviceCanBeDefaultDevice, kAudioObjectPropertyScopeOutput) == 0);
    CHECK(get(object_id(kMain, kDeviceObject), kAudioObjectPropertyControlList, kAudioObjectPropertyScopeGlobal,
              sizeof streams, streams, &size) == 0);
    CHECK(size == 2 * sizeof(AudioObjectID));
    CHECK(get_u32(object_id(kMix56, kInputStream), kAudioStreamPropertyDirection, kAudioObjectPropertyScopeGlobal) == 1);
    CHECK(get_u32(object_id(kBridge, kOutputStream), kAudioStreamPropertyDirection, kAudioObjectPropertyScopeGlobal) == 0);

    // Objects that do not exist.
    AudioObjectPropertyAddress cls = address(kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal);
    CHECK(!gInterface.HasProperty(gDriver, object_id(kMix56, kOutputStream), 0, &cls));
    CHECK(!gInterface.HasProperty(gDriver, object_id(kMix56, kVolumeControl), 0, &cls));
    CHECK(!gInterface.HasProperty(gDriver, object_id(kBridge, kVolumeControl), 0, &cls));
    CHECK(!gInterface.HasProperty(gDriver, 2 + kDeviceCount * kObjectsPerDevice, 0, &cls));

    AudioStreamBasicDescription format;
    CHECK(get(object_id(kMix34, kInputStream), kAudioStreamPropertyVirtualFormat, kAudioObjectPropertyScopeGlobal,
              sizeof format, &format, NULL) == 0);
    CHECK(format.mSampleRate == 48000 && format.mChannelsPerFrame == 2 && format.mBitsPerChannel == 32);

    AudioServerPlugInCustomPropertyInfo info[4];
    CHECK(get(kAudioObjectPlugInObject, kAudioObjectPropertyCustomPropertyInfoList, kAudioObjectPropertyScopeGlobal,
              sizeof info, info, &size) == 0);
    CHECK(size == sizeof info && info[1].mSelector == kPropertyGains);
    Boolean settable = false;
    AudioObjectPropertyAddress gains = address(kPropertyGains, kAudioObjectPropertyScopeGlobal);
    CHECK(gInterface.IsPropertySettable(gDriver, kAudioObjectPlugInObject, 0, &gains, &settable) == 0 && settable);
}

static void test_mixing(void) {
    float in[N * 2], out[N * 2];
    start(kVirtual56);
    CHECK(changed(kPropertyRunning));
    usleep(3000);
    start(kMix56);
    start(kMix34);
    CHECK(gDevice[kMix56].offset >= gDevice[kVirtual56].offset);

    // The factory mix routes Virtual 5/6 to Mix 5/6 and nothing to Mix 3/4.
    for (int i = 0; i < N; i++) {
        in[2 * i] = sinf((float)i * 0.05f) * 0.5f;
        in[2 * i + 1] = -in[2 * i];
    }
    Float64 t0 = 4096;
    for (int b = 0; b < 3; b++) write_device(kVirtual56, t0 + b * N, in, N);
    for (int b = 0; b < 2; b++) read_device(kMix56, same_moment(kVirtual56, kMix56, t0 + b * N), out, N);
    // The second buffer is past the gain ramp.
    for (int i = 0; i < N * 2; i++) CHECK_NEAR(out[i], in[i], 1e-6);
    read_device(kMix34, same_moment(kVirtual56, kMix34, t0), out, N);
    float sum = 0;
    for (int i = 0; i < N * 2; i++) sum += fabsf(out[i]);
    CHECK(sum == 0);

    // Main left to the right of Mix 5/6 at half gain, Virtual 5/6 off: the
    // same bytes VibeFuse sends to the Windows driver.
    UInt8 property[8 + 3 * 8];
    write_i32(property, kMagic);
    write_i32(property + 4, kCrosspoints);
    put_entry(property + 8, 0, 0, 2, 1, kUnity / 2);
    put_entry(property + 16, 2, 0, 2, 0, 0);
    put_entry(property + 24, 2, 1, 2, 1, 0);
    CHECK(send_gains(property, sizeof property) == 0);
    start(kMain);
    float main[N * 2];
    for (int i = 0; i < N; i++) main[2 * i] = 0.25f, main[2 * i + 1] = 0.75f;
    Float64 t1 = t0 + 3 * N;
    for (int b = 0; b < 2; b++) {
        write_device(kVirtual56, t1 + b * N, in, N);
        write_device(kMain, same_moment(kVirtual56, kMain, t1 + b * N), main, N);
    }
    read_device(kMix56, same_moment(kVirtual56, kMix56, t1), out, N);  // ramp
    read_device(kMix56, same_moment(kVirtual56, kMix56, t1 + N), out, N);
    for (int i = 0; i < N; i++) {
        CHECK_NEAR(out[2 * i], 0, 1e-6);
        CHECK_NEAR(out[2 * i + 1], 0.125, 1e-6);
    }

    // Start the meters afresh: everything above faded out long ago.
    CFRelease(get_data(kPropertyMeters));
    gLastMeterQuery = 0;

    // Output gain of Mix 5/6 right: -6 dB.
    UInt8 output_gain[8 + 6];
    write_i32(output_gain, kMagic);
    write_i32(output_gain + 4, kOutputGains);
    output_gain[8] = 2, output_gain[9] = 1;
    write_i32(output_gain + 10, kUnity / 2);
    CHECK(send_gains(output_gain, sizeof output_gain) == 0);
    Float64 t2 = t1 + 2 * N;
    for (int b = 0; b < 2; b++) write_device(kMain, same_moment(kVirtual56, kMain, t2 + b * N), main, N);
    read_device(kMix56, same_moment(kVirtual56, kMix56, t2), out, N);
    read_device(kMix56, same_moment(kVirtual56, kMix56, t2 + N), out, N);
    CHECK_NEAR(out[1], 0.0625, 1e-6);

    // Nothing written for a while: silence, not the old ring contents.
    read_device(kMix56, same_moment(kVirtual56, kMix56, t2 + 40 * N), out, N);
    sum = 0;
    for (int i = 0; i < N * 2; i++) sum += fabsf(out[i]);
    CHECK(sum == 0);

    // Meters hold the peak since the last query: Main left at 0.25, Mix 5/6
    // right at 0.125 as its output gain started to go down.
    gLastMeterQuery = 0;
    CFDataRef meters = get_data(kPropertyMeters);
    CHECK(CFDataGetLength(meters) == 8 + (kSources + kOutputs) * 8);
    const UInt8 *m = CFDataGetBytePtr(meters);
    CHECK(read_i32(m) == kMagic && read_i32(m + 4) == 301);
    int16_t main_left = (int16_t)(m[8 + 6] | m[8 + 7] << 8);
    CHECK_NEAR(main_left / 100.0, 20 * log10(0.25), 0.02);
    const UInt8 *mix_right = m + 8 + (kSources + 5) * 8;
    CHECK(mix_right[4] == 2 && mix_right[5] == 1 && read_i32(mix_right) == 1);
    CHECK_NEAR((int16_t)(mix_right[6] | mix_right[7] << 8) / 100.0, 20 * log10(0.125), 0.02);
    CFRelease(meters);

    // Malformed properties are refused.
    UInt8 bad[8] = {0x4E, 0, 0, 0, 200, 0, 0, 0};
    CHECK(send_gains(bad, sizeof bad) != 0);

    stop(kMain);
    stop(kMix34);
    stop(kMix56);
    stop(kVirtual56);
    CFDataRef running = get_data(kPropertyRunning);
    CHECK(read_i32(CFDataGetBytePtr(running)) == 0);
    CFRelease(running);
}

static void test_volume_and_bridge(void) {
    float in[N * 2], out[N * 2];
    AudioObjectID volume = object_id(kVirtual78, kVolumeControl);
    AudioObjectPropertyAddress scalar = address(kAudioLevelControlPropertyScalarValue, kAudioObjectPropertyScopeGlobal);
    Float32 half = 0.5f;
    CHECK(gInterface.SetPropertyData(gDriver, volume, 0, &scalar, 0, NULL, sizeof half, &half) == 0);
    Float32 db = 0;
    CHECK(get(volume, kAudioLevelControlPropertyDecibelValue, kAudioObjectPropertyScopeGlobal, sizeof db, &db, NULL) == 0);
    CHECK_NEAR(db, 40 * log10(0.5), 1e-4);

    // Virtual 7/8 goes to the Monitor mix, which the bridge reads; the
    // bridge's own output (the hardware inputs) goes nowhere by default.
    UInt8 property[8 + 2 * 8];
    write_i32(property, kMagic);
    write_i32(property + 4, kCrosspoints);
    put_entry(property + 8, 2, 2, 1, 0, kUnity);
    put_entry(property + 16, 1, 0, 1, 1, kUnity);
    CHECK(send_gains(property, sizeof property) == 0);
    start(kVirtual78);
    start(kBridge);
    CFDataRef running = get_data(kPropertyRunning);
    CHECK(read_i32(CFDataGetBytePtr(running)) == 1);  // the bridge does not count
    CFRelease(running);
    for (int i = 0; i < N; i++) in[2 * i] = 0.8f, in[2 * i + 1] = 0.1f;
    Float64 t = 100000;
    for (int b = 0; b < 2; b++) {
        write_device(kVirtual78, t + b * N, in, N);
        // Hardware input 1 on the bridge's output.
        write_device(kBridge, same_moment(kVirtual78, kBridge, t + b * N), in, N);
    }
    read_device(kBridge, same_moment(kVirtual78, kBridge, t), out, N);
    read_device(kBridge, same_moment(kVirtual78, kBridge, t + N), out, N);
    // Left: Virtual 7/8 left at volume 0.5 (gain 0.25). Right: input 1.
    CHECK_NEAR(out[0], 0.8 * 0.25, 1e-6);
    CHECK_NEAR(out[1], 0.8, 1e-6);

    AudioObjectPropertyAddress mute = address(kAudioBooleanControlPropertyValue, kAudioObjectPropertyScopeGlobal);
    UInt32 on = 1;
    CHECK(gInterface.SetPropertyData(gDriver, object_id(kVirtual78, kMuteControl), 0, &mute, 0, NULL, sizeof on, &on) == 0);
    write_device(kVirtual78, t + 2 * N, in, N);
    write_device(kBridge, same_moment(kVirtual78, kBridge, t + 2 * N), in, N);
    read_device(kBridge, same_moment(kVirtual78, kBridge, t + 2 * N), out, N);
    CHECK_NEAR(out[0], 0, 1e-6);
    stop(kBridge);
    stop(kVirtual78);
}

static void test_storage_and_rate(void) {
    // Everything set so far survives a restart of the audio server.
    save_state();
    int32_t before = atomic_load(&gCrosspoint[0][8]);
    CHECK(before == kUnity);
    factory_gains();
    store_float(&gDevice[kVirtual78].volume_bits, 1);
    atomic_store(&gDevice[kVirtual78].muted, 0);
    load_state();
    CHECK(atomic_load(&gCrosspoint[0][8]) == kUnity);
    CHECK(atomic_load(&gCrosspoint[5][0]) == kUnity / 2);
    CHECK_NEAR(load_float(&gDevice[kVirtual78].volume_bits), 0.5, 1e-6);
    CHECK(atomic_load(&gDevice[kVirtual78].muted) == 1);

    // Changing the rate of one device changes all of them.
    AudioObjectPropertyAddress rate = address(kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal);
    Float64 r96 = 96000;
    CHECK(gInterface.SetPropertyData(gDriver, object_id(kMix34, kDeviceObject), 0, &rate, 0, NULL, sizeof r96, &r96) == 0);
    drain();
    CHECK(gConfigRequests == kDeviceCount);
    Float64 now = 0;
    CHECK(get(object_id(kMain, kDeviceObject), kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal,
              sizeof now, &now, NULL) == 0);
    CHECK(now == 96000);
    Float64 bad = 12345;
    CHECK(gInterface.SetPropertyData(gDriver, object_id(kMix34, kDeviceObject), 0, &rate, 0, NULL, sizeof bad, &bad) != 0);
}

static void test_timestamps(void) {
    start(kMain);
    Float64 sample = -1;
    UInt64 host = 0, seed = 0;
    CHECK(gInterface.GetZeroTimeStamp(gDriver, object_id(kMain, kDeviceObject), 1, &sample, &host, &seed) == 0);
    CHECK(sample == 0 && seed == 1);
    CHECK(fabs((Float64)host - gDevice[kMain].start_host) < 2);
    stop(kMain);
}

int main(void) {
    gStorage = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    void *driver = VibeFuseMixer_Create(NULL, kAudioServerPlugInTypeUUID);
    CHECK(driver == gDriver);
    CHECK(VibeFuseMixer_Create(NULL, kAudioServerPlugInDriverInterfaceUUID) == NULL);
    AudioServerPlugInHostRef host = &gTestHost;
    CHECK(gInterface.Initialize(gDriver, host) == 0);
    test_objects();
    test_mixing();
    test_volume_and_bridge();
    test_storage_and_rate();
    test_timestamps();
    drain();
    if (failures) {
        fprintf(stderr, "%d check(s) failed\n", failures);
        return 1;
    }
    printf("mixer driver: all checks passed\n");
    return 0;
}
