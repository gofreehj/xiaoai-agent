/*
 * ax5_kws_server.c — Sherpa-ONNX KWS UDP server for Redmi AX5 (armv7l).
 *
 * Receives 16kHz mono S16_LE PCM frames from speaker over UDP (port 5005),
 * runs streaming Zipformer KWS inference via libsherpa-onnx-c-api.so,
 * and emits UDP wake packets (WAKE:<seq>:<keyword>\n) back to port 5006.
 */

#define _POSIX_C_SOURCE 200809L

#include <arpa/inet.h>
#include <dlfcn.h>
#include <errno.h>
#include <math.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#include "sherpa-onnx/c-api/c-api.h"

/* Dynamically loaded Sherpa-ONNX C API symbols (avoids cross-linker PLT/ABI issues on armhf) */
typedef const SherpaOnnxKeywordSpotter *(*fn_CreateKeywordSpotter)(
    const SherpaOnnxKeywordSpotterConfig *);
typedef void (*fn_DestroyKeywordSpotter)(const SherpaOnnxKeywordSpotter *);
typedef const SherpaOnnxOnlineStream *(*fn_CreateKeywordStream)(
    const SherpaOnnxKeywordSpotter *);
typedef void (*fn_DestroyOnlineStream)(const SherpaOnnxOnlineStream *);
typedef void (*fn_AcceptWaveform)(const SherpaOnnxOnlineStream *, int32_t,
                                  const float *, int32_t);
typedef int32_t (*fn_IsKeywordStreamReady)(const SherpaOnnxKeywordSpotter *,
                                           const SherpaOnnxOnlineStream *);
typedef void (*fn_DecodeKeywordStream)(const SherpaOnnxKeywordSpotter *,
                                       const SherpaOnnxOnlineStream *);
typedef const SherpaOnnxKeywordResult *(*fn_GetKeywordResult)(
    const SherpaOnnxKeywordSpotter *, const SherpaOnnxOnlineStream *);
typedef void (*fn_DestroyKeywordResult)(const SherpaOnnxKeywordResult *);
typedef void (*fn_ResetKeywordStream)(const SherpaOnnxKeywordSpotter *,
                                      const SherpaOnnxOnlineStream *);

static struct {
  void *handle;
  fn_CreateKeywordSpotter CreateKeywordSpotter;
  fn_DestroyKeywordSpotter DestroyKeywordSpotter;
  fn_CreateKeywordStream CreateKeywordStream;
  fn_DestroyOnlineStream DestroyOnlineStream;
  fn_AcceptWaveform AcceptWaveform;
  fn_IsKeywordStreamReady IsKeywordStreamReady;
  fn_DecodeKeywordStream DecodeKeywordStream;
  fn_GetKeywordResult GetKeywordResult;
  fn_DestroyKeywordResult DestroyKeywordResult;
  fn_ResetKeywordStream ResetKeywordStream;
} g_api;

static int load_sherpa_api(const char *so_path) {
  g_api.handle = dlopen(so_path, RTLD_NOW | RTLD_LOCAL);
  if (!g_api.handle) {
    fprintf(stderr, "dlopen(%s) failed: %s\n", so_path, dlerror());
    return -1;
  }
#define LOAD_SYM(field, sym)                                              \
  do {                                                                    \
    *(void **)(&g_api.field) = dlsym(g_api.handle, sym);                  \
    if (!g_api.field) {                                                   \
      fprintf(stderr, "dlsym(%s) failed: %s\n", sym, dlerror());          \
      return -1;                                                          \
    }                                                                     \
  } while (0)

  LOAD_SYM(CreateKeywordSpotter, "SherpaOnnxCreateKeywordSpotter");
  LOAD_SYM(DestroyKeywordSpotter, "SherpaOnnxDestroyKeywordSpotter");
  LOAD_SYM(CreateKeywordStream, "SherpaOnnxCreateKeywordStream");
  LOAD_SYM(DestroyOnlineStream, "SherpaOnnxDestroyOnlineStream");
  LOAD_SYM(AcceptWaveform, "SherpaOnnxOnlineStreamAcceptWaveform");
  LOAD_SYM(IsKeywordStreamReady, "SherpaOnnxIsKeywordStreamReady");
  LOAD_SYM(DecodeKeywordStream, "SherpaOnnxDecodeKeywordStream");
  LOAD_SYM(GetKeywordResult, "SherpaOnnxGetKeywordResult");
  LOAD_SYM(DestroyKeywordResult, "SherpaOnnxDestroyKeywordResult");
  LOAD_SYM(ResetKeywordStream, "SherpaOnnxResetKeywordStream");
#undef LOAD_SYM
  return 0;
}

#define SAMPLE_RATE 16000
#define MAX_UDP_PACKET 4096
#define MAX_PCM_SAMPLES 2048
#define PREROLL_SAMPLES (SAMPLE_RATE * 400 / 1000) /* 400ms pre-roll */

/* Optional 8-byte XPCM header: "XPCM" + u16_le seq + u16_le flags */
#define XPCM_HDR_LEN 8
#define XPCM_FLAG_PCM 1
#define XPCM_FLAG_RESET 2
#define XPCM_FLAG_PING 3
#define XPCM_FLAG_SYS_ACTIVE 0x0100

static volatile sig_atomic_t g_running = 1;

static void on_signal(int sig) {
  (void)sig;
  g_running = 0;
}

static uint64_t monotonic_ms(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (uint64_t)ts.tv_sec * 1000ULL + (uint64_t)(ts.tv_nsec / 1000000L);
}

static void usage(const char *prog) {
  fprintf(stderr,
          "Usage: %s [options]\n"
          "  --encoder PATH             Path to encoder onnx\n"
          "  --decoder PATH             Path to decoder onnx\n"
          "  --joiner PATH              Path to joiner onnx\n"
          "  --tokens PATH              Path to tokens.txt\n"
          "  --keywords-file PATH       Path to keywords.txt\n"
          "  --sherpa-lib PATH          Path to libsherpa-onnx-c-api.so (default: libsherpa-onnx-c-api.so)\n"
          "  --num-threads N            ONNX threads (default: 2)\n"
          "  --keywords-score F         Keywords bonus score (default: 1.0)\n"
          "  --keywords-threshold F     Detection threshold (default: 0.25)\n"
          "  --listen-port PORT         UDP listen port for PCM (default: 5005)\n"
          "  --wake-port PORT           Optional fixed UDP target port (default: 0 = reply to sender port)\n"
          "  --speaker-ip IP            Optional fixed speaker IP (default: auto-learn)\n"
          "  --cooldown-ms MS           Cooldown between wakes (default: 2000)\n"
          "  --vad-rms N                Energy gate RMS in S16 units, 0=off (default: 100)\n"
          "  --hangover-ms MS           Active hangover after energy drops (default: 2500)\n"
          "  --stats-sec N              Log RMS/decode stats every N sec, 0=off (default: 0)\n",
          prog);
}

static int parse_flag_str(int *i, int argc, char **argv, const char *name,
                          const char **out) {
  size_t len = strlen(name);
  if (strcmp(argv[*i], name) == 0 && *i + 1 < argc) {
    *out = argv[++(*i)];
    return 1;
  }
  if (strncmp(argv[*i], name, len) == 0 && argv[*i][len] == '=') {
    *out = argv[*i] + len + 1;
    return 1;
  }
  return 0;
}

static void feed_and_decode(const SherpaOnnxKeywordSpotter *kws,
                            const SherpaOnnxOnlineStream *stream,
                            const float *samples, int32_t n_samples,
                            int sock, const struct sockaddr_in *peer_addr,
                            int has_peer, uint16_t wake_port,
                            int cooldown_ms, uint64_t *last_wake_ms,
                            uint32_t *wake_seq, int in_sys_window,
                            char *retry_msg, size_t *retry_len,
                            struct sockaddr_in *retry_addr,
                            uint64_t *retry_at_ms) {
  if (n_samples <= 0) return;

  g_api.AcceptWaveform(stream, SAMPLE_RATE, samples, n_samples);

  while (g_api.IsKeywordStreamReady(kws, stream)) {
    g_api.DecodeKeywordStream(kws, stream);
    const SherpaOnnxKeywordResult *r = g_api.GetKeywordResult(kws, stream);
    if (r) {
      if (r->keyword && r->keyword[0] != '\0') {
        if (in_sys_window && strcmp(r->keyword, "__CMD_EXIT__") == 0) {
          fprintf(stdout,
                  "[KWS] Suppressed system-echo trigger '%s' (sys=1)\n",
                  r->keyword);
          fflush(stdout);
          g_api.ResetKeywordStream(kws, stream);
          g_api.DestroyKeywordResult(r);
          continue;
        }
        uint64_t now = monotonic_ms();
        if (now - *last_wake_ms >= (uint64_t)cooldown_ms) {
          *last_wake_ms = now;
          (*wake_seq)++;
          fprintf(stdout,
                  "[KWS] TRIGGERED keyword='%s' sys=%d seq=%u json=%s\n",
                  r->keyword, in_sys_window, *wake_seq,
                  r->json ? r->json : "{}");
          fflush(stdout);

          if (has_peer) {
            struct sockaddr_in dst = *peer_addr;
            if (wake_port > 0) {
              dst.sin_port = htons(wake_port);
            }
            char msg[256];
            int mlen = snprintf(msg, sizeof(msg), "WAKE:%u:%s\n",
                                *wake_seq, r->keyword);
            if (mlen > 0 && (size_t)mlen < sizeof(msg)) {
              sendto(sock, msg, (size_t)mlen, 0,
                     (const struct sockaddr *)&dst, sizeof(dst));
              /* Schedule one non-blocking duplicate 20ms later for Wi-Fi reliability */
              memcpy(retry_msg, msg, (size_t)mlen);
              *retry_len = (size_t)mlen;
              *retry_addr = dst;
              *retry_at_ms = now + 20;
            }
          }
        } else {
          fprintf(stdout, "[KWS] Suppressed duplicate trigger '%s' (cooldown)\n",
                  r->keyword);
          fflush(stdout);
        }
        g_api.ResetKeywordStream(kws, stream);
      }
      g_api.DestroyKeywordResult(r);
    }
  }
}

int main(int argc, char **argv) {
  const char *encoder = NULL;
  const char *decoder = NULL;
  const char *joiner = NULL;
  const char *tokens = NULL;
  const char *keywords_file = NULL;
  const char *sherpa_lib = "libsherpa-onnx-c-api.so";
  const char *speaker_ip = NULL;
  const char *val = NULL;

  int num_threads = 2;
  int max_active_paths = 12;
  float keywords_score = 1.0f;
  float keywords_threshold = 0.25f;
  int listen_port = 5005;
  int wake_port = 0; /* 0 = reply directly to sender's UDP source port */
  int cooldown_ms = 2000;
  int vad_rms = 100;
  int hangover_ms = 2500;
  int stats_sec = 0;

  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "-h") == 0 || strcmp(argv[i], "--help") == 0) {
      usage(argv[0]);
      return 0;
    } else if (parse_flag_str(&i, argc, argv, "--encoder", &encoder)) {
    } else if (parse_flag_str(&i, argc, argv, "--decoder", &decoder)) {
    } else if (parse_flag_str(&i, argc, argv, "--joiner", &joiner)) {
    } else if (parse_flag_str(&i, argc, argv, "--tokens", &tokens)) {
    } else if (parse_flag_str(&i, argc, argv, "--keywords-file", &keywords_file)) {
    } else if (parse_flag_str(&i, argc, argv, "--sherpa-lib", &sherpa_lib)) {
    } else if (parse_flag_str(&i, argc, argv, "--speaker-ip", &speaker_ip)) {
    } else if (parse_flag_str(&i, argc, argv, "--num-threads", &val)) {
      num_threads = atoi(val);
    } else if (parse_flag_str(&i, argc, argv, "--max-active-paths", &val)) {
      max_active_paths = atoi(val);
    } else if (parse_flag_str(&i, argc, argv, "--keywords-score", &val)) {
      keywords_score = strtof(val, NULL);
    } else if (parse_flag_str(&i, argc, argv, "--keywords-threshold", &val)) {
      keywords_threshold = strtof(val, NULL);
    } else if (parse_flag_str(&i, argc, argv, "--listen-port", &val)) {
      listen_port = atoi(val);
    } else if (parse_flag_str(&i, argc, argv, "--wake-port", &val)) {
      wake_port = atoi(val);
    } else if (parse_flag_str(&i, argc, argv, "--cooldown-ms", &val)) {
      cooldown_ms = atoi(val);
    } else if (parse_flag_str(&i, argc, argv, "--vad-rms", &val)) {
      vad_rms = atoi(val);
    } else if (parse_flag_str(&i, argc, argv, "--hangover-ms", &val)) {
      hangover_ms = atoi(val);
    } else if (parse_flag_str(&i, argc, argv, "--stats-sec", &val)) {
      stats_sec = atoi(val);
    } else {
      fprintf(stderr, "Unknown argument: %s\n", argv[i]);
      usage(argv[0]);
      return 1;
    }
  }

  if (!encoder || !decoder || !joiner || !tokens || !keywords_file) {
    fprintf(stderr, "Missing required model/keywords arguments.\n");
    usage(argv[0]);
    return 1;
  }

  signal(SIGINT, on_signal);
  signal(SIGTERM, on_signal);

  fprintf(stdout,
          "========================================================\n"
          "  ax5-kws-server: Sherpa-ONNX 3.3M Zipformer KWS\n"
          "  Listen UDP: %d | Wake UDP: %d | Threads: %d\n"
          "  Threshold: %.2f | Score: %.2f | VAD RMS: %d\n"
          "========================================================\n",
          listen_port, wake_port, num_threads, keywords_threshold,
          keywords_score, vad_rms);
  fflush(stdout);

  SherpaOnnxKeywordSpotterConfig config;
  memset(&config, 0, sizeof(config));
  config.feat_config.sample_rate = SAMPLE_RATE;
  config.feat_config.feature_dim = 80;
  config.model_config.transducer.encoder = encoder;
  config.model_config.transducer.decoder = decoder;
  config.model_config.transducer.joiner = joiner;
  config.model_config.tokens = tokens;
  config.model_config.num_threads = num_threads;
  config.model_config.provider = "cpu";
  config.model_config.modeling_unit = "cjkchar";
  config.max_active_paths = max_active_paths > 0 ? max_active_paths : 12;
  config.num_trailing_blanks = 0;
  config.keywords_score = keywords_score;
  config.keywords_threshold = keywords_threshold;
  config.keywords_file = keywords_file;

  if (load_sherpa_api(sherpa_lib) != 0) {
    return 1;
  }

  uint64_t t0 = monotonic_ms();
  const SherpaOnnxKeywordSpotter *kws = g_api.CreateKeywordSpotter(&config);
  if (!kws) {
    fprintf(stderr, "Failed to create SherpaOnnxKeywordSpotter!\n");
    return 1;
  }
  const SherpaOnnxOnlineStream *stream = g_api.CreateKeywordStream(kws);
  if (!stream) {
    fprintf(stderr, "Failed to create SherpaOnnxOnlineStream!\n");
    g_api.DestroyKeywordSpotter(kws);
    return 1;
  }
  fprintf(stdout, "[KWS] Model loaded in %llu ms, ready!\n",
          (unsigned long long)(monotonic_ms() - t0));
  fflush(stdout);

  int sock = socket(AF_INET, SOCK_DGRAM, 0);
  if (sock < 0) {
    perror("socket");
    g_api.DestroyOnlineStream(stream);
    g_api.DestroyKeywordSpotter(kws);
    return 1;
  }

  int reuse = 1;
  setsockopt(sock, SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof(reuse));
  int rcvbuf = 256 * 1024;
  setsockopt(sock, SOL_SOCKET, SO_RCVBUF, &rcvbuf, sizeof(rcvbuf));

  struct timeval tv;
  tv.tv_sec = 0;
  tv.tv_usec = 50000; /* 50ms timeout for clean signal/retry handling */
  setsockopt(sock, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));

  struct sockaddr_in bind_addr;
  memset(&bind_addr, 0, sizeof(bind_addr));
  bind_addr.sin_family = AF_INET;
  bind_addr.sin_addr.s_addr = htonl(INADDR_ANY);
  bind_addr.sin_port = htons((uint16_t)listen_port);

  if (bind(sock, (struct sockaddr *)&bind_addr, sizeof(bind_addr)) < 0) {
    perror("bind");
    close(sock);
    g_api.DestroyOnlineStream(stream);
    g_api.DestroyKeywordSpotter(kws);
    return 1;
  }

  struct sockaddr_in peer_addr;
  memset(&peer_addr, 0, sizeof(peer_addr));
  int has_peer = 0;
  if (speaker_ip && speaker_ip[0] != '\0') {
    peer_addr.sin_family = AF_INET;
    if (inet_pton(AF_INET, speaker_ip, &peer_addr.sin_addr) == 1) {
      has_peer = 1;
      fprintf(stdout, "[KWS] Fixed speaker target: %s:%d\n", speaker_ip, wake_port);
      fflush(stdout);
    }
  }

  float preroll[PREROLL_SAMPLES];
  int preroll_pos = 0;
  int preroll_filled = 0;

  int active = (vad_rms <= 0) ? 1 : 0;
  int hangover_samples = 0;
  uint64_t last_wake_ms = 0;
  uint64_t last_sys_active_ms = 0;
  uint32_t wake_seq = 0;

  char retry_msg[256];
  size_t retry_len = 0;
  struct sockaddr_in retry_addr;
  uint64_t retry_at_ms = 0;

  uint8_t pkt[MAX_UDP_PACKET];
  float fbuf[MAX_PCM_SAMPLES];

  uint64_t stats_start_ms = monotonic_ms();
  uint64_t stats_pkts = 0;
  uint64_t stats_active_pkts = 0;
  double stats_rms_sum = 0.0;
  double stats_rms_min = 1e9;
  double stats_rms_max = 0.0;

  while (g_running) {
    if (stats_sec > 0) {
      uint64_t now = monotonic_ms();
      if (now - stats_start_ms >= (uint64_t)stats_sec * 1000ULL) {
        double avg = stats_pkts ? (stats_rms_sum / (double)stats_pkts) : 0.0;
        double pct = stats_pkts ? (100.0 * (double)stats_active_pkts / (double)stats_pkts) : 0.0;
        fprintf(stdout,
                "[KWS] stats (%ds): pkts=%llu active=%.1f%% rms(min=%.0f avg=%.0f max=%.0f)\n",
                stats_sec, (unsigned long long)stats_pkts, pct,
                stats_pkts ? stats_rms_min : 0.0, avg, stats_rms_max);
        fflush(stdout);
        stats_start_ms = now;
        stats_pkts = 0;
        stats_active_pkts = 0;
        stats_rms_sum = 0.0;
        stats_rms_min = 1e9;
        stats_rms_max = 0.0;
      }
    }
    if (retry_len > 0 && monotonic_ms() >= retry_at_ms) {
      sendto(sock, retry_msg, retry_len, 0,
             (const struct sockaddr *)&retry_addr, sizeof(retry_addr));
      retry_len = 0;
    }

    struct sockaddr_in src_addr;
    socklen_t src_len = sizeof(src_addr);
    ssize_t n = recvfrom(sock, pkt, sizeof(pkt), 0,
                         (struct sockaddr *)&src_addr, &src_len);
    if (n < 0) {
      if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR) {
        continue;
      }
      perror("recvfrom");
      break;
    }

    if (!speaker_ip) {
      if (!has_peer || peer_addr.sin_addr.s_addr != src_addr.sin_addr.s_addr) {
        char ip_str[INET_ADDRSTRLEN];
        inet_ntop(AF_INET, &src_addr.sin_addr, ip_str, sizeof(ip_str));
        fprintf(stdout, "[KWS] Learned speaker IP: %s\n", ip_str);
        fflush(stdout);
      }
      peer_addr = src_addr;
      has_peer = 1;
    }

    const uint8_t *pcm_ptr = pkt;
    size_t pcm_bytes = (size_t)n;
    uint16_t flags = XPCM_FLAG_PCM;

    if (n >= XPCM_HDR_LEN && memcmp(pkt, "XPCM", 4) == 0) {
      flags = (uint16_t)(pkt[6] | (pkt[7] << 8));
      pcm_ptr = pkt + XPCM_HDR_LEN;
      pcm_bytes = (size_t)n - XPCM_HDR_LEN;
    }

    uint16_t base_flags = flags & 0x00FF;
    int sys_active = (flags & XPCM_FLAG_SYS_ACTIVE) != 0;
    if (sys_active) {
      last_sys_active_ms = monotonic_ms();
    }
    int in_sys_window =
        (sys_active ||
         (last_sys_active_ms > 0 &&
          (monotonic_ms() - last_sys_active_ms) <= 800))
            ? 1
            : 0;

    if (base_flags == XPCM_FLAG_PING) {
      struct sockaddr_in dst = src_addr;
      if (wake_port > 0) {
        dst.sin_port = htons((uint16_t)wake_port);
      }
      const char pong[] = "XPONG\n";
      sendto(sock, pong, sizeof(pong) - 1, 0,
             (const struct sockaddr *)&dst, sizeof(dst));
      continue;
    }

    if (base_flags == XPCM_FLAG_RESET) {
      g_api.ResetKeywordStream(kws, stream);
      preroll_pos = 0;
      preroll_filled = 0;
      hangover_samples = 0;
      active = (vad_rms <= 0) ? 1 : 0;
      continue;
    }

    if (base_flags != XPCM_FLAG_PCM || pcm_bytes == 0 || (pcm_bytes % 2) != 0) {
      continue;
    }

    int n_samples = (int)(pcm_bytes / 2);
    if (n_samples > MAX_PCM_SAMPLES) {
      n_samples = MAX_PCM_SAMPLES;
    }

    double sum_sq = 0.0;
    for (int i = 0; i < n_samples; i++) {
      int16_t s = (int16_t)(pcm_ptr[i * 2] | (pcm_ptr[i * 2 + 1] << 8));
      sum_sq += (double)s * (double)s;
      fbuf[i] = (float)s / 32768.0f;
    }
    double rms = sqrt(sum_sq / (double)n_samples);

    stats_pkts++;
    stats_rms_sum += rms;
    if (rms < stats_rms_min) stats_rms_min = rms;
    if (rms > stats_rms_max) stats_rms_max = rms;

    if (vad_rms <= 0) {
      stats_active_pkts++;
      feed_and_decode(kws, stream, fbuf, n_samples, sock, &peer_addr, has_peer,
                      (uint16_t)wake_port, cooldown_ms, &last_wake_ms,
                      &wake_seq, in_sys_window, retry_msg, &retry_len,
                      &retry_addr, &retry_at_ms);
      continue;
    }

    if (rms >= (double)vad_rms) {
      if (!active) {
        active = 1;
        /* Flush pre-roll ring buffer so onset syllable is never clipped */
        if (preroll_filled > 0) {
          if (preroll_filled < PREROLL_SAMPLES) {
            feed_and_decode(kws, stream, preroll, preroll_filled, sock,
                            &peer_addr, has_peer, (uint16_t)wake_port,
                            cooldown_ms, &last_wake_ms, &wake_seq, in_sys_window,
                            retry_msg, &retry_len, &retry_addr, &retry_at_ms);
          } else {
            int first_len = PREROLL_SAMPLES - preroll_pos;
            if (first_len > 0) {
              feed_and_decode(kws, stream, preroll + preroll_pos, first_len,
                              sock, &peer_addr, has_peer, (uint16_t)wake_port,
                              cooldown_ms, &last_wake_ms, &wake_seq,
                              in_sys_window, retry_msg, &retry_len,
                              &retry_addr, &retry_at_ms);
            }
            if (preroll_pos > 0) {
              feed_and_decode(kws, stream, preroll, preroll_pos, sock,
                              &peer_addr, has_peer, (uint16_t)wake_port,
                              cooldown_ms, &last_wake_ms, &wake_seq,
                              in_sys_window, retry_msg, &retry_len,
                              &retry_addr, &retry_at_ms);
            }
          }
        }
      }
      hangover_samples = (SAMPLE_RATE / 1000) * hangover_ms;
    }

    if (active) {
      stats_active_pkts++;
      feed_and_decode(kws, stream, fbuf, n_samples, sock, &peer_addr, has_peer,
                      (uint16_t)wake_port, cooldown_ms, &last_wake_ms,
                      &wake_seq, in_sys_window, retry_msg, &retry_len,
                      &retry_addr, &retry_at_ms);
      hangover_samples -= n_samples;
      if (hangover_samples <= 0) {
        active = 0;
        hangover_samples = 0;
        g_api.ResetKeywordStream(kws, stream);
      }
    }

    /* Always keep pre-roll updated with latest samples */
    for (int i = 0; i < n_samples; i++) {
      preroll[preroll_pos] = fbuf[i];
      preroll_pos = (preroll_pos + 1) % PREROLL_SAMPLES;
      if (preroll_filled < PREROLL_SAMPLES) {
        preroll_filled++;
      }
    }
  }

  fprintf(stdout, "[KWS] Shutting down...\n");
  close(sock);
  g_api.DestroyOnlineStream(stream);
  g_api.DestroyKeywordSpotter(kws);
  if (g_api.handle) dlclose(g_api.handle);
  return 0;
}
