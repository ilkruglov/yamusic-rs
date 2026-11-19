// Обработчик ASIO не принимает контекст. Поток плеера владеет драйвером
// и останавливает его перед освобождением буферов и микшера.
#define NOMINMAX
#include <windows.h>
#include <objbase.h>
#include <iasiodrv.h>
#include <algorithm>
#include <atomic>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <new>
#include <memory>

using Render = void (*)(void*, float*, size_t);
struct Output {
    IASIO* driver = nullptr;
    HWND window = nullptr;
    bool com = false, buffers_created = false, started = false, output_ready = false;
    long frames = 0, channels = 0;
    double sample_rate = 0;
    ASIOBufferInfo buffers[2]{};
    ASIOSampleType formats[2]{};
    std::unique_ptr<float[]> samples;
    std::atomic_flag rendering = ATOMIC_FLAG_INIT;
    std::atomic<Render> render{nullptr};
    void* context = nullptr;
    std::atomic<bool> reset{false};
    std::atomic<unsigned> callbacks{0};
};
static std::atomic<Output*> current{nullptr};

static int width(long type) {
    if (type < 0 || type > ASIOSTInt32LSB24) return 0;
    switch (type & 15) {
    case 0: return 2;
    case 1: return 3;
    case 2: case 3: case 8: case 9: case 10: case 11: return 4;
    case 4: return 8;
    default: return 0;
    }
}

// Драйвер получает отдельный буфер на каждый канал микшера.
extern "C" void ya_asio_channel(long type, const float* src, long stride, unsigned char* dst, long frames) {
    const int bytes = width(type);
    const int format = type & 15;
    const bool little = (type & 16) != 0;
    for (long i = 0; i < frames; ++i, dst += bytes) {
        const float sample = src[i * stride];
        uint64_t bits = 0;
        if (format == 3) {
            memcpy(&bits, &sample, 4);
        } else if (format == 4) {
            const double wide = sample;
            memcpy(&bits, &wide, 8);
        } else {
            constexpr int aligned_bits[] = {16, 18, 20, 24};
            const int precision = format >= 8 ? aligned_bits[format - 8] : bytes * 8;
            const double scale = double(uint64_t(1) << (precision - 1));
            const double normalized = std::isfinite(sample) ? sample : 0.0;
            bits = static_cast<uint32_t>(static_cast<int32_t>(std::clamp(normalized * scale, -scale, scale - 1)));
        }
        if (little) memcpy(dst, &bits, bytes);
        else for (int j = 0; j < bytes; ++j) dst[bytes - 1 - j] = static_cast<unsigned char>(bits >> (8 * j));
    }
}

static void buffer_switch(long index, ASIOBool) {
    auto* s = current.load();
    if (!s || (index != 0 && index != 1)) return;
    const auto render = s->render.load(std::memory_order_acquire);
    if (!render || s->rendering.test_and_set(std::memory_order_acquire)) return;
    render(s->context, s->samples.get(), size_t(s->frames) * s->channels);
    for (long ch = 0; ch < s->channels; ++ch)
        ya_asio_channel(s->formats[ch], s->samples.get() + ch, s->channels,
                        static_cast<unsigned char*>(s->buffers[ch].buffers[index]), s->frames);
    s->callbacks.fetch_add(1, std::memory_order_relaxed);
    if (s->output_ready) s->driver->outputReady();
    s->rendering.clear(std::memory_order_release);
}
static ASIOTime* buffer_time(ASIOTime* time, long index, ASIOBool direct) {
    buffer_switch(index, direct);
    return time;
}
static void changed_rate(ASIOSampleRate rate) {
    if (auto* s = current.load(); s && rate != s->sample_rate) s->reset.store(true);
}
static long message(long selector, long value, void*, double*) {
    if (selector == kAsioSelectorSupported)
        return value == kAsioResetRequest || value == kAsioBufferSizeChange || value == kAsioEngineVersion;
    if (selector == kAsioEngineVersion) return 2;
    if (selector == kAsioResetRequest || selector == kAsioBufferSizeChange) {
        changed_rate(0);
        return 1;
    }
    return 0;
}
static ASIOCallbacks callbacks{buffer_switch, changed_rate, message, buffer_time};

extern "C" void ya_asio_close(Output* s) {
    if (!s) return;
    if (s->started) s->driver->stop();
    if (s->buffers_created) s->driver->disposeBuffers();
    Output* expected = s;
    current.compare_exchange_strong(expected, nullptr);
    if (s->driver) s->driver->Release();
    if (s->window) DestroyWindow(s->window);
    if (s->com) CoUninitialize();
    delete s;
}

extern "C" const char* ya_asio_open(const wchar_t* id, Output** out, unsigned* rate, unsigned short* channels) {
    std::unique_ptr<Output, decltype(&ya_asio_close)> state(new(std::nothrow) Output(), ya_asio_close);
    auto* s = state.get();
    if (!s) return "out of memory";
    Output* empty = nullptr;
    if (!current.compare_exchange_strong(empty, s)) return "another ASIO driver is active";
    const HRESULT hr = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
    if (FAILED(hr) && hr != RPC_E_CHANGED_MODE) return "CoInitializeEx failed";
    s->com = SUCCEEDED(hr);
    CLSID clsid{};
    if (FAILED(CLSIDFromString(id, &clsid)) ||
        FAILED(CoCreateInstance(clsid, nullptr, CLSCTX_INPROC_SERVER, clsid, reinterpret_cast<void**>(&s->driver))))
        return "CoCreateInstance failed";
    // Дескриптор окна нужен драйверу и при запуске плеера в трей.
    s->window = CreateWindowExW(0, L"STATIC", L"yamusic-rs ASIO", WS_POPUP, 0, 0, 0, 0, nullptr, nullptr, GetModuleHandleW(nullptr), nullptr);
    if (!s->window || !s->driver->init(s->window)) return "ASIO driver initialization failed";
    long inputs, outputs, min, max, granularity;
    double hz = 0;
    if (s->driver->getChannels(&inputs, &outputs) != ASE_OK || outputs < 1) return "no output channels";
    if (s->driver->getSampleRate(&hz) != ASE_OK || !(hz >= 8000 && hz <= 1536000)) return "invalid sample rate";
    if (s->driver->getBufferSize(&min, &max, &s->frames, &granularity) != ASE_OK ||
        s->frames < 1 || s->frames > 1048576) return "invalid buffer size";
    s->sample_rate = hz;
    s->channels = std::min(outputs, 2L);
    for (long i = 0; i < s->channels; ++i) {
        ASIOChannelInfo info{};
        info.channel = i;
        if (s->driver->getChannelInfo(&info) != ASE_OK || !width(info.type))
            return "unsupported ASIO channel format";
        s->formats[i] = info.type;
        s->buffers[i].channelNum = i;
    }
    s->samples.reset(new(std::nothrow) float[size_t(s->frames) * s->channels]);
    if (!s->samples) return "out of memory";
    if (s->driver->createBuffers(s->buffers, s->channels, s->frames, &callbacks) != ASE_OK) return "createBuffers failed";
    s->buffers_created = true;
    for (long i = 0; i < s->channels; ++i)
        for (auto* half : s->buffers[i].buffers) {
            if (!half) return "missing ASIO buffer";
            memset(half, 0, size_t(width(s->formats[i])) * s->frames);
        }
    s->output_ready = s->driver->outputReady() == ASE_OK;
    *rate = static_cast<unsigned>(hz);
    *channels = static_cast<unsigned short>(s->channels);
    *out = state.release();
    return nullptr;
}
extern "C" bool ya_asio_start(Output* s, void* context, Render render) {
    s->context = context;
    s->render.store(render, std::memory_order_release);
    s->started = s->driver->start() == ASE_OK;
    return s->started;
}
extern "C" unsigned ya_asio_progress(Output* s, bool* reset) {
    *reset = s->reset.load();
    return s->callbacks.load(std::memory_order_relaxed);
}
