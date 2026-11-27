/*
 * Minimal Sherpa-ONNX C API definitions required by deploy/kws-server/ax5/src/ax5_kws_server.c.
 *
 * Verified byte-for-byte against `libsherpa-onnx-c-api.so` v1.12.30 (armhf) deployed on Redmi AX5.
 * Keep the _Static_assert checks below intact so ABI drift fails at compile time.
 */
#ifndef SHERPA_ONNX_C_API_C_API_H_
#define SHERPA_ONNX_C_API_C_API_H_

#include <stddef.h>
#include <stdint.h>

typedef struct SherpaOnnxFeatureConfig {
  int32_t sample_rate;
  int32_t feature_dim;
} SherpaOnnxFeatureConfig;

typedef struct SherpaOnnxOnlineTransducerModelConfig {
  const char *encoder;
  const char *decoder;
  const char *joiner;
} SherpaOnnxOnlineTransducerModelConfig;

typedef struct SherpaOnnxOnlineParaformerModelConfig {
  const char *encoder;
  const char *decoder;
} SherpaOnnxOnlineParaformerModelConfig;

typedef struct SherpaOnnxOnlineZipformer2CtcModelConfig {
  const char *model;
} SherpaOnnxOnlineZipformer2CtcModelConfig;

typedef struct SherpaOnnxOnlineNemoCtcModelConfig {
  const char *model;
} SherpaOnnxOnlineNemoCtcModelConfig;

typedef struct SherpaOnnxOnlineToneCtcModelConfig {
  const char *model;
} SherpaOnnxOnlineToneCtcModelConfig;

typedef struct SherpaOnnxOnlineModelConfig {
  SherpaOnnxOnlineTransducerModelConfig transducer;
  SherpaOnnxOnlineParaformerModelConfig paraformer;
  SherpaOnnxOnlineZipformer2CtcModelConfig zipformer2_ctc;
  const char *tokens;
  int32_t num_threads;
  const char *provider;
  int32_t debug;
  const char *model_type;
  const char *modeling_unit;
  const char *bpe_vocab;
  const char *tokens_buf;
  int32_t tokens_buf_size;
  SherpaOnnxOnlineNemoCtcModelConfig nemo_ctc;
  SherpaOnnxOnlineToneCtcModelConfig t_one_ctc;
} SherpaOnnxOnlineModelConfig;

typedef struct SherpaOnnxKeywordSpotterConfig {
  SherpaOnnxFeatureConfig feat_config;
  SherpaOnnxOnlineModelConfig model_config;
  int32_t max_active_paths;
  int32_t num_trailing_blanks;
  float keywords_score;
  float keywords_threshold;
  const char *keywords_file;
  const char *keywords_buf;
  int32_t keywords_buf_size;
} SherpaOnnxKeywordSpotterConfig;

typedef struct SherpaOnnxKeywordSpotter SherpaOnnxKeywordSpotter;
typedef struct SherpaOnnxOnlineStream SherpaOnnxOnlineStream;

typedef struct SherpaOnnxKeywordResult {
  const char *keyword;
  const char *tokens;
  const char *const *tokens_arr;
  int32_t count;
  float *timestamps;
  float start_time;
  const char *json;
} SherpaOnnxKeywordResult;

_Static_assert(sizeof(SherpaOnnxKeywordSpotterConfig) == 104, "config size");
_Static_assert(offsetof(SherpaOnnxKeywordSpotterConfig, feat_config.sample_rate) == 0, "sample_rate");
_Static_assert(offsetof(SherpaOnnxKeywordSpotterConfig, feat_config.feature_dim) == 4, "feature_dim");
_Static_assert(offsetof(SherpaOnnxKeywordSpotterConfig, model_config.transducer.encoder) == 8, "encoder");
_Static_assert(offsetof(SherpaOnnxKeywordSpotterConfig, model_config.transducer.decoder) == 12, "decoder");
_Static_assert(offsetof(SherpaOnnxKeywordSpotterConfig, model_config.transducer.joiner) == 16, "joiner");
_Static_assert(offsetof(SherpaOnnxKeywordSpotterConfig, model_config.tokens) == 32, "tokens");
_Static_assert(offsetof(SherpaOnnxKeywordSpotterConfig, model_config.num_threads) == 36, "num_threads");
_Static_assert(offsetof(SherpaOnnxKeywordSpotterConfig, model_config.provider) == 40, "provider");
_Static_assert(offsetof(SherpaOnnxKeywordSpotterConfig, model_config.modeling_unit) == 52, "modeling_unit");
_Static_assert(offsetof(SherpaOnnxKeywordSpotterConfig, max_active_paths) == 76, "max_active_paths");
_Static_assert(offsetof(SherpaOnnxKeywordSpotterConfig, keywords_score) == 84, "keywords_score");
_Static_assert(offsetof(SherpaOnnxKeywordSpotterConfig, keywords_threshold) == 88, "keywords_threshold");
_Static_assert(offsetof(SherpaOnnxKeywordSpotterConfig, keywords_file) == 92, "keywords_file");
_Static_assert(offsetof(SherpaOnnxKeywordResult, keyword) == 0, "keyword");
_Static_assert(offsetof(SherpaOnnxKeywordResult, json) == 24, "json");

#endif
