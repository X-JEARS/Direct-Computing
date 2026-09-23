#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <x264.h>

typedef struct dc_x264_encoder {
    x264_t *encoder;
    uint8_t *output;
    size_t output_capacity;
    int force_idr;
} dc_x264_encoder;

static int dc_x264_option(x264_param_t *params, const char *name, const char *value,
                          char *error, size_t error_len) {
    int result = x264_param_parse(params, name, value);
    if (result != 0 && error && error_len) {
        snprintf(error, error_len, "libx264 rejected option %s=%s (code %d)", name, value, result);
    }
    return result;
}

dc_x264_encoder *dc_x264_encoder_new(uint32_t width, uint32_t height, uint32_t bitrate_bps,
                                     uint32_t fps, uint32_t max_slice_len,
                                     char *error, size_t error_len) {
    x264_param_t params;
    if (x264_param_default_preset(&params, "veryfast", "zerolatency,fastdecode") != 0) {
        snprintf(error, error_len, "libx264 rejected the real-time preset");
        return NULL;
    }
    params.i_csp = X264_CSP_BGRA;
    params.i_width = (int)width;
    params.i_height = (int)height;

    uint32_t bitrate_kbps = (bitrate_bps + 999u) / 1000u;
    if (bitrate_kbps == 0) bitrate_kbps = 1;
    uint32_t vbv_buffer_kbits = (bitrate_kbps + 1u) / 2u;
    uint32_t keyint = fps > UINT32_MAX / 5u ? UINT32_MAX : fps * 5u;
    char fps_value[32], timebase_value[32], bitrate_value[32], vbv_value[32];
    char slice_value[32], keyint_value[32];
    snprintf(fps_value, sizeof(fps_value), "%u/1", fps);
    snprintf(timebase_value, sizeof(timebase_value), "1/%u", fps);
    snprintf(bitrate_value, sizeof(bitrate_value), "%u", bitrate_kbps);
    snprintf(vbv_value, sizeof(vbv_value), "%u", vbv_buffer_kbits);
    snprintf(slice_value, sizeof(slice_value), "%u", max_slice_len);
    snprintf(keyint_value, sizeof(keyint_value), "%u", keyint);

#define DC_OPT(name, value) do { if (dc_x264_option(&params, name, value, error, error_len) != 0) return NULL; } while (0)
    DC_OPT("fps", fps_value);
    DC_OPT("timebase", timebase_value);
    DC_OPT("bitrate", bitrate_value);
    DC_OPT("vbv-maxrate", bitrate_value);
    DC_OPT("vbv-bufsize", vbv_value);
    DC_OPT("slice-max-size", slice_value);
    DC_OPT("keyint", keyint_value);
    DC_OPT("min-keyint", "1");
    DC_OPT("scenecut", "0");
    DC_OPT("bframes", "0");
    DC_OPT("rc-lookahead", "0");
    DC_OPT("sync-lookahead", "0");
    DC_OPT("sliced-threads", "1");
    DC_OPT("repeat-headers", "1");
    DC_OPT("annexb", "1");
    DC_OPT("ref", "1");
#undef DC_OPT
    if (x264_param_apply_profile(&params, "high") != 0) {
        snprintf(error, error_len, "libx264 rejected high profile");
        return NULL;
    }

    dc_x264_encoder *context = (dc_x264_encoder *)calloc(1, sizeof(*context));
    if (!context) {
        snprintf(error, error_len, "allocating x264 context failed");
        return NULL;
    }
    context->encoder = x264_encoder_open(&params);
    if (!context->encoder) {
        snprintf(error, error_len, "opening x264 encoder failed");
        free(context);
        return NULL;
    }
    return context;
}

void dc_x264_encoder_force_idr(dc_x264_encoder *context) {
    if (context) context->force_idr = 1;
}

int dc_x264_encoder_encode(dc_x264_encoder *context, const uint8_t *bgra, int stride,
                           int64_t pts, const uint8_t **output, size_t *output_len,
                           int *keyframe, char *error, size_t error_len) {
    if (!context || !context->encoder || !bgra || !output || !output_len || !keyframe) return -1;
    x264_picture_t input, encoded_picture;
    x264_picture_init(&input);
    input.i_pts = pts;
    input.i_type = context->force_idr ? X264_TYPE_IDR : X264_TYPE_AUTO;
    context->force_idr = 0;
    input.img.i_csp = X264_CSP_BGRA;
    input.img.i_plane = 1;
    input.img.i_stride[0] = stride;
    input.img.plane[0] = (uint8_t *)bgra;

    x264_nal_t *nals = NULL;
    int nal_count = 0;
    int encoded_len = x264_encoder_encode(context->encoder, &nals, &nal_count, &input,
                                          &encoded_picture);
    if (encoded_len < 0) {
        snprintf(error, error_len, "libx264 failed to encode a frame");
        return -1;
    }
    if (encoded_len == 0 || nal_count == 0) {
        *output = NULL;
        *output_len = 0;
        *keyframe = 0;
        return 0;
    }
    if (context->output_capacity < (size_t)encoded_len) {
        uint8_t *replacement = (uint8_t *)realloc(context->output, (size_t)encoded_len);
        if (!replacement) {
            snprintf(error, error_len, "allocating x264 output buffer failed");
            return -1;
        }
        context->output = replacement;
        context->output_capacity = (size_t)encoded_len;
    }
    size_t offset = 0;
    for (int index = 0; index < nal_count; ++index) {
        if (!nals[index].p_payload || nals[index].i_payload <= 0) continue;
        memcpy(context->output + offset, nals[index].p_payload, (size_t)nals[index].i_payload);
        offset += (size_t)nals[index].i_payload;
    }
    *output = context->output;
    *output_len = offset;
    *keyframe = encoded_picture.b_keyframe != 0;
    return 0;
}

void dc_x264_encoder_close(dc_x264_encoder *context) {
    if (!context) return;
    if (context->encoder) x264_encoder_close(context->encoder);
    free(context->output);
    free(context);
}
