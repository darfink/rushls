# FLAC fixtures

These files contain synthetic sine waves generated with FFmpeg. No external recordings are included.
Each file contains 0.71 seconds of audio, 1,024-sample frames, and a short final frame.

```sh
ffmpeg -v error -f lavfi -i 'sine=frequency=997:sample_rate=44100:duration=0.71' -ac 1 -c:a flac -frame_size 1024 -sample_fmt s16 -y mono-44100.flac
ffmpeg -v error -f lavfi -i 'sine=frequency=613:sample_rate=48000:duration=0.71' -ac 2 -c:a flac -frame_size 1024 -sample_fmt s32 -y stereo-48000.flac
```

The stereo encoder stores 24-bit FLAC samples from its 32-bit input representation.
