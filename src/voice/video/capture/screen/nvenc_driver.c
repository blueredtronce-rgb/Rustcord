/* Windows ABI shim for the NVENC driver. The capture and pipeline live in Rust.
 * nvEncodeAPI.h is NVIDIA's MIT-licensed Video Codec SDK 12.1 interface header.
 */
#define WIN32_LEAN_AND_MEAN
#include <stdint.h>
#include <stdio.h>
#include "vendor/nvEncodeAPI.h"

typedef struct {
    HMODULE module;
    NV_ENCODE_API_FUNCTION_LIST api;
    void *encoder;
    NV_ENC_REGISTERED_PTR input;
    NV_ENC_OUTPUT_PTR bits;
    NV_ENC_INPUT_PTR mapped;
    uint32_t width, height;
    int locked;
} RcNvenc;

typedef struct {
    const unsigned char *data;
    uint32_t length;
    uint32_t keyframe;
} RcNvencPacket;

static int success(NVENCSTATUS status, const char *step, char *error, uint32_t capacity) {
    if (status == NV_ENC_SUCCESS) return 1;
    snprintf(error, capacity, "%s (NVENC status %d)", step, (int)status);
    return 0;
}

int rc_nvenc_release(RcNvenc *encoder, char *error, uint32_t capacity) {
    int ok = 1;
    if (encoder->locked) {
        ok = success(encoder->api.nvEncUnlockBitstream(encoder->encoder, encoder->bits),
                     "unlock bitstream", error, capacity);
        encoder->locked = 0;
    }
    if (encoder->mapped) {
        NVENCSTATUS status = encoder->api.nvEncUnmapInputResource(encoder->encoder, encoder->mapped);
        if (ok) ok = success(status, "unmap texture", error, capacity);
        encoder->mapped = NULL;
    }
    return ok;
}

void rc_nvenc_close(RcNvenc *encoder) {
    if (!encoder) return;
    char error[96];
    if (encoder->encoder) {
        rc_nvenc_release(encoder, error, sizeof(error));
        if (encoder->bits) encoder->api.nvEncDestroyBitstreamBuffer(encoder->encoder, encoder->bits);
        if (encoder->input) encoder->api.nvEncUnregisterResource(encoder->encoder, encoder->input);
        encoder->api.nvEncDestroyEncoder(encoder->encoder);
    }
    if (encoder->module) FreeLibrary(encoder->module);
    free(encoder);
}

RcNvenc *rc_nvenc_open(void *device, void *texture, uint32_t width, uint32_t height, uint32_t fps,
                      uint32_t bitrate, char *error, uint32_t capacity) {
    RcNvenc *out = calloc(1, sizeof(*out));
    if (!out) {
        snprintf(error, capacity, "out of memory");
        return NULL;
    }
    out->width = width;
    out->height = height;
    out->module = LoadLibraryExW(L"nvEncodeAPI64.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!out->module) {
        snprintf(error, capacity, "NVIDIA driver unavailable (Windows error %lu)", GetLastError());
        goto fail;
    }
    typedef NVENCSTATUS (NVENCAPI *CreateApi)(NV_ENCODE_API_FUNCTION_LIST *);
    typedef NVENCSTATUS (NVENCAPI *GetVersion)(uint32_t *);
    CreateApi create = (CreateApi)GetProcAddress(out->module, "NvEncodeAPICreateInstance");
    GetVersion max_version = (GetVersion)GetProcAddress(out->module, "NvEncodeAPIGetMaxSupportedVersion");
    uint32_t version = 0;
    if (!create || !max_version || max_version(&version) != NV_ENC_SUCCESS ||
        version < ((NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION)) {
        snprintf(error, capacity, "NVIDIA driver does not support NVENC API 12.1");
        goto fail;
    }
    out->api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    if (!success(create(&out->api), "load function table", error, capacity)) goto fail;
    if (!out->api.nvEncOpenEncodeSessionEx || !out->api.nvEncDestroyEncoder ||
        !out->api.nvEncGetEncodePresetConfigEx || !out->api.nvEncInitializeEncoder ||
        !out->api.nvEncRegisterResource || !out->api.nvEncUnregisterResource ||
        !out->api.nvEncCreateBitstreamBuffer || !out->api.nvEncDestroyBitstreamBuffer ||
        !out->api.nvEncMapInputResource || !out->api.nvEncUnmapInputResource ||
        !out->api.nvEncEncodePicture || !out->api.nvEncLockBitstream || !out->api.nvEncUnlockBitstream) {
        snprintf(error, capacity, "NVIDIA driver returned an incomplete function table");
        goto fail;
    }
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS session = {0};
    session.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    session.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
    session.device = device;
    session.apiVersion = NVENCAPI_VERSION;
    if (!success(out->api.nvEncOpenEncodeSessionEx(&session, &out->encoder), "open DX11 session", error, capacity)) goto fail;

    NV_ENC_PRESET_CONFIG preset = {0};
    preset.version = NV_ENC_PRESET_CONFIG_VER;
    preset.presetCfg.version = NV_ENC_CONFIG_VER;
    if (!success(out->api.nvEncGetEncodePresetConfigEx(out->encoder, NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P3_GUID,
            NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, &preset), "read H.264 preset", error, capacity)) goto fail;
    NV_ENC_CONFIG *config = &preset.presetCfg;
    config->profileGUID = NV_ENC_H264_PROFILE_BASELINE_GUID;
    config->gopLength = fps * 5;
    config->frameIntervalP = 1;
    config->rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    config->rcParams.averageBitRate = bitrate;
    config->rcParams.maxBitRate = bitrate;
    config->rcParams.vbvBufferSize = bitrate * 2 / fps;
    config->rcParams.vbvInitialDelay = config->rcParams.vbvBufferSize;
    config->rcParams.enableLookahead = 0;
    config->rcParams.zeroReorderDelay = 1;
    config->encodeCodecConfig.h264Config.idrPeriod = config->gopLength;
    config->encodeCodecConfig.h264Config.repeatSPSPPS = 1;

    NV_ENC_INITIALIZE_PARAMS init = {0};
    init.version = NV_ENC_INITIALIZE_PARAMS_VER;
    init.encodeGUID = NV_ENC_CODEC_H264_GUID;
    init.presetGUID = NV_ENC_PRESET_P3_GUID;
    init.encodeWidth = width;
    init.encodeHeight = height;
    init.darWidth = width;
    init.darHeight = height;
    init.frameRateNum = fps;
    init.frameRateDen = 1;
    init.enablePTD = 1;
    init.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    init.encodeConfig = config;
    if (!success(out->api.nvEncInitializeEncoder(out->encoder, &init), "initialize H.264", error, capacity)) goto fail;

    NV_ENC_REGISTER_RESOURCE resource = {0};
    resource.version = NV_ENC_REGISTER_RESOURCE_VER;
    resource.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
    resource.width = width;
    resource.height = height;
    resource.resourceToRegister = texture;
    resource.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
    resource.bufferUsage = NV_ENC_INPUT_IMAGE;
    if (!success(out->api.nvEncRegisterResource(out->encoder, &resource), "register NV12 texture", error, capacity)) goto fail;
    out->input = resource.registeredResource;

    NV_ENC_CREATE_BITSTREAM_BUFFER buffer = {0};
    buffer.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    if (!success(out->api.nvEncCreateBitstreamBuffer(out->encoder, &buffer), "create bitstream", error, capacity)) goto fail;
    out->bits = buffer.bitstreamBuffer;
    return out;
fail:
    rc_nvenc_close(out);
    return NULL;
}

int rc_nvenc_encode(RcNvenc *encoder, uint64_t timestamp, int keyframe, RcNvencPacket *packet,
                    char *error, uint32_t capacity) {
    NV_ENC_MAP_INPUT_RESOURCE resource = {0};
    resource.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    resource.registeredResource = encoder->input;
    if (!success(encoder->api.nvEncMapInputResource(encoder->encoder, &resource), "map texture", error, capacity)) return 0;
    encoder->mapped = resource.mappedResource;

    NV_ENC_PIC_PARAMS picture = {0};
    picture.version = NV_ENC_PIC_PARAMS_VER;
    picture.inputWidth = encoder->width;
    picture.inputHeight = encoder->height;
    picture.inputTimeStamp = timestamp;
    picture.inputBuffer = encoder->mapped;
    picture.outputBitstream = encoder->bits;
    picture.bufferFmt = resource.mappedBufferFmt;
    picture.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    if (keyframe) picture.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
    if (!success(encoder->api.nvEncEncodePicture(encoder->encoder, &picture), "encode picture", error, capacity)) goto fail;

    NV_ENC_LOCK_BITSTREAM bits = {0};
    bits.version = NV_ENC_LOCK_BITSTREAM_VER;
    bits.outputBitstream = encoder->bits;
    if (!success(encoder->api.nvEncLockBitstream(encoder->encoder, &bits), "read bitstream", error, capacity)) goto fail;
    encoder->locked = 1;
    packet->data = bits.bitstreamBufferPtr;
    packet->length = bits.bitstreamSizeInBytes;
    packet->keyframe = bits.pictureType == NV_ENC_PIC_TYPE_IDR;
    return 1;
fail: {
    char cleanup[96];
    rc_nvenc_release(encoder, cleanup, sizeof(cleanup));
    return 0;
}
}
