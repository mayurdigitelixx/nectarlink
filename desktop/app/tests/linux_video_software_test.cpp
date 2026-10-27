// SPDX-License-Identifier: GPL-3.0-or-later
#include "../cpp/linux_video_codec.cpp"
#include <cassert>
#include <cstring>

int main()
{
    const uint8_t keyframe[] = {
        0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xc0, 0x0a, 0xd9, 0x04, 0x26, 0xc0, 0x44, 0x00, 0x00, 0x03, 0x00,
        0x04, 0x00, 0x00, 0x03, 0x00, 0xc8, 0x3c, 0x48, 0x99, 0x20, 0x00, 0x00, 0x00, 0x01, 0x68, 0xcb, 0x83,
        0xcb, 0x20, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x0a, 0xf1, 0x18, 0xa0, 0x00, 0x27, 0xa7, 0x1c, 0x00,
        0x04, 0xbe, 0x23, 0x80, 0x00, 0x82, 0xec, 0x9c, 0x9c, 0x9d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75,
        0xd7, 0x5e,
    };
    Decoder decoder;
    const AVCodec* codec = decoder.functions.find_decoder("h264");
    assert(codec);
    decoder.initialize_software(codec);
    decoder.functions.check(decoder.functions.new_packet(decoder.packet, sizeof(keyframe)), "Allocate packet");
    std::memcpy(decoder.packet->data, keyframe, sizeof(keyframe));
    decoder.functions.check(decoder.functions.send_packet(decoder.context, decoder.packet), "Send packet");
    decoder.functions.check(decoder.functions.receive_frame(decoder.context, decoder.hardware), "Receive picture");
    const auto picture = decoded_picture(decoder, decoder.hardware);
    assert(picture.width == 64 && picture.height == 64);
    assert(picture.y && picture.uv && picture.stride_y >= 64 && picture.stride_uv >= 64);

    AVFrame* planar = decoder.functions.frame_alloc();
    assert(planar);
    uint8_t y[] = {1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0};
    uint8_t u[] = {11, 12, 0};
    uint8_t v[] = {21, 22, 0};
    planar->width = 5;
    planar->height = 3;
    planar->format = AV_PIX_FMT_YUV420P;
    planar->data[0] = y;
    planar->data[1] = u;
    planar->data[2] = v;
    planar->linesize[0] = 6;
    planar->linesize[1] = 3;
    planar->linesize[2] = 3;
    const auto converted = decoded_picture(decoder, planar);
    assert(converted.width == 4 && converted.height == 2);
    assert(converted.stride_y == 6 && converted.stride_uv == 4);
    assert(converted.uv[0] == 11 && converted.uv[1] == 21);
    assert(converted.uv[2] == 12 && converted.uv[3] == 22);
    uint8_t nv12[] = {11, 21, 12, 22, 0, 0};
    planar->format = AV_PIX_FMT_NV12;
    planar->data[1] = nv12;
    planar->data[2] = nullptr;
    planar->linesize[1] = 6;
    const auto packed = decoded_picture(decoder, planar);
    assert(packed.width == 4 && packed.height == 2 && packed.stride_uv == 6);
    assert(packed.uv == nv12);
    decoder.functions.frame_free(&planar);
}
