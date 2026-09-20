# Fixed-cadence GAP fixture

The h264_fixed_aac.ts fixture contains eight seconds of generated test video and a sine tone.
It has progressive H.264 at 25 fps, no B-frames, two-second IDR spacing, and mono AAC-LC at 48 kHz.
The explicit fixed-cadence declaration ensures packet loss reaches video GAP detection.

Generate with FFmpeg and libx264:

    ffmpeg -v error -f lavfi -i testsrc2=size=160x90:rate=25:duration=8 -f lavfi -i sine=frequency=440:sample_rate=48000:duration=8 -c:v libx264 -preset ultrafast -crf 32 -g 50 -bf 0 -x264-params force-cfr=1:scenecut=0 -c:a aac -b:a 64k -f mpegts h264_fixed_aac.ts

Tests remove packets after demuxing, before normalization.
All matched controls use the same encoded bytes.
