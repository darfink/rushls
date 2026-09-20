#!/usr/bin/env python3
"""Generate independent FFmpeg HLS controls with one unavailable whole segment.

No timestamps or encoded packets are modified when applying EXT-X-GAP.
"""
import argparse
from pathlib import Path
import shutil
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rushls-variants', type=Path, help='Optional build-video-gap-variants output to combine with continuous audio')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    for kind in ('fmp4-video', 'fmp4-av', 'ts-video'):
        directory = args.output / (kind + '-control')
        directory.mkdir(exist_ok=True)
        command = ['ffmpeg', '-y', '-v', 'error', '-f', 'lavfi', '-i',
                   'testsrc2=size=320x180:rate=25:duration=12']
        if kind == 'fmp4-av':
            command += ['-f', 'lavfi', '-i',
                        'sine=frequency=440:sample_rate=48000:duration=12']
        command += ['-c:v', 'libx264', '-preset', 'medium', '-g', '50', '-bf', '0',
                    '-x264-params', 'scenecut=0:ref=1']
        command += ['-c:a', 'aac'] if kind == 'fmp4-av' else ['-an']
        command += ['-f', 'hls', '-hls_time', '2', '-hls_list_size', '0']
        extension = 'ts' if kind == 'ts-video' else 'm4s'
        if extension == 'm4s':
            command += ['-hls_segment_type', 'fmp4', '-hls_fmp4_init_filename', 'init.mp4']
        command += ['-hls_segment_filename', str(directory / ('seg%d.' + extension)),
                    str(directory / 'index.m3u8')]
        subprocess.run(command, check=True)
        original = (directory / 'index.m3u8').read_text()
        for variant in ('gap', 'gap-discontinuity', 'gap-vod'):
            target = args.output / (kind + '-' + variant)
            shutil.copytree(directory, target, dirs_exist_ok=True)
            lines = original.splitlines()
            index = lines.index('seg2.' + extension)
            lines.insert(index - 1, '#EXT-X-GAP')
            if variant == 'gap-discontinuity':
                index = lines.index('seg3.' + extension)
                lines.insert(index - 1, '#EXT-X-DISCONTINUITY')
            if variant == 'gap-vod':
                lines.insert(2, '#EXT-X-PLAYLIST-TYPE:VOD')
            (target / 'index.m3u8').write_text('\n'.join(lines) + '\n')
            (target / ('seg2.' + extension)).unlink()
    if args.rushls_variants:
        audio = args.output / 'continuous-audio'
        audio.mkdir(exist_ok=True)
        subprocess.run([
            'ffmpeg', '-y', '-v', 'error', '-f', 'lavfi', '-i',
            'sine=frequency=440:sample_rate=48000:duration=8', '-c:a', 'aac',
            '-f', 'hls', '-hls_time', '2', '-hls_list_size', '0',
            '-hls_segment_type', 'fmp4', '-hls_fmp4_init_filename', 'init.mp4',
            '-hls_segment_filename', str(audio / 'audio%d.m4s'),
            str(audio / 'index.m3u8')], check=True)
        for name in ('00-control', '01-exact-gap', '06-whole-parent-gap'):
            target = args.output / ('rushls-av-' + name)
            shutil.copytree(args.rushls_variants / name, target, dirs_exist_ok=True)
            shutil.copytree(audio, target / 'audio', dirs_exist_ok=True)
            master = (target / 'master-full.m3u8').read_text()
            master = master.replace('#EXT-X-STREAM-INF:',
                '#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="Audio",'
                'DEFAULT=YES,AUTOSELECT=YES,URI="audio/index.m3u8"\n'
                '#EXT-X-STREAM-INF:AUDIO="audio",')
            master = master.replace('CODECS="', 'CODECS="mp4a.40.2,')
            (target / 'master-full.m3u8').write_text(master)
    print(args.output)


if __name__ == '__main__':
    main()
