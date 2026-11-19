#!/usr/bin/env bash
# writes the sample files of README.md beside this script: pink noise from ffmpeg, through each
# encoder's own program and decoded again by ffmpeg. needs ffmpeg, lame, opusenc (opus-tools),
# oggenc (vorbis-tools) and fdkaac, which `nix develop .#samples` brings
set -euo pipefail
cd "$(dirname "$0")"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

ff() { ffmpeg -hide_banner -loglevel error -y "$@"; }
# a decode as 16-bit FLAC, without the tags that would say where it came from
to_flac() { ff -i "$1" "${@:3}" -map_metadata -1 -fflags +bitexact -c:a flac -sample_fmt s16 "$2"; }

# 34 seconds of two partly correlated channels of pink noise that swells and falls
noise() {
  ff -f lavfi -i "anoisesrc=color=pink:seed=$2:r=$1:d=34:a=0.5" \
    -f lavfi -i "anoisesrc=color=pink:seed=$(($2 + 100)):r=$1:d=34:a=0.5" \
    -filter_complex "[0][1]join=inputs=2:channel_layout=stereo,pan=stereo|c0=c0+0.5*c1|c1=c0-0.5*c1,volume='0.3*(0.75+0.25*sin(1.3*t))':eval=frame" \
    -bitexact -c:a pcm_s16le "$3"
}
noise 44100 1 "$work/source.wav"
noise 48000 1 "$work/source-48.wav"
noise 44100 3 "$work/other.wav"

to_flac "$work/source.wav" lossless_16-44.flac

lame --quiet -b 128 "$work/source.wav" lame-128.mp3
lame --quiet -b 320 "$work/source.wav" "$work/lame-320.mp3"
to_flac lame-128.mp3 lame-128_16-44.flac
to_flac "$work/lame-320.mp3" lame-320_16-44.flac
to_flac lame-128.mp3 lame-128-then-48k_16-48.flac -ar 48000
# the first twelve seconds from an MP3, the rest lossless
lame --quiet -b 128 "$work/other.wav" "$work/other.mp3"
ff -i "$work/other.mp3" -i "$work/source.wav" -filter_complex \
  "[0]atrim=end=12[a];[1]atrim=start=12,asetpts=PTS-STARTPTS[b];[a][b]concat=n=2:v=0:a=1" \
  -c:a pcm_s16le "$work/intro.wav"
to_flac "$work/intro.wav" lame-128-intro_16-44.flac

ff -i "$work/source.wav" -c:a aac -b:a 128k ffmpeg-aac-128.m4a
to_flac ffmpeg-aac-128.m4a ffmpeg-aac-128_16-44.flac
fdkaac -S -b 128 -o "$work/fdkaac-128.m4a" "$work/source.wav"
to_flac "$work/fdkaac-128.m4a" fdkaac-128_16-44.flac

opusenc --quiet --bitrate 128 "$work/source-48.wav" opusenc-128.opus
to_flac opusenc-128.opus opusenc-128_16-48.flac
to_flac opusenc-128.opus opusenc-128-then-44k_16-44.flac -ar 44100

oggenc --quiet -q 6 -o "$work/oggenc-q6.ogg" "$work/source.wav"
# ffmpeg's Ogg reader stamps the last Vorbis packets out of order, so the samples are counted
to_flac "$work/oggenc-q6.ogg" oggenc-q6_16-44.flac -af asetpts=N/SR/TB

to_flac "$work/source.wav" cut-17k_16-44.flac -af "firequalizer=gain='if(lt(f,17000),0,-INF)'"
to_flac "$work/source.wav" dual-mono_16-44.flac -af "pan=stereo|c0=c0|c1=c0"
