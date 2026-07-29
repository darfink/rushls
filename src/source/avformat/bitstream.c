#include <errno.h>
#include <stdint.h>

#include <libavcodec/bsf.h>
#include <libavcodec/codec_par.h>
#include <libavcodec/packet.h>
#include <libavutil/error.h>
#include <libavutil/rational.h>

void *rushls_aac_adtstoasc_alloc(const AVCodecParameters *parameters,
                                 AVRational time_base, int *error) {
  const AVBitStreamFilter *filter = av_bsf_get_by_name("aac_adtstoasc");
  AVBSFContext *context = NULL;
  int result;

  if (!parameters || !error) {
    return NULL;
  }
  if (!filter) {
    *error = AVERROR(ENOENT);
    return NULL;
  }
  result = av_bsf_alloc(filter, &context);
  if (result < 0) {
    *error = result;
    return NULL;
  }
  result = avcodec_parameters_copy(context->par_in, parameters);
  if (result >= 0) {
    context->time_base_in = time_base;
    result = av_bsf_init(context);
  }
  if (result < 0) {
    av_bsf_free(&context);
    *error = result;
    return NULL;
  }
  *error = 0;
  return context;
}

int rushls_bitstream_send(void *opaque, AVPacket *packet) {
  if (!opaque) {
    return AVERROR(EINVAL);
  }
  return av_bsf_send_packet(opaque, packet);
}

int rushls_bitstream_receive(void *opaque, AVPacket *packet) {
  if (!opaque || !packet) {
    return AVERROR(EINVAL);
  }
  return av_bsf_receive_packet(opaque, packet);
}

void rushls_bitstream_free(void **opaque) {
  AVBSFContext *context;

  if (!opaque) {
    return;
  }
  context = *opaque;
  av_bsf_free(&context);
  *opaque = NULL;
}
