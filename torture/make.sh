#!/usr/bin/env bash
# writes the torture set of README.md beside this script, with manifest.tsv: generated audio
# through real encoders, muxers and byte edits at computed offsets. needs what
# `nix develop .#torture` brings. `make.sh long` adds the hours-long files of group D
set -euo pipefail
cd "$(dirname "$0")"
long=${1:-}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
manifest=manifest.tsv
: > "$manifest"

ff() { ffmpeg -hide_banner -loglevel error -nostdin -y "$@"; }
# a manifest line: the file, what it should read as, where that comes from, how it was made
note() { printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" >> "$manifest"; }
size() { stat -c %s "$1"; }
# writes the bytes of a printf format at an offset of a file
poke() { printf "$3" | dd of="$1" bs=1 seek="$2" conv=notrunc status=none; }
# flips the lowest bit of the byte at an offset of a file
flip() {
  local byte
  byte=$(od -An -tu1 -j "$2" -N 1 "$1")
  poke "$1" "$2" "$(printf '\\x%02x' $((byte ^ 1)))"
}
# the offset of the first match of a pattern of bytes at or after an offset of a file
find_at() { LC_ALL=C grep -obUaP "$2" "$1" | awk -F: -v from="${3:-0}" '!found && $1 >= from { print $1; found = 1 }'; }
# a number as little-endian or big-endian bytes, as a printf format
le32() { printf '\\x%02x\\x%02x\\x%02x\\x%02x' $(($1 & 255)) $(($1 >> 8 & 255)) $(($1 >> 16 & 255)) $(($1 >> 24 & 255)); }
be32() { printf '\\x%02x\\x%02x\\x%02x\\x%02x' $(($1 >> 24 & 255)) $(($1 >> 16 & 255)) $(($1 >> 8 & 255)) $(($1 & 255)); }

le64() { le32 $(($1 & 0xffffffff)); le32 $(($1 >> 32)); }
be64() { be32 $(($1 >> 32)); be32 $(($1 & 0xffffffff)); }

# two partly correlated channels of pink noise that swells and falls, as samples/make.sh makes,
# in float: seconds, rate, seed, file
music() {
  ff -f lavfi -i "anoisesrc=color=pink:seed=$3:r=$2:d=$1:a=0.5" \
    -f lavfi -i "anoisesrc=color=pink:seed=$(($3 + 100)):r=$2:d=$1:a=0.5" \
    -filter_complex "[0][1]join=inputs=2:channel_layout=stereo,pan=stereo|c0=c0+0.5*c1|c1=c0-0.5*c1,volume='0.3*(0.75+0.25*sin(1.3*t))':eval=frame" \
    -c:a pcm_f32le "$4"
}
music 34 44100 1 "$work/m44.wav"
music 34 48000 1 "$work/m48.wav"
music 34 96000 1 "$work/m96.wav"
music 34 22050 1 "$work/m22.wav"
music 34 32000 1 "$work/m32.wav"
music 34 44100 3 "$work/other.wav"

# a signal in float from a filter graph over one of the sources: name, source, graph
from() { ff -i "$work/$2.wav" -filter_complex "$3" -c:a pcm_f32le "$work/$1.wav"; }
# a signal in float from a graph with no input: name, graph
gen() { ff -filter_complex "$2" -c:a pcm_f32le "$work/$1.wav"; }
# a signal that went through 16 bits before a filter, as an edit of a finished file does:
# name, source, graph
after16() {
  ff -i "$work/$2.wav" -c:a pcm_s16le "$work/$1.16.wav"
  ff -i "$work/$1.16.wav" -filter_complex "$3" -c:a pcm_s16le "$work/$1.wav"
}
# both channels of an aeval through the same expression of `x`
both() { local e=$1; printf "aeval='%s|%s':c=same" "${e//x/val(0)}" "${e//x/val(1)}"; }

rate_of() { ffprobe -v error -show_entries stream=sample_rate -of csv=p=0 "$1"; }
khz() { case $1 in 44100) echo 44 ;; 22050) echo 22 ;; 88200) echo 88 ;; 176400) echo 176 ;; *) echo $(($1 / 1000)) ;; esac; }

# a WAV file as one lossless format: source, file. the format is the file's extension, the
# depth the `_<bits>-` of its name
lossless() {
  local bits fmt codec
  bits=$(sed -E 's/.*_([0-9]+|f32)-[0-9]+\..*/\1/' <<< "$2")
  case $bits in 16) fmt=s16 codec=pcm_s16le ;; 24) fmt=s32 codec=pcm_s24le ;; 8) fmt=u8 codec=pcm_u8 ;; f32) fmt=flt codec=pcm_f32le ;; esac
  case $2 in
    *.flac) ff -i "$1" -map_metadata -1 -fflags +bitexact -c:a flac -sample_fmt "$fmt" "$2" ;;
    *.wav) ff -i "$1" -map_metadata -1 -fflags +bitexact -c:a "$codec" "$2" ;;
    *.aiff) ff -i "$1" -map_metadata -1 -fflags +bitexact -c:a "${codec/le/be}" "$2" ;;
    *.caf) ff -i "$1" -map_metadata -1 -fflags +bitexact -c:a "${codec/le/be}" "$2" ;;
    *.m4a) ff -i "$1" -map_metadata -1 -fflags +bitexact -c:a alac -sample_fmt "${fmt}p" "$2" ;;
    *.wv)
      ff -i "$1" -c:a "$codec" "$work/in.wav"
      wavpack -q -y "$work/in.wav" -o "$2"
      ;;
    *.ape)
      ff -i "$1" -c:a "$codec" "$work/in.wav"
      mac "$work/in.wav" "$2" -c2000 > /dev/null 2>&1
      ;;
  esac
}

# group A: a signal as 16-bit FLAC and at 24 bits in one of the other lossless formats, taken
# in turn: name, expectation, source of the expectation, how it was made
turn=0
honest() {
  local name=$1 khz other
  khz=$(khz "$(rate_of "$work/$name.wav")")
  others=(wav m4a wv ape)
  other=${others[$((turn++ % 4))]}
  lossless "$work/$name.wav" "A-${name}_16-$khz.flac"
  lossless "$work/$name.wav" "A-${name}_24-$khz.$other"
  note "A-${name}_16-$khz.flac" "$2" "$3" "$4"
  note "A-${name}_24-$khz.$other" "$2" "$3" "$4"
}
never="README Verdicts: only a grid makes a likely transcode"
limits="README Limitations"

for n in 576 1152 1024 2048; do
  from "loop-$n" m44 "atrim=start_sample=44100:end_sample=$((44100 + n)),aloop=loop=-1:size=$n,atrim=end=34"
  honest "loop-$n" honest "$never" "$n samples of the noise, repeated"
done
for n in 120 240 480 960; do
  from "loop-$n" m48 "atrim=start_sample=48000:end_sample=$((48000 + n)),aloop=loop=-1:size=$n,atrim=end=34"
  honest "loop-$n" honest "$never" "$n samples of the noise at 48 kHz, repeated"
done
from loop-1152-bed m44 "asplit[a][b];[a]atrim=start_sample=44100:end_sample=45252,aloop=loop=-1:size=1152,atrim=end=34[l];[b]volume=0.2[q];[l][q]amix=inputs=2:normalize=0"
honest loop-1152-bed honest "$never" "a 1152-sample loop over the noise 14 dB down"
from stft-2048 m44 "afftfilt=real='re*(1-0.5*b/nb)':imag='im*(1-0.5*b/nb)':win_size=2048:overlap=0.5:win_func=sine"
honest stft-2048 honest "$never" "a spectral tilt in 2048-sample sine windows at half overlap (afftfilt)"
from denoised m44 "afftdn=nr=12:nf=-45"
honest denoised honest "$never" "a spectral denoiser (afftdn)"
from stretched m44 "atempo=1.07"
honest stretched honest "$never" "a 7% time stretch (atempo)"
from pitched m44 "asetrate=44100*1.06,aresample=44100,atempo=1/1.06"
honest pitched honest "$never" "a pitch shift of a semitone by resampling and a time stretch"
from gate-1152 m44 "$(both 'x*lt(mod(n\,1152)\,576)')"
honest gate-1152 honest "$never" "a gate open for 576 of every 1152 samples"
from click-1152 m44 "$(both 'x+0.4*eq(mod(n\,1152)\,0)')"
honest click-1152 honest "$never" "a click every 1152 samples over the noise"
from tremolo-1152 m44 "$(both 'x*(0.7+0.3*sin(2*PI*n/1152))')"
honest tremolo-1152 honest "$never" "a tremolo of one cycle every 1152 samples"

from steps-1.5db m44 "$(both 'x*pow(2\,(mod(floor(t*4)\,16)-15)/4)')"
honest steps-1.5db honest "$never" "the level stepped by a fourth root of two every quarter second"
partials=
for k in $(seq 0 24); do
  partials+="+0.15*pow(2\,-$k/4)*sin(2*PI*220*pow(2\,$k/4)*t)"
done
gen partials "aevalsrc='${partials#+}|${partials#+}':s=44100:d=34"
honest partials honest "$never, $limits: naturally band-limited audio" "25 partials a fourth root of two apart in frequency and level"
ff -i "$work/m44.wav" -c:a pcm_u8 "$work/u8.wav"
ff -i "$work/u8.wav" -c:a pcm_s16le "$work/sampler-8bit.wav"
honest sampler-8bit honest "$never" "8-bit samples stored at 16 and 24 bits"
after16 sampler-12bit m44 "$(both 'floor(x*2048)/2048')"
honest sampler-12bit honest "$never" "12-bit samples stored at 16 and 24 bits"
after16 bitcrushed m44 "acrusher=bits=6:mode=lin:aa=1:samples=4:mix=1"
honest bitcrushed honest "$never" "a bitcrusher at 6 bits and a quarter of the rate (acrusher)"
for law in mulaw alaw; do
  ff -i "$work/m44.wav" -c:a "pcm_$law" "$work/$law.law.wav"
  ff -i "$work/$law.law.wav" -c:a pcm_s16le "$work/$law.wav"
  honest "$law" honest "$never" "a $law decode stored at 16 and 24 bits"
done
ff -i "$work/m44.wav" -c:a adpcm_ima_wav "$work/adpcm.ima.wav"
ff -i "$work/adpcm.ima.wav" -c:a pcm_s16le "$work/adpcm.wav"
honest adpcm honest "$never" "an IMA ADPCM decode stored at 16 and 24 bits"
for gain in -6 -1 3; do
  after16 "gain$gain" m44 "volume=${gain}dB"
  honest "gain$gain" honest "$never" "the 16-bit file with a gain of $gain dB, at 16 bits again"
done

little="README Limitations: needs about 30 seconds of non-silent audio"
gen silence "anullsrc=r=44100:cl=stereo,atrim=end=34"
honest silence honest "$little" "digital silence"
gen silence-dither "aevalsrc='(random(0)-random(1))/32768|(random(2)-random(3))/32768':s=44100:d=34"
honest silence-dither honest "$little" "silence with triangular dither at the 16th bit"
gen click-in-silence "aevalsrc='0.8*eq(n\,441000)|0.8*eq(n\,441000)':s=44100:d=34"
honest click-in-silence honest "$little" "one click in digital silence"
from music-then-silence m44 "atrim=end=25,apad=pad_dur=300"
honest music-then-silence honest "$little" "25 s of the noise, then five minutes of silence"
band="$never, $limits: naturally band-limited audio"
for hz in 1000 15900 19000; do
  gen "sine-$hz" "sine=f=$hz:r=44100:d=34,volume=4,pan=stereo|c0=c0|c1=c0"
  honest "sine-$hz" honest "$band" "a sine at $hz Hz on both channels"
done
gen sweep "aevalsrc='0.5*sin(2*PI*(20*t+323*t*t))|0.5*sin(2*PI*(20*t+323*t*t))':s=44100:d=34"
honest sweep honest "$band" "a sine swept from 20 Hz to 22 kHz"
for colour in white pink brown; do
  ff -f lavfi -i "anoisesrc=color=$colour:seed=5:r=44100:d=34:a=0.3" \
    -f lavfi -i "anoisesrc=color=$colour:seed=6:r=44100:d=34:a=0.3" \
    -filter_complex "[0][1]join=inputs=2:channel_layout=stereo" -c:a pcm_f32le "$work/$colour.wav"
  honest "$colour" honest "$band" "$colour noise, the two channels apart"
done
gen square-clipped "aevalsrc='if(gt(sin(2*PI*220*t)\,0)\,1\,-1)|if(gt(sin(2*PI*220*t)\,0)\,1\,-1)':s=44100:d=34"
honest square-clipped honest "$never" "a square wave at full scale"
from dc m44 "$(both '0.3*x+0.6')"
honest dc honest "$never" "the noise on a DC offset of 0.6 of full scale"
from right-silent m44 "pan=stereo|c0=c0|c1=0*c0"
honest right-silent honest "$never" "the right channel digital silence"
from inverted m44 "pan=stereo|c0=c0|c1=-1*c0"
honest inverted honest "$never" "the right channel the left inverted"
from dual-mono m44 "pan=stereo|c0=c0|c1=c0"
honest dual-mono honest "$never, samples/README: dual-mono reads consistent, flagged" "one channel copied to both"
from mono m44 "pan=mono|c0=c0"
honest mono honest "$never" "one channel"
from brickwall-20k m44 "firequalizer=gain='if(lt(f,20000),0,-INF)'"
honest brickwall-20k honest "$limits: a cutoff near 20 kHz reads as inconclusive" "a brickwall filter at 20 kHz"
from rolloff-12k m44 "lowpass=f=12000:poles=1"
honest rolloff-12k honest "$band" "a 6 dB an octave roll-off from 12 kHz"
from up-22k m22 "aresample=44100"
honest up-22k honest "$never, README Features: flags upsampled audio" "a 22.05 kHz source brought to 44.1 kHz"
from up-32k m32 "aresample=44100"
honest up-32k honest "$never, README Features: flags upsampled audio" "a 32 kHz source brought to 44.1 kHz"
from up-22k-to-96 m22 "aresample=96000"
honest up-22k-to-96 honest "$never, README Features: flags upsampled audio" "a 22.05 kHz source brought to 96 kHz"
from up-32k-to-96 m32 "aresample=96000"
honest up-32k-to-96 honest "$never, README Features: flags upsampled audio" "a 32 kHz source brought to 96 kHz"
from tape m44 "lowpass=f=5000:poles=2,$(both 'x+0.0005*(random(ch)*2-1)')"
honest tape honest "$band" "the noise rolled off from 5 kHz under hiss 66 dB down"
ff -i "$work/m96.wav" -f lavfi -i "anoisesrc=color=white:seed=8:r=96000:d=34:a=0.02" \
  -f lavfi -i "anoisesrc=color=white:seed=9:r=96000:d=34:a=0.02" \
  -filter_complex "[0]firequalizer=gain='if(lt(f,21000),0,-INF)'[a];[1][2]join=inputs=2:channel_layout=stereo,highpass=f=30000:poles=2[n];[a][n]amix=inputs=2:normalize=0" \
  -c:a pcm_f32le "$work/hires-shaped.wav"
honest hires-shaped honest "$never" "96 kHz: the noise up to 21 kHz and shaped noise rising above 30 kHz, as a DSD conversion leaves"
for seconds in 29 30 31; do
  from "length-$seconds" m44 "atrim=end=$seconds"
  honest "length-$seconds" honest "$never, $little" "$seconds s of the noise"
done

# float samples a plugin left behind, as WAV: frames 100000 on get the bytes of the value
from float-over m44 "volume=6"
cp "$work/float-over.wav" A-float-over_f32-44.wav
note A-float-over_f32-44.wav honest "$never" "float WAV with samples up to 2.7"
lossless "$work/float-over.wav" A-float-over_f32-44.wv
note A-float-over_f32-44.wv honest "$never" "float WavPack with samples up to 2.7"
data=$(($(find_at "$work/m44.wav" 'data') + 8))
declare -A floats=([nan]='\x00\x00\xc0\x7f' [inf]='\x00\x00\x80\x7f' [denormal]='\x01\x00\x00\x00')
for kind in nan inf denormal; do
  cp "$work/m44.wav" "A-float-${kind}_f32-44.wav"
  for frame in $(seq 100000 100063); do
    poke "A-float-${kind}_f32-44.wav" $((data + frame * 8)) "${floats[$kind]}${floats[$kind]}"
  done
  note "A-float-${kind}_f32-44.wav" honest "$never" "float WAV with 64 frames of $kind in it"
done

# group B: decodes of lossy files, after what people do to them
to16() { ff -i "$1" "${@:3}" -c:a pcm_s16le "$work/$2.wav"; }
ff -i "$work/m44.wav" -c:a pcm_s16le "$work/s44.wav"
ff -i "$work/m48.wav" -c:a pcm_s16le "$work/s48.wav"
ff -i "$work/other.wav" -c:a pcm_s16le "$work/o44.wav"
lame --quiet -b 128 "$work/s44.wav" "$work/lame128.mp3"
lame --quiet -V 0 "$work/s44.wav" "$work/lameV0.mp3"
lame --quiet -b 320 "$work/s44.wav" "$work/lame320.mp3"
lame --quiet -b 128 "$work/o44.wav" "$work/other128.mp3"
ff -i "$work/s44.wav" -c:a aac -b:a 128k "$work/ffaac128.m4a"
fdkaac -S -b 128 -o "$work/fdk128.m4a" "$work/s44.wav"
opusenc --quiet --bitrate 128 "$work/s48.wav" "$work/opus128.opus"
oggenc --quiet -q 6 -o "$work/vorbis6.ogg" "$work/s44.wav"
to16 "$work/lame128.mp3" lame128
to16 "$work/lameV0.mp3" lameV0
to16 "$work/lame320.mp3" lame320
to16 "$work/other128.mp3" other128
to16 "$work/ffaac128.m4a" ffaac128
to16 "$work/fdk128.m4a" fdk128
to16 "$work/opus128.opus" opus128
to16 "$work/vorbis6.ogg" vorbis6 -af asetpts=N/SR/TB

table="samples/README"
enc="README Encoder family"
# a decode as 16-bit FLAC: name, expectation, source, how
decode() {
  local khz
  khz=$(khz "$(rate_of "$work/$1.wav")")
  lossless "$work/$1.wav" "B-${1}_16-$khz.flac"
  note "B-${1}_16-$khz.flac" "$2" "$3" "$4"
}
decode lame128 "lossy:MP3:LAME" "$table" "lame -b 128, decoded"
decode lameV0 "lossy:MP3:LAME" "README Features: tuned on V0 to V9" "lame -V 0, decoded"
decode lame320 "lossy:MP3:LAME" "$table" "lame -b 320, decoded"
decode ffaac128 "lossy:AAC:ffmpeg or fdk-aac" "$table" "ffmpeg's AAC at 128 kbps, decoded"
decode fdk128 "lossy:AAC:ffmpeg or fdk-aac" "$table" "fdkaac -b 128, decoded"
decode opus128 "lossy:Opus:libopus" "$table" "opusenc --bitrate 128, decoded"
decode vorbis6 "lossy:AAC" "$table: read as an AAC grid" "oggenc -q 6, decoded"

# one treatment of a decode: base, treatment's name, expectation, source, how, then the graph
treated() {
  local base=$1 name="$1-then-$2"
  ff -i "$work/$base.wav" -filter_complex "$6" -c:a pcm_s16le "$work/$name.wav"
  decode "$name" "$3" "$4" "$5"
}
codec_of() { case $1 in lame*) echo MP3 ;; ffaac* | fdk*) echo AAC ;; opus*) echo Opus ;; vorbis*) echo AAC ;; esac; }
family_of() { case $1 in lame*) echo :LAME ;; ffaac* | fdk*) echo ":ffmpeg or fdk-aac" ;; opus*) echo :libopus ;; vorbis*) echo ;; esac; }
for base in lame128 ffaac128 opus128; do
  codec=$(codec_of $base)
  found="lossy:$codec$(family_of $base)"
  # a cut start moves the grid, so the name the offset gave is gone
  cut="lossy:$codec:none"
  rate=$(rate_of "$work/$base.wav")
  grid="README Features: the grid"
  clip="$limits: clipping hides the grid frame by frame"
  level=$found level_src=$grid changing=$found
  if [ $codec = Opus ]; then
    level_src="README Features: with or without a gain after the decode"
    changing="atmost"
  fi
  treated $base gain-6 "$level" "$level_src" "6 dB down" "volume=-6dB"
  treated $base gain+3 "$level" "$level_src" "3 dB up" "volume=3dB"
  treated $base normalised "$level" "$level_src" "peak normalised to full scale" "volume=2.2"
  treated $base replaygain "$level" "$level_src" "a ReplayGain of -7.3 dB applied" "volume=-7.3dB"
  treated $base fade "$changing" "$grid, $limits: a level that changes over time after an Opus decode" "a 10 s fade in and out" "afade=t=in:d=10,afade=t=out:st=24:d=10"
  treated $base limiter "$changing" "$grid, $limits: a level that changes over time after an Opus decode" "8 dB up into a limiter" "volume=8dB,alimiter=limit=0.9"
  treated $base clipped atmost "$clip" "12 dB up, clipping" "volume=12dB"
  ff -i "$work/$base.wav" -filter_complex "volume=-3dB" -c:a pcm_f32le "$work/$base.float.wav"
  ff -i "$work/$base.float.wav" -af "aresample=osf=s16:dither_method=triangular" -c:a pcm_s16le "$work/$base-then-dither.wav"
  decode "$base-then-dither" "$level" "$level_src" "3 dB down in float, dithered to 16 bits"
  lossless "$work/$base.wav" "B-$base-then-pad24_24-$(khz "$rate").flac"
  note "B-$base-then-pad24_24-$(khz "$rate").flac" "$found" "$grid, README Features: flags bit-padded samples" "padded into 24 bits"
  ff -i "$work/$base.wav" -c:a pcm_u8 "$work/$base.u8.wav"
  to16 "$work/$base.u8.wav" "$base-then-8bit"
  decode "$base-then-8bit" open "nothing says" "truncated to 8 bits, stored at 16"
  other=$((rate == 44100 ? 48000 : 44100))
  if [ $codec = Opus ]; then
    treated $base to-44 "$found" "$table" "converted to 44.1 kHz" "aresample=44100"
    treated $base to-44-and-back atmost "nothing says of two conversions" "to 44.1 kHz and back" "aresample=44100,aresample=48000"
  else
    treated $base to-48 "lossy:$codec" "README Features: finds that grid after a conversion between 44.1 and 48 kHz" "converted to 48 kHz" "aresample=48000"
    treated $base to-48-and-back atmost "nothing says of two conversions" "to 48 kHz and back" "aresample=48000,aresample=44100"
  fi
  treated $base to-96 atmost "$limits: after any other conversion possibly lossy at most" "converted to 96 kHz" "aresample=96000"
  treated $base to-88 atmost "$limits: after any other conversion possibly lossy at most" "converted to 88.2 kHz" "aresample=88200"
  treated $base speed atmost "$limits: after any other conversion possibly lossy at most" "0.1% faster, as off a drifting clock" "asetrate=$rate*1.001,aresample=$rate"
  for n in 1 100 577 10000; do
    treated $base "cut-$n" "$cut" "$grid, $enc: the offsets are those of a decode whose start was not cut" "the first $n samples cut" "atrim=start_sample=$n"
  done
  treated $base silence-first "$cut" "$grid, $enc" "0.701 s of silence put in front, which is no whole number of frames" "adelay=701:all=1"
  treated $base end-cut "$found" "$grid" "the last 3 s cut" "atrim=end=31"
  treated $base track-2 "$cut" "$grid, $enc" "an album decode's second track, cut at sample 123457" "atrim=start_sample=123457"
  treated $base swapped "$found" "$grid" "channels swapped" "pan=stereo|c0=c1|c1=c0"
  treated $base to-mono "$found" "$grid" "folded to mono" "pan=mono|c0=0.5*c0+0.5*c1"
  treated $base mono-to-stereo "$found" "$grid" "folded to mono, copied to both channels" "pan=stereo|c0=0.5*c0+0.5*c1|c1=0.5*c0+0.5*c1"
  treated $base mid-side "$found" "$grid" "mid and side in place of left and right" "pan=stereo|c0=0.5*c0+0.5*c1|c1=0.5*c0-0.5*c1"
  treated $base to-5.1 "$found" "$grid, $limits: reads only the first two channels" "upmixed to 5.1" "pan=5.1|c0=c0|c1=c1|c2=0.5*c0+0.5*c1|c3=0.1*c0|c4=0.7*c0|c5=0.7*c1"
  treated $base analog atmost "$limits: after any other conversion possibly lossy at most" "an analog round trip: an odd rate and back, noise 70 dB down, a low-pass at 20 kHz" "aresample=47117,aresample=$rate,$(both 'x+0.0003*(random(ch)*2-1)'),lowpass=f=20000"
done
# a start cut by 577 samples and a gain, on the other bases
for base in lameV0 lame320 fdk128 vorbis6; do
  codec=$(codec_of $base)
  treated $base cut-577 "lossy:$codec:none" "README Features: the grid, $enc" "the first 577 samples cut" "atrim=start_sample=577"
  treated $base gain-6 "lossy:$codec$(family_of $base)" "README Features: the grid" "6 dB down" "volume=-6dB"
done

# time: joins and mixes
ff -i "$work/lame128.wav" -i "$work/ffaac128.wav" -filter_complex "[0]atrim=end=17[a];[1]atrim=start=17,asetpts=N/SR/TB[b];[a][b]concat=n=2:v=0:a=1" -c:a pcm_s16le "$work/joined-mp3-aac.wav"
decode joined-mp3-aac "lossy:any" "nothing says which codec a join names" "17 s of the MP3 decode, then 17 s of the AAC decode"
ff -i "$work/other128.wav" -i "$work/s44.wav" -filter_complex "[0]atrim=end=12[a];[1]atrim=start=12,asetpts=N/SR/TB[b];[a][b]concat=n=2:v=0:a=1" -c:a pcm_s16le "$work/lame128-intro.wav"
decode lame128-intro "atmost" "$table: possibly lossy, mixed sources" "12 s from lame -b 128, then lossless audio"
ff -i "$work/lame128.wav" -i "$work/o44.wav" -filter_complex "[0]atrim=end=20[a];[1]atrim=start=14,asetpts=N/SR/TB[b];[a][b]acrossfade=d=6" -c:a pcm_s16le "$work/crossfade.wav"
decode crossfade atmost "nothing says of a crossfade" "an MP3 decode crossfaded over 6 s into lossless audio"
ff -i "$work/s44.wav" -i "$work/lame128.wav" -i "$work/o44.wav" -i "$work/ffaac128.wav" -filter_complex "[0][1]acrossfade=d=4[x];[x][2]acrossfade=d=4[y];[y][3]acrossfade=d=4" -c:a pcm_s16le "$work/dj-mix.wav"
decode dj-mix "lossy:any" "$limits: a grid shows that the samples were decoded at some point" "a mix: lossless, an MP3 decode, lossless, an AAC decode, 4 s crossfades"

# lossy twice
twice="nothing says which codec two generations name"
ff -i "$work/lame128.wav" -c:a aac -b:a 128k "$work/t1.m4a"
to16 "$work/t1.m4a" mp3-then-aac
decode mp3-then-aac "lossy:any" "$twice" "MP3 128, decoded, AAC 128, decoded"
lame --quiet -b 128 "$work/ffaac128.wav" "$work/t2.mp3"
to16 "$work/t2.mp3" aac-then-mp3
decode aac-then-mp3 "lossy:any" "$twice" "AAC 128, decoded, MP3 128, decoded"
lame --quiet -b 320 "$work/lame128.wav" "$work/t3.mp3"
to16 "$work/t3.mp3" mp3-128-then-320
decode mp3-128-then-320 "lossy:MP3" "$twice" "MP3 128, decoded, MP3 320, decoded"
ff -i "$work/opus128.wav" -ar 44100 -c:a pcm_s16le "$work/t4.wav"
lame --quiet -b 192 "$work/t4.wav" "$work/t4.mp3"
to16 "$work/t4.mp3" opus-then-mp3
decode opus-then-mp3 "lossy:any" "$twice" "Opus 128, decoded, to 44.1 kHz, MP3 192, decoded"

# the lossy codecs the analyzer does not look for
unlooked="nothing says; a codec not looked for must not be named MP3, AAC or Opus with a family"
ff -i "$work/s44.wav" -c:a mp2 -b:a 192k "$work/x.mp2"
to16 "$work/x.mp2" mp2-192
decode mp2-192 other "$unlooked" "MP2 at 192 kbps (ffmpeg), decoded"
ff -i "$work/s44.wav" -c:a ac3 -b:a 192k "$work/x.ac3"
to16 "$work/x.ac3" ac3-192
decode ac3-192 other "$unlooked" "AC-3 at 192 kbps (ffmpeg), decoded"
ff -i "$work/s44.wav" -c:a wmav2 -b:a 128k "$work/x.wma"
to16 "$work/x.wma" wma-128
decode wma-128 other "$unlooked" "WMA 2 at 128 kbps (ffmpeg), decoded"
ff -i "$work/s44.wav" -c:a sbc -b:a 328k "$work/x.sbc"
to16 "$work/x.sbc" sbc-328
decode sbc-328 other "$unlooked" "SBC at 328 kbps (ffmpeg), decoded"
ff -i "$work/s44.wav" -c:a aptx "$work/x.aptx"
ff -f aptx -sample_rate 44100 -i "$work/x.aptx" -c:a pcm_s16le "$work/aptx.wav"
decode aptx other "$unlooked" "aptX (ffmpeg), decoded"

# the same decode in every lossless format the analyzer reads
same="one decode reads the same in every lossless format"
for file in lame128_16-44.wav lame128_16-44.aiff lame128_16-44.caf lame128_16-44.m4a lame128_16-44.wv lame128_16-44.ape; do
  lossless "$work/lame128.wav" "B-$file"
  note "B-$file" "as:B-lame128_16-44.flac" "$same" "lame -b 128, decoded, as ${file##*.}"
done
ff -i "$work/lame128.wav" -c:a alac "B-lame128-alac_16-44.caf"
note B-lame128-alac_16-44.caf "as:B-lame128_16-44.flac" "$same" "lame -b 128, decoded, as ALAC in CAF"
for level in 1000 3000 4000 5000; do
  mac "$work/lame128.wav" "B-lame128-c${level}_16-44.ape" -c$level > /dev/null 2>&1
  note "B-lame128-c${level}_16-44.ape" "as:B-lame128_16-44.flac" "$same" "lame -b 128, decoded, as Monkey's Audio at level $level"
done

# group C: containers and streams. the honest noise and the MP3 decode in every wrapper
lossless "$work/s44.wav" C-honest_16-44.flac
note C-honest_16-44.flac honest "$table: lossless reads consistent" "the noise as FLAC"
wraps="one reading whatever the container"
wrap() { note "$1" "as:$2" "$wraps" "$3"; }
for base in honest lame128; do
  src=$work/s44.wav ref=C-honest_16-44.flac
  [ $base = lame128 ] && src=$work/lame128.wav ref=B-lame128_16-44.flac
  ff -i "$src" -c:a flac -f ogg "C-$base-flac-in-ogg.oga"
  wrap "C-$base-flac-in-ogg.oga" $ref "FLAC in Ogg"
  ff -i "$src" -c:a flac -strict -2 "C-$base-flac-in-mp4.mp4"
  wrap "C-$base-flac-in-mp4.mp4" $ref "FLAC in MP4"
  ff -i "$src" -c:a flac "C-$base-flac-in-matroska.mka"
  wrap "C-$base-flac-in-matroska.mka" $ref "FLAC in Matroska"
  ff -i "$src" -c:a pcm_s16le "C-$base-plain.wav"
  wrap "C-$base-plain.wav" $ref "plain WAV"
  ff -i "$src" -c:a pcm_s24le "C-$base-extensible.wav"
  wrap "C-$base-extensible.wav" $ref "WAVE_FORMAT_EXTENSIBLE at 24 bits"
  ff -i "$src" -c:a pcm_s16le -rf64 always "C-$base-rf64.wav"
  wrap "C-$base-rf64.wav" $ref "RF64"
  ff -i "$src" -c:a pcm_s16le -f w64 "C-$base-wave64.w64"
  wrap "C-$base-wave64.w64" $ref "Wave64"
  ff -i "$src" -c:a pcm_s16le -write_bext 1 -metadata description="torture" -metadata comment="$(head -c 60000 /dev/zero | tr '\0' x)" "C-$base-broadcast.wav"
  wrap "C-$base-broadcast.wav" $ref "broadcast WAV with bext and a 60 KB LIST"
  ff -i "$src" -c:a pcm_s16be "C-$base.aiff"
  wrap "C-$base.aiff" $ref "AIFF"
  ff -i "$src" -c:a pcm_s16le -f aiff "C-$base-sowt.aifc"
  wrap "C-$base-sowt.aifc" $ref "AIFF-C sowt"
  ff -i "$src" -c:a pcm_s16be "C-$base.caf"
  wrap "C-$base.caf" $ref "CAF"
done
cp "$work/opus128.opus" C-opus.opus
note C-opus.opus "consistent" "$table: consistent with its bitrate" "opusenc --bitrate 128"
for file in C-opus-in-webm.webm C-opus-in-matroska.mka C-opus-in-mp4.mp4; do
  ff -i "$work/opus128.opus" -c:a copy "$file"
  wrap "$file" C-opus.opus "the Opus stream copied into ${file##*.}"
done

# what taggers and rippers do
flac --silent --force -o "$work/ref.flac" "$work/s44.wav" 2> /dev/null
first=$(find_at "$work/ref.flac" '\xff\xf8' 42)
tagged="a tag is not audio: the reading is the untagged file's"
{ printf 'ID3\x04\x00\x00\x00\x00\x10\x00'; head -c 2048 /dev/zero; cat C-honest_16-44.flac; } > C-id3v2-before-flac.flac
note C-id3v2-before-flac.flac "as:C-honest_16-44.flac" "$tagged" "an ID3v2 tag of 2 KB in front of the FLAC"
{ printf 'ID3\x04\x00\x00\x7f\x7f\x7f\x7f'; head -c 2048 /dev/zero; cat C-honest_16-44.flac; } > C-id3v2-size-lies.flac
note C-id3v2-size-lies.flac limited "a damaged file reads with a limitation or is not read" "an ID3v2 tag that states 256 MB in front of the FLAC"
cp "$work/lame128.mp3" C-lame128.mp3
note C-lame128.mp3 consistent "$table: consistent with its bitrate" "lame -b 128"
ff -i "$work/lame128.mp3" -c:a copy -write_id3v1 1 -write_apetag 1 -metadata title=torture C-id3v1-ape-after.mp3
note C-id3v1-ape-after.mp3 "as:C-lame128.mp3" "$tagged" "ID3v1 and APE tags after the MP3"
wavpack -q -y -w "Title=torture" -w "Comment=$(head -c 5000 /dev/zero | tr '\0' x)" "$work/s44.wav" -o C-ape-tag-after.wv
note C-ape-tag-after.wv "as:C-honest_16-44.flac" "$tagged" "an APE tag after the WavPack file"
ff -f lavfi -i "nullsrc=s=2700x2600,geq=random(1)*255:128:128" -frames:v 1 -pix_fmt bgr24 "$work/cover.bmp"
ff -i "$work/lame128.mp3" -i "$work/cover.bmp" -map 0:a -map 1:v -c:a copy -c:v copy -id3v2_version 3 -disposition:v attached_pic C-cover-art-20mb.mp3
note C-cover-art-20mb.mp3 "as:C-lame128.mp3" "$tagged" "20 MB of cover art in the MP3's ID3v2 tag"
flac --silent --force --no-seektable -o C-no-seektable.flac "$work/s44.wav" 2> /dev/null
note C-no-seektable.flac "as:C-honest_16-44.flac" "$wraps" "flac --no-seektable"
cp "$work/ref.flac" C-seektable-wrong.flac
points=$(($(od -An -tu1 -j 43 -N 3 C-seektable-wrong.flac | awk '{ print $1 * 65536 + $2 * 256 + $3 }') / 18))
for point in $(seq 1 $((points - 1))); do
  # every seek point states the first frame's offset
  poke C-seektable-wrong.flac $((46 + point * 18 + 8)) '\x00\x00\x00\x00\x00\x00\x00\x00'
done
note C-seektable-wrong.flac "as:C-honest_16-44.flac" "$wraps" "a seek table whose every point is the first frame"
ff -i "$work/s44.wav" -c:a flac -f flac pipe:1 > C-flac-piped.flac
note C-flac-piped.flac limited "rustdoc Limitation::GridNotScanned" "a FLAC written to a pipe, which states no length"
ff -i "$work/s44.wav" -c:a pcm_s16le -f wav pipe:1 > C-wav-piped.wav
note C-wav-piped.wav limited "rustdoc Limitation::GridNotScanned" "a WAV written to a pipe, its data size 0xFFFFFFFF"
cp C-honest-plain.wav C-wav-size-0.wav
poke C-wav-size-0.wav $(($(find_at C-wav-size-0.wav 'data') + 4)) '\x00\x00\x00\x00'
note C-wav-size-0.wav limited "rustdoc Limitation::GridNotScanned" "a WAV whose data size is 0"
cp "$work/lame128.mp3" C-xing-lies.mp3
poke C-xing-lies.mp3 $(($(find_at C-xing-lies.mp3 'Xing|Info') + 8)) "$(be32 12)"
note C-xing-lies.mp3 limited "rustdoc Limitation::LongerThanStated" "a Xing header that states 12 frames"
cp "$work/lame128.mp3" C-xing-lies-long.mp3
poke C-xing-lies-long.mp3 $(($(find_at C-xing-lies-long.mp3 'Xing|Info') + 8)) "$(be32 4000000)"
note C-xing-lies-long.mp3 limited "rustdoc Limitation::LengthPastBytes" "a Xing header that states 4,000,000 frames"
lame --quiet --freeformat -b 300 "$work/s44.wav" C-freeformat.mp3
note C-freeformat.mp3 open "nothing says of free-format MP3" "lame --freeformat -b 300"
for rate in 8 11.025 12 16 22.05 24; do
  lame --quiet --resample $rate -b 32 "$work/s44.wav" "C-mpeg2-$rate.mp3"
  note "C-mpeg2-$rate.mp3" open "README Supported formats: MP3; nothing says of MPEG-2 and 2.5 rates" "lame --resample $rate -b 32"
done

# what muxers do
ff -i "$work/s44.wav" -c:a alac "C-moov-last.m4a"
ff -i "$work/s44.wav" -c:a alac -movflags +faststart "C-moov-first.m4a"
ff -i "$work/s44.wav" -c:a alac -movflags frag_keyframe+empty_moov -frag_duration 2000000 "C-fragmented.m4a"
wrap C-moov-last.m4a C-honest_16-44.flac "ALAC, moov at the end"
wrap C-moov-first.m4a C-honest_16-44.flac "ALAC, moov at the start"
wrap C-fragmented.m4a C-honest_16-44.flac "ALAC in a fragmented MP4"
ff -ss 1 -i C-moov-last.m4a -c:a copy C-edit-list.m4a
note C-edit-list.m4a honest "$never" "ALAC with an edit list that skips the first second"
ff -f lavfi -i "testsrc=d=34:s=160x120:r=5" -i "$work/s44.wav" -c:v mpeg4 -c:a alac C-with-video.mp4
wrap C-with-video.mp4 C-honest_16-44.flac "ALAC beside a video track in MP4"
ff -f lavfi -i "testsrc=d=34:s=160x120:r=5" -i "$work/s44.wav" -c:v mpeg4 -c:a flac C-with-video.mkv
wrap C-with-video.mkv C-honest_16-44.flac "FLAC beside a video track in Matroska"
ff -i "$work/lame128.mp3" -i "$work/s44.wav" -map 0:a -map 1:a -c:a:0 copy -c:a:1 flac C-two-tracks-mp3-flac.mka
note C-two-tracks-mp3-flac.mka open "nothing says which of two audio tracks is read" "Matroska: an MP3 track, then a FLAC track"
ff -i "$work/s44.wav" -i "$work/lame128.mp3" -map 0:a -map 1:a -c:a:0 flac -c:a:1 copy C-two-tracks-flac-mp3.mka
note C-two-tracks-flac-mp3.mka open "nothing says which of two audio tracks is read" "Matroska: a FLAC track, then an MP3 track"
oggenc --quiet -q 6 -o "$work/v48.ogg" "$work/s48.wav"
cat "$work/vorbis6.ogg" "$work/v48.ogg" > C-chained-two-rates.ogg
note C-chained-two-rates.ogg limited "nothing says of a chained Ogg; a reading must say what it left out" "two Vorbis streams chained, at 44.1 and at 48 kHz"
cat "$work/vorbis6.ogg" "$work/opus128.opus" > C-chained-vorbis-opus.ogg
note C-chained-vorbis-opus.ogg limited "nothing says of a chained Ogg; a reading must say what it left out" "a Vorbis stream, then an Opus stream, chained"
lame --quiet -b 128 -m m --resample 32 "$work/s44.wav" "$work/mono32.mp3"
cat "$work/lame128.mp3" "$work/mono32.mp3" > C-mp3-changes.mp3
note C-mp3-changes.mp3 limited "nothing says of a stream that changes; a reading must say what it left out" "an MP3 that turns from 44.1 kHz stereo to 32 kHz mono half way"

# what disks and networks do. expectation: the exit status of the help, and a limitation on
# any reading of a file not read whole
damage="a damaged file reads with a limitation or is not read"
: > C-empty.flac
note C-empty.flac unread "check --help: a file the analyzer cannot open as audio" "zero bytes"
head -c "$first" "$work/ref.flac" > C-header-only.flac
note C-header-only.flac limited "$damage" "a FLAC's metadata and no frame"
declare -A whole=([flac]=$work/ref.flac [mp3]=$work/lame128.mp3 [wav]=C-honest-plain.wav [m4a]=C-moov-first.m4a [wv]=B-lame128_16-44.wv [ape]=B-lame128_16-44.ape [opus]=C-opus.opus [ogg]=$work/vorbis6.ogg)
for ext in flac mp3 wav m4a wv ape opus ogg; do
  bytes=$(size "${whole[$ext]}")
  for percent in 10 50 99; do
    head -c $((bytes * percent / 100)) "${whole[$ext]}" > "C-cut-$percent.$ext"
    note "C-cut-$percent.$ext" limited "$damage" "a $ext file cut at $percent% of its bytes"
  done
  { cat "${whole[$ext]}"; head -c 100000 /dev/urandom; } > "C-garbage-after.$ext"
  note "C-garbage-after.$ext" open "nothing says of bytes after the audio" "100 KB of random bytes after a $ext file"
  cp "${whole[$ext]}" "C-hole.$ext"
  dd if=/dev/zero of="C-hole.$ext" bs=1024 seek=$((bytes / 2048)) count=256 conv=notrunc status=none
  note "C-hole.$ext" limited "$damage" "256 KB of zeros in the middle of a $ext file"
  { tail -c +$((bytes / 2 + 1)) "${whole[$ext]}"; head -c $((bytes / 2)) "${whole[$ext]}"; } > "C-halves-swapped.$ext"
  note "C-halves-swapped.$ext" limited "$damage" "the two halves of a $ext file swapped"
done
second=$(find_at "$work/ref.flac" '\xff\xf8' $((first + 2000)))
cp "$work/ref.flac" C-flip-frame-header.flac
flip C-flip-frame-header.flac $((second + 2))
note C-flip-frame-header.flac limited "$damage" "one bit flipped in a FLAC frame's header"
cp "$work/ref.flac" C-flip-audio.flac
flip C-flip-audio.flac $((second + 500))
note C-flip-audio.flac limited "$damage" "one bit flipped in a FLAC frame's audio, so its CRC fails"
cp "$work/lame128.mp3" C-flip-audio.mp3
flip C-flip-audio.mp3 $(($(size "$work/lame128.mp3") / 2))
note C-flip-audio.mp3 open "MP3 frames have no checksum here: nothing can tell" "one bit flipped in the middle of an MP3"
seq 1 20000 > C-text.flac
note C-text.flac unread "check --help: a file the analyzer cannot open as audio" "a text file named .flac"
ff -f lavfi -i "testsrc=s=640x480" -frames:v 1 "$work/x.jpg"
cp "$work/x.jpg" C-jpeg.flac
note C-jpeg.flac unread "check --help: a file the analyzer cannot open as audio" "a JPEG named .flac"
printf 'FILE "album.flac" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n' > C-cue.flac
note C-cue.flac unread "check --help: a file the analyzer cannot open as audio" "a cue sheet named .flac"
# every format under every other format's extension
declare -A named=([flac]=C-honest_16-44.flac [wav]=C-honest-plain.wav [m4a]=C-moov-first.m4a [mp3]=C-lame128.mp3 [ogg]=$work/vorbis6.ogg [opus]=C-opus.opus [wv]=B-lame128_16-44.wv [ape]=B-lame128_16-44.ape [aiff]=C-honest.aiff [caf]=C-honest.caf [mka]=C-honest-flac-in-matroska.mka)
cp "$work/vorbis6.ogg" C-vorbis.ogg
note C-vorbis.ogg open "README Supported formats: Vorbis" "oggenc -q 6"
named[ogg]=C-vorbis.ogg
for is in "${!named[@]}"; do
  for ext in "${!named[@]}"; do
    [ "$is" = "$ext" ] && continue
    cp "${named[$is]}" "C-$is-named.$ext"
    note "C-$is-named.$ext" "as:${named[$is]}" "the reading must not depend on the extension" "a $is file named .$ext"
  done
done

# what formats allow at the edge
supported="README Supported formats"
wavpack -q -y -b256 "$work/s44.wav" -o C-wavpack-hybrid.wv
note C-wavpack-hybrid.wv "refused:hybrid WavPack" "$supported" "wavpack -b256"
ff -i "$work/s44.wav" -c:a pcm_u8 "$work/u8s.wav"
wavpack -q -y "$work/u8s.wav" -o C-wavpack-8bit.wv
note C-wavpack-8bit.wv "refused:8-bit" "$supported" "wavpack of 8-bit samples"
ff -i "$work/s44.wav" -filter_complex "pan=5.1|c0=c0|c1=c1|c2=c0|c3=c1|c4=c0|c5=c1" -c:a pcm_s16le "$work/six.wav"
wavpack -q -y "$work/six.wav" -o C-wavpack-6ch.wv
note C-wavpack-6ch.wv "refused:channels" "$supported" "wavpack of six channels"
lossless "$work/m44.wav" C-wavpack-float_f32-44.wv
note C-wavpack-float_f32-44.wv honest "$supported: WavPack as float" "wavpack of float samples"
ff -i "$work/m44.wav" -c:a pcm_s32le "$work/s32.wav"
wavpack -q -y "$work/s32.wav" -o C-wavpack-32bit.wv
note C-wavpack-32bit.wv honest "$supported: WavPack at 32 bits" "wavpack of 32-bit samples"
mac "$work/s32.wav" C-ape-32bit.ape -c2000 > /dev/null 2>&1
note C-ape-32bit.ape "refused:32-bit" "$supported" "mac of 32-bit samples"
mac "$work/m44.wav" C-ape-float.ape -c2000 > /dev/null 2>&1
note C-ape-float.ape "refused:float" "$supported" "mac of float samples"
mac "$work/u8s.wav" C-ape-8bit.ape -c2000 > /dev/null 2>&1
note C-ape-8bit.ape honest "$supported: Monkey's Audio at 8 bits" "mac of 8-bit samples"
mac "$work/six.wav" C-ape-6ch.ape -c2000 > /dev/null 2>&1
note C-ape-6ch.ape honest "$supported, $limits: reads only the first two channels" "mac of six channels"
fdkaac -S -p 23 -b 128 -o C-aac-ld.m4a "$work/s44.wav"
note C-aac-ld.m4a open "$supported lists AAC-LC, HE-AAC and HE-AAC v2 only" "fdkaac -p 23 (AAC-LD)"
fdkaac -S -p 39 -b 128 -o C-aac-eld.m4a "$work/s44.wav"
note C-aac-eld.m4a open "$supported lists AAC-LC, HE-AAC and HE-AAC v2 only" "fdkaac -p 39 (AAC-ELD)"
fdkaac -S -p 5 -b 64 -o C-he-aac.m4a "$work/s44.wav"
note C-he-aac.m4a open "$supported: HE-AAC with the band its decoder rebuilds" "fdkaac -p 5 -b 64"
fdkaac -S -p 29 -b 32 -o C-he-aac-v2.m4a "$work/s44.wav"
note C-he-aac-v2.m4a open "$supported: HE-AAC v2" "fdkaac -p 29 -b 32"
fdkaac -S -p 2 -b 128 -f 2 -o C-aac-adts.aac "$work/s44.wav"
note C-aac-adts.aac consistent "$supported: ADTS" "fdkaac -f 2"
for rate in 8000 384000 768000 768001; do
  ff -f lavfi -i "anoisesrc=color=white:seed=4:r=$rate:d=31:a=0.3" -c:a pcm_s16le "C-rate-$rate.wav"
done
note C-rate-8000.wav honest "$never" "white noise at 8 kHz"
note C-rate-384000.wav honest "$never" "white noise at 384 kHz"
note C-rate-768000.wav honest "$never" "white noise at 768 kHz, the highest rate read"
note C-rate-768001.wav "refused:above" "rustdoc Error::Unsupported" "white noise at 768,001 Hz"
ff -i "$work/s44.wav" -filter_complex "pan=7.1|c0=c0|c1=c1|c2=c0|c3=c1|c4=c0|c5=c1|c6=c0|c7=c1" -c:a flac C-8ch.flac
note C-8ch.flac "as:C-honest_16-44.flac" "$limits: reads only the first two channels" "eight channels, the first two the noise"
ff -i "$work/six.wav" -c:a flac C-6ch.flac
note C-6ch.flac "as:C-honest_16-44.flac" "$limits: reads only the first two channels" "six channels, the first two the noise"

# DSD at 2.8224 MHz, two channels of the pattern that is silence: bytes a channel, file. DSF
# holds them in blocks of 4096 bytes a channel, DSDIFF a byte a channel in turn
dsd() {
  local bytes=$1 data=$(($1 * 2))
  {
    printf 'DSD '"$(le64 28)$(le64 $((92 + data)))$(le64 0)"
    printf 'fmt '"$(le64 52)$(le32 1)$(le32 0)$(le32 2)$(le32 2)$(le32 2822400)$(le32 1)$(le64 $((bytes * 8)))$(le32 4096)$(le32 0)"
    printf 'data'"$(le64 $((12 + data)))"
    head -c "$data" /dev/zero | tr '\0' '\151'
  } > "$2.dsf"
  {
    printf 'FRM8'"$(be64 $((4 + 16 + 86 + 12 + data)))"'DSD '
    printf 'FVER'"$(be64 4)"'\x01\x05\x00\x00'
    printf 'PROP'"$(be64 74)"'SND '
    printf 'FS  '"$(be64 4)$(be32 2822400)"
    printf 'CHNL'"$(be64 10)"'\x00\x02SLFTSRGT'
    printf 'CMPR'"$(be64 20)"'DSD \x0enot compressed\x00'
    printf 'DSD '"$(be64 $data)"
    head -c "$data" /dev/zero | tr '\0' '\151'
  } > "$2.dff"
}
dsd $((34 * 352800)) C-dsd-silence
note C-dsd-silence.dsf honest "$little" "34 s of DSD64 silence as DSF"
note C-dsd-silence.dff honest "$little" "34 s of DSD64 silence as DSDIFF"

# seeds for the fuzz targets, which `just fuzz-seed` copies: a second or less in each format
mkdir -p seeds
from second m44 "atrim=end=1,aresample=16000,pan=mono|c0=c0"
ff -i "$work/second.wav" -c:a pcm_s16le "$work/second16.wav"
lossless "$work/second16.wav" seeds/second_16-16.flac
cp "$work/second16.wav" seeds/second.wav
lossless "$work/second16.wav" seeds/second_16-16.wv
lossless "$work/second16.wav" seeds/second_16-16.ape
lame --quiet -b 32 "$work/second16.wav" seeds/second.mp3
oggenc --quiet -q 2 -o seeds/second.ogg "$work/second16.wav"
opusenc --quiet --bitrate 24 "$work/second16.wav" seeds/second.opus
ff -i seeds/second.opus -c:a copy seeds/second-opus.webm
ff -i seeds/second.opus -c:a copy seeds/second-opus.mp4
fdkaac -S -b 32 -o seeds/second-aac.m4a "$work/second16.wav"
fdkaac -S -b 32 -f 2 -o seeds/second.aac "$work/second16.wav"
dsd 16384 seeds/second

# group D: length
grid=40960
for frames in $((grid - 1)) $grid $((grid * 4)); do
  from "frames-$frames" m44 "atrim=end_sample=$frames"
  lossless "$work/frames-$frames.wav" "D-frames-${frames}_16-44.flac"
  note "D-frames-${frames}_16-44.flac" honest "$little" "$frames sample frames of the noise"
done
from half-second m44 "atrim=end=0.5"
lossless "$work/half-second.wav" D-half-second_16-44.flac
note D-half-second_16-44.flac honest "$little" "half a second of the noise"
from short-decode lame128 "atrim=end=4"
lossless "$work/short-decode.wav" D-short-decode_16-44.flac
note D-short-decode_16-44.flac open "$little; nothing says whether a grid in 4 s is a likely transcode" "4 s of the MP3 decode"

if [ "$long" = long ]; then
  # a tone under quiet noise packs small, so the hours cost little disk
  for hours in 1 6 24; do
    ff -f lavfi -i "sine=f=440:r=44100:d=$((hours * 3600))" -af "volume=2,aeval='val(0)+0.001*(random(0)*2-1)':c=same,pan=stereo|c0=c0|c1=c0" -c:a flac "D-hours-${hours}_16-44.flac"
    note "D-hours-${hours}_16-44.flac" honest "rustdoc Coverage::Full: time and measures grow with the length" "$hours hours of a tone under noise"
  done
  # 8-bit mono at 44.1 kHz past 2 GB and past 4 GB, the samples a hole in the file
  for gb in 2 4; do
    file=D-wav-past-${gb}gb.wav
    ff -i "$work/u8.wav" -ac 1 -map_metadata -1 -fflags +bitexact -c:a pcm_u8 "$file"
    truncate -s $((gb * 1073741824 + 65536)) "$file"
    at=$(find_at "$file" 'data')
    # the sizes a writer that counts in 32 bits leaves
    poke "$file" 4 "$(le32 $((($(size "$file") - 8) & 0xffffffff)))"
    poke "$file" $((at + 4)) "$(le32 $((($(size "$file") - at - 8) & 0xffffffff)))"
    note "$file" limited "a WAV's sizes are 32 bits: past them the header cannot state the length" "8-bit mono WAV of $gb GB, sparse"
  done
fi

sort -o "$manifest" "$manifest"
echo "$(wc -l < "$manifest") cases in $manifest"
